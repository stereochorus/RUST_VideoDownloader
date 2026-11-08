// Popup: talks to mdl-daemon directly over its own native port.
// When the popup closes, the port closes and the shim exits; downloads keep running in the daemon.

const HOST = "com.mdl.host";
const $ = (id) => document.getElementById(id);
const jobs = new Map();   // id -> snapshot
const speeds = new Map(); // id -> bytes/s
let port;

function connect() {
  port = chrome.runtime.connectNative(HOST);
  port.onMessage.addListener(onReply);
  port.onDisconnect.addListener(() => {
    const err = chrome.runtime.lastError?.message || "disconnected";
    setStatus("err", "offline");
    showBanner(
      /not found|not installed|forbidden/i.test(err)
        ? `Native host not installed. Run <code>install.ps1</code> in the project folder, then reopen this popup.<br><small>${esc(err)}</small>`
        : `Lost connection to mdl-daemon: ${esc(err)}`
    );
  });
  port.postMessage({ cmd: "subscribe" });
}

function onReply(msg) {
  switch (msg.type) {
    case "jobs":
      setStatus("ok", "daemon running");
      jobs.clear();
      for (const j of msg.jobs) jobs.set(j.id, j);
      renderAll();
      break;
    case "event": {
      const ev = msg.event;
      if (ev.kind === "progress") {
        const j = jobs.get(ev.id);
        if (!j) break;
        Object.assign(j, { downloaded: ev.downloaded, total: ev.total });
        speeds.set(ev.id, ev.speed);
        renderJob(j);
      } else if (ev.kind === "changed") {
        if (ev.job.state.state !== "running") speeds.delete(ev.job.id);
        const isNew = !jobs.has(ev.job.id);
        jobs.set(ev.job.id, ev.job);
        isNew ? renderAll() : renderJob(ev.job);
      } else if (ev.kind === "removed") {
        jobs.delete(ev.id);
        renderAll();
      }
      break;
    }
    case "error":
      showBanner(esc(msg.message));
      setTimeout(() => ($("banner").hidden = true), 4000);
      break;
  }
}

function send(cmd, id) {
  port?.postMessage({ cmd, id });
}

// ---- rendering ---------------------------------------------------------------------

function renderAll() {
  const list = $("jobs");
  list.textContent = "";
  // Newest first.
  for (const j of [...jobs.values()].sort((a, b) => b.id - a.id)) {
    const li = document.createElement("li");
    li.id = `job-${j.id}`;
    li.innerHTML = `
      <div class="row"><span class="name"></span><span class="actions"></span></div>
      <div class="bar"><i></i></div>
      <div class="meta"></div>`;
    list.appendChild(li);
    renderJob(j);
  }
  $("empty").hidden = jobs.size > 0;
}

const ACTIONS = {
  running:   [["⏸", "pause", "Pause"], ["✕", "cancel", "Cancel and delete partial file"]],
  queued:    [["⏸", "pause", "Pause"], ["✕", "cancel", "Cancel"]],
  muxing:    [["✕", "cancel", "Cancel"]],
  paused:    [["▶", "resume", "Resume"], ["✕", "cancel", "Cancel and delete partial file"]],
  failed:    [["↻", "resume", "Retry (continues where it stopped)"], ["🗑", "remove", "Remove from list"]],
  done:      [["📂", "reveal", "Show in folder"], ["🗑", "remove", "Remove from list (keeps the file)"]],
  cancelled: [["🗑", "remove", "Remove from list"]],
};

function renderJob(j) {
  const li = $(`job-${j.id}`);
  if (!li) return;
  const state = j.state.state;
  li.className = `job ${state}`;

  const name = j.dest.split(/[\\/]/).pop();
  const nameEl = li.querySelector(".name");
  nameEl.textContent = name;
  nameEl.title = j.dest;

  const pct = j.total > 0 ? (j.downloaded / j.total) * 100 : 0;
  li.querySelector(".bar > i").style.width = `${state === "done" ? 100 : pct}%`;

  const meta = li.querySelector(".meta");
  if (state === "failed") {
    meta.innerHTML = `<span class="error"></span>`;
    meta.firstChild.textContent = j.state.error;
  } else {
    const parts = [state];
    if (j.total > 0) parts.push(`${pct.toFixed(1)}%`, `${fmtBytes(j.downloaded)} / ${fmtBytes(j.total)}`);
    const s = speeds.get(j.id);
    if (state === "running" && s) {
      parts.push(`${fmtBytes(s)}/s`);
      if (j.total > j.downloaded) parts.push(`${fmtEta((j.total - j.downloaded) / s)} left`);
    }
    meta.textContent = parts.join(" · ");
  }

  const key = (ACTIONS[state] || []).map((a) => a[1]).join();
  const actions = li.querySelector(".actions");
  if (actions.dataset.key !== key) {
    actions.dataset.key = key;
    actions.textContent = "";
    for (const [icon, cmd, title] of ACTIONS[state] || []) {
      const b = document.createElement("button");
      b.textContent = icon;
      b.title = title;
      b.addEventListener("click", () => send(cmd, j.id));
      actions.appendChild(b);
    }
  }
}

function fmtBytes(n) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}

function fmtEta(sec) {
  if (!isFinite(sec)) return "?";
  if (sec < 60) return `${Math.ceil(sec)}s`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ${Math.round(sec % 60)}s`;
  return `${Math.floor(sec / 3600)}h ${Math.round((sec % 3600) / 60)}m`;
}

function setStatus(cls, text) {
  const s = $("status");
  s.className = `status ${cls}`;
  s.textContent = text;
}

function showBanner(html) {
  $("banner").innerHTML = html;
  $("banner").hidden = false;
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

// ---- save folder setting -----------------------------------------------------------

chrome.storage.local.get("saveDir").then(({ saveDir = "" }) => ($("saveDir").value = saveDir));
$("saveDir").addEventListener("change", (e) => {
  const v = e.target.value.trim();
  if (v && !/^([a-zA-Z]:[\\/]|\\\\)/.test(v)) {
    showBanner("Please enter a full path like <code>D:\\Videos</code>.");
    return;
  }
  $("banner").hidden = true;
  chrome.storage.local.set({ saveDir: v });
});

connect();
