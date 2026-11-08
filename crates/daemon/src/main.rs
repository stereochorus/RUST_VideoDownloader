//! mdl-daemon: long-running download manager.
//!
//! Owns all jobs (so downloads survive the browser/popup closing), persists them to
//! %LOCALAPPDATA%\mdl\jobs.json, and serves newline-delimited JSON on \\.\pipe\mdl.
//! Logs go to stderr — never stdout — so the same habits carry over to the NM shim.

mod jobs;
mod resolve;

#[cfg(not(windows))]
compile_error!("mdl-daemon currently uses Windows named pipes; Unix socket support is not written yet");

use anyhow::{Context, Result};
use jobs::Manager;
use mdl_protocol::{PIPE_NAME, Reply, Request};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    let data_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("mdl");
    std::fs::create_dir_all(&data_dir)?;

    // first_pipe_instance doubles as a single-instance lock.
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(PIPE_NAME)
        .context("could not create pipe — is mdl-daemon already running?")?;

    let mgr = Manager::load(data_dir.join("jobs.json"));
    tokio::spawn(mgr.clone().progress_ticker());
    eprintln!("mdl-daemon listening on {PIPE_NAME} (data: {})", data_dir.display());

    loop {
        server.connect().await?;
        let client = server;
        server = ServerOptions::new().create(PIPE_NAME)?;
        let mgr = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client, mgr).await {
                eprintln!("client error: {e:#}");
            }
        });
    }
}

async fn handle_client(pipe: NamedPipeServer, mgr: Arc<Manager>) -> Result<()> {
    let (read, mut write) = tokio::io::split(pipe);

    // All outgoing lines go through one channel so replies and pushed events never interleave mid-line.
    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let mut line = serde_json::to_vec(&reply).expect("Reply always serializes");
            line.push(b'\n');
            if write.write_all(&line).await.is_err() {
                break; // client went away
            }
        }
    });

    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => Reply::Error { message: format!("bad request: {e}") },
            Ok(Request::Subscribe) => {
                let mut events = mgr.subscribe();
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(event) => {
                                if tx.send(Reply::Event { event }).is_err() {
                                    break;
                                }
                            }
                            // Slow reader skipped some progress ticks; the next one catches it up.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                });
                Reply::Jobs { jobs: mgr.list() } // initial snapshot, then live events
            }
            // Page jobs reply only after yt-dlp has looked the page up (a few seconds).
            Ok(Request::Add { url, dest_dir, filename, headers, resolve: true }) => {
                eprintln!("page lookup: {url}");
                match mgr.add_page(url.clone(), dest_dir, filename, headers).await {
                    Ok(job) => {
                        eprintln!("  -> job {} : {}", job.id, job.dest);
                        Reply::Added { job }
                    }
                    Err(e) => {
                        eprintln!("  -> failed: {e:#}");
                        Reply::Error { message: format!("{e:#}\n(tried: {})", short_url(&url)) }
                    }
                }
            }
            Ok(req) => dispatch(&mgr, req),
        };
        if tx.send(reply).is_err() {
            break;
        }
    }
    drop(tx);
    writer.abort();
    Ok(())
}

/// `https://www.facebook.com/watch/?v=123&x=y` → `facebook.com/watch/?v=123&x=y` (max 80 chars).
fn short_url(url: &str) -> String {
    let s = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    if s.chars().count() > 80 { format!("{}…", s.chars().take(80).collect::<String>()) } else { s.to_string() }
}

fn dispatch(mgr: &Arc<Manager>, req: Request) -> Reply {
    let result = match req {
        Request::Add { url, dest_dir, filename, headers, .. } => {
            return match mgr.add(url, dest_dir, filename, headers) {
                Ok(job) => Reply::Added { job },
                Err(e) => Reply::Error { message: format!("{e:#}") },
            };
        }
        Request::List => return Reply::Jobs { jobs: mgr.list() },
        Request::Ping => return Reply::Pong { version: env!("CARGO_PKG_VERSION").into() },
        Request::Pause { id } => mgr.pause(id),
        Request::Resume { id } => mgr.resume(id),
        Request::Cancel { id } => mgr.cancel(id),
        Request::Remove { id } => mgr.remove(id),
        Request::Reveal { id } => mgr.reveal(id),
        Request::Subscribe => unreachable!("handled by caller"),
    };
    match result {
        Ok(()) => Reply::Ok,
        Err(e) => Reply::Error { message: format!("{e:#}") },
    }
}
