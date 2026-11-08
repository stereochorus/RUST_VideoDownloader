//! mdl-shim: Chrome/Edge native-messaging host. A thin, short-lived relay.
//!
//!   extension ──(u32 LE length + JSON on stdin/stdout)──► mdl-shim ──(JSON lines)──► \\.\pipe\mdl
//!
//! Chrome starts one shim per `connectNative` port and kills it when the port closes, which is
//! exactly why no download state lives here — it all lives in mdl-daemon, which the shim starts
//! on demand (detached, so it outlives the shim and the browser).
//!
//! RULE: stdout is the protocol channel. Never print to it; log to %LOCALAPPDATA%\mdl\shim.log.

mod framing;

#[cfg(not(windows))]
compile_error!("mdl-shim currently uses Windows named pipes");

use anyhow::{Context, Result, bail};
use mdl_protocol::{PIPE_NAME, Reply, Request};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::mpsc;

const ERROR_PIPE_BUSY: i32 = 231;

#[tokio::main]
async fn main() {
    // Chrome passes the calling origin (chrome-extension://<id>/) as the first argument.
    let origin = std::env::args().nth(1).unwrap_or_default();
    log(&format!("start (origin {origin})"));
    match run().await {
        Ok(()) => log("exit: browser closed the port"),
        Err(e) => log(&format!("exit with error: {e:#}")),
    }
}

async fn run() -> Result<()> {
    // Single writer task for stdout so frames from different sources never interleave.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            if framing::write_msg(&mut stdout, &msg).await.is_err() {
                break;
            }
        }
    });
    let send_reply = |tx: &mpsc::UnboundedSender<Vec<u8>>, r: &Reply| {
        let _ = tx.send(serde_json::to_vec(r).expect("Reply serializes"));
    };

    let pipe = match connect_or_spawn_daemon().await {
        Ok(p) => p,
        Err(e) => {
            send_reply(&out_tx, &Reply::Error { message: format!("cannot reach mdl-daemon: {e:#}") });
            drop(out_tx);
            let _ = stdout_task.await; // flush the error before exiting
            return Err(e);
        }
    };
    let (pipe_read, mut pipe_write) = tokio::io::split(pipe);

    // daemon → browser: each JSON line becomes one framed message.
    let tx = out_tx.clone();
    let mut pipe_to_browser = tokio::spawn(async move {
        let mut lines = BufReader::new(pipe_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() > framing::MAX_TO_BROWSER {
                log(&format!("dropping {}-byte message: over Chrome's 1 MB limit", line.len()));
                continue;
            }
            if tx.send(line.into_bytes()).is_err() {
                break;
            }
        }
    });

    // browser → daemon: validate, then forward as one line. Junk never reaches the daemon.
    let mut stdin = tokio::io::stdin();
    loop {
        tokio::select! {
            msg = framing::read_msg(&mut stdin) => {
                let Some(msg) = msg? else { break }; // EOF: Chrome closed the port
                match serde_json::from_slice::<Request>(&msg) {
                    Ok(req) => {
                        let mut line = serde_json::to_vec(&req)?;
                        line.push(b'\n');
                        pipe_write.write_all(&line).await.context("writing to daemon")?;
                    }
                    Err(e) => send_reply(&out_tx, &Reply::Error { message: format!("bad request: {e}") }),
                }
            }
            _ = &mut pipe_to_browser => {
                send_reply(&out_tx, &Reply::Error { message: "mdl-daemon disconnected".into() });
                break;
            }
        }
    }
    drop(out_tx);
    pipe_to_browser.abort();
    let _ = tokio::time::timeout(Duration::from_secs(1), stdout_task).await;
    Ok(())
}

async fn connect_or_spawn_daemon() -> Result<NamedPipeClient> {
    if let Some(p) = try_connect().await? {
        return Ok(p);
    }
    spawn_daemon()?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(p) = try_connect().await? {
            return Ok(p);
        }
    }
    bail!("started mdl-daemon but it did not open {PIPE_NAME} within 5s (see daemon.log)")
}

/// `Ok(None)` = daemon not running.
async fn try_connect() -> Result<Option<NamedPipeClient>> {
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(p) => return Ok(Some(p)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("opening daemon pipe"),
        }
    }
}

/// Start mdl-daemon.exe (next to this exe) fully detached: no console, no inherited stdio
/// (inheriting our stdout would corrupt the browser channel), and broken away from Chrome's
/// job object when allowed, so closing the browser does not kill downloads.
fn spawn_daemon() -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let exe = std::env::current_exe()?.with_file_name("mdl-daemon.exe");
    if !exe.exists() {
        bail!("{} not found — run install.ps1", exe.display());
    }
    let log_file = || -> Result<Stdio> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(data_dir()?.join("daemon.log"))?;
        Ok(Stdio::from(f))
    };
    let spawn = |flags| {
        Command::new(&exe)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file()?)
            .spawn()
            .map_err(anyhow::Error::from)
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match spawn(base | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(_) => log("spawned mdl-daemon (broken away from job)"),
        Err(e) => {
            // The browser's job object may forbid breakaway; still start it.
            log(&format!("breakaway spawn failed ({e:#}); retrying without it"));
            spawn(base)?;
            log("spawned mdl-daemon");
        }
    }
    Ok(())
}

fn data_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("mdl");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn log(msg: &str) {
    let Ok(dir) = data_dir() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("shim.log")) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{ts} [{}] {msg}", std::process::id());
    }
}
