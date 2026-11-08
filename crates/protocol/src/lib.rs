//! Wire types shared by mdl-daemon, mdl-cli and (later) the native-messaging shim.
//!
//! Transport: newline-delimited JSON over the named pipe [`PIPE_NAME`].
//! The client sends one [`Request`] per line; the daemon answers each with one [`Reply`] line.
//! After `Subscribe`, the daemon keeps pushing [`Reply::Event`] lines until the client disconnects.

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\mdl";

pub type JobId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Add {
        url: String,
        /// Empty = the daemon's default (%USERPROFILE%\Downloads).
        #[serde(default)]
        dest_dir: String,
        /// Suggested name; the daemon sanitizes it (no paths, no reserved characters).
        #[serde(default)]
        filename: Option<String>,
        /// Extra HTTP headers (cookies, referer) captured by the browser extension.
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// `url` is a web page (YouTube, Facebook, ...), not a media file: the daemon asks yt-dlp
        /// for the real stream URLs. The reply arrives once that lookup is done (a few seconds).
        #[serde(default)]
        resolve: bool,
    },
    List,
    Pause { id: JobId },
    Resume { id: JobId },
    /// Stop and delete partial data.
    Cancel { id: JobId },
    /// Remove a finished/failed/cancelled job from the list (file on disk is kept).
    Remove { id: JobId },
    /// Open Explorer with the job's file (or its folder) selected.
    Reveal { id: JobId },
    Subscribe,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Added { job: JobSnapshot },
    Jobs { jobs: Vec<JobSnapshot> },
    Error { message: String },
    Event { event: Event },
    Pong { version: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Sent ~2×/s for each running job.
    Progress { id: JobId, downloaded: u64, total: u64, speed: u64 },
    /// Sent whenever a job changes state (includes the full snapshot).
    Changed { job: JobSnapshot },
    Removed { id: JobId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    /// Joining separately downloaded video and audio (ffmpeg, a few seconds).
    Muxing,
    Paused,
    Done,
    Failed { error: String },
    Cancelled,
}

impl JobState {
    pub fn is_finished(&self) -> bool {
        matches!(self, JobState::Done | JobState::Failed { .. } | JobState::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub url: String,
    pub dest: String,
    pub state: JobState,
    pub downloaded: u64,
    pub total: u64,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Set for jobs added with `resolve`: the page to re-resolve on every (re)start, because the
    /// media URLs it yields expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape_is_stable() {
        let r: Request = serde_json::from_str(r#"{"cmd":"pause","id":3}"#).unwrap();
        assert!(matches!(r, Request::Pause { id: 3 }));
        let r: Request = serde_json::from_str(r#"{"cmd":"add","url":"u","dest_dir":"d"}"#).unwrap();
        assert!(matches!(r, Request::Add { filename: None, .. }));
    }

    #[test]
    fn state_serializes_tagged() {
        let s = serde_json::to_string(&JobState::Failed { error: "x".into() }).unwrap();
        assert_eq!(s, r#"{"state":"failed","error":"x"}"#);
    }
}
