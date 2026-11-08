// media_downloader content script: shows a "Download video" button over the video under the pointer.
// Sites often cover <video> with transparent overlays (custom controls), so instead of relying on
// mouse events reaching the video we hit-test pointer position against every video's rectangle.

(() => {
  if (window.__mdlLoaded) return;
  window.__mdlLoaded = true;

  const MIN_W = 160, MIN_H = 90;
  let current = null;     // video the button is attached to
  let hideTimer = null;
  let busy = false;

  // Shadow DOM keeps the page's CSS away from our button (and ours away from the page).
  const host = document.createElement("mdl-download-button");
  host.style.cssText = "all: initial; position: fixed; z-index: 2147483647; display: none; top: 0; left: 0;";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `
    <style>
      button {
        font: 600 13px/1 system-ui, "Segoe UI", sans-serif;
        display: flex; align-items: center; gap: 6px;
        padding: 8px 12px; border: 0; border-radius: 8px; cursor: pointer;
        background: rgba(20, 20, 24, .88); color: #fff;
        box-shadow: 0 2px 10px rgba(0,0,0,.35);
        max-width: 320px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
        transition: background .15s;
      }
      button:hover { background: #1565c0; }
      button.ok { background: #2e7d32; }
      button.err { background: #c62828; white-space: normal; text-align: left; line-height: 1.3; }
      svg { flex: none; }
    </style>
    <button type="button" title="Download with media_downloader">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"
           stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12m0 0l-5-5m5 5l5-5M5 21h14"/></svg>
      <span>Download video</span>
    </button>`;
  const btn = root.querySelector("button");
  const label = root.querySelector("span");
  (document.body || document.documentElement).appendChild(host);

  // Pages that are themselves one video: yt-dlp can use the address bar URL directly.
  const SINGLE_VIDEO_PAGE =
    /youtube\.com\/(watch|shorts\/|live\/)|youtu\.be\/|facebook\.com\/(.+\/videos\/|reel\/|watch\/?\?v=|share\/[rv]\/)|fb\.watch\/|tiktok\.com\/@[^/]*\/video\/\d|instagram\.com\/(p|reels?|tv)\/[\w-]/i;
  // A link to exactly one video: it must carry the video's ID. Facebook also has ID-less links like
  // /SomePage/videos/ (a page's video tab) that yt-dlp cannot use.
  const ONE_VIDEO_LINK =
    /youtube\.com\/(watch\?(.*&)?v=[\w-]{6,}|shorts\/[\w-]{6,})|youtu\.be\/[\w-]{6,}|facebook\.com\/(.+\/videos\/(.+\/)?\d{6,}|reel\/\d{6,}|watch\/?\?(.*&)?v=\d{6,}|share\/[rv]\/\w+)|fb\.watch\/\w+|tiktok\.com\/@[^/]*\/video\/\d{15,}|instagram\.com\/(p|reels?|tv)\/[\w-]{5,}/i;
  // Feed posts: one container per post (Facebook/Instagram <article>, YouTube renderers, TikTok items).
  const POST =
    '[role="article"], article, ytd-rich-item-renderer, ytd-video-renderer, ytd-compact-video-renderer, ytd-reel-item-renderer, [data-e2e="recommend-list-item-container"]';

  /** URL of the page that holds just this video. On feeds (YouTube home, Facebook timeline) the
   *  address bar shows the feed, so look for the video's own link near the player, but never
   *  outside the post that contains it (that would be some other post's video). */
  function videoPageUrl(v) {
    if (SINGLE_VIDEO_PAGE.test(location.href)) return location.href;
    const post = v.closest(POST);
    for (let el = v, depth = 0; el && depth < 30; el = el.parentElement, depth++) {
      for (const a of el.querySelectorAll?.("a[href]") || []) {
        if (ONE_VIDEO_LINK.test(a.href)) return a.href;
      }
      if (el === post) break;
    }
    return tiktokFeedUrl(v) || location.href;
  }

  /** TikTok's For You feed has no per-video links, but its player container is named
   *  `xgwrapper-<n>-<videoId>`. yt-dlp accepts `tiktok.com/@/video/<id>` without the username. */
  function tiktokFeedUrl(v) {
    if (!/(^|\.)tiktok\.com$/i.test(location.hostname)) return null;
    for (let el = v, depth = 0; el && depth < 15; el = el.parentElement, depth++) {
      const m = (el.id || "").match(/(?:^|\D)(\d{17,20})(?:\D|$)/);
      if (m) return `https://www.tiktok.com/@/video/${m[1]}`;
    }
    return null;
  }

  function videoSrc(v) {
    return v.currentSrc || v.src || v.querySelector("source[src]")?.src || "";
  }

  function videoAt(x, y) {
    for (const v of document.querySelectorAll("video")) {
      const r = v.getBoundingClientRect();
      if (r.width < MIN_W || r.height < MIN_H) continue;
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return v;
    }
    return null;
  }

  function place() {
    if (!current || !current.isConnected) return hide();
    const r = current.getBoundingClientRect();
    host.style.top = `${Math.max(r.top + 10, 4)}px`;
    host.style.left = `${Math.min(r.right - 10, window.innerWidth - 4)}px`;
    host.style.transform = "translateX(-100%)";
  }

  function show(v) {
    clearTimeout(hideTimer);
    if (current !== v) {
      current = v;
      if (!busy) setState("", "Download video");
    }
    host.style.display = "block";
    place();
  }

  function hide() {
    if (busy) return;
    host.style.display = "none";
    current = null;
  }

  function setState(cls, text) {
    btn.className = cls;
    label.textContent = text;
  }

  let lastMove = 0;
  document.addEventListener(
    "mousemove",
    (e) => {
      const now = performance.now();
      if (now - lastMove < 80) return;
      lastMove = now;
      if (e.composedPath().includes(host)) return clearTimeout(hideTimer);
      const v = videoAt(e.clientX, e.clientY);
      if (v) show(v);
      else if (current) {
        clearTimeout(hideTimer);
        hideTimer = setTimeout(hide, 1200);
      }
    },
    { passive: true, capture: true }
  );
  // "Click a video" also summons it (touchpads, or the pointer was already resting there).
  document.addEventListener("click", (e) => {
    if (e.composedPath().includes(host)) return;
    const v = videoAt(e.clientX, e.clientY);
    if (v) show(v);
  }, { capture: true });
  window.addEventListener("scroll", place, { passive: true, capture: true });
  window.addEventListener("resize", place, { passive: true });

  btn.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (busy || !current) return;
    if (!chrome.runtime?.id) {
      setState("err", "Extension was reloaded: refresh this page.");
      return;
    }
    busy = true;
    const src = videoSrc(current);
    // Page lookups run yt-dlp first, which takes a few seconds.
    setState("", /^https?:/i.test(src) ? "Adding…" : "Finding video…");
    try {
      const res = await chrome.runtime.sendMessage({
        type: "download",
        src,
        pageUrl: location.href,
        videoPageUrl: videoPageUrl(current),
        title: document.title,
      });
      setState(res?.ok ? "ok" : "err", res?.ok ? "✓ Added. Open the toolbar popup for progress" : `✗ ${res?.message || "failed"}`);
    } catch (err) {
      setState("err", `✗ ${err.message || err}`);
    }
    setTimeout(() => {
      busy = false;
      setState("", "Download video");
      hide();
    }, 3500);
  });
})();
