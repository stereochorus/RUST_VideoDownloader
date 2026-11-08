//! mdl-engine: resumable, parallel HTTP Range downloader.
//!
//! Layout on disk while downloading:
//!   video.mp4.part      preallocated data file, chunks written at their offsets
//!   video.mp4.mdl.json  [`DownloadState`] — which chunks are finished
//! On success `.part` is renamed to `video.mp4` and the sidecar is deleted.
//! Killing the process at any moment is safe: rerun and only unfinished chunks are fetched.

mod state;

pub use state::DownloadState;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Options {
    pub connections: usize,
    pub chunk_size: u64,
    pub retries: u32,
    /// Extra request headers (later: cookies/referer passed from the browser extension).
    pub headers: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Self {
        Self { connections: 4, chunk_size: 4 * 1024 * 1024, retries: 5, headers: Vec::new() }
    }
}

/// Live counters a UI can poll. `downloaded` includes bytes restored from a previous run.
#[derive(Debug, Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub downloaded: AtomicU64,
    pub resumed_from: AtomicU64,
    /// Set to stop the download at the next read; data on disk stays resumable.
    pub cancel: AtomicBool,
}

/// Returned (inside `anyhow::Error`) when a download stopped because `Progress::cancel` was set.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Non-success HTTP status from the server.
#[derive(Debug)]
pub struct HttpStatus(pub u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            429 => f.write_str("server rate-limited us (HTTP 429)"),
            403 | 410 => write!(f, "HTTP {} — the URL may have expired", self.0),
            s => write!(f, "server returned HTTP {s}"),
        }
    }
}

impl std::error::Error for HttpStatus {}

/// Worth retrying: rate limits, server errors, and network-level failures (no status at all).
fn is_transient(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<HttpStatus>() {
        Some(HttpStatus(s)) => *s == 429 || *s >= 500,
        None => err.downcast_ref::<reqwest::Error>().is_some(),
    }
}

/// True if `err` means "stopped on request" rather than a real failure.
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

/// What the server told us about the file.
#[derive(Debug)]
struct Probe {
    total: Option<u64>,
    ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
}

pub struct Downloader {
    client: Client,
    opts: Options,
}

impl Downloader {
    pub fn new(opts: Options) -> Result<Self> {
        let mut hm = header::HeaderMap::new();
        for (k, v) in &opts.headers {
            hm.insert(header::HeaderName::from_bytes(k.as_bytes())?, header::HeaderValue::from_str(v)?);
        }
        let client = Client::builder()
            .user_agent("mdl/0.1")
            .default_headers(hm)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client, opts })
    }

    /// Download `url` to `dest`, resuming from a sidecar if one matches the remote file.
    pub async fn download(&self, url: &str, dest: &Path, progress: Arc<Progress>) -> Result<PathBuf> {
        self.download_keyed(url, None, dest, progress).await
    }

    /// Like [`download`], but resume state is matched by `key` instead of the URL.
    ///
    /// Use this when the URL changes between attempts while the bytes do not (signed CDN links from
    /// YouTube/Facebook expire within hours and are re-resolved each run). With a key, ETag and
    /// Last-Modified are not compared either, since every fresh link may report different ones;
    /// the key plus the exact file size identify the stream.
    ///
    /// Several downloads may share one [`Progress`]: counters are added to, never overwritten, so a
    /// video and an audio stream fetched in parallel show up as one combined progress bar.
    pub async fn download_keyed(&self, url: &str, key: Option<&str>, dest: &Path, progress: Arc<Progress>) -> Result<PathBuf> {
        let part = append_ext(dest, "part");
        let sidecar = append_ext(dest, "mdl.json");
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let probe = self.probe_with_retry(url, &progress).await?;
        let (Some(total), true) = (probe.total, probe.ranges) else {
            // No size or no Range support: plain single stream, cannot resume.
            self.single_stream(url, &part, &progress).await?;
            return finish(&part, &sidecar, dest);
        };
        progress.total.fetch_add(total, Ordering::Relaxed);

        let (state_key, etag, last_modified) = match key {
            Some(k) => (k, None, None),
            None => (url, probe.etag, probe.last_modified),
        };
        let state = match DownloadState::load(&sidecar) {
            Some(s) if part.exists() && s.matches_remote(state_key, total, &etag, &last_modified) => s,
            _ => {
                // Fresh start (or remote file changed): preallocate and write a new sidecar.
                let f = std::fs::File::create(&part).context("creating .part file")?;
                f.set_len(total)?;
                let s = DownloadState::new(state_key, total, etag, last_modified, self.opts.chunk_size);
                s.save(&sidecar)?;
                s
            }
        };
        let already = state.bytes_done();
        progress.downloaded.fetch_add(already, Ordering::Relaxed);
        progress.resumed_from.fetch_add(already, Ordering::Relaxed);

        let queue = Arc::new(Mutex::new(state.pending().into_iter().collect::<VecDeque<_>>()));
        let state = Arc::new(Mutex::new(state));

        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..self.opts.connections.max(1) {
            let ctx = Worker {
                client: self.client.clone(),
                url: url.to_owned(),
                part: part.clone(),
                sidecar: sidecar.clone(),
                queue: queue.clone(),
                state: state.clone(),
                progress: progress.clone(),
                retries: self.opts.retries,
            };
            workers.spawn(ctx.run());
        }
        while let Some(res) = workers.join_next().await {
            if let Err(e) = res? {
                workers.abort_all();
                return Err(e); // state on disk is still valid; rerun resumes
            }
        }

        if progress.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        if !state.lock().unwrap().pending().is_empty() {
            bail!("download ended with unfinished chunks");
        }
        finish(&part, &sidecar, dest)
    }

    /// Probe with the same backoff as chunks: a 429/5xx/network blip must not fail the whole job.
    async fn probe_with_retry(&self, url: &str, progress: &Progress) -> Result<Probe> {
        let mut attempt = 0;
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            match self.probe(url).await {
                Ok(p) => return Ok(p),
                Err(e) if attempt < self.opts.retries && is_transient(&e) => {
                    attempt += 1;
                    let wait = Duration::from_millis(500 * 2u64.pow(attempt.min(6)));
                    eprintln!("probe failed ({e:#}); retry {attempt}/{} in {wait:?}", self.opts.retries);
                    tokio::time::sleep(wait).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Ask for byte 0 only: a 206 proves Range support and Content-Range gives the full size.
    async fn probe(&self, url: &str) -> Result<Probe> {
        let resp = self.client.get(url).header(header::RANGE, "bytes=0-0").send().await?;
        let hdr = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let etag = hdr(header::ETAG);
        let last_modified = hdr(header::LAST_MODIFIED);
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {
                let total = hdr(header::CONTENT_RANGE)
                    .and_then(|cr| cr.rsplit('/').next().and_then(|t| t.parse().ok()));
                Ok(Probe { total, ranges: total.is_some(), etag, last_modified })
            }
            s if s.is_success() => Ok(Probe { total: resp.content_length(), ranges: false, etag, last_modified }),
            s => Err(HttpStatus(s.as_u16()).into()),
        }
    }

    async fn single_stream(&self, url: &str, part: &Path, progress: &Progress) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let resp = self.client.get(url).send().await?.error_for_status()?;
        if let Some(len) = resp.content_length() {
            progress.total.fetch_add(len, Ordering::Relaxed);
        }
        let mut file = tokio::fs::File::create(part).await?;
        let mut stream = resp.bytes_stream();
        while let Some(bytes) = stream.next().await {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            let bytes = bytes?;
            file.write_all(&bytes).await?;
            progress.downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        file.flush().await?;
        Ok(())
    }
}

struct Worker {
    client: Client,
    url: String,
    part: PathBuf,
    sidecar: PathBuf,
    queue: Arc<Mutex<VecDeque<usize>>>,
    state: Arc<Mutex<DownloadState>>,
    progress: Arc<Progress>,
    retries: u32,
}

impl Worker {
    fn cancelled(&self) -> bool {
        self.progress.cancel.load(Ordering::Relaxed)
    }

    async fn run(self) -> Result<()> {
        loop {
            if self.cancelled() {
                return Ok(());
            }
            let Some(idx) = self.queue.lock().unwrap().pop_front() else { return Ok(()) };
            let (start, end) = self.state.lock().unwrap().chunk_range(idx);

            let mut attempt = 0;
            loop {
                match self.fetch_chunk(start, end).await {
                    Ok(()) => break,
                    Err(_) if self.cancelled() => return Ok(()),
                    Err(e) if attempt < self.retries => {
                        attempt += 1;
                        let wait = Duration::from_millis(500 * 2u64.pow(attempt.min(6)));
                        eprintln!("chunk {idx} failed ({e:#}); retry {attempt}/{} in {wait:?}", self.retries);
                        tokio::time::sleep(wait).await;
                    }
                    Err(e) => return Err(e.context(format!("chunk {idx} failed after {} retries", self.retries))),
                }
            }

            let state = self.state.clone();
            let sidecar = self.sidecar.clone();
            tokio::task::spawn_blocking(move || {
                let mut s = state.lock().unwrap();
                s.done[idx] = true;
                s.save(&sidecar)
            })
            .await??;
        }
    }

    /// Fetch one chunk into memory, then write it at its offset. Progress counted on arrival;
    /// on failure the partial bytes are subtracted again so the counter stays honest.
    async fn fetch_chunk(&self, start: u64, end: u64) -> Result<()> {
        let resp = self
            .client
            .get(&self.url)
            .header(header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::TOO_MANY_REQUESTS => bail!("server rate-limited us (429); try fewer --connections"),
            StatusCode::FORBIDDEN | StatusCode::GONE => bail!("got {} — the URL may have expired", resp.status()),
            s => bail!("expected 206 Partial Content, got {s}"),
        }
        let expected = (end - start + 1) as usize;
        let mut buf = Vec::with_capacity(expected);
        let mut stream = resp.bytes_stream();
        let result: Result<()> = async {
            while let Some(bytes) = stream.next().await {
                if self.cancelled() {
                    return Err(Cancelled.into());
                }
                let bytes = bytes?;
                buf.extend_from_slice(&bytes);
                self.progress.downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
            if buf.len() != expected {
                bail!("short chunk: got {} of {expected} bytes", buf.len());
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            self.progress.downloaded.fetch_sub(buf.len() as u64, Ordering::Relaxed);
            return Err(e);
        }

        let part = self.part.clone();
        tokio::task::spawn_blocking(move || write_at(&part, start, &buf)).await??;
        Ok(())
    }
}

fn write_at(path: &Path, offset: u64, data: &[u8]) -> Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    let mut written = 0;
    while written < data.len() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_write(&file, &data[written..], offset + written as u64)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::write_at(&file, &data[written..], offset + written as u64)?;
        if n == 0 {
            bail!("write returned 0 bytes");
        }
        written += n;
    }
    file.sync_data()?; // chunk is durable before the sidecar marks it done
    Ok(())
}

fn finish(part: &Path, sidecar: &Path, dest: &Path) -> Result<PathBuf> {
    std::fs::rename(part, dest).with_context(|| format!("renaming to {}", dest.display()))?;
    let _ = std::fs::remove_file(sidecar);
    Ok(dest.to_path_buf())
}

fn append_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Best-effort filename from the URL path (`.../cat.mp4?sig=x` → `cat.mp4`).
pub fn filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("").trim();
    let clean: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    if clean.is_empty() { "download.bin".into() } else { clean }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_strips_query() {
        assert_eq!(filename_from_url("https://x.com/a/cat.mp4?sig=1"), "cat.mp4");
        assert_eq!(filename_from_url("https://x.com/"), "download.bin");
    }
}
