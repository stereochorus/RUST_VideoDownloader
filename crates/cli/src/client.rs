//! Minimal client for mdl-daemon's named pipe.

use anyhow::{Context, Result, bail};
use mdl_protocol::{PIPE_NAME, Reply, Request};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const ERROR_PIPE_BUSY: i32 = 231;

pub struct Client {
    lines: Lines<BufReader<ReadHalf<NamedPipeClient>>>,
    write: WriteHalf<NamedPipeClient>,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let pipe = loop {
            match ClientOptions::new().open(PIPE_NAME) {
                Ok(p) => break p,
                // All server instances busy for a moment — retry shortly.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    bail!("mdl-daemon is not running (start it with: cargo run --release -p mdl-daemon)")
                }
                Err(e) => return Err(e).context("connecting to mdl-daemon"),
            }
        };
        let (read, write) = tokio::io::split(pipe);
        Ok(Self { lines: BufReader::new(read).lines(), write })
    }

    pub async fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Reply> {
        let line = self.lines.next_line().await?.context("daemon closed the connection")?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Send one request and return its reply, turning `Reply::Error` into an `Err`.
    pub async fn call(&mut self, req: Request) -> Result<Reply> {
        self.send(&req).await?;
        match self.recv().await? {
            Reply::Error { message } => bail!(message),
            r => Ok(r),
        }
    }
}
