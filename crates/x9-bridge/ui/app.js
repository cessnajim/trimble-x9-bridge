// X9 Bridge phone UI. Plain modules, no build step, no network beyond the bridge:
// the Pi has no internet in the field. Every API call sends the token; the
// event stream is read with fetch so it can send it too.

const PHASES = {
  waiting: "Waiting for the scanner to clear the last task",
  leveling: "Levelling",
  sweeping: "Scanning",
  imaging: "Taking pictures",
  color: "Taking pictures",
  downloading: "Downloading from the scanner",
  decoding: "Decoding points",
  writing: "Writing the file",
};

// ---------------------------------------------------------------- storage ---
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch { /* private mode */ } },
};

// ------------------------------------------------------------------ state ---
const S = {
  token: store.get("x9.token"),
  me: null,
  live: false,
  status: null,
  jobs: [],
  runs: [],
  recipes: [],
  recipe: {},      // recipe_id -> full recipe
  projects: [],
  modes: [],
  log: [],
  scans: [],
  scanById: {},    // every scan seen, so a link to a scan in another project still opens
  project: store.get("x9.project"),
  previews: new Map(), // scan_id -> object URL | "loading" | {error}
  dims: {},        // scan_id -> [w, h]
  boxes: {},       // picker key -> {x0, y0, x1, y1}
  drawMode: {},    // picker key -> bool
  aim: {},         // scan_id -> aim result
  exports: {},     // export_id -> export record
  watchExport: {}, // scan_id -> job_id
  sheet: null,
  toast: null,
  busy: false,
  route: { tab: "now" },
  signinError: "",
};

// ------------------------------------------------------------------ utils ---
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const deg = (rad) => `${(rad * 180 / Math.PI).toFixed(1)}°`;
const isOp = () => S.me && (S.me.role === "operator" || S.me.role === "admin");
const canAct = () => S.me && S.me.role !== "viewer";

function clock(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return isNaN(d) ? "" : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
function ago(iso) {
  if (!iso) return "";
  const s = Math.max(0, (Date.now() - new Date(iso)) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(iso).toLocaleDateString();
}
function elapsed(iso) {
  if (!iso) return "";
  const s = Math.max(0, Math.round((Date.now() - new Date(iso)) / 1000));
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`;
}
const span = (b, w) => (b.x1 >= b.x0 ? b.x1 - b.x0 : b.x1 + w - b.x0);
const gb = (b) => (b == null ? "–" : `${(b / 1e9).toFixed(1)} GB`);
const num = (n) => (n == null ? "–" : Number(n).toLocaleString());

function stateChip(state) {
  const cls = { succeeded: "good", done: "good", failed: "bad", cancelled: "", awaiting_approval: "warn", waiting_for_person: "warn", running: "info", queued: "info" }[state] ?? "";
  const label = { awaiting_approval: "needs approval", waiting_for_person: "needs you" }[state] ?? state;
  return `<span class="chip ${cls}">${esc(label)}</span>`;
}

function describeJob(j) {
  if (j.kind === "export") return `Export${j.result?.export_id && S.exports[j.result.export_id] ? " " + S.exports[j.result.export_id].format?.toUpperCase() : ""}`;
  const r = j.request;
  if (!r) return "Scan";
  const what = r.kind === "area" ? "Area scan" : "Full-dome scan";
  const extra = [r.mode, r.color ? "colour" : null].filter(Boolean).join(" · ");
  return `${what} · ${extra}`;
}
function jobDetail(j) {
  const r = j.request;
  const bits = [];
  if (j.project_name) bits.push(j.project_name);
  if (r?.window) bits.push(`window ${deg(r.window.delta_ha)} × ${deg(r.window.delta_va)}`);
  if (r?.note) bits.push(`“${r.note}”`);
  return bits.join(" · ");
}

function problemText(p) {
  if (!p) return "";
  if (typeof p === "string") return p;
  return [p.title, p.detail].filter(Boolean).join(": ") + (p.hint ? ` (${p.hint})` : "");
}

// -------------------------------------------------------------------- api ---
class ApiError extends Error {
  constructor(problem, status) {
    super(problemText(problem) || `HTTP ${status}`);
    this.problem = problem;
    this.status = status;
  }
}

async function api(path, { method = "GET", body, headers = {}, raw = false } = {}) {
  const h = { Authorization: `Bearer ${S.token}`, ...headers };
  if (body !== undefined) h["Content-Type"] = "application/json";
  const r = await fetch("/v1" + path, { method, headers: h, body: body === undefined ? undefined : JSON.stringify(body) });
  if (r.status === 401) {
    signOut("That token was not accepted.");
    throw new ApiError({ title: "Not signed in" }, 401);
  }
  if (!r.ok) {
    let p = null;
    try { p = await r.json(); } catch { /* not a problem document */ }
    throw new ApiError(p, r.status);
  }
  if (raw) return r;
  if (r.status === 204) return null;
  return (r.headers.get("content-type") || "").includes("json") ? r.json() : r.text();
}

async function act(fn, done) {
  if (S.busy) return;
  S.busy = true;
  render(true);
  try {
    const out = await fn();
    if (done) toast(done);
    return out;
  } catch (e) {
    if (e.status !== 401) toast(e.message, true);
  } finally {
    S.busy = false;
    render(true);
  }
}

function toast(text, bad = false) {
  S.toast = { text, bad };
  render(true);
  clearTimeout(toast.t);
  toast.t = setTimeout(() => { S.toast = null; render(true); }, bad ? 6000 : 2800);
}

// ---------------------------------------------------------------- loading ---
const upsert = (list, item, key) => {
  const i = list.findIndex((x) => x[key] === item[key]);
  if (i >= 0) list[i] = item; else list.unshift(item);
};
const byNewest = (a, b) => String(b.created_at).localeCompare(String(a.created_at));

async function load(what) {
  try {
    switch (what) {
      case "status": S.status = await api("/status"); break;
      case "jobs": S.jobs = (await api("/jobs")).sort(byNewest); break;
      case "runs": S.runs = (await api("/runs")).sort(byNewest); break;
      case "recipes": S.recipes = await api("/recipes"); break;
      case "projects":
        S.projects = await api("/projects");
        if (!S.project || !S.projects.some((p) => p.project_id === S.project)) {
          const latest = S.jobs.find((j) => j.request?.project_id && S.projects.some((p) => p.project_id === j.request.project_id));
          if (latest) { S.project = latest.request.project_id; store.set("x9.project", S.project); load("scans"); }
        }
        break;
      case "modes": S.modes = await api("/scan-modes"); break;
      case "log": S.log = (await api("/log")).reverse(); break;
      case "scans":
        S.scans = S.project ? await api(`/scans?project_id=${encodeURIComponent(S.project)}`) : [];
        for (const sc of S.scans) S.scanById[sc.scan_id] = sc;
        break;
    }
  } catch (e) {
    if (e.status !== 401 && what !== "status") console.warn(what, e.message);
    if (what === "status") S.status = { error: e.message };
  }
  render();
}

async function recipe(id) {
  if (!S.recipe[id]) {
    S.recipe[id] = "loading";
    try { S.recipe[id] = await api(`/recipes/${encodeURIComponent(id)}`); } catch (e) { S.recipe[id] = { error: e.message }; }
    render();
  }
  return typeof S.recipe[id] === "object" ? S.recipe[id] : null;
}

function preview(scanId) {
  const p = S.previews.get(scanId);
  if (p) return p;
  S.previews.set(scanId, "loading");
  api(`/scans/${encodeURIComponent(scanId)}/preview`, { raw: true })
    .then((r) => r.blob())
    .then((b) => S.previews.set(scanId, URL.createObjectURL(b)))
    .catch((e) => S.previews.set(scanId, { error: e.message }))
    .finally(() => render());
  return "loading";
}

async function loadExport(id) {
  try { S.exports[id] = await api(`/exports/${encodeURIComponent(id)}`); } catch (e) { S.exports[id] = { error: e.message }; }
  render();
}

async function download(path, name) {
  const r = await api(path, { raw: true });
  const cd = r.headers.get("content-disposition") || "";
  const m = cd.match(/filename="([^"]+)"/);
  const url = URL.createObjectURL(await r.blob());
  const a = document.createElement("a");
  a.href = url;
  a.download = m ? m[1] : name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

// ----------------------------------------------------------------- events ---
function onEvent(chunk) {
  let kind = "message";
  let data = "";
  for (const line of chunk.split("\n")) {
    if (line.startsWith("event:")) kind = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  if (!data) return;
  let v;
  try { v = JSON.parse(data); } catch { return; }
  if (kind === "job") {
    upsert(S.jobs, v, "job_id");
    S.jobs.sort(byNewest);
    if (v.kind === "export" && v.state === "succeeded" && v.result?.export_id && !S.exports[v.result.export_id]) loadExport(v.result.export_id);
    if (v.kind === "scan" && v.state === "succeeded") { load("projects"); load("scans"); load("status"); }
  } else if (kind === "run") {
    upsert(S.runs, v, "run_id");
    S.runs.sort(byNewest);
  } else if (kind === "log") {
    if (!S.log.some((e) => e.entry_id === v.entry_id)) S.log.unshift(v);
    S.log.length = Math.min(S.log.length, 500);
  }
  render();
}

async function stream() {
  let delay = 1000;
  while (S.token) {
    try {
      const r = await fetch("/v1/events", { headers: { Authorization: `Bearer ${S.token}` } });
      if (r.status === 401) { signOut("That token was not accepted."); return; }
      if (!r.ok || !r.body) throw new Error(`events: HTTP ${r.status}`);
      S.live = true;
      delay = 1000;
      refreshAll();
      const reader = r.body.pipeThrough(new TextDecoderStream()).getReader();
      let buf = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += value.replace(/\r\n/g, "\n");
        let i;
        while ((i = buf.indexOf("\n\n")) >= 0) {
          onEvent(buf.slice(0, i));
          buf = buf.slice(i + 2);
        }
      }
    } catch { /* reconnect below */ }
    S.live = false;
    render();
    await sleep(delay);
    delay = Math.min(delay * 2, 15000);
  }
}

function refreshAll() {
  ["status", "jobs", "runs", "recipes", "projects", "modes", "log", "scans"].forEach(load);
}

// ----------------------------------------------------------------- routes ---
function parseRoute() {
  const [tab, id] = location.hash.replace(/^#\/?/, "").split("/");
  if (tab === "run" && id) return { tab: "runs", run: decodeURIComponent(id) };
  if (tab === "scan" && id) return { tab: "scans", scan: decodeURIComponent(id) };
  return { tab: ["now", "runs", "scans", "log"].includes(tab) ? tab : "now" };
}

// ---------------------------------------------------------------- sign in ---
async function signIn(token) {
  S.token = token.trim();
  S.signinError = "";
  try {
    S.me = await api("/whoami");
    store.set("x9.token", S.token);
    render(true);
    refreshAll();
    stream();
    return true;
  } catch (e) {
    S.token = null;
    S.signinError = e.status === 401 ? "That token was not accepted." : `Could not reach the bridge: ${e.message}`;
    render(true);
    return false;
  }
}

function signOut(why = "") {
  S.token = null;
  S.me = null;
  S.signinError = why;
  store.set("x9.token", null);
  render(true);
}

// ---------------------------------------------------------------- render ---
const ICONS = {
  now: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>',
  runs: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 6h11M9 12h11M9 18h11"/><path d="M4 6l1 1 2-2M4 12l1 1 2-2M4 18l1 1 2-2"/></svg>',
  scans: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="3" width="8" height="11" rx="2"/><circle cx="12" cy="8" r="2"/><path d="M12 14v3M7 21l5-4 5 4"/></svg>',
  log: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h9l4 4v14H6z"/><path d="M9 11h7M9 15h7M9 7h4"/></svg>',
};

function needsYou() {
  const approvals = isOp() ? S.jobs.filter((j) => j.state === "awaiting_approval").length : 0;
  const inputs = S.runs.filter((r) => r.state === "waiting_for_person" && r.waiting_for === "input" && mayAdvance(r)).length;
  return approvals + inputs;
}
const mayAdvance = (r) => S.me && (isOp() || r.started_by === S.me.name);
const mayCancelJob = (j) => S.me && (isOp() || j.requested_by === S.me.name);

function viewSignin() {
  return `<div class="signin">
    <img class="logo" src="icon.svg" alt="">
    <h1>X9 Bridge</h1>
    <p class="meta">Paste the token you were given for this bridge. It stays on this phone.</p>
    ${S.signinError ? `<div class="problem">${esc(S.signinError)}</div>` : ""}
    <form data-form="signin">
      <label class="field"><span>Token</span><input name="token" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" required></label>
      <button class="btn primary wide" ${S.busy ? "disabled" : ""}>Sign in</button>
    </form>
  </div>`;
}

function viewTop() {
  const r = S.route;
  const title = r.run ? "Run" : r.scan ? "Scan" : { now: "Now", runs: "Recipes and runs", scans: "Scans", log: "Field log" }[r.tab];
  const back = r.run || r.scan ? `<button class="back" data-act="back">‹ Back</button>` : "";
  return `<header class="top">${back}<h1>${esc(title)}</h1>
    <span class="live ${S.live ? "on" : ""}">${S.live ? "Live" : "Offline"}</span>
    <button class="who" data-act="me">${esc(S.me?.name ?? "")}</button></header>`;
}

function viewNav() {
  const n = needsYou();
  const tab = (id, label) => `<a href="#/${id}" class="${S.route.tab === id ? "on" : ""}">${ICONS[id]}${label}${id === "now" && n ? `<span class="badge">${n}</span>` : ""}</a>`;
  return `<nav class="nav">${tab("now", "Now")}${tab("runs", "Runs")}${tab("scans", "Scans")}${tab("log", "Log")}</nav>`;
}

function jobCard(j, { actions = true } = {}) {
  const phase = j.phase ? PHASES[j.phase] ?? j.phase : j.state === "queued" ? "Queued" : "";
  const pct = typeof j.progress === "number" ? Math.round(j.progress * 100) : null;
  const running = j.state === "running" || j.state === "queued";
  const scanLink = j.result?.scan_id ? `<a href="#/scan/${encodeURIComponent(j.result.scan_id)}">Open scan</a>` : "";
  const partial = j.partial_scan_id ? `<div class="note">The scanner kept the partial sweep. <a href="#/scan/${encodeURIComponent(j.partial_scan_id)}">Keep or delete it</a></div>` : "";
  return `<div class="card ${j.state === "awaiting_approval" ? "attn" : ""}">
    <div class="line top-align"><strong class="grow">${esc(describeJob(j))}</strong>${stateChip(j.state)}</div>
    <div class="meta">${esc(jobDetail(j))}${jobDetail(j) ? " · " : ""}by ${esc(j.requested_by)} · ${esc(clock(j.created_at))}</div>
    ${running ? `<div class="bar ${pct == null ? "busy" : ""}"><i style="width:${pct ?? 0}%"></i></div>
      <div class="line meta"><span>${esc(phase)}${pct != null ? ` · ${pct}%` : ""}</span><span>${esc(elapsed(j.started_at))}</span></div>` : ""}
    ${j.problem ? `<div class="problem">${esc(problemText(j.problem))}</div>` : ""}
    ${partial}
    ${scanLink && !running ? `<div class="meta" style="margin-top:6px">${scanLink}</div>` : ""}
    ${actions && j.state === "awaiting_approval" && isOp() ? `<div class="btns">
      <button class="btn good" data-act="approve" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Approve</button>
      <button class="btn danger" data-act="cancel-job" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Deny</button></div>` : ""}
    ${actions && j.state === "awaiting_approval" && !isOp() ? `<p class="hint">Waiting for an operator to approve.</p>` : ""}
    ${actions && running && mayCancelJob(j) ? `<div class="btns"><button class="btn danger" data-act="cancel-job" data-id="${esc(j.job_id)}" ${S.busy ? "disabled" : ""}>Stop</button></div>` : ""}
  </div>`;
}

function viewNow() {
  const st = S.status;
  let scanner;
  if (!st) scanner = `<div class="card status"><p class="meta">Reading the scanner…</p></div>`;
  else if (st.error) scanner = `<div class="card"><div class="problem">${esc(st.error)}</div></div>`;
  else {
    const s = st.scanner || {};
    if (!s.connected) {
      scanner = `<div class="card attn"><h3>Scanner not connected</h3>
        <p class="meta">The bridge can't reach the X9. Check that it is on and linked to the Pi by USB or its Wi-Fi.</p></div>`;
    } else {
      const batt = s.battery_pct ?? null;
      const battCls = batt == null ? "" : batt < 15 ? "bad" : batt < 30 ? "warn" : "ok";
      const lev = s.leveling?.state ?? "–";
      scanner = `<div class="card status">
        <div class="line"><div class="battery"><span class="pct ${battCls}">${batt ?? "–"}%</span><span class="meta">${s.on_ac_power ? "on power" : "battery"}</span></div>
        ${s.busy || S.jobs.some((j) => j.kind === "scan" && j.state === "running") ? `<span class="chip info">busy</span>` : `<span class="chip good">ready</span>`}</div>
        <div class="bar"><i style="width:${batt ?? 0}%"></i></div>
        <div class="facts">
          <div class="fact"><b>Level</b><span class="${lev === "ok" ? "ok" : "warn"}">${esc(lev)}</span></div>
          <div class="fact"><b>Moved since last scan</b><span class="${s.moved_since_last_scan ? "warn" : "ok"}">${s.moved_since_last_scan ? "yes" : "no"}</span></div>
          <div class="fact"><b>Storage free</b><span>${gb(s.storage_free_bytes)}</span></div>
          <div class="fact"><b>Bridge</b><span>${esc(st.bridge?.version ?? "")}</span></div>
        </div></div>`;
    }
  }

  const approvals = S.jobs.filter((j) => j.state === "awaiting_approval");
  const waiting = S.runs.filter((r) => r.state === "waiting_for_person" && r.waiting_for === "input");
  const active = S.jobs.filter((j) => j.state === "running" || j.state === "queued");
  const recent = S.jobs.filter((j) => ["succeeded", "failed", "cancelled"].includes(j.state)).slice(0, 5);

  const needs = [
    ...approvals.map((j) => jobCard(j)),
    ...waiting.map((r) => {
      const rec = S.recipe[r.recipe_id];
      const step = typeof rec === "object" && rec.steps ? rec.steps.find((s) => (s.id ?? s.step_id) === r.current_step_id) : null;
      if (!S.recipe[r.recipe_id]) recipe(r.recipe_id);
      return `<a class="card link attn" href="#/run/${encodeURIComponent(r.run_id)}">
        <div class="line"><strong class="grow ellip">${esc(recipeTitle(r.recipe_id))}</strong>${stateChip(r.state)}</div>
        <div class="meta">${esc(step?.instructions ?? `Step “${r.current_step_id}” is waiting for input`)}</div></a>`;
    }),
  ];

  return `<section>${scanner}</section>
    ${needs.length ? `<h2>Needs you</h2><section>${needs.join("")}</section>` : ""}
    <h2>Now</h2><section>${active.length ? active.map((j) => jobCard(j)).join("") : `<p class="empty">Nothing running.</p>`}
      ${canAct() ? `<button class="btn primary wide" data-act="sheet-scan">New full-dome scan</button>` : ""}</section>
    ${recent.length ? `<h2>Recent</h2><section>${recent.map((j) => jobCard(j, { actions: false })).join("")}</section>` : ""}`;
}

const CHECKS = { point_count: "Point count", levelled: "Levelled", coordinates: "Coordinates", intensity: "Intensity" };

const recipeTitle = (id) => S.recipes.find((r) => r.recipe_id === id)?.title ?? id;

function viewRuns() {
  const recipes = S.recipes.length
    ? S.recipes.map((r) => `<button class="card link" style="width:100%;text-align:left" data-act="sheet-run" data-id="${esc(r.recipe_id)}" ${canAct() ? "" : "disabled"}>
        <div class="line"><strong>${esc(r.title)}</strong><span class="meta">v${esc(r.version)}</span></div>
        <div class="meta">${esc(r.summary ?? "")}</div></button>`).join("")
    : `<p class="empty">No recipes installed on this bridge.</p>`;
  const runs = S.runs.length
    ? S.runs.map((r) => `<a class="card link ${r.state === "waiting_for_person" ? "attn" : ""}" href="#/run/${encodeURIComponent(r.run_id)}">
        <div class="line"><strong class="grow ellip">${esc(recipeTitle(r.recipe_id))}</strong>${stateChip(r.state)}</div>
        <div class="meta">by ${esc(r.started_by)} · ${esc(ago(r.created_at))}${r.current_step_id && !["succeeded"].includes(r.state) ? ` · step “${esc(r.current_step_id)}”` : ""}</div></a>`).join("")
    : `<p class="empty">No runs yet.</p>`;
  return `<h2>Start a recipe</h2><section>${recipes}</section><h2>Runs</h2><section>${runs}</section>`;
}

function stepDot(state, i) {
  const map = { done: ["done", "✓"], running: ["now", "…"], waiting: ["wait", "!"], failed: ["failed", "×"], skipped: ["", "–"] };
  const [cls, ch] = map[state] ?? ["", String(i + 1)];
  return `<span class="dot ${cls}">${ch}</span>`;
}

function stepOutput(rs, def) {
  const o = rs.output;
  if (!o) return "";
  if (o.export_id) {
    const ex = S.exports[o.export_id];
    if (!ex) loadExport(o.export_id);
    return ex && !ex.error ? `${num(ex.point_count)} points · ${esc(ex.format?.toUpperCase())} · <a href="#" data-act="dl-export" data-id="${esc(o.export_id)}">Download</a>` : "Export ready";
  }
  if (o.scan_id) return `<a href="#/scan/${encodeURIComponent(o.scan_id)}">Open scan</a>`;
  if (o.window) return `Window ${deg(o.window.delta_ha)} × ${deg(o.window.delta_va)}`;
  if (def?.kind === "check") return o.ok ? `Checks passed: ${esc((o.checked || []).join(", "))}` : "";
  if (def?.kind === "person") {
    const parts = Object.keys(o).filter((k) => k !== "box").map((k) => `${k}: ${typeof o[k] === "object" ? JSON.stringify(o[k]) : o[k]}`);
    if (o.box) parts.unshift(o.box.x1 >= o.box.x0 ? `box ${o.box.x1 - o.box.x0} × ${o.box.y1 - o.box.y0} px` : `box across the panorama's seam, ${o.box.y1 - o.box.y0} px high`);
    return esc(parts.join(" · ") || "Done");
  }
  return "";
}

function viewRun(id) {
  const r = S.runs.find((x) => x.run_id === id);
  if (!r) return `<p class="empty">This run isn't on the bridge (runs are kept until it restarts).</p>`;
  const rec = S.recipe[r.recipe_id];
  if (!rec) recipe(r.recipe_id);
  const defs = typeof rec === "object" && rec.steps ? rec.steps : [];
  const def = (sid) => defs.find((s) => (s.id ?? s.step_id) === sid);
  const final = ["succeeded", "failed", "cancelled"].includes(r.state);

  let panel = "";
  const cur = r.steps.find((s) => s.step_id === r.current_step_id);
  if (r.state === "waiting_for_person" && r.waiting_for === "input" && cur) {
    const d = def(cur.step_id) || {};
    const needs = d.needs || [];
    const key = `run:${r.run_id}:${cur.step_id}`;
    let previewScan = null;
    if (d.preview_of) previewScan = r.steps.find((s) => s.step_id === d.preview_of)?.output?.scan_id ?? null;
    const fields = needs.filter((n) => n !== "box").map((n) => `<label class="field"><span>${esc(n)}</span><input type="text" name="${esc(n)}" inputmode="decimal" autocomplete="off"></label>`).join("");
    panel = `<div class="card attn"><h3>${esc(d.title ?? cur.step_id)}</h3>
      <p>${esc(d.instructions ?? "This step needs your input.")}</p>
      ${needs.includes("box") ? (previewScan ? picker(key, previewScan) : `<div class="problem">No preview to draw on.</div>`) : ""}
      ${mayAdvance(r) ? `<form data-form="advance" data-run="${esc(r.run_id)}" data-step="${esc(cur.step_id)}" data-key="${esc(key)}">${fields}
        <button class="btn primary wide" ${S.busy || (needs.includes("box") && !S.boxes[key]) ? "disabled" : ""}>Continue</button></form>`
        : `<p class="hint">Only ${esc(r.started_by)} or an operator can continue this run.</p>`}</div>`;
  } else if (r.state === "waiting_for_person" && r.waiting_for === "approval" && cur?.job_id) {
    const j = S.jobs.find((x) => x.job_id === cur.job_id);
    panel = j ? jobCard(j) : "";
  } else if (cur?.job_id && !final) {
    const j = S.jobs.find((x) => x.job_id === cur.job_id);
    panel = j ? jobCard(j) : "";
  }

  const steps = r.steps.map((s, i) => {
    const d = def(s.step_id);
    return `<li>${stepDot(s.state, i)}<div class="grow"><strong>${esc(d?.title ?? s.step_id)}</strong>
      <div class="meta">${esc(d?.kind ?? "")}${s.state !== "pending" ? ` · ${esc(s.state)}` : ""}</div>
      <div class="meta">${stepOutput(s, d)}</div></div></li>`;
  }).join("");

  return `<section><div class="card">
      <div class="line"><h3 class="grow">${esc(recipeTitle(r.recipe_id))}</h3>${stateChip(r.state)}</div>
      <div class="meta">Started by ${esc(r.started_by)} · ${esc(ago(r.created_at))}</div>
      ${r.problem ? `<div class="problem">${esc(problemText(r.problem))}</div>` : ""}
    </div></section>
    ${panel ? `<section>${panel}</section>` : ""}
    <h2>Steps</h2><section class="card"><ul class="steps">${steps}</ul></section>
    ${!final && mayAdvance(r) ? `<button class="btn danger wide" data-act="cancel-run" data-id="${esc(r.run_id)}" ${S.busy ? "disabled" : ""}>Cancel run</button>` : ""}`;
}

function picker(key, scanId) {
  const p = preview(scanId);
  const draw = S.drawMode[key] ?? true;
  const b = S.boxes[key];
  const dims = S.dims[scanId];
  let img;
  if (p === "loading") img = `<div class="placeholder">Loading preview…</div>`;
  else if (p?.error) img = `<div class="problem">${esc(p.error)}</div>`;
  else {
    const box = b && dims ? `<div class="boxsel" style="left:${(b.x0 / (2 * dims[0])) * 100}%;top:${(b.y0 / dims[1]) * 100}%;width:${(span(b, dims[0]) / (2 * dims[0])) * 100}%;height:${((b.y1 - b.y0) / dims[1]) * 100}%"></div>` : "";
    img = `<div class="pano" data-keep-scroll="${esc(key)}"><div class="pano-in ${draw ? "draw" : ""}" data-picker="${esc(key)}" data-scan="${esc(scanId)}"><img src="${esc(p)}" alt="Scan preview" data-dims="${esc(scanId)}"><img src="${esc(p)}" alt="" aria-hidden="true" class="seam">${box}</div></div>`;
  }
  return `<div class="pano-tools"><span class="seg"><button type="button" class="${draw ? "on" : ""}" data-act="draw" data-key="${esc(key)}" data-on="1">Draw box</button><button type="button" class="${draw ? "" : "on"}" data-act="draw" data-key="${esc(key)}" data-on="0">Pan</button></span>
    ${b ? `<button type="button" class="btn small" data-act="clear-box" data-key="${esc(key)}">Clear</button>` : ""}</div>
    ${img}
    <p class="hint">${b && dims ? `Box ${span(b, dims[0])} × ${b.y1 - b.y0} px${b.x1 < b.x0 ? ", across the panorama's seam" : ""}.` : draw ? "Drag on the picture to draw a box around what to scan. The panorama repeats after its end, so a box can cross the seam. Switch to Pan to scroll." : "Swipe to scroll the panorama, then switch to Draw box."}</p>`;
}

function viewScans() {
  const opts = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
  const head = `<div class="card">
    <label class="field" style="margin-top:0"><span>Project</span>
      <select data-change="project"><option value="">Choose a project…</option>${opts}</select></label>
    ${canAct() ? `<div class="btns"><button class="btn" data-act="sheet-project">New project</button>
      <button class="btn primary" data-act="sheet-scan" ${S.project ? "" : "disabled"}>New scan</button></div>` : ""}</div>`;
  if (!S.project) return head;
  const stations = new Map();
  for (const s of S.scans) {
    const k = s.station ?? 0;
    if (!stations.has(k)) stations.set(k, { dome: null, areas: [] });
    if (s.kind === "area") stations.get(k).areas.push(s); else stations.get(k).dome = s;
  }
  const keys = [...stations.keys()].sort((a, b) => b - a);
  const row = (s, label) => `<a class="card link" href="#/scan/${encodeURIComponent(s.scan_id)}">
      <div class="line"><strong>${esc(label)}</strong><span class="meta">${esc(s.mode ?? "")}</span></div>
      <div class="meta">${num(s.point_count)} points${s.created_at ? ` · ${esc(ago(s.created_at))}` : ""}</div></a>`;
  const list = keys.length ? keys.map((k) => {
    const st = stations.get(k);
    return `<div class="station">${st.dome ? row(st.dome, `Station ${k} · full dome`) : `<div class="meta">Station ${k}</div>`}
      ${st.areas.sort((a, b) => String(a.created_at).localeCompare(String(b.created_at))).map((a, i) => `<div class="subscan">${row(a, `Area scan ${i + 1}`)}</div>`).join("")}</div>`;
  }).join("") : `<p class="empty">No scans in this project yet.</p>`;
  return head + `<h2>Scans</h2><section>${list}</section>`;
}

function viewScan(id) {
  const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
  if (!s) {
    if (!viewScan.tried?.has(id)) {
      viewScan.tried = viewScan.tried ?? new Set();
      viewScan.tried.add(id);
      api(`/scans/${encodeURIComponent(id)}`)
        .then((v) => { S.scanById[v.scan_id] = v; render(); })
        .catch((e) => { viewScan.tried.delete(id); toast(e.message, true); });
    }
    return `<p class="empty">Loading scan…</p>`;
  }
  const key = `aim:${id}`;
  const aim = S.aim[id];
  const partialOf = S.jobs.find((j) => j.partial_scan_id === id);
  const exportJobId = S.watchExport[id];
  const exportJob = exportJobId ? S.jobs.find((j) => j.job_id === exportJobId) : null;
  const exportId = exportJob?.result?.export_id;
  const ex = exportId ? S.exports[exportId] : null;
  const modeOpts = S.modes.map((m) => `<option value="${esc(m.mode)}" ${m.mode === (store.get("x9.mode") || "STD_L0") ? "selected" : ""}>${esc(m.mode)}</option>`).join("");

  const title = s.kind === "area" ? `Station ${s.station} · area scan` : `Station ${s.station} · full dome`;
  return `<section><div class="card">
      <h3>${esc(title)}</h3>
      <div class="meta">${esc(s.mode ?? "")} · ${num(s.point_count)} points${s.created_at ? ` · ${esc(clock(s.created_at))}` : ""}${s.window ? ` · window ${deg(s.window.delta_ha)} × ${deg(s.window.delta_va)}` : ""}</div>
      ${partialOf ? `<div class="note">This is the partial sweep of a cancelled scan.</div>` : ""}
    </div></section>
    <h2>Aim an area scan</h2>
    <section class="card">
      ${picker(key, id)}
      ${canAct() ? `<div class="btns"><button class="btn" data-act="aim" data-id="${esc(id)}" ${S.boxes[key] && !S.busy ? "" : "disabled"}>Work out the window</button></div>` : ""}
      ${aim ? `<div class="facts"><div class="fact"><b>Across</b><span>${deg(aim.window.delta_ha)}</span></div><div class="fact"><b>Up and down</b><span>${deg(aim.window.delta_va)}</span></div></div>
        <label class="field"><span>Scan mode</span><select name="mode" data-change="mode">${modeOpts}</select></label>
        <button class="btn primary wide" data-act="scan-area" data-id="${esc(id)}" ${S.busy ? "disabled" : ""}>Scan this area</button>` : ""}
    </section>
    <h2>Points</h2>
    <section class="card">
      ${exportJob && exportJob.state !== "succeeded" ? jobCard(exportJob) : ""}
      ${ex && !ex.error ? `<div class="line"><strong>${esc(ex.format?.toUpperCase())} · ${num(ex.point_count)} points</strong></div>
        <ul class="steps">${(ex.checks || []).map((c) => `<li><span class="dot ${c.ok ? "done" : "failed"}">${c.ok ? "✓" : "×"}</span><div class="grow"><strong>${esc(CHECKS[c.name] ?? c.name)}</strong><div class="meta">${esc(c.detail ?? "")}</div></div></li>`).join("")}</ul>
        <button class="btn primary wide" data-act="dl-export" data-id="${esc(exportId)}">Download ${esc(ex.format?.toUpperCase())}</button>` : ""}
      ${canAct() ? `<div class="btns"><button class="btn" data-act="export" data-id="${esc(id)}" data-format="ply" ${S.busy ? "disabled" : ""}>Export PLY</button>
        <button class="btn" data-act="export" data-id="${esc(id)}" data-format="las" ${S.busy ? "disabled" : ""}>Export LAS</button></div>` : ""}
      <div class="btns"><button class="btn" data-act="dl-tzf" data-id="${esc(id)}">Download raw .tzf</button></div>
    </section>
    ${isOp() ? `<h2>Delete</h2><section><button class="btn danger wide" data-act="delete-scan" data-id="${esc(id)}" ${S.busy ? "disabled" : ""}>Delete this scan from the scanner</button></section>` : ""}`;
}

function viewLog() {
  const form = canAct() ? `<form class="card" data-form="note">
      <label class="field" style="margin-top:0"><span>Add a note</span><textarea name="text" maxlength="2000" placeholder="What you see, what you changed, anything worth remembering"></textarea></label>
      <button class="btn primary wide" ${S.busy ? "disabled" : ""}>Add to the log</button></form>` : "";
  const entries = S.log.length ? S.log.map((e) => `<div class="entry"><div class="meta"><span class="chip">${esc(e.kind)}</span><span>${esc(clock(e.at))}</span><span class="ellip">${esc(e.author)}</span></div><div>${esc(e.text)}</div></div>`).join("") : `<p class="empty">Nothing logged yet.</p>`;
  return `${form}<h2>Field log</h2><section class="card">${entries}</section>`;
}

function viewSheet() {
  const sh = S.sheet;
  if (!sh) return "";
  let body = "";
  if (sh.type === "scan") {
    const proj = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
    const modes = S.modes.map((m) => `<option value="${esc(m.mode)}" ${m.mode === (store.get("x9.mode") || "STD_L0") ? "selected" : ""}>${esc(m.mode)} · about ${Math.round((m.full_dome_seconds ?? 0) / 60)} min</option>`).join("");
    body = `<h3>New full-dome scan</h3>
      <form data-form="scan">
        <label class="field"><span>Project</span><select name="project_id" required><option value="">Choose…</option>${proj}</select></label>
        <label class="field"><span>Mode</span><select name="mode" required>${modes}</select></label>
        <label class="check"><input type="checkbox" name="color"> Colour pictures (adds a few minutes)</label>
        <label class="field"><span>Note (optional)</span><input type="text" name="note" maxlength="200"></label>
        ${isOp() ? "" : `<p class="hint">Your scans wait for an operator's approval.</p>`}
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy ? "disabled" : ""}>Start scan</button></div>
      </form>`;
  } else if (sh.type === "run") {
    const rec = S.recipe[sh.id];
    if (!rec) recipe(sh.id);
    const proj = S.projects.map((p) => `<option value="${esc(p.project_id)}" ${p.project_id === S.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
    const inputs = typeof rec === "object" && rec.inputs ? rec.inputs.map((i) => i.choices
      ? `<label class="field"><span>${esc(i.prompt ?? i.name)}</span><select name="in:${esc(i.name)}" required><option value="">Choose…</option>${i.choices.map((c) => `<option>${esc(c)}</option>`).join("")}</select></label>`
      : `<label class="field"><span>${esc(i.prompt ?? i.name)}</span><input type="text" name="in:${esc(i.name)}" ${i.type === "number" ? 'inputmode="decimal"' : ""} required></label>`).join("") : "";
    body = `<h3>${esc(typeof rec === "object" ? rec.title ?? sh.id : sh.id)}</h3>
      <p class="meta">${esc(typeof rec === "object" ? rec.summary ?? "" : "Loading…")}</p>
      <form data-form="run" data-id="${esc(sh.id)}">
        <label class="field"><span>Project</span><select name="project_id" required><option value="">Choose…</option>${proj}</select></label>
        ${inputs}
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy || typeof rec !== "object" ? "disabled" : ""}>Start</button></div>
      </form>`;
  } else if (sh.type === "project") {
    body = `<h3>New project</h3>
      <form data-form="project">
        <label class="field"><span>Name</span><input type="text" name="name" maxlength="64" required placeholder="e.g. 2026-10-21 kitchen window"></label>
        <div class="btns"><button type="button" class="btn" data-act="close">Cancel</button><button class="btn primary" ${S.busy ? "disabled" : ""}>Create</button></div>
      </form>`;
  } else if (sh.type === "me") {
    body = `<h3>${esc(S.me?.name ?? "")}</h3>
      <p class="meta">Signed in as ${esc(S.me?.role ?? "")}. ${isOp() ? "You can approve scans and delete them." : S.me?.role === "agent" ? "Your scans wait for an operator's approval." : "You can look but not change anything."}</p>
      <div class="btns"><button class="btn" data-act="close">Close</button><button class="btn danger" data-act="signout">Sign out</button></div>`;
  }
  return `<div class="scrim" data-act="scrim"><div class="sheet" role="dialog" aria-modal="true">${body}</div></div>`;
}

function viewMain() {
  const r = S.route;
  if (r.run) return viewRun(r.run);
  if (r.scan) return viewScan(r.scan);
  return { now: viewNow, runs: viewRuns, scans: viewScans, log: viewLog }[r.tab]();
}

// Re-rendering replaces the page, so hold it while someone is typing or
// drawing, and keep panorama scroll positions.
let pending = false;
let drawing = null;
function render(force = false) {
  const root = document.getElementById("app");
  const active = document.activeElement;
  const typing = active && root.contains(active) && /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName) && active.type !== "checkbox";
  if (!force && (typing || drawing)) { pending = true; return; }
  if (!force) {
    if (render.queued) return;
    render.queued = true;
    requestAnimationFrame(() => { render.queued = false; render(true); });
    return;
  }
  pending = false;
  // Form fields keep what was typed or picked, and focus, across redraws.
  const fieldKey = (el) => { const f = el.closest("form[data-form]"); return f && el.name ? [f.dataset.form, f.dataset.id, f.dataset.run, f.dataset.step, el.name].join("|") : null; };
  const fields = {};
  root.querySelectorAll("form[data-form] [name]").forEach((el) => { const k = fieldKey(el); if (k) fields[k] = el.type === "checkbox" ? el.checked : el.value; });
  const focusKey = document.activeElement && root.contains(document.activeElement) ? fieldKey(document.activeElement) : null;
  const scroll = {};
  root.querySelectorAll("[data-keep-scroll]").forEach((el) => { scroll[el.dataset.keepScroll] = el.scrollLeft; });
  if (!S.token || !S.me) {
    if (!root.querySelector("[data-form=signin]") || S.signinError !== render.lastErr || S.busy !== render.lastBusy) root.innerHTML = viewSignin();
    render.lastErr = S.signinError;
    render.lastBusy = S.busy;
    return;
  }
  root.innerHTML = viewTop() + `<main>${viewMain()}</main>` + viewNav() + viewSheet() +
    (S.toast ? `<div class="toast ${S.toast.bad ? "bad" : ""}" role="status">${esc(S.toast.text)}</div>` : "");
  root.querySelectorAll("[data-keep-scroll]").forEach((el) => { if (scroll[el.dataset.keepScroll] != null) el.scrollLeft = scroll[el.dataset.keepScroll]; });
  root.querySelectorAll("form[data-form] [name]").forEach((el) => {
    const k = fieldKey(el);
    if (k == null || !(k in fields)) return;
    if (el.type === "checkbox") el.checked = fields[k]; else el.value = fields[k];
    if (k === focusKey) el.focus({ preventScroll: true });
  });
  root.querySelectorAll("img[data-dims]").forEach((img) => {
    const set = () => {
      const id = img.dataset.dims;
      const d = S.dims[id];
      if (!d || d[0] !== img.naturalWidth) { S.dims[id] = [img.naturalWidth, img.naturalHeight]; render(); }
    };
    if (img.complete && img.naturalWidth) set(); else img.addEventListener("load", set, { once: true });
  });
}
document.addEventListener("focusout", () => setTimeout(() => { if (pending) render(); }, 0));

// ---------------------------------------------------------------- drawing ---
function pointIn(el, e, dims) {
  // The picker shows the panorama twice: x runs over 0 … 2 × width.
  const r = el.getBoundingClientRect();
  const x = Math.round(((e.clientX - r.left) / r.width) * 2 * dims[0]);
  const y = Math.round(((e.clientY - r.top) / r.height) * dims[1]);
  return [Math.max(0, Math.min(2 * dims[0], x)), Math.max(0, Math.min(dims[1], y))];
}
document.addEventListener("pointerdown", (e) => {
  const el = e.target.closest("[data-picker]");
  if (!el || !el.classList.contains("draw")) return;
  const dims = S.dims[el.dataset.scan];
  if (!dims) return;
  e.preventDefault();
  el.setPointerCapture(e.pointerId);
  const start = pointIn(el, e, dims);
  let box = el.querySelector(".boxsel");
  if (!box) { box = document.createElement("div"); box.className = "boxsel"; el.append(box); }
  drawing = { el, key: el.dataset.picker, dims, start, end: start, box };
});
document.addEventListener("pointermove", (e) => {
  if (!drawing) return;
  drawing.end = pointIn(drawing.el, e, drawing.dims);
  const [w, h] = drawing.dims;
  const [a, b] = [drawing.start, drawing.end];
  Object.assign(drawing.box.style, {
    left: `${(Math.min(a[0], b[0]) / (2 * w)) * 100}%`, top: `${(Math.min(a[1], b[1]) / h) * 100}%`,
    width: `${(Math.min(Math.abs(a[0] - b[0]), w - 1) / (2 * w)) * 100}%`, height: `${(Math.abs(a[1] - b[1]) / h) * 100}%`,
  });
});
function endDraw() {
  if (!drawing) return;
  const { key, start: a, end: b, dims } = drawing;
  drawing = null;
  if (Math.abs(a[0] - b[0]) >= 4 && Math.abs(a[1] - b[1]) >= 4) {
    // Back to preview pixels: x0 in [0, width); a box past the end wraps (x1 < x0).
    const W = dims[0];
    let lo = Math.min(a[0], b[0]);
    let hi = Math.min(Math.max(a[0], b[0]), lo + W - 1);
    if (lo >= W) { lo -= W; hi -= W; }
    const x1 = hi > W ? hi - W : hi;
    S.boxes[key] = { x0: lo, y0: Math.min(a[1], b[1]), x1, y1: Math.max(a[1], b[1]) };
    const scanId = key.startsWith("aim:") ? key.slice(4) : null;
    if (scanId) delete S.aim[scanId];
  }
  render(true);
}
document.addEventListener("pointerup", endDraw);
document.addEventListener("pointercancel", endDraw);

// ---------------------------------------------------------------- actions ---
const actions = {
  back: () => history.length > 1 ? history.back() : (location.hash = `#/${S.route.tab}`),
  me: () => { S.sheet = { type: "me" }; render(true); },
  close: () => { S.sheet = null; render(true); },
  scrim: (el, e) => { if (e.target === el) { S.sheet = null; render(true); } },
  signout: () => { S.sheet = null; signOut(); },
  "sheet-scan": () => { S.sheet = { type: "scan" }; load("projects"); render(true); },
  "sheet-run": (el) => { S.sheet = { type: "run", id: el.dataset.id }; load("projects"); render(true); },
  "sheet-project": () => { S.sheet = { type: "project" }; render(true); },
  approve: (el) => act(() => api(`/jobs/${encodeURIComponent(el.dataset.id)}/approve`, { method: "POST" }).then((j) => upsert(S.jobs, j, "job_id")), "Approved"),
  "cancel-job": (el) => {
    const j = S.jobs.find((x) => x.job_id === el.dataset.id);
    const running = j?.state === "running";
    if (running && !confirm("Stop this scan? The scanner keeps the part it has swept.")) return;
    return act(() => api(`/jobs/${encodeURIComponent(el.dataset.id)}/cancel`, { method: "POST" }).then((j2) => upsert(S.jobs, j2, "job_id")), running ? "Stopping" : "Denied");
  },
  "cancel-run": (el) => {
    if (!confirm("Cancel this run? A scan in progress is stopped too.")) return;
    return act(() => api(`/runs/${encodeURIComponent(el.dataset.id)}/cancel`, { method: "POST" }).then((r) => upsert(S.runs, r, "run_id")), "Cancelling");
  },
  draw: (el) => { S.drawMode[el.dataset.key] = el.dataset.on === "1"; render(true); },
  "clear-box": (el) => { delete S.boxes[el.dataset.key]; render(true); },
  aim: (el) => {
    const id = el.dataset.id;
    const box = S.boxes[`aim:${id}`];
    return act(async () => { S.aim[id] = await api("/aim", { method: "POST", body: { reference_scan_id: id, box } }); });
  },
  "scan-area": (el) => {
    const id = el.dataset.id;
    const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
    const mode = document.querySelector("select[data-change=mode]")?.value || store.get("x9.mode") || "STD_L0";
    return act(async () => {
      const j = await api("/scans", { method: "POST", body: { project_id: s.project_id, kind: "area", mode, window: S.aim[id].window, note: `area from station ${s.station}` } });
      upsert(S.jobs, j, "job_id");
      location.hash = "#/now";
    }, isOp() ? "Area scan queued" : "Area scan sent for approval");
  },
  export: (el) => act(async () => {
    const j = await api(`/scans/${encodeURIComponent(el.dataset.id)}/exports`, { method: "POST", body: { format: el.dataset.format, intensity: "auto" } });
    upsert(S.jobs, j, "job_id");
    S.watchExport[el.dataset.id] = j.job_id;
  }),
  "dl-export": (el, e) => { e.preventDefault(); return act(() => download(`/exports/${encodeURIComponent(el.dataset.id)}/file`, `${el.dataset.id}`)); },
  "dl-tzf": (el) => act(() => download(`/scans/${encodeURIComponent(el.dataset.id)}/tzf`, `${el.dataset.id}.tzf`)),
  "delete-scan": (el) => {
    const id = el.dataset.id;
    const s = S.scans.find((x) => x.scan_id === id) ?? S.scanById[id];
    if (!confirm(`Delete station ${s?.station ?? ""} ${s?.kind === "area" ? "area scan" : "full dome"} from the scanner? This can't be undone.`)) return;
    return act(async () => {
      await api(`/scans/${encodeURIComponent(id)}`, { method: "DELETE", headers: { "X-Confirm": id } });
      S.scans = S.scans.filter((x) => x.scan_id !== id);
      location.hash = "#/scans";
    }, "Scan deleted");
  },
};

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-act]");
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.act];
  if (fn) fn(el, e);
});

document.addEventListener("change", (e) => {
  const el = e.target;
  if (el.dataset.change === "project") {
    S.project = el.value || null;
    store.set("x9.project", S.project);
    S.scans = [];
    load("scans");
  } else if (el.dataset.change === "mode") {
    store.set("x9.mode", el.value);
  }
});

const forms = {
  signin: async (f) => { S.busy = true; render(true); await signIn(f.token.value); S.busy = false; render(true); },
  scan: (f) => {
    store.set("x9.mode", f.mode.value);
    return act(async () => {
      const body = { project_id: f.project_id.value, kind: "full_dome", mode: f.mode.value, color: f.color.checked };
      if (f.note.value.trim()) body.note = f.note.value.trim();
      const j = await api("/scans", { method: "POST", body });
      upsert(S.jobs, j, "job_id");
      S.sheet = null;
      location.hash = "#/now";
    }, isOp() ? "Scan queued" : "Scan sent for approval");
  },
  run: (f) => act(async () => {
    const inputs = {};
    for (const el of f.elements) {
      if (!el.name?.startsWith("in:")) continue;
      const v = el.value.trim();
      inputs[el.name.slice(3)] = el.inputMode === "decimal" && v !== "" && !isNaN(Number(v)) ? Number(v) : v;
    }
    const r = await api("/runs", { method: "POST", body: { recipe_id: f.dataset.id, project_id: f.project_id.value, inputs } });
    upsert(S.runs, r, "run_id");
    S.sheet = null;
    location.hash = `#/run/${encodeURIComponent(r.run_id)}`;
  }, "Run started"),
  advance: (f) => act(async () => {
    const input = {};
    for (const el of f.elements) {
      if (!el.name) continue;
      const v = el.value.trim();
      input[el.name] = v !== "" && !isNaN(Number(v)) ? Number(v) : v;
    }
    const box = S.boxes[f.dataset.key];
    if (box) input.box = box;
    const r = await api(`/runs/${encodeURIComponent(f.dataset.run)}/advance`, { method: "POST", body: { step_id: f.dataset.step, input } });
    upsert(S.runs, r, "run_id");
  }, "Sent"),
  project: (f) => act(async () => {
    const p = await api("/projects", { method: "POST", body: { name: f.name.value.trim() } });
    await load("projects");
    S.project = p.project_id;
    store.set("x9.project", S.project);
    S.sheet = null;
    load("scans");
  }, "Project created"),
  note: (f) => act(async () => {
    const body = { text: f.text.value.trim() };
    if (S.project) body.project_id = S.project;
    const e = await api("/log", { method: "POST", body });
    if (e && !S.log.some((x) => x.entry_id === e.entry_id)) S.log.unshift(e);
    f.text.value = "";
  }, "Noted"),
};

document.addEventListener("submit", (e) => {
  const f = e.target.closest("[data-form]");
  if (!f) return;
  e.preventDefault();
  if (S.busy) return;
  forms[f.dataset.form]?.(f);
});

window.addEventListener("hashchange", () => {
  S.route = parseRoute();
  S.sheet = null;
  if (S.route.tab === "scans" && !S.route.scan) { load("projects"); load("scans"); }
  window.scrollTo(0, 0);
  render(true);
});
document.addEventListener("visibilitychange", () => { if (!document.hidden && S.me) load("status"); });

// ------------------------------------------------------------------- boot ---
async function boot() {
  // A token can arrive in the link (?token=…), e.g. from a QR code; it is then
  // kept on the phone and removed from the address bar.
  const q = new URLSearchParams(location.search);
  const t = q.get("token");
  if (t) {
    history.replaceState(null, "", location.pathname + location.hash);
    S.token = t;
  }
  S.route = parseRoute();
  if (S.token) {
    if (!(await signIn(S.token))) return;
  } else {
    render(true);
  }
  setInterval(() => { if (S.me && !document.hidden) load("status"); }, 5000);
  setInterval(() => { if (S.me && S.jobs.some((j) => j.state === "running")) render(); }, 1000);
}
boot();
