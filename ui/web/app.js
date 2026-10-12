// photo-stamp dashboard. Talks only to photo-stamp-ui (same origin), which passes /api/s/<key>/…
// on to the service watching that folder.
"use strict";

const $ = (id) => document.getElementById(id);

// Build an element: el("td", {class: "x"}, "text", child…). Text is never parsed as HTML.
function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "dataset") Object.assign(e.dataset, v);
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === undefined || c === null || c === false) continue;
    e.append(c instanceof Node ? c : String(c));
  }
  return e;
}

const state = {
  services: [],
  here: null,
  key: null,
  snap: null,        // last /status snapshot, kept current by events
  events: null,      // EventSource
  results: [],       // newest first
  lastNow: null,     // the most recent finished entry, shown when idle
  oldestLoaded: null,
  etaAt: 0,          // when progress.eta_secs was received
  pendingRows: [],   // results held back while the table is hovered
  hoveringTable: false,
  logFilter: "",
  logProblems: false,
};

// ------------------------------------------------------------------ helpers

async function api(path, opts = {}) {
  const res = await fetch(path, {
    method: opts.method || "GET",
    headers: opts.body !== undefined ? { "Content-Type": "application/json" } : {},
    body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
  });
  let data = null;
  try { data = await res.json(); } catch { /* not JSON */ }
  if (!res.ok) throw new Error((data && data.error) || `${res.status} ${res.statusText}`);
  return data;
}
const svc = (path) => `/api/s/${state.key}/${path}`;

function toast(text) {
  const t = $("toast");
  t.textContent = text;
  t.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (t.hidden = true), 4000);
}

function duration(secs) {
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs} s`;
  const m = Math.round(secs / 60);
  if (m < 60) return `${m} min`;
  const h = Math.floor(m / 60);
  return `${h} h ${m % 60} min`;
}
function ms(n) {
  return n < 1000 ? `${n} ms` : n < 60000 ? `${(n / 1000).toFixed(1)} s` : duration(n / 1000);
}
function mb(bytes) {
  const v = bytes / 1048576;
  return v >= 100 ? `${Math.round(v)} MB` : `${v.toFixed(1)} MB`;
}
function clock(t) {
  const d = new Date(t);
  const today = new Date();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  return d.toDateString() === today.toDateString() ? time : `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
}
function since(t) {
  return duration((Date.now() - t) / 1000);
}
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
const thumb = (id, which) => svc(`thumb/${id}/${which}`);

// ------------------------------------------------------------------ services

async function loadServices() {
  let list;
  try {
    list = await api("/api/services");
  } catch (e) {
    showEmpty("The dashboard can't be reached", "Start photo-stamp-ui again, then reload this page.");
    return;
  }
  state.services = list.services;
  state.here = list.here;
  const pick = $("folder-select");
  pick.replaceChildren(...state.services.map((s) => el("option", { value: s.key }, s.root)));
  $("folder-pick").hidden = state.services.length < 2;

  let saved = null;
  try { saved = localStorage.getItem("photo-stamp-folder"); } catch { /* private mode */ }
  const chosen = state.services.find((s) => s.key === state.key) || state.services.find((s) => s.key === saved) || state.services[0];
  if (!chosen) {
    setState("stopped", "Not running");
    if (state.here) {
      showEmpty("Nothing is being watched yet",
        `Start photo-stamp to stamp photos dropped into ${state.here}. Stamped photos go to its outbox folder.`,
        "Start watching this folder", startHere);
    } else {
      showEmpty("Nothing is being watched yet",
        "Run photo-stamp.exe in the folder you want to drop photos into, then reload this page.");
    }
    return;
  }
  pick.value = chosen.key;
  select(chosen.key);
}

function current() {
  return state.services.find((s) => s.key === state.key);
}

function select(key) {
  if (state.events) state.events.close();
  state.events = null;
  state.key = key;
  state.snap = null;
  state.results = [];
  state.lastNow = null;
  try { localStorage.setItem("photo-stamp-folder", key); } catch { /* private mode */ }
  const s = current();
  $("folder-name").textContent = state.services.length < 2 ? s.root : "";
  renderStartup();
  if (!s.running) {
    stopped();
    return;
  }
  connect();
}

function stopped() {
  const s = current();
  setState("stopped", "Not running");
  $("progress").hidden = true;
  showEmpty("photo-stamp isn't running",
    `Photos dropped into ${s.root} won't be stamped until it's started again.`,
    s.can_start || state.here ? "Start photo-stamp" : null, startService);
}

function showEmpty(title, text, action, onAction) {
  $("empty").hidden = false;
  $("empty-title").textContent = title;
  $("empty-text").textContent = text;
  const b = $("empty-action");
  b.hidden = !action;
  if (action) {
    b.textContent = action;
    b.onclick = async () => {
      b.disabled = true;
      try { await onAction(); } finally { b.disabled = false; }
    };
  }
  for (const p of document.querySelectorAll(".panel")) p.hidden = true;
  $("btn-start").hidden = !action;
  $("btn-pause").hidden = $("btn-resume").hidden = $("btn-stop").hidden = true;
}

function hideEmpty() {
  $("empty").hidden = true;
  showTab(currentTab());
}

async function startHere() {
  setState("loading", "Starting");
  try {
    const r = await api("/api/start-here", { method: "POST", body: {} });
    state.key = r.key;
    await loadServices();
  } catch (e) {
    toast(e.message);
    loadServices();
  }
}

async function startService() {
  setState("loading", "Starting");
  try {
    await api(svc("start"), { method: "POST", body: {} });
  } catch (e) {
    toast(e.message);
  }
  await loadServices();
}

// ------------------------------------------------------------------ live connection

function connect() {
  setState("loading", "Connecting");
  const es = new EventSource(svc("events"));
  state.events = es;
  let failures = 0;
  es.onmessage = (m) => {
    failures = 0;
    let ev;
    try { ev = JSON.parse(m.data); } catch { return; }
    if (ev.type === "hello") {
      refresh();
      return;
    }
    if (!state.snap || ev.seq <= state.snap.seq) return;
    state.snap.seq = ev.seq;
    apply(ev);
  };
  es.onerror = () => {
    // The browser reconnects by itself; if the service is gone, show that instead.
    if (++failures >= 2) {
      es.close();
      state.events = null;
      loadServices();
    }
  };
}

async function refresh() {
  try {
    const snap = await api(svc("status"));
    state.snap = snap;
    state.results = snap.recent.slice();
    state.oldestLoaded = state.results.length ? state.results[state.results.length - 1].id : null;
    state.lastNow = state.results.find((e) => e.outcome === "stamped") || null;
    state.etaAt = Date.now();
    hideEmpty();
    renderAll();
  } catch (e) {
    loadServices();
  }
}

function apply(ev) {
  const s = state.snap;
  switch (ev.type) {
    case "queue":
      s.queue = ev.queue;
      setProgress(ev.progress);
      renderQueue();
      break;
    case "started":
      s.current = ev.current;
      setProgress(ev.progress);
      s.queue = s.queue.filter((q) => q.path !== ev.current.path);
      renderQueue();
      renderNow();
      break;
    case "stage":
      if (s.current && s.current.id === ev.id) s.current = ev.current;
      renderNow();
      break;
    case "preview":
      if (s.current && s.current.id === ev.id) {
        if (ev.which === "before") s.current.preview = true;
        if (ev.which === "after") s.current.stamped = true;
      }
      renderNow();
      break;
    case "deferred":
      s.current = null;
      s.queue = s.queue.filter((q) => q.path !== ev.queued.path).concat([ev.queued]);
      setProgress(ev.progress);
      renderQueue();
      renderNow();
      break;
    case "done":
      s.current = null;
      s.session = ev.session;
      s.batch = ev.batch;
      setProgress(ev.progress);
      if (ev.entry.outcome === "stamped") state.lastNow = ev.entry;
      addResult(ev.entry);
      renderNow();
      renderTotals();
      renderExceptions();
      break;
    case "idle":
      s.batch = { done: 0, stamped: 0, unchanged: 0, failed: 0, started: 0 };
      setProgress({ done: 0, total: 0 });
      break;
    case "paused":
      s.paused = ev.paused;
      s.settings.paused = ev.paused;
      renderState();
      renderQueue();
      break;
    case "settings":
      s.settings = ev.settings;
      s.paused = ev.settings.paused;
      renderState();
      break;
    case "health":
      s.health = ev.health;
      renderHealth();
      break;
    case "log":
      s.log.push(ev.line);
      if (s.log.length > 1000) s.log.shift();
      appendLog(ev.line);
      break;
  }
}

// ------------------------------------------------------------------ header and progress

function setState(kind, text) {
  const st = $("state");
  st.dataset.state = kind;
  st.textContent = text;
}

function renderState() {
  const s = state.snap;
  if (!s) return;
  const busy = !!s.current;
  if (s.paused) setState("paused", "Paused");
  else if (busy) setState("working", "Stamping");
  else setState("running", "Watching");
  $("btn-pause").hidden = s.paused;
  $("btn-resume").hidden = !s.paused;
  $("btn-stop").hidden = false;
  $("btn-start").hidden = true;
}

function setProgress(p) {
  state.snap.progress = p;
  state.etaAt = Date.now();
  renderProgress();
}

function renderProgress() {
  const p = state.snap && state.snap.progress;
  const bar = $("progress");
  if (!p || !p.total) {
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  $("progress-fill").style.width = `${(100 * p.done) / p.total}%`;
  $("progress-count").textContent = `${p.done} of ${plural(p.total, "photo", "photos")}`;
  let eta = "";
  if (state.snap.paused) eta = "Paused";
  else if (p.eta_secs != null) {
    const left = p.eta_secs - (Date.now() - state.etaAt) / 1000;
    eta = left > 1 ? `About ${duration(left)} left` : "Almost done";
    if (p.per_min) eta += `, ${p.per_min < 10 ? p.per_min.toFixed(1) : Math.round(p.per_min)} a minute`;
  }
  $("progress-eta").textContent = eta;
}
setInterval(() => state.snap && renderProgress(), 1000);

// ------------------------------------------------------------------ now and queue

const STAGES = ["locating", "drawing", "saving"];

function renderNow() {
  const s = state.snap;
  const now = $("now");
  const img = $("now-img");
  const cur = s.current;
  renderState();
  if (cur) {
    now.dataset.busy = "";
    $("now-path").textContent = cur.path;
    $("now-placeholder").hidden = cur.preview;
    $("now-placeholder").textContent = "Reading the photo";
    const which = cur.stamped ? "after" : "before";
    if (cur.preview) {
      const src = thumb(cur.id, which);
      if (img.getAttribute("src") !== src) img.src = src;
      img.dataset.which = which;
    } else {
      img.removeAttribute("src");
    }
    const at = cur.stage === "moving" ? STAGES.length : STAGES.indexOf(cur.stage);
    for (const li of $("stages").children) {
      const i = STAGES.indexOf(li.dataset.stage);
      li.dataset.state = i < at ? "done" : i === at ? "active" : "";
    }
    $("now-caption").replaceChildren();
    return;
  }
  delete now.dataset.busy;
  const last = state.lastNow;
  if (last && last.thumbs) {
    img.src = thumb(last.id, "after");
    img.dataset.which = "after";
    $("now-placeholder").hidden = true;
    $("now-path").textContent = `Last stamped: ${last.path}`;
    $("now-caption").replaceChildren(...last.caption.map((l) => el("div", {}, l)));
  } else {
    img.removeAttribute("src");
    $("now-placeholder").hidden = false;
    $("now-placeholder").textContent = s.paused
      ? "Paused. New photos are noticed but wait until you resume."
      : `Drop photos into ${s.root} to stamp them.`;
    $("now-path").textContent = "";
    $("now-caption").replaceChildren();
  }
}

function waitingChip(q) {
  const until = q.until ? ` until ${clock(q.until)}` : "";
  switch (q.kind) {
    case "settling": return el("span", { class: "chip", dataset: { kind: "settling" } }, "Copying");
    case "ready": return el("span", { class: "chip", dataset: { kind: "ready" } }, "Next up");
    case "paused": return el("span", { class: "chip", dataset: { kind: "paused" } }, "Paused");
    case "offline": return el("span", { class: "chip", dataset: { kind: "offline" }, title: `Map data couldn't be reached; trying again${until}` }, "Waiting for map data");
    case "busy": return el("span", { class: "chip", dataset: { kind: "busy" }, title: `Another program has it open; trying again${until}` }, "In use");
    default: return el("span", { class: "chip" }, q.kind);
  }
}

function renderQueue() {
  const q = state.snap.queue;
  $("queue-count").textContent = q.length || "";
  $("queue-empty").hidden = q.length > 0;
  $("queue-list").replaceChildren(...q.slice(0, 300).map((item) =>
    el("li", {}, el("span", { class: "path", title: item.path }, item.path), waitingChip(item))));
  if (q.length > 300) $("queue-list").append(el("li", { class: "quiet" }, `and ${q.length - 300} more`));
}

// ------------------------------------------------------------------ results

function outcomeLabel(e) {
  return { stamped: "Stamped", unchanged: "Not stamped", failed: "Couldn't process" }[e.outcome];
}

function resultRow(e) {
  const cap = e.caption.length
    ? [el("b", {}, e.caption[0]), e.caption.length > 1 ? `, ${e.caption.slice(1).join(", ")}` : ""]
    : [e.reason || ""];
  return el("tr", { onclick: () => openViewer(e), title: e.output || e.path },
    el("td", { class: "thumb" }, e.thumbs
      ? el("img", { src: thumb(e.id, "after"), alt: "", loading: "lazy", width: 52, height: 40 })
      : el("div", { class: "no-thumb" })),
    el("td", { class: "path" }, e.path),
    el("td", {}, el("span", { class: "outcome", dataset: { outcome: e.outcome } }, outcomeLabel(e))),
    el("td", { class: "cap" }, cap),
    el("td", { class: "num" }, ms(e.ms)),
    el("td", { class: "num" }, clock(e.finished)));
}

function renderResults() {
  const body = $("results");
  body.replaceChildren(...state.results.map(resultRow));
  $("results-empty").hidden = state.results.length > 0;
  $("results-more").hidden = state.results.length < 100;
  state.pendingRows = [];
  $("new-rows").hidden = true;
}

function addResult(e) {
  state.results.unshift(e);
  if (state.hoveringTable || $("results-wrap").scrollTop > 40) {
    // Don't move rows under the pointer: hold new ones until asked.
    state.pendingRows.push(e);
    const b = $("new-rows");
    b.hidden = false;
    b.textContent = `Show ${plural(state.pendingRows.length, "new photo", "new photos")}`;
    return;
  }
  const row = resultRow(e);
  row.classList.add("fresh");
  $("results").prepend(row);
  $("results-empty").hidden = true;
}

$("new-rows").onclick = () => {
  renderResults();
  $("results-wrap").scrollTop = 0;
};
$("results-wrap").addEventListener("mouseenter", () => (state.hoveringTable = true));
$("results-wrap").addEventListener("mouseleave", () => (state.hoveringTable = false));

$("results-more").onclick = async () => {
  if (!state.oldestLoaded) return;
  try {
    const older = await api(svc(`results?before=${state.oldestLoaded}&limit=100`));
    state.results.push(...older);
    if (older.length) state.oldestLoaded = older[older.length - 1].id;
    $("results").append(...older.map(resultRow));
    $("results-more").hidden = older.length < 100;
    renderExceptions();
  } catch (e) {
    toast(e.message);
  }
};

function renderTotals() {
  const t = state.snap.session;
  const all = state.snap.all_time;
  const parts = [];
  if (t.stamped + t.unchanged + t.failed) {
    parts.push(`${t.stamped} stamped since ${clock(state.snap.started)}`);
    if (t.downloaded) parts.push(`${mb(t.downloaded)} of map data`);
  }
  parts.push(`${all.stamped} stamped in all`);
  $("totals").textContent = parts.join(", ");
}

// ------------------------------------------------------------------ viewer

function openViewer(e) {
  const d = $("viewer");
  const show = (which) => {
    $("viewer-img").src = thumb(e.id, which);
    for (const b of d.querySelectorAll("[data-which]")) b.setAttribute("aria-pressed", String(b.dataset.which === which));
  };
  for (const b of d.querySelectorAll("[data-which]")) b.onclick = () => show(b.dataset.which);
  d.querySelector(".viewer-toggle").hidden = !e.thumbs;
  $("viewer-img").hidden = !e.thumbs;
  if (e.thumbs) show("after");
  $("viewer-caption").replaceChildren(...(e.caption.length ? e.caption : [outcomeLabel(e) + (e.reason ? `: ${e.reason}` : "")]).map((l) => el("div", {}, l)));
  const where = e.output ? `Saved as ${e.output}` : `Still at ${e.path} in the watched folder`;
  $("viewer-path").textContent = `${where}. Took ${ms(e.ms)}${e.downloaded ? `, downloading ${mb(e.downloaded)} of map data` : ""}.`;
  d.showModal();
}

// ------------------------------------------------------------------ exceptions

function renderExceptions() {
  const all = state.results.filter((e) => e.outcome !== "stamped");
  const failedPaths = new Set();
  // A photo that failed and was later processed isn't a problem any more.
  const later = new Set(state.results.filter((e) => e.outcome !== "failed").map((e) => e.path));
  const items = all.filter((e) => {
    if (e.outcome === "failed") {
      if (failedPaths.has(e.path) || later.has(e.path)) return false;
      failedPaths.add(e.path);
    }
    return true;
  });
  $("exceptions-count").textContent = items.length || "";
  $("exceptions-empty").hidden = items.length > 0;
  const groups = new Map();
  for (const e of items) {
    const key = e.outcome === "failed" ? "Couldn't be processed" : e.reason ? e.reason[0].toUpperCase() + e.reason.slice(1) : "Not stamped";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(e);
  }
  $("exceptions").replaceChildren(...[...groups].map(([title, list]) =>
    el("section", { class: "group" },
      el("h2", {}, title, el("span", { class: "count" }, list.length)),
      el("ul", {}, list.map((e) => el("li", {},
        el("span", { class: "path" }, e.path),
        el("span", { class: "why" }, e.outcome === "failed" ? e.reason : e.output ? `In ${e.output}` : ""),
        e.outcome === "failed" ? el("button", { class: "ghost small", onclick: (ev) => retry(e, ev.target) }, "Try again") : null))))));
}

async function retry(e, button) {
  button.disabled = true;
  try {
    await api(svc("retry"), { method: "POST", body: { path: e.path } });
    toast(`Trying ${e.path} again`);
  } catch (err) {
    toast(err.message);
    button.disabled = false;
  }
}

// ------------------------------------------------------------------ health and log

function tile(title, rows, extra) {
  return el("section", { class: "tile" },
    el("h2", {}, title),
    el("dl", {}, rows.filter(Boolean).flatMap(([k, v, cls]) => [el("dt", {}, k), el("dd", { class: cls }, v)])),
    extra);
}

function renderHealth() {
  const s = state.snap;
  const h = s.health || {};
  const o = h.overture;
  const all = s.all_time;
  const tiles = [
    tile("photo-stamp", [
      ["Status", s.paused ? "Paused" : "Running", s.paused ? "warn" : "good"],
      ["Version", s.version],
      ["Running for", since(s.started)],
      ["Watching", s.root],
      ["Outbox", s.outbox],
    ]),
  ];
  if (o) {
    const cacheShare = o.cache_cap_bytes ? Math.min(1, o.cache_bytes / o.cache_cap_bytes) : 0;
    const online = o.online === null || o.online === undefined
      ? ["Connection", "Nothing downloaded yet"]
      : o.online ? ["Connection", "Working", "good"] : ["Connection", o.last_error ? `Failing: ${o.last_error}` : "Failing", "bad"];
    tiles.push(tile("Map data (Overture Maps)", [
      ["Release", o.release ? `${o.release}${o.release_age_days != null ? `, ${o.release_age_days} days old` : ""}` : "Not chosen yet"],
      ["Indexes", o.indexes_ready === o.indexes_total ? "Ready" : `${o.indexes_ready} of ${o.indexes_total} ready`, o.indexes_ready === o.indexes_total ? "good" : "warn"],
      online,
      ["Cache", `${mb(o.cache_bytes)} of ${mb(o.cache_cap_bytes)}`],
      ["Downloaded", `${mb(o.downloaded_bytes)} since starting`],
    ], el("div", { class: "meter", title: "Cache use" }, el("div"))));
    // Set the meter width through the DOM (no inline style attributes under the page's CSP).
    tiles[tiles.length - 1].querySelector(".meter div").style.width = `${(cacheShare * 100).toFixed(1)}%`;
  } else {
    tiles.push(tile("Map data", [["Mode", "Offline: bundled towns and landmarks only", "warn"]]));
  }
  tiles.push(tile("Place names", [
    ["Bundled data", h.places_built ? `Built ${h.places_built}` : "Unknown"],
    ["Sources", "GeoNames, Wikidata"],
  ]));
  const avg = all.stamped ? all.ms / all.stamped : 0;
  tiles.push(tile("All time", [
    ["Stamped", all.stamped.toLocaleString()],
    ["Not stamped", all.unchanged.toLocaleString()],
    ["Couldn't process", all.failed.toLocaleString(), all.failed ? "warn" : null],
    ["Average", avg ? ms(Math.round(avg)) + " a photo" : "–"],
    ["Map data", mb(all.downloaded)],
  ]));
  $("tiles").replaceChildren(...tiles);
}

function logLine(l) {
  const problem = l.level === "warn" || l.level === "error";
  if (state.logProblems && !problem) return null;
  if (state.logFilter && !l.text.toLowerCase().includes(state.logFilter)) return null;
  return el("div", { class: problem ? l.level : "" },
    el("time", {}, new Date(l.at).toLocaleTimeString()), l.text);
}

function renderLog() {
  const log = $("log");
  log.replaceChildren(...state.snap.log.map(logLine).filter(Boolean));
  log.scrollTop = log.scrollHeight;
}

function appendLog(l) {
  const log = $("log");
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 30;
  const line = logLine(l);
  if (!line) return;
  log.append(line);
  while (log.childElementCount > 1000) log.firstElementChild.remove();
  if (atBottom) log.scrollTop = log.scrollHeight;
}

$("log-filter").addEventListener("input", (e) => {
  state.logFilter = e.target.value.trim().toLowerCase();
  if (state.snap) renderLog();
});
$("log-problems").addEventListener("change", (e) => {
  state.logProblems = e.target.checked;
  if (state.snap) renderLog();
});
setInterval(() => state.snap && currentTab() === "health" && renderHealth(), 30000);

// ------------------------------------------------------------------ settings

function renderSettings() {
  const st = state.snap.settings;
  const form = $("settings");
  if (form.contains(document.activeElement)) return; // don't overwrite while editing
  $("set-outbox").value = st.outbox || "";
  $("set-outbox").placeholder = `${state.snap.root}${state.snap.root.includes("\\") ? "\\" : "/"}outbox`;
  $("set-quality").value = st.quality;
  $("set-quality-out").textContent = st.quality;
  $("set-settle").value = st.settle_secs;
  $("set-poll").value = st.poll_secs;
  $("set-notify").checked = st.notify;
  $("set-offline").checked = st.offline;
}

function renderStartup() {
  const s = current();
  const row = $("set-startup-row");
  row.hidden = !s || s.startup === null || s.startup === undefined;
  if (s && !row.hidden) $("set-startup").checked = s.startup;
}

$("set-quality").addEventListener("input", (e) => ($("set-quality-out").textContent = e.target.value));

$("settings").addEventListener("submit", async (e) => {
  e.preventDefault();
  const st = state.snap.settings;
  const next = {
    ...st,
    outbox: $("set-outbox").value.trim() || null,
    quality: Number($("set-quality").value),
    settle_secs: Number($("set-settle").value),
    poll_secs: Number($("set-poll").value),
    notify: $("set-notify").checked,
    offline: $("set-offline").checked,
  };
  const status = $("set-status");
  const save = $("set-save");
  save.disabled = true;
  try {
    const s = current();
    const wantStartup = $("set-startup").checked;
    if (!$("set-startup-row").hidden && wantStartup !== s.startup) {
      const r = await api(svc("startup"), { method: "POST", body: { on: wantStartup } });
      s.startup = r.on;
    }
    const r = await api(svc("settings"), { method: "PUT", body: next });
    state.snap.settings = r.settings;
    if (r.restart) {
      status.textContent = "Restarting photo-stamp to apply the change";
      if (state.events) state.events.close();
      state.events = null;
      await api(svc("restart"), { method: "POST", body: {} });
      await loadServices();
    }
    status.textContent = "Saved";
    setTimeout(() => (status.textContent = ""), 3000);
  } catch (err) {
    status.textContent = err.message;
  } finally {
    save.disabled = false;
  }
});

// ------------------------------------------------------------------ tabs and buttons

function currentTab() {
  const t = document.querySelector('.tabs [aria-selected="true"]');
  return t ? t.dataset.tab : "activity";
}

function showTab(name) {
  for (const b of document.querySelectorAll(".tabs button")) b.setAttribute("aria-selected", String(b.dataset.tab === name));
  if (!$("empty").hidden) return;
  for (const p of document.querySelectorAll(".panel")) p.hidden = p.dataset.panel !== name;
  if (state.snap && name === "health") {
    renderHealth();
    renderLog();
  }
  if (state.snap && name === "settings") renderSettings();
}
for (const b of document.querySelectorAll(".tabs button")) b.addEventListener("click", () => showTab(b.dataset.tab));

$("folder-select").addEventListener("change", (e) => select(e.target.value));

$("btn-pause").onclick = () => api(svc("pause"), { method: "POST", body: {} }).catch((e) => toast(e.message));
$("btn-resume").onclick = () => api(svc("resume"), { method: "POST", body: {} }).catch((e) => toast(e.message));
$("btn-start").onclick = () => (state.key ? startService() : startHere());
$("btn-stop").onclick = async () => {
  const busy = state.snap && state.snap.current;
  const note = busy ? " It finishes the photo it's working on first." : "";
  if (!confirm(`Stop photo-stamp? Photos dropped into the folder won't be stamped until it's started again.${note}`)) return;
  setState("loading", "Stopping");
  try {
    await api(svc("stop"), { method: "POST", body: {} });
  } catch (e) {
    toast(e.message);
  }
  if (state.events) state.events.close();
  state.events = null;
  setTimeout(loadServices, 1500);
};

function renderAll() {
  renderState();
  renderProgress();
  renderNow();
  renderQueue();
  renderResults();
  renderTotals();
  renderExceptions();
  const tab = currentTab();
  if (tab === "health") {
    renderHealth();
    renderLog();
  }
  if (tab === "settings") renderSettings();
}

loadServices();
