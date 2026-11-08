mod client;

use anyhow::Result;
use clap::{Parser, Subcommand};
use client::Client;
use indicatif::{ProgressBar, ProgressStyle};
use mdl_engine::{Downloader, Options, Progress, filename_from_url};
use mdl_protocol::{Event, JobId, JobSnapshot, JobState, Reply, Request};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "mdl-cli", about = "media_downloader command line")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download directly in this process (no daemon). Rerun the same command to resume.
    Get {
        url: String,
        dir: PathBuf,
        #[arg(short, long)]
        output: Option<String>,
        #[arg(short, long, default_value_t = 4)]
        connections: usize,
        #[arg(long, default_value_t = 4)]
        chunk_mb: u64,
    },
    /// Queue a download in the daemon.
    Add {
        url: String,
        dir: PathBuf,
        #[arg(short, long)]
        output: Option<String>,
        /// URL is a web page (YouTube, Facebook, ...): find the video with yt-dlp
        #[arg(long)]
        page: bool,
    },
    /// Show all daemon jobs.
    List,
    Pause { id: JobId },
    Resume { id: JobId },
    /// Stop a job and delete its partial data.
    Cancel { id: JobId },
    /// Remove a finished job from the list (keeps the file).
    Remove { id: JobId },
    /// Live view of all jobs (Ctrl-C to quit; downloads keep running).
    Watch,
    /// Check the daemon is alive.
    Ping,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse().cmd).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cmd: Cmd) -> Result<()> {
    let simple = |req| async move {
        Client::connect().await?.call(req).await?;
        println!("ok");
        anyhow::Ok(())
    };
    match cmd {
        Cmd::Get { url, dir, output, connections, chunk_mb } => local_get(url, dir, output, connections, chunk_mb).await,
        Cmd::Add { url, dir, output, page } => {
            // The daemon has its own working directory, so always send an absolute path.
            let dest_dir = std::path::absolute(&dir)?.to_string_lossy().into_owned();
            let req = Request::Add { url, dest_dir, filename: output, headers: vec![], resolve: page };
            if let Reply::Added { job } = Client::connect().await?.call(req).await? {
                println!("added job {} → {}", job.id, job.dest);
            }
            Ok(())
        }
        Cmd::List => {
            if let Reply::Jobs { jobs } = Client::connect().await?.call(Request::List).await? {
                if jobs.is_empty() {
                    println!("no jobs");
                }
                for j in &jobs {
                    println!("{}", job_line(j, None));
                }
            }
            Ok(())
        }
        Cmd::Pause { id } => simple(Request::Pause { id }).await,
        Cmd::Resume { id } => simple(Request::Resume { id }).await,
        Cmd::Cancel { id } => simple(Request::Cancel { id }).await,
        Cmd::Remove { id } => simple(Request::Remove { id }).await,
        Cmd::Ping => {
            if let Reply::Pong { version } = Client::connect().await?.call(Request::Ping).await? {
                println!("mdl-daemon {version} is running");
            }
            Ok(())
        }
        Cmd::Watch => watch().await,
    }
}

async fn watch() -> Result<()> {
    let mut client = Client::connect().await?;
    let Reply::Jobs { jobs } = client.call(Request::Subscribe).await? else { return Ok(()) };
    let mut jobs: BTreeMap<JobId, JobSnapshot> = jobs.into_iter().map(|j| (j.id, j)).collect();
    let mut speeds: BTreeMap<JobId, u64> = BTreeMap::new();
    let mut drawn = 0;
    loop {
        // Redraw in place: move the cursor up over the previous frame.
        if drawn > 0 {
            print!("\x1b[{drawn}A");
        }
        let lines: Vec<String> = if jobs.is_empty() {
            vec!["no jobs".into()]
        } else {
            jobs.values().map(|j| job_line(j, speeds.get(&j.id).copied())).collect()
        };
        for l in &lines {
            println!("\x1b[2K{l}");
        }
        drawn = lines.len();

        match client.recv().await? {
            Reply::Event { event: Event::Progress { id, downloaded, total, speed } } => {
                if let Some(j) = jobs.get_mut(&id) {
                    j.downloaded = downloaded;
                    j.total = total;
                }
                speeds.insert(id, speed);
            }
            Reply::Event { event: Event::Changed { job } } => {
                if job.state != JobState::Running {
                    speeds.remove(&job.id);
                }
                jobs.insert(job.id, job);
            }
            Reply::Event { event: Event::Removed { id } } => {
                jobs.remove(&id);
                // Clear the leftover line from the taller previous frame.
                print!("\x1b[{drawn}A");
                for _ in 0..drawn {
                    println!("\x1b[2K");
                }
            }
            _ => {}
        }
    }
}

fn job_line(j: &JobSnapshot, speed: Option<u64>) -> String {
    let state = match &j.state {
        JobState::Failed { error } => format!("failed: {error}"),
        s => format!("{s:?}").to_lowercase(),
    };
    let pct = if j.total > 0 { format!("{:5.1}%", j.downloaded as f64 * 100.0 / j.total as f64) } else { "    -".into() };
    let speed = speed.map(|s| format!("  {:.1} MiB/s", s as f64 / 1048576.0)).unwrap_or_default();
    let name = std::path::Path::new(&j.dest).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("#{:<3} {:<10} {pct}  {:>7.1} MiB  {name}{speed}", j.id, state, j.total as f64 / 1048576.0)
}

async fn local_get(url: String, dir: PathBuf, output: Option<String>, connections: usize, chunk_mb: u64) -> Result<()> {
    let dest = dir.join(output.unwrap_or_else(|| filename_from_url(&url)));
    let opts = Options { connections, chunk_size: chunk_mb.max(1) * 1024 * 1024, ..Default::default() };
    let dl = Downloader::new(opts)?;
    let progress = Arc::new(Progress::default());

    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{spinner} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta} {msg}")?
            .progress_chars("=> "),
    );
    let ticker = {
        let (bar, p) = (bar.clone(), progress.clone());
        tokio::spawn(async move {
            let mut announced = false;
            loop {
                bar.set_length(p.total.load(Ordering::Relaxed));
                bar.set_position(p.downloaded.load(Ordering::Relaxed));
                let resumed = p.resumed_from.load(Ordering::Relaxed);
                if resumed > 0 && !announced {
                    bar.println(format!("Resuming: {:.1} MiB already on disk", resumed as f64 / 1048576.0));
                    bar.reset_eta();
                    announced = true;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
    };

    let result = dl.download(&url, &dest, progress).await;
    ticker.abort();
    match result {
        Ok(path) => {
            bar.finish_with_message("done");
            println!("Saved {}", path.display());
            Ok(())
        }
        Err(e) => {
            bar.abandon_with_message("failed");
            Err(e.context("download failed — rerun the same command to resume"))
        }
    }
}
