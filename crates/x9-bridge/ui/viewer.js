// Point viewer: draws a scan's thinned points (GET /v1/scans/{id}/view, the
// X9V1 format) with WebGL, coloured by intensity, height or coverage, with the
// coverage plan (shadows on the floor, and as a picture) from
// GET /v1/scans/{id}/coverage. No libraries: the bridge has no internet.
// It lives outside #app, so the app's re-rendering never touches the canvas.

const CLASS_RGB = [[26, 152, 80], [145, 207, 96], [254, 224, 139], [252, 141, 89]];
const CLASS_NAMES = ["Fine", "Good", "Coarse", "Sparse"];
const SHADOW_RGB = [215, 48, 39];
const MODES = ["intensity", "height", "coverage"];

const VS = `
attribute vec3 pos;
attribute float grey;
attribute float cls;
uniform mat4 mvp;
uniform int mode;
uniform float size;
uniform vec2 zr;
uniform float zcut;
uniform vec3 classes[4];
varying vec3 col;
vec3 ramp(float t) {
  t = clamp(t, 0.0, 1.0);
  return clamp(vec3(1.5 - abs(4.0 * t - 3.0), 1.5 - abs(4.0 * t - 2.0), 1.5 - abs(4.0 * t - 1.0)), 0.0, 1.0);
}
void main() {
  if (pos.z > zcut) { gl_Position = vec4(2.0, 2.0, 2.0, 1.0); return; }
  gl_Position = mvp * vec4(pos, 1.0);
  gl_PointSize = size;
  if (mode == 0) { float g = pow(grey / 255.0, 0.8); col = vec3(g); }
  else if (mode == 1) { col = ramp((pos.z - zr.x) / max(zr.y - zr.x, 0.01)); }
  else { col = cls < 0.5 ? classes[0] : cls < 1.5 ? classes[1] : cls < 2.5 ? classes[2] : classes[3]; }
}`;
const FS = `
precision mediump float;
varying vec3 col;
void main() { gl_FragColor = vec4(col, 1.0); }`;
const QVS = `
attribute vec2 uv;
uniform mat4 mvp;
uniform vec4 rect;
uniform float z;
varying vec2 v;
void main() { v = uv; gl_Position = mvp * vec4(rect.xy + uv * rect.zw, z, 1.0); }`;
const QFS = `
precision mediump float;
uniform sampler2D tex;
varying vec2 v;
void main() { gl_FragColor = texture2D(tex, vec2(v.x, 1.0 - v.y)); }`;

// ------------------------------------------------------------------ maths ---
function perspective(fovy, aspect, near, far) {
  const f = 1 / Math.tan(fovy / 2), nf = 1 / (near - far);
  return [f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) * nf, -1, 0, 0, 2 * far * near * nf, 0];
}
function lookAt(e, c, up) {
  let z = [e[0] - c[0], e[1] - c[1], e[2] - c[2]];
  let l = Math.hypot(...z); z = z.map((v) => v / l);
  let x = [up[1] * z[2] - up[2] * z[1], up[2] * z[0] - up[0] * z[2], up[0] * z[1] - up[1] * z[0]];
  l = Math.hypot(...x) || 1; x = x.map((v) => v / l);
  const y = [z[1] * x[2] - z[2] * x[1], z[2] * x[0] - z[0] * x[2], z[0] * x[1] - z[1] * x[0]];
  const d = (a) => -(a[0] * e[0] + a[1] * e[1] + a[2] * e[2]);
  return [x[0], y[0], z[0], 0, x[1], y[1], z[1], 0, x[2], y[2], z[2], 0, d(x), d(y), d(z), 1];
}
function mul(a, b) {
  const o = new Array(16).fill(0);
  for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) for (let k = 0; k < 4; k++) o[c * 4 + r] += a[k * 4 + r] * b[c * 4 + k];
  return o;
}
const pct = (sorted, q) => sorted[Math.min(sorted.length - 1, Math.max(0, Math.round((sorted.length - 1) * q)))];
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const millions = (n) => n >= 1e6 ? `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)} M` : n >= 1e3 ? `${Math.round(n / 1e3)} k` : String(n);

// ------------------------------------------------------------------ store ---
const pref = {
  get(k, d) { try { const v = localStorage.getItem("x9.view." + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("x9.view." + k, JSON.stringify(v)); } catch { /* private mode */ } },
};

// ----------------------------------------------------------------- viewer ---
export function createViewer(root, { fetchRaw, fetchJson, onClose }) {
  const V = {
    scanId: null, title: "", meta: "", n: 0, stats: null, coverage: null, planUrl: null,
    mode: pref.get("mode", "intensity"), size: pref.get("size", 2), cut: true, plan: pref.get("plan", false),
    follow: pref.get("follow", true), live: "",
    cam: { target: [0, 0, 0], yaw: -Math.PI / 4, pitch: 0.95, dist: 10 },
    home: null, gl: null, prog: null, qprog: null, bufs: null, floorTex: null, floor: null,
    dirty: true, loading: null, token: 0,
  };

  root.innerHTML = `
    <div class="vw-top">
      <button class="back" data-vw="close">‹ Back</button>
      <div class="vw-title"><strong data-vw-title></strong><span class="meta" data-vw-meta></span></div>
      <label class="vw-follow" title="Open each new scan in this project as soon as it is done"><input type="checkbox" data-vw-follow> Follow new scans</label>
    </div>
    <div class="vw-stage"><canvas></canvas>
      <div class="vw-status" data-vw-status hidden></div>
      <div class="vw-live" data-vw-live hidden></div>
      <div class="vw-legend" data-vw-legend hidden></div>
      <div class="vw-plan" data-vw-plan hidden></div>
    </div>
    <div class="vw-bar">
      <div class="seg" role="group" aria-label="Colour">${MODES.map((m) => `<button data-vw="mode" data-mode="${m}">${m[0].toUpperCase() + m.slice(1)}</button>`).join("")}</div>
      <button class="tog" data-vw="plan" title="The coverage plan: what the scanner saw from above, and its shadows">Plan</button>
      <button class="tog" data-vw="cut" title="Hide the ceiling to see into the room">No ceiling</button>
      <button data-vw="scanner" title="Look around from where the scanner stood">From scanner</button>
      <button data-vw="reset">Overview</button>
      <label class="vw-size" title="Point size">Size <input type="range" min="1" max="6" step="0.5" data-vw-size></label>
    </div>`;
  const canvas = root.querySelector("canvas");
  const $ = (sel) => root.querySelector(sel);

  // ---------------------------------------------------------------- GL ---
  function initGL() {
    const gl = canvas.getContext("webgl", { antialias: false, preserveDrawingBuffer: false });
    if (!gl) return null;
    const compile = (vs, fs) => {
      const p = gl.createProgram();
      for (const [type, src] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]]) {
        const s = gl.createShader(type);
        gl.shaderSource(s, src);
        gl.compileShader(s);
        if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
        gl.attachShader(p, s);
      }
      gl.linkProgram(p);
      if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
      return p;
    };
    V.prog = compile(VS, FS);
    V.qprog = compile(QVS, QFS);
    V.quad = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, V.quad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
    return gl;
  }

  function upload(buf) {
    const gl = V.gl;
    const dv = new DataView(buf);
    const magic = String.fromCharCode(...new Uint8Array(buf, 0, 4));
    if (magic !== "X9V1") throw new Error("not a point view from this bridge");
    const n = dv.getUint32(4, true);
    const stride = dv.getUint32(8, true);
    const pos = new Float32Array(buf, 16, n * 3);
    const grey = new Uint8Array(buf, 16 + 12 * n, n);
    const cls = new Uint8Array(buf, 16 + 13 * n, n);
    if (V.bufs) for (const b of Object.values(V.bufs)) gl.deleteBuffer(b);
    V.bufs = {};
    for (const [k, data] of [["pos", pos], ["grey", grey], ["cls", cls]]) {
      V.bufs[k] = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, V.bufs[k]);
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    }
    // Room shape from a sample: floor, ceiling, middle and size.
    const step = Math.max(1, Math.floor(n / 50000));
    const xs = [], ys = [], zs = [];
    for (let i = 0; i < n; i += step) { xs.push(pos[3 * i]); ys.push(pos[3 * i + 1]); zs.push(pos[3 * i + 2]); }
    for (const a of [xs, ys, zs]) a.sort((p, q) => p - q);
    const floor = pct(zs, 0.01), ceil = pct(zs, 0.99);
    const span = Math.max(pct(xs, 0.98) - pct(xs, 0.02), pct(ys, 0.98) - pct(ys, 0.02), ceil - floor, 1);
    V.stats = { n, stride, floor, ceil, centre: [pct(xs, 0.5), pct(ys, 0.5), floor + (ceil - floor) * 0.3], span };
    V.n = n;
  }

  function floorOverlay(cov, img) {
    const gl = V.gl;
    const c = document.createElement("canvas");
    c.width = img.naturalWidth;
    c.height = img.naturalHeight;
    const g = c.getContext("2d");
    g.drawImage(img, 0, 0);
    const d = g.getImageData(0, 0, c.width, c.height);
    for (let i = 0; i < d.data.length; i += 4) {
      const shadow = d.data[i] === SHADOW_RGB[0] && d.data[i + 1] === SHADOW_RGB[1] && d.data[i + 2] === SHADOW_RGB[2];
      d.data[i + 3] = shadow ? 190 : 0;
    }
    if (V.floorTex) gl.deleteTexture(V.floorTex);
    V.floorTex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, V.floorTex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, d);
    for (const p of [gl.TEXTURE_MIN_FILTER, gl.TEXTURE_MAG_FILTER]) gl.texParameteri(gl.TEXTURE_2D, p, gl.NEAREST);
    for (const p of [gl.TEXTURE_WRAP_S, gl.TEXTURE_WRAP_T]) gl.texParameteri(gl.TEXTURE_2D, p, gl.CLAMP_TO_EDGE);
    const p = cov.plan;
    V.floor = [p.origin_m[0], p.origin_m[1], p.width * p.cell_m, p.height * p.cell_m];
  }

  // ------------------------------------------------------------- camera ---
  function overview() {
    const s = V.stats;
    if (!s) return;
    V.cam = { target: s.centre.slice(), yaw: -Math.PI / 4, pitch: 0.95, dist: s.span * 1.1 };
    V.dirty = true;
  }
  function fromScanner() {
    V.cam = { target: [0, 0, 0], yaw: V.cam.yaw, pitch: 0.05, dist: 0.05 };
    V.dirty = true;
  }
  function eye() {
    const c = V.cam, cp = Math.cos(c.pitch);
    return [c.target[0] + c.dist * cp * Math.cos(c.yaw), c.target[1] + c.dist * cp * Math.sin(c.yaw), c.target[2] + c.dist * Math.sin(c.pitch)];
  }

  function draw() {
    const gl = V.gl;
    if (!gl) return;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.round(canvas.clientWidth * dpr), h = Math.round(canvas.clientHeight * dpr);
    if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
    gl.viewport(0, 0, w, h);
    gl.clearColor(0.043, 0.063, 0.125, 1);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    if (!V.bufs || !V.stats) return;
    const span = V.stats.span;
    const near = Math.max(0.01, Math.min(V.cam.dist * 0.05, 0.2));
    const mvp = mul(perspective(1.0, w / Math.max(h, 1), near, span * 20 + V.cam.dist * 4), lookAt(eye(), V.cam.target, [0, 0, 1]));
    gl.enable(gl.DEPTH_TEST);
    gl.useProgram(V.prog);
    const u = (name) => gl.getUniformLocation(V.prog, name);
    gl.uniformMatrix4fv(u("mvp"), false, mvp);
    gl.uniform1i(u("mode"), MODES.indexOf(V.mode));
    gl.uniform1f(u("size"), V.size * dpr);
    gl.uniform2f(u("zr"), V.stats.floor, V.stats.ceil);
    gl.uniform1f(u("zcut"), V.cut ? V.stats.ceil - 0.15 : 1e9);
    gl.uniform3fv(u("classes"), CLASS_RGB.flat().map((v) => v / 255));
    for (const [k, size, type] of [["pos", 3, gl.FLOAT], ["grey", 1, gl.UNSIGNED_BYTE], ["cls", 1, gl.UNSIGNED_BYTE]]) {
      const loc = gl.getAttribLocation(V.prog, k);
      gl.bindBuffer(gl.ARRAY_BUFFER, V.bufs[k]);
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, type, false, 0, 0);
    }
    gl.drawArrays(gl.POINTS, 0, V.n);
    for (const k of ["pos", "grey", "cls"]) gl.disableVertexAttribArray(gl.getAttribLocation(V.prog, k));
    // Shadows on the floor, in coverage colours.
    if (V.mode === "coverage" && V.floorTex && V.floor) {
      gl.useProgram(V.qprog);
      gl.enable(gl.BLEND);
      gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
      gl.depthMask(false);
      const q = (name) => gl.getUniformLocation(V.qprog, name);
      gl.uniformMatrix4fv(q("mvp"), false, mvp);
      gl.uniform4fv(q("rect"), V.floor);
      gl.uniform1f(q("z"), V.stats.floor + 0.02);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, V.floorTex);
      gl.uniform1i(q("tex"), 0);
      const loc = gl.getAttribLocation(V.qprog, "uv");
      gl.bindBuffer(gl.ARRAY_BUFFER, V.quad);
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      gl.disableVertexAttribArray(loc);
      gl.depthMask(true);
      gl.disable(gl.BLEND);
    }
  }

  function loop() {
    if (V.dirty && !root.hidden) { V.dirty = false; draw(); }
    requestAnimationFrame(loop);
  }

  // ------------------------------------------------------------ the page ---
  function chrome() {
    $("[data-vw-title]").textContent = V.title;
    $("[data-vw-meta]").textContent = V.meta;
    $("[data-vw-follow]").checked = V.follow;
    $("[data-vw-size]").value = V.size;
    root.querySelectorAll("[data-vw=mode]").forEach((b) => b.classList.toggle("on", b.dataset.mode === V.mode));
    root.querySelector("[data-vw=plan]").classList.toggle("on", V.plan);
    root.querySelector("[data-vw=cut]").classList.toggle("on", V.cut);
    const live = $("[data-vw-live]");
    live.hidden = !V.live;
    live.textContent = V.live;
    const cov = V.coverage;
    const legend = $("[data-vw-legend]");
    legend.hidden = V.mode !== "coverage" || !cov;
    if (cov) {
      legend.innerHTML = `<div class="vw-h">Point spacing</div>` + cov.classes.map((c, i) => `<div class="vw-key"><i style="background:rgb(${CLASS_RGB[i]})"></i>${CLASS_NAMES[i]}${c.max_mm ? ` ≤ ${c.max_mm} mm` : ""}<b>${Math.round(c.share * 100)}%</b></div>`).join("")
        + (cov.plan.shadows_assessed
          ? `<div class="vw-key"><i style="background:rgb(${SHADOW_RGB})"></i>Shadow on the floor<b>${cov.plan.shadow_m2} m²</b></div>`
          : `<div class="meta">Shadows are assessed on full domes only.</div>`);
    }
    const plan = $("[data-vw-plan]");
    plan.hidden = !V.plan || !cov;
    if (cov && V.planUrl) {
      const p = cov.plan;
      plan.innerHTML = `<div class="vw-h">Coverage from above</div>
        <img src="${V.planUrl}" alt="Coverage plan: what the scanner saw, from above" style="aspect-ratio:${p.width}/${p.height}">
        <div class="meta">${p.seen_m2} m² seen · ${p.shadows_assessed ? `${p.shadow_m2} m² in shadow (${Math.round(p.shadow_share * 100)}%)` : "shadows not assessed (not a full dome)"} · ${p.cell_m * 100} cm cells · + marks the scanner</div>`;
    }
  }

  function status(text) {
    const el = $("[data-vw-status]");
    el.hidden = !text;
    el.textContent = text || "";
  }

  async function load(scanId, scan) {
    const token = ++V.token;
    V.scanId = scanId;
    V.title = scan ? (scan.kind === "area" ? `Station ${scan.station} · area scan` : `Station ${scan.station} · full dome`) : "Scan";
    V.meta = "";
    V.coverage = null;
    V.cut = !scan || scan.kind !== "area";
    chrome();
    status("Preparing the points on the bridge…");
    try {
      const r = await fetchRaw(`/scans/${encodeURIComponent(scanId)}/view`);
      const total = Number(r.headers.get("content-length")) || 0;
      const reader = r.body.getReader();
      const parts = [];
      let got = 0;
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        parts.push(value);
        got += value.length;
        if (token !== V.token) return;
        status(total ? `Loading points… ${Math.round((got / total) * 100)}%` : `Loading points… ${(got / 1e6).toFixed(0)} MB`);
      }
      const buf = new Uint8Array(got);
      let o = 0;
      for (const p of parts) { buf.set(p, o); o += p.length; }
      if (token !== V.token) return;
      upload(buf.buffer);
      overview();
      V.meta = `${millions(V.n)} points shown${V.stats.stride > 1 ? ` (every ${V.stats.stride}${V.stats.stride === 2 ? "nd" : V.stats.stride === 3 ? "rd" : "th"} row and column)` : ""}`;
      status("");
      chrome();
    } catch (e) {
      if (token === V.token) status(`Could not load the points: ${e.message}`);
      return;
    }
    try {
      const cov = await fetchJson(`/scans/${encodeURIComponent(scanId)}/coverage`);
      if (token !== V.token) return;
      V.coverage = cov;
      V.meta += ` · of ${millions(cov.point_count)} · spacing ${cov.spacing_mm.p50} mm median, ${cov.spacing_mm.p90} mm at the 90th percentile`;
      const blob = await (await fetchRaw(`/scans/${encodeURIComponent(scanId)}/coverage/image`)).blob();
      if (token !== V.token) return;
      if (V.planUrl) URL.revokeObjectURL(V.planUrl);
      V.planUrl = URL.createObjectURL(blob);
      const img = new Image();
      img.onload = () => { if (token === V.token) { floorOverlay(cov, img); V.dirty = true; } };
      img.src = V.planUrl;
      chrome();
    } catch (e) {
      console.warn("coverage", e);
    }
    V.dirty = true;
  }

  // ------------------------------------------------------------ controls ---
  const ptrs = new Map();
  let last = null;
  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    ptrs.set(e.pointerId, { x: e.clientX, y: e.clientY, button: e.button, shift: e.shiftKey });
    last = null;
  });
  const endPtr = (e) => { ptrs.delete(e.pointerId); last = null; };
  canvas.addEventListener("pointerup", endPtr);
  canvas.addEventListener("pointercancel", endPtr);
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());
  // Drag the scene with the pointer: move the target against the camera's
  // right vector and along its up vector.
  function pan(dx, dy) {
    const c = V.cam, k = Math.max(c.dist, 0.5) * 0.0018;
    const [sy, cy, sp, cp] = [Math.sin(c.yaw), Math.cos(c.yaw), Math.sin(c.pitch), Math.cos(c.pitch)];
    const right = [-sy, cy, 0], up = [-sp * cy, -sp * sy, cp];
    for (let i = 0; i < 3; i++) c.target[i] += (-right[i] * dx + up[i] * dy) * k;
  }
  canvas.addEventListener("pointermove", (e) => {
    const p = ptrs.get(e.pointerId);
    if (!p) return;
    const dx = e.clientX - p.x, dy = e.clientY - p.y;
    p.x = e.clientX; p.y = e.clientY;
    if (ptrs.size === 2) {
      const [a, b] = [...ptrs.values()];
      const d = Math.hypot(a.x - b.x, a.y - b.y), mx = (a.x + b.x) / 2, my = (a.y + b.y) / 2;
      if (last) {
        V.cam.dist = Math.max(0.05, V.cam.dist * (last.d / Math.max(d, 1)));
        pan(mx - last.mx, my - last.my);
      }
      last = { d, mx, my };
    } else if (p.button === 2 || p.shift || e.shiftKey) {
      pan(dx, dy);
    } else {
      V.cam.yaw -= dx * 0.006;
      V.cam.pitch = Math.max(-1.5, Math.min(1.55, V.cam.pitch + dy * 0.006));
    }
    V.dirty = true;
  });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    V.cam.dist = Math.max(0.05, V.cam.dist * Math.exp(e.deltaY * 0.0012));
    V.dirty = true;
  }, { passive: false });

  root.addEventListener("click", (e) => {
    const b = e.target.closest("[data-vw]");
    if (!b) return;
    const act = b.dataset.vw;
    if (act === "close") onClose();
    else if (act === "mode") { V.mode = b.dataset.mode; pref.set("mode", V.mode); }
    else if (act === "plan") { V.plan = !V.plan; pref.set("plan", V.plan); }
    else if (act === "cut") V.cut = !V.cut;
    else if (act === "scanner") fromScanner();
    else if (act === "reset") overview();
    V.dirty = true;
    chrome();
  });
  root.addEventListener("input", (e) => {
    if (e.target.matches("[data-vw-size]")) { V.size = Number(e.target.value); pref.set("size", V.size); V.dirty = true; }
  });
  root.addEventListener("change", (e) => {
    if (e.target.matches("[data-vw-follow]")) { V.follow = e.target.checked; pref.set("follow", V.follow); }
  });
  new ResizeObserver(() => { V.dirty = true; }).observe(canvas);

  try {
    V.gl = initGL();
  } catch (e) {
    console.error(e);
  }
  if (!V.gl) status("This browser can't draw the points (no WebGL).");
  requestAnimationFrame(loop);

  return {
    /** Show a scan; `scan` is its record, for the title, when known. */
    open(scanId, scan) {
      root.hidden = false;
      document.body.classList.add("viewing");
      if (scanId !== V.scanId) load(scanId, scan);
      else if (scan && !V.title.startsWith("Station")) { V.title = scan.kind === "area" ? `Station ${scan.station} · area scan` : `Station ${scan.station} · full dome`; chrome(); }
      V.dirty = true;
    },
    close() {
      root.hidden = true;
      document.body.classList.remove("viewing");
    },
    get scanId() { return V.scanId; },
    get follow() { return V.follow && !root.hidden; },
    /** A line about a scan in progress, or "" when there is none. */
    setLive(text) { if (text !== V.live) { V.live = text; chrome(); } },
  };
}
