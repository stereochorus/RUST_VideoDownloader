//! JobManager: owns every download, its state machine, scheduling and persistence.
//!
//! State machine:
//!   Queued ──schedule──► Running ──ok──► Done
//!     ▲  │                 │  │ └─error─► Failed ──resume──► Queued
//!     │  └──pause──► Paused ◄─┘pause
//!     └────resume─────┘     any non-finished ──cancel──► Cancelled (partial files deleted)
//!
//! Pause = set the engine's cancel flag. The engine stops at the next read, and since every
//! finished chunk is already recorded in the sidecar, resume simply runs the download again.

use crate::resolve::{self, Kind, Resolved};
use anyhow::{Context, Result, bail};
use mdl_engine::{Cancelled, Downloader, Options, Progress, filename_from_url, is_cancelled};
use mdl_protocol::{Event, JobId, JobSnapshot, JobState};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

pub const MAX_ACTIVE: usize = 3;

struct Job {
    snap: JobSnapshot,
    progress: Option<Arc<Progress>>,
    running: bool, // a download task exists (may still be winding down after pause)
    last_bytes: u64,
    /// Streams resolved when the job was added, used once by the first run (later runs re-resolve).
    cached: Option<Resolved>,
}

impl Job {
    fn new(snap: JobSnapshot, cached: Option<Resolved>) -> Self {
        Self { snap, progress: None, running: false, last_bytes: 0, cached }
    }
}

struct Inner {
    jobs: BTreeMap<JobId, Job>,
    next_id: JobId,
}

pub struct Manager {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
    store: PathBuf,
}

impl Manager {
    /// Load jobs from `store` (jobs.json). Jobs that were active when the daemon died are re-queued.
    pub fn load(store: PathBuf) -> Arc<Self> {
        let saved: Vec<JobSnapshot> = std::fs::read(&store)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut jobs = BTreeMap::new();
        for mut snap in saved {
            if matches!(snap.state, JobState::Running | JobState::Muxing) {
                snap.state = JobState::Queued;
            }
            jobs.insert(snap.id, Job::new(snap, None));
        }
        let next_id = jobs.keys().max().map_or(1, |m| m + 1);
        let (events, _) = broadcast::channel(256);
        let mgr = Arc::new(Self { inner: Mutex::new(Inner { jobs, next_id }), events, store });
        mgr.schedule();
        mgr
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn list(&self) -> Vec<JobSnapshot> {
        self.inner.lock().unwrap().jobs.values().map(|j| j.snap.clone()).collect()
    }

    pub fn add(
        self: &Arc<Self>,
        url: String,
        dest_dir: String,
        filename: Option<String>,
        headers: Vec<(String, String)>,
    ) -> Result<JobSnapshot> {
        check_url(&url)?;
        let name = filename.unwrap_or_else(|| filename_from_url(&url));
        self.insert(url, None, &dest_dir, &name, headers, None)
    }

    /// Add a web page (YouTube, Facebook, ...): resolve it now so the user gets an immediate error
    /// for unsupported pages, and so the file can be named after the real video title.
    pub async fn add_page(
        self: &Arc<Self>,
        page_url: String,
        dest_dir: String,
        filename: Option<String>,
        headers: Vec<(String, String)>,
    ) -> Result<JobSnapshot> {
        check_url(&page_url)?;
        let resolved = resolve::resolve(&page_url, cookie_of(&headers)).await?;
        if resolved.streams.len() > 1 && !resolve::have_tool("ffmpeg") {
            bail!("This video has separate video and audio, which needs ffmpeg. Install it with:  winget install Gyan.FFmpeg");
        }
        let title = filename.unwrap_or_else(|| resolved.title.clone());
        let name = if Path::new(&title).extension().is_some_and(|e| e.eq_ignore_ascii_case(&resolved.ext)) {
            title
        } else {
            format!("{title}.{}", resolved.ext)
        };
        self.insert(page_url.clone(), Some(page_url), &dest_dir, &name, headers, Some(resolved))
    }

    fn insert(
        self: &Arc<Self>,
        url: String,
        page_url: Option<String>,
        dest_dir: &str,
        name: &str,
        headers: Vec<(String, String)>,
        cached: Option<Resolved>,
    ) -> Result<JobSnapshot> {
        let dest_dir = if dest_dir.trim().is_empty() { default_dir() } else { PathBuf::from(dest_dir.trim()) };
        if !dest_dir.is_absolute() {
            bail!("save folder must be an absolute path, got {}", dest_dir.display());
        }
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            let name = sanitize_filename(name);
            let taken: Vec<PathBuf> = inner.jobs.values().map(|j| PathBuf::from(&j.snap.dest)).collect();
            let dest = unique_path(&dest_dir.join(name), &taken);
            let id = inner.next_id;
            inner.next_id += 1;
            let snap = JobSnapshot {
                id,
                url,
                dest: dest.to_string_lossy().into_owned(),
                state: JobState::Queued,
                downloaded: 0,
                total: 0,
                headers,
                page_url,
            };
            inner.jobs.insert(id, Job::new(snap.clone(), cached));
            snap
        };
        self.changed(&snap);
        self.schedule();
        Ok(snap)
    }

    pub fn pause(self: &Arc<Self>, id: JobId) -> Result<()> {
        self.transition(id, |job| match job.snap.state {
            JobState::Running | JobState::Queued => {
                if let Some(p) = &job.progress {
                    p.cancel.store(true, Ordering::Relaxed);
                }
                job.snap.state = JobState::Paused;
                Ok(())
            }
            ref s => bail!("cannot pause a job that is {s:?}"),
        })
    }

    pub fn resume(self: &Arc<Self>, id: JobId) -> Result<()> {
        self.transition(id, |job| match job.snap.state {
            JobState::Paused | JobState::Failed { .. } => {
                job.snap.state = JobState::Queued;
                Ok(())
            }
            ref s => bail!("cannot resume a job that is {s:?}"),
        })?;
        self.schedule();
        Ok(())
    }

    pub fn cancel(self: &Arc<Self>, id: JobId) -> Result<()> {
        let mut delete_now = None;
        self.transition(id, |job| {
            if job.snap.state.is_finished() {
                bail!("job already finished");
            }
            if let Some(p) = &job.progress {
                p.cancel.store(true, Ordering::Relaxed);
            }
            job.snap.state = JobState::Cancelled;
            if !job.running {
                delete_now = Some(job.snap.dest.clone());
            }
            Ok(())
        })?;
        if let Some(dest) = delete_now {
            delete_partial(&dest); // otherwise the task deletes them once it has stopped
        }
        Ok(())
    }

    /// Show the job's file in Explorer (falls back to its folder while still downloading).
    pub fn reveal(&self, id: JobId) -> Result<()> {
        let dest = {
            let inner = self.inner.lock().unwrap();
            let Some(job) = inner.jobs.get(&id) else { bail!("no job {id}") };
            PathBuf::from(&job.snap.dest)
        };
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("explorer.exe");
        if dest.exists() {
            // Explorer only understands `/select,"C:\path with spaces\f.mp4"`. Rust's normal `arg()`
            // quotes the whole token (`"/select,C:\path with spaces\f.mp4"`), which Explorer misreads
            // and opens Documents instead, so pass it raw.
            cmd.raw_arg(format!("/select,\"{}\"", dest.display()));
        } else {
            cmd.arg(dest.parent().unwrap_or(&dest));
        }
        cmd.spawn()?;
        Ok(())
    }

    pub fn remove(&self, id: JobId) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            match inner.jobs.get(&id) {
                None => bail!("no job {id}"),
                Some(j) if !j.snap.state.is_finished() => bail!("pause or cancel job {id} before removing it"),
                Some(_) => inner.jobs.remove(&id),
            };
        }
        let _ = self.events.send(Event::Removed { id });
        self.persist();
        Ok(())
    }

    /// Apply `f` to job `id` under the lock, then persist + broadcast the new snapshot.
    fn transition(&self, id: JobId, f: impl FnOnce(&mut Job) -> Result<()>) -> Result<()> {
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            let Some(job) = inner.jobs.get_mut(&id) else { bail!("no job {id}") };
            f(job)?;
            job.snap.clone()
        };
        self.changed(&snap);
        Ok(())
    }

    /// Start queued jobs (oldest first) until MAX_ACTIVE downloads are running.
    fn schedule(self: &Arc<Self>) {
        let mut started = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let mut active = inner.jobs.values().filter(|j| j.running).count();
            for job in inner.jobs.values_mut() {
                if active >= MAX_ACTIVE {
                    break;
                }
                // `!running` guards against a paused task that is still winding down.
                if job.snap.state == JobState::Queued && !job.running {
                    let cached = job.cached.take();
                    let progress = Arc::new(Progress::default());
                    job.progress = Some(progress.clone());
                    job.running = true;
                    job.last_bytes = job.snap.downloaded;
                    job.snap.state = JobState::Running;
                    active += 1;
                    started.push((job.snap.clone(), progress, cached));
                }
            }
        }
        for (snap, progress, cached) in started {
            self.changed(&snap);
            let mgr = self.clone();
            tokio::spawn(async move {
                let result = run_download(&mgr, &snap, progress.clone(), cached).await;
                mgr.finished(snap.id, result, &progress);
            });
        }
    }

    /// Called when a download task ends, for any reason.
    fn finished(self: &Arc<Self>, id: JobId, result: Result<PathBuf>, progress: &Progress) {
        let mut delete = None;
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            let Some(job) = inner.jobs.get_mut(&id) else { return };
            job.running = false;
            job.progress = None;
            // Only trust the counters if this run got far enough to learn the size;
            // an early failure (e.g. probe error) must not wipe the progress we knew about.
            if progress.total.load(Ordering::Relaxed) > 0 {
                job.snap.downloaded = progress.downloaded.load(Ordering::Relaxed);
                job.snap.total = progress.total.load(Ordering::Relaxed);
            }
            match result {
                Ok(_) => job.snap.state = JobState::Done,
                // Stopped on request: keep whatever state pause/cancel already set.
                Err(e) if is_cancelled(&e) => {}
                Err(e) if matches!(job.snap.state, JobState::Running | JobState::Muxing) => {
                    job.snap.state = JobState::Failed { error: format!("{e:#}") };
                }
                Err(_) => {} // failed while being paused/cancelled — the user's intent wins
            }
            if job.snap.state == JobState::Cancelled {
                delete = Some(job.snap.dest.clone());
            }
            job.snap.clone()
        };
        if let Some(dest) = delete {
            delete_partial(&dest);
        }
        self.changed(&snap);
        self.schedule(); // a slot freed up, or a quick pause→resume is waiting
    }

    /// Broadcast progress for running jobs. Runs forever; call once.
    pub async fn progress_ticker(self: Arc<Self>) {
        const TICK: Duration = Duration::from_millis(500);
        loop {
            tokio::time::sleep(TICK).await;
            let updates: Vec<Event> = {
                let mut inner = self.inner.lock().unwrap();
                inner
                    .jobs
                    .values_mut()
                    .filter_map(|job| {
                        let p = job.progress.as_ref()?;
                        let downloaded = p.downloaded.load(Ordering::Relaxed);
                        let total = p.total.load(Ordering::Relaxed);
                        if total == 0 {
                            return None; // still probing; keep showing the last known progress
                        }
                        let speed = downloaded.saturating_sub(job.last_bytes) * 1000 / TICK.as_millis() as u64;
                        job.last_bytes = downloaded;
                        job.snap.downloaded = downloaded;
                        job.snap.total = total;
                        Some(Event::Progress { id: job.snap.id, downloaded, total, speed })
                    })
                    .collect()
            };
            for ev in updates {
                let _ = self.events.send(ev);
            }
        }
    }

    /// Running → Muxing (only if nobody paused/cancelled meanwhile).
    fn set_muxing(&self, id: JobId) {
        let _ = self.transition(id, |job| {
            if job.snap.state == JobState::Running {
                job.snap.state = JobState::Muxing;
            }
            Ok(())
        });
    }

    fn changed(&self, snap: &JobSnapshot) {
        let _ = self.events.send(Event::Changed { job: snap.clone() });
        self.persist();
    }

    fn persist(&self) {
        let snaps = self.list();
        let tmp = self.store.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(&snaps)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(&tmp, b)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, &self.store)?));
        if let Err(e) = res {
            eprintln!("warning: could not save job list: {e:#}");
        }
    }
}

async fn run_download(mgr: &Manager, snap: &JobSnapshot, progress: Arc<Progress>, cached: Option<Resolved>) -> Result<PathBuf> {
    let dest = Path::new(&snap.dest);
    let Some(page) = &snap.page_url else {
        let opts = Options { headers: snap.headers.clone(), ..Default::default() };
        return Downloader::new(opts)?.download(&snap.url, dest, progress).await;
    };

    // Page job: media URLs expire, so every run after the first resolves the page again.
    // Resume still works because the engine matches its state file by `page#format`, not by URL.
    let resolved = match cached {
        Some(r) => r,
        None => resolve::resolve(page, cookie_of(&snap.headers)).await?,
    };
    for st in &resolved.streams {
        let names: Vec<&str> = st.headers.iter().map(|(k, _)| k.as_str()).collect();
        let host = st.url.split('/').nth(2).unwrap_or("?");
        eprintln!("job {}: {:?} format {} from {host}, headers: {}", snap.id, st.kind, st.format_id, names.join(","));
        if let Some((_, c)) = st.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
            let cookie_names: Vec<&str> = c.split("; ").map(|p| p.split('=').next().unwrap_or("")).collect();
            eprintln!("  cookie names: {} ({} bytes)", cookie_names.join(","), c.len());
        }
    }
    let wait = resolve::wait_needed(resolved.available_at);
    if !wait.is_zero() {
        tokio::time::sleep(wait).await; // YouTube links answer 403 until their `available_at`
    }
    if progress.cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }

    let fetch = |s: &resolve::Stream, to: PathBuf| {
        let key = format!("{page}#{}", s.format_id);
        let (url, headers, progress) = (s.url.clone(), s.headers.clone(), progress.clone());
        async move {
            // A finished stream from an earlier run whose mux then failed: don't fetch it again.
            if to.is_file() && !part_of(&to).exists() {
                let len = std::fs::metadata(&to)?.len();
                progress.total.fetch_add(len, Ordering::Relaxed);
                progress.downloaded.fetch_add(len, Ordering::Relaxed);
                return Ok(to);
            }
            let opts = Options { headers, ..Default::default() };
            Downloader::new(opts)?.download_keyed(&url, Some(&key), &to, progress).await
        }
    };

    match resolved.streams.as_slice() {
        [single] => fetch(single, dest.to_path_buf()).await,
        streams => {
            let video = streams.iter().find(|s| s.kind == Kind::Video).context("no video stream")?;
            let audio = streams.iter().find(|s| s.kind == Kind::Audio).context("no audio stream")?;
            let (v, a) = tokio::try_join!(fetch(video, side_file(dest, "video")), fetch(audio, side_file(dest, "audio")))?;

            mgr.set_muxing(snap.id);
            let tmp = mux_tmp(dest);
            resolve::mux(&v, &a, &tmp).await?;
            if progress.cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Cancelled.into());
            }
            std::fs::rename(&tmp, dest).with_context(|| format!("renaming to {}", dest.display()))?;
            let _ = std::fs::remove_file(v);
            let _ = std::fs::remove_file(a);
            Ok(dest.to_path_buf())
        }
    }
}

fn check_url(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("only http(s) URLs are supported");
    }
    Ok(())
}

fn cookie_of(headers: &[(String, String)]) -> Option<&str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.as_str())
}

/// `C:\v\Cat.mp4` + "video" → `C:\v\Cat.mp4.video`
fn side_file(dest: &Path, tag: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(format!(".{tag}"));
    PathBuf::from(s)
}

fn part_of(p: &Path) -> PathBuf {
    side_file(p, "part")
}

/// `Cat.mp4` → `Cat.mp4.mux.mp4` (ffmpeg picks the container from the extension).
fn mux_tmp(dest: &Path) -> PathBuf {
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    side_file(dest, &format!("mux.{ext}"))
}

/// Remove everything a cancelled job may have left: the main .part/.mdl.json, the separate
/// video/audio streams (finished or partial), and a half-written mux output.
fn delete_partial(dest: &str) {
    let dest = Path::new(dest);
    let mut files = vec![mux_tmp(dest)];
    for base in [dest.to_path_buf(), side_file(dest, "video"), side_file(dest, "audio")] {
        files.push(side_file(&base, "part"));
        files.push(side_file(&base, "mdl.json"));
        if base != dest {
            files.push(base);
        }
    }
    for f in files {
        let _ = std::fs::remove_file(f);
    }
}

fn default_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Downloads")
}

/// Turn an untrusted suggested name (often a page title) into a safe single file name:
/// drops any directory part, replaces Windows-reserved characters, trims dots/spaces,
/// avoids reserved device names, and caps the length.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    s = s.trim().trim_matches('.').trim().to_string();
    if s.chars().count() > 150 {
        let ext = Path::new(&s).extension().and_then(|e| e.to_str()).filter(|e| e.len() <= 5).map(|e| format!(".{e}"));
        let ext = ext.unwrap_or_default();
        s = s.chars().take(150 - ext.len()).collect::<String>().trim_end().to_string() + &ext;
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4);
    if s.is_empty() || reserved { format!("download_{s}") } else { s }
}

/// `video.mp4` → `video (1).mp4` if the file exists on disk or another job already targets it.
fn unique_path(want: &Path, taken: &[PathBuf]) -> PathBuf {
    let busy = |p: &Path| p.exists() || taken.iter().any(|t| t == p);
    if !busy(want) {
        return want.to_path_buf();
    }
    let stem = want.file_stem().and_then(|s| s.to_str()).unwrap_or("download");
    let ext = want.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    (1..)
        .map(|n| want.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !busy(p))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal_and_reserved() {
        assert_eq!(sanitize_filename(r"..\..\Windows\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("My: video? <1>.mp4"), "My_ video_ _1_.mp4");
        assert_eq!(sanitize_filename(".."), "download_");
        assert_eq!(sanitize_filename("CON.mp4"), "download_CON.mp4");
        assert_eq!(sanitize_filename(" cat.mp4. "), "cat.mp4");
        let long = format!("{}.mp4", "a".repeat(300));
        let s = sanitize_filename(&long);
        assert!(s.ends_with(".mp4") && s.chars().count() <= 150);
    }

    #[test]
    fn unique_path_skips_taken() {
        let dir = std::env::temp_dir().join("mdl-test-unique-nonexistent");
        let want = dir.join("a.mp4");
        assert_eq!(unique_path(&want, &[]), want);
        assert_eq!(unique_path(&want, &[want.clone()]), dir.join("a (1).mp4"));
        assert_eq!(unique_path(&want, &[want.clone(), dir.join("a (1).mp4")]), dir.join("a (2).mp4"));
    }
}
