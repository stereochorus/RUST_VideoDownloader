//! On-disk resume state: the `<file>.mdl.json` sidecar next to `<file>.part`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadState {
    pub url: String,
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub chunk_size: u64,
    /// One flag per chunk; `true` once the chunk is fully written to disk.
    pub done: Vec<bool>,
}

impl DownloadState {
    pub fn new(url: &str, total: u64, etag: Option<String>, last_modified: Option<String>, chunk_size: u64) -> Self {
        let chunks = total.div_ceil(chunk_size) as usize;
        Self { url: url.to_owned(), total, etag, last_modified, chunk_size, done: vec![false; chunks] }
    }

    /// Byte range (inclusive) covered by chunk `i`.
    pub fn chunk_range(&self, i: usize) -> (u64, u64) {
        let start = i as u64 * self.chunk_size;
        let end = (start + self.chunk_size).min(self.total) - 1;
        (start, end)
    }

    pub fn bytes_done(&self) -> u64 {
        (0..self.done.len())
            .filter(|&i| self.done[i])
            .map(|i| {
                let (s, e) = self.chunk_range(i);
                e - s + 1
            })
            .sum()
    }

    pub fn pending(&self) -> Vec<usize> {
        (0..self.done.len()).filter(|&i| !self.done[i]).collect()
    }

    /// Same remote file as before? Size must match; validators must match when both sides have them.
    pub fn matches_remote(&self, url: &str, total: u64, etag: &Option<String>, last_modified: &Option<String>) -> bool {
        let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        self.url == url && self.total == total && same(&self.etag, etag) && same(&self.last_modified, last_modified)
    }

    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Atomic save: write a temp file, then rename over the sidecar, so a crash never leaves half a JSON file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?).context("writing state temp file")?;
        std::fs::rename(&tmp, path).context("replacing state file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_math_covers_whole_file() {
        let s = DownloadState::new("u", 10, None, None, 4);
        assert_eq!(s.done.len(), 3);
        assert_eq!(s.chunk_range(0), (0, 3));
        assert_eq!(s.chunk_range(2), (8, 9));
    }

    #[test]
    fn bytes_done_counts_short_last_chunk() {
        let mut s = DownloadState::new("u", 10, None, None, 4);
        s.done[2] = true;
        assert_eq!(s.bytes_done(), 2);
        assert_eq!(s.pending(), vec![0, 1]);
    }

    #[test]
    fn etag_change_invalidates() {
        let s = DownloadState::new("u", 10, Some("a".into()), None, 4);
        assert!(s.matches_remote("u", 10, &Some("a".into()), &None));
        assert!(!s.matches_remote("u", 10, &Some("b".into()), &None));
        assert!(!s.matches_remote("u", 11, &Some("a".into()), &None));
    }
}
