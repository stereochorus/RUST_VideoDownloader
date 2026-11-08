//! Page-URL resolution through yt-dlp, and muxing through ffmpeg.
//!
//! yt-dlp only *finds* the real media URLs (YouTube's signed/"n"-challenged links, Facebook's DASH
//! tracks, and hundreds of other sites). The bytes are still fetched by our own resumable engine;
//! when video and audio come as separate streams, ffmpeg joins them without re-encoding.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Only plain HTTP(S) files: our engine downloads byte ranges, not HLS/DASH segment playlists.
const PLAIN: &str = "[protocol!*=m3u8][protocol!*=dash]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Muxed,
}

#[derive(Debug, Clone)]
pub struct Stream {
    pub url: String,
    pub format_id: String,
    pub kind: Kind,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub title: String,
    pub ext: String,
    pub streams: Vec<Stream>,
    /// Unix time before which the URLs answer 403 (YouTube hands out links that activate a few
    /// seconds later). `None` = usable now.
    pub available_at: Option<u64>,
}

/// Best quality up to 1080p, preferring H.264 + AAC (plays everywhere), else anything plain.
fn format_selector() -> String {
    format!("bv*{PLAIN}[height<=1080][vcodec^=avc1]+ba{PLAIN}[ext=m4a]/bv*{PLAIN}[height<=1080]+ba{PLAIN}/b{PLAIN}")
}

/// Resolve without cookies first: logged-in YouTube sessions make yt-dlp use player clients that
/// need extra tokens and often return fewer formats. Retry with the browser's cookies only if the
/// site says it needs a login (private/friends-only Facebook videos, age-gated YouTube, ...).
pub async fn resolve(page_url: &str, cookie: Option<&str>) -> Result<Resolved> {
    match run_ytdlp(page_url, None).await {
        Err(e) if cookie.is_some() && needs_login(&e.to_string()) => run_ytdlp(page_url, cookie).await,
        other => other,
    }
}

fn needs_login(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["login", "log in", "sign in", "cookies", "private", "confirm your age", "not available"]
        .iter()
        .any(|k| m.contains(k))
}

async fn run_ytdlp(page_url: &str, cookie: Option<&str>) -> Result<Resolved> {
    let exe = find_tool("yt-dlp").context("yt-dlp is not installed. Install it with:  winget install yt-dlp.yt-dlp")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "-f", &format_selector()]);
    if let Some(c) = cookie {
        cmd.arg("--add-header").arg(format!("Cookie:{c}"));
    }
    cmd.arg("--").arg(page_url);
    cmd.env("PATH", tool_path()) // lets yt-dlp find deno (YouTube challenges) even with a stale PATH
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .context("yt-dlp took longer than 2 minutes")?
        .context("running yt-dlp")?;
    if !out.status.success() {
        bail!(friendly_error(&String::from_utf8_lossy(&out.stderr)));
    }
    parse(&out.stdout)
}

/// Turn yt-dlp's last `ERROR:` line into something a user can act on.
fn friendly_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .unwrap_or_else(|| stderr.lines().last().unwrap_or("yt-dlp failed"))
        .trim_start_matches("ERROR:")
        .trim();
    // Drop the "[youtube] abc123: " prefix.
    let msg = match line.find("]") {
        Some(i) if line.starts_with('[') => line[i + 1..].trim_start().splitn(2, ": ").last().unwrap_or(line),
        _ => line,
    };
    if msg.contains("Requested format is not available") {
        "This video is only offered as an HLS/DASH stream, which is not supported yet.".into()
    } else if msg.contains("Unsupported URL") {
        "No downloadable video found on this page. Open the video itself (its own page) and try again.".into()
    } else {
        msg.to_string()
    }
}

pub fn parse(json: &[u8]) -> Result<Resolved> {
    let v: Value = serde_json::from_slice(json).context("yt-dlp returned invalid JSON")?;
    if v["_type"] == "playlist" {
        bail!("This page is a playlist. Open a single video and try again.");
    }
    let formats: Vec<&Value> = match v["requested_formats"].as_array() {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => vec![&v],
    };
    let single = formats.len() == 1;
    let mut streams = Vec::new();
    let mut available_at: Option<u64> = None;
    for f in formats {
        let url = f["url"].as_str().context("yt-dlp gave a format without a URL")?.to_string();
        let vcodec = f["vcodec"].as_str().unwrap_or("");
        let kind = if single {
            Kind::Muxed
        } else if !vcodec.is_empty() && vcodec != "none" {
            Kind::Video
        } else {
            Kind::Audio
        };
        let mut headers: Vec<(String, String)> = f["http_headers"]
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        // Some CDNs (TikTok) only serve the file with cookies that yt-dlp picked up while reading the
        // page. yt-dlp reports them in a separate `cookies` field, not in `http_headers`.
        if let Some(cookie) = f["cookies"].as_str().or(v["cookies"].as_str()).and_then(cookie_header) {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("cookie"));
            headers.push(("Cookie".into(), cookie));
        }
        if let Some(t) = f["available_at"].as_f64() {
            available_at = available_at.max(Some(t.ceil() as u64));
        }
        streams.push(Stream { url, format_id: f["format_id"].as_str().unwrap_or("0").to_string(), kind, headers });
    }
    if !single && !(streams.iter().any(|s| s.kind == Kind::Video) && streams.iter().any(|s| s.kind == Kind::Audio)) {
        bail!("yt-dlp returned an unexpected pair of formats");
    }
    Ok(Resolved {
        title: v["title"].as_str().unwrap_or("video").to_string(),
        ext: v["ext"].as_str().unwrap_or("mp4").to_string(),
        streams,
        available_at,
    })
}

/// yt-dlp's `cookies` field is Set-Cookie style: `a=1; Domain=.x.com; Path=/; Secure; b=2; ...`.
/// Keep only the `name=value` pairs and drop the attributes, giving a request `Cookie` header.
fn cookie_header(raw: &str) -> Option<String> {
    const ATTRS: [&str; 9] = ["domain", "path", "expires", "max-age", "secure", "httponly", "samesite", "comment", "version"];
    let pairs: Vec<String> = raw
        .split(';')
        .map(str::trim)
        .filter(|p| match p.split_once('=') {
            Some((name, _)) => !name.is_empty() && !ATTRS.contains(&name.trim().to_ascii_lowercase().as_str()),
            None => false, // bare flags like `Secure`
        })
        .map(|p| {
            let (name, value) = p.split_once('=').unwrap();
            format!("{name}={}", unquote_cookie_value(value))
        })
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// yt-dlp serializes cookies with Python's `http.cookies`, which wraps values containing special
/// characters in double quotes and escapes them (`\"`, `\\`, octal `\073` for `;`). Servers want
/// the raw value: TikTok answers 403 when `tt_chain_token` arrives with the quotes still on.
fn unquote_cookie_value(v: &str) -> String {
    let Some(inner) = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else { return v.to_string() };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let oct = &bytes[i + 1..(i + 4).min(bytes.len())];
            if oct.len() == 3 && oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                out.push((oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0'));
                i += 4;
            } else {
                out.push(bytes[i + 1]);
                i += 2;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How long until the resolved links start working (capped; a far-future value is a site quirk).
pub fn wait_needed(available_at: Option<u64>) -> Duration {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    match available_at {
        Some(t) if t >= now => Duration::from_secs((t - now + 1).min(30)),
        _ => Duration::ZERO,
    }
}

/// Join a video-only and an audio-only file into `out` without re-encoding.
pub async fn mux(video: &Path, audio: &Path, out: &Path) -> Result<()> {
    let exe = find_tool("ffmpeg").context("ffmpeg is not installed. Install it with:  winget install Gyan.FFmpeg")?;
    let output = tokio::process::Command::new(exe)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy", "-movflags", "+faststart"])
        .arg(out)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running ffmpeg")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg could not merge video and audio: {}", err.lines().last().unwrap_or("unknown error"));
    }
    Ok(())
}

pub fn have_tool(name: &str) -> bool {
    find_tool(name).is_some()
}

/// Look next to our own exe, on PATH, and in winget's folders. The daemon is usually started by
/// the browser, whose PATH predates any `winget install`, so PATH alone is not enough.
fn find_tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.exe");
    tool_dirs().iter().map(|d| d.join(&file)).find(|p| p.is_file())
}

fn tool_path() -> std::ffi::OsString {
    std::env::join_paths(tool_dirs()).unwrap_or_default()
}

fn tool_dirs() -> &'static Vec<PathBuf> {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            dirs.push(d);
        }
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            let winget = local.join("Microsoft").join("WinGet");
            dirs.push(winget.join("Links"));
            // Packages\<pkg>\[...\]tool.exe, e.g. Gyan.FFmpeg_...\ffmpeg-9.0.1-full_build\bin\ffmpeg.exe
            collect_tool_dirs(&winget.join("Packages"), 3, &mut dirs);
        }
        dirs
    })
}

fn collect_tool_dirs(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth > 0 {
                collect_tool_dirs(&path, depth - 1, out);
            }
        } else if ["yt-dlp.exe", "ffmpeg.exe", "deno.exe"]
            .iter()
            .any(|t| path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(t)))
        {
            out.push(dir.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_separate_video_and_audio() {
        let json = br#"{"title":"Cat","ext":"mp4","requested_formats":[
            {"url":"https://v","format_id":"299","vcodec":"avc1","available_at":100.2,"http_headers":{"User-Agent":"UA"}},
            {"url":"https://a","format_id":"140-6","vcodec":"none","acodec":"mp4a"}]}"#;
        let r = parse(json).unwrap();
        assert_eq!(r.title, "Cat");
        assert_eq!(r.streams.len(), 2);
        assert_eq!(r.streams[0].kind, Kind::Video);
        assert_eq!(r.streams[1].kind, Kind::Audio);
        assert_eq!(r.streams[0].headers, vec![("User-Agent".into(), "UA".into())]);
        assert_eq!(r.available_at, Some(101));
    }

    #[test]
    fn parses_single_muxed_format() {
        let r = parse(br#"{"title":"T","ext":"mp4","url":"https://x","format_id":"18","vcodec":"avc1"}"#).unwrap();
        assert_eq!(r.streams.len(), 1);
        assert_eq!(r.streams[0].kind, Kind::Muxed);
    }

    #[test]
    fn cookies_field_becomes_cookie_header() {
        let raw = "tt_chain_token=abc; Domain=.tiktok.com; Path=/; Secure; Expires=Wed, 01 Jan 2027 00:00:00 GMT; ttwid=x=y; HttpOnly";
        assert_eq!(cookie_header(raw).as_deref(), Some("tt_chain_token=abc; ttwid=x=y"));
        assert_eq!(cookie_header("Domain=.x.com; Path=/"), None);
        // Python-style quoting is removed (this is what made TikTok answer 403).
        assert_eq!(cookie_header(r#"tt_chain_token="G5b/Qx+=="; Path=/"#).as_deref(), Some("tt_chain_token=G5b/Qx+=="));
        assert_eq!(unquote_cookie_value(r#""a\"b\\c\073d""#), r#"a"b\c;d"#);
        assert_eq!(unquote_cookie_value("plain"), "plain");
        let r = parse(br#"{"title":"T","ext":"mp4","url":"https://cdn","format_id":"1","vcodec":"h264",
            "http_headers":{"Referer":"https://www.tiktok.com/"},"cookies":"tt=1; Domain=.tiktok.com; Path=/"}"#).unwrap();
        assert!(r.streams[0].headers.contains(&("Cookie".into(), "tt=1".into())));
    }

    #[test]
    fn rejects_playlists() {
        assert!(parse(br#"{"_type":"playlist","entries":[]}"#).is_err());
    }

    #[test]
    fn friendly_errors() {
        assert!(friendly_error("ERROR: [youtube] abc: Requested format is not available. Use --list").contains("HLS/DASH"));
        assert!(friendly_error("ERROR: Unsupported URL: https://x").contains("No downloadable video"));
        assert_eq!(friendly_error("ERROR: [facebook] 123: This video is private"), "This video is private");
    }

    #[test]
    fn wait_is_capped() {
        assert_eq!(wait_needed(None), Duration::ZERO);
        assert_eq!(wait_needed(Some(u64::MAX / 2)), Duration::from_secs(30));
    }
}