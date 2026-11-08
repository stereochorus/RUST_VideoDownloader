// media_downloader service worker: turns a "download" click from content.js into an `add` for
// mdl-daemon. It holds no download state: MV3 workers die after ~30 s idle, the daemon is the
// source of truth.
//
// Two kinds of request:
//   direct  the <video> has a real http(s) file URL (plain MP4 sites): download that URL.
//   page    the <video> plays a blob: (YouTube, Facebook, most streaming sites): send the page URL;
//           the daemon asks yt-dlp for the real streams, then downloads and merges them.

const HOST = "com.mdl.host";

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg?.type !== "download") return;
  handleDownload(msg)
    .then(sendResponse)
    .catch((e) => sendResponse({ ok: false, message: String(e?.message || e) }));
  return true; // async response
});

async function handleDownload({ src, pageUrl, videoPageUrl, title }) {
  const direct = /^https?:/i.test(src || "");
  const url = direct ? src : videoPageUrl || pageUrl;

  const headers = [["Referer", pageUrl], ["User-Agent", navigator.userAgent]];
  const cookies = await chrome.cookies.getAll({ url }).catch(() => []);
  if (cookies.length) headers.push(["Cookie", cookies.map((c) => `${c.name}=${c.value}`).join("; ")]);

  const { saveDir = "" } = await chrome.storage.local.get("saveDir");
  const reply = await chrome.runtime.sendNativeMessage(HOST, {
    cmd: "add",
    url,
    dest_dir: saveDir,
    // For page downloads the daemon names the file after the real video title from yt-dlp.
    filename: direct ? suggestName(title, url) : null,
    headers,
    resolve: !direct,
  });

  if (reply?.type === "added") {
    flashBadge("+1", "#2e7d32");
    return { ok: true, message: `Saving to ${reply.job.dest}` };
  }
  flashBadge("!", "#c62828");
  return { ok: false, message: reply?.message || "mdl-daemon did not answer" };
}

/** Page title + the URL's extension (the daemon sanitizes again; this just makes it pleasant). */
function suggestName(title, url) {
  const ext = (new URL(url).pathname.match(/\.(mp4|webm|m4v|mov|mkv|mp3|m4a)$/i) || [".mp4"])[0].toLowerCase();
  const base = (title || "video").replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ").replace(/\s+/g, " ").trim().slice(0, 120);
  return (base || "video") + ext;
}

function flashBadge(text, color) {
  chrome.action.setBadgeBackgroundColor({ color });
  chrome.action.setBadgeText({ text });
  setTimeout(() => chrome.action.setBadgeText({ text: "" }), 4000);
}