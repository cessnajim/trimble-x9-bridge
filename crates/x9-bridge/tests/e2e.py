#!/usr/bin/env python3
"""End-to-end check of x9-bridge against x9-mock (no hardware).

usage: e2e.py <target/release dir>
Starts the mock and the bridge on spare ports, walks the main flows, and
prints PASS/FAIL per check. Exit code 1 if anything failed.
"""
import json, os, re, subprocess, sys, tempfile, threading, time, uuid, urllib.request, urllib.error

BIN = sys.argv[1]
MOCK, BRIDGE = "127.0.0.1:34599", "127.0.0.1:8799"
# Optional real scans for file / aim / export checks: a directory with
# dome.tzf + dome.jpg (the 2026-10-19 full dome) and area.tzf + area.jpg.
FIX = os.environ.get("E2E_FIXTURES")
TOK = {"viewer": "viewer-token-0123456789", "agent": "agent-token-0123456789", "agent2": "agent2-token-0123456789",
       "operator": "operator-token-0123456789"}
ROLE = {"agent2": "agent"}  # a second agent, to check what one may do to another's work
results = []

def check(name, cond, detail=""):
    cond = bool(cond)
    results.append(cond)
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))

def call(method, path, who=None, body=None, headers=None, base=BRIDGE):
    req = urllib.request.Request(f"http://{base}/v1{path}", method=method,
                                 data=json.dumps(body).encode() if body is not None else None)
    req.add_header("Content-Type", "application/json")
    if who:
        req.add_header("Authorization", f"Bearer {TOK[who]}")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, (json.loads(raw) if raw else None)

def fetch(path, who):
    req = urllib.request.Request(f"http://{BRIDGE}/v1{path}")
    req.add_header("Authorization", f"Bearer {TOK[who]}")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type", ""), e.read()

def wait_run(run_id, pred, timeout=120):
    t0 = time.time()
    while time.time() - t0 < timeout:
        _, r = call("GET", f"/runs/{run_id}", "viewer")
        if pred(r) or r["state"] in ("failed", "cancelled", "succeeded"):
            return r
        time.sleep(0.2)
    return r

def wait_job(job_id, states, timeout=60):
    t0 = time.time()
    while time.time() - t0 < timeout:
        _, j = call("GET", f"/jobs/{job_id}", "viewer")
        if j["state"] in states:
            return j
        time.sleep(0.2)
    return j

def wait_up(base, path):
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://{base}{path}", timeout=1)
            return
        except Exception:
            time.sleep(0.1)
    raise SystemExit(f"{base} did not come up")

tmp = tempfile.mkdtemp()
recipes = os.path.join(tmp, "recipes")
os.makedirs(recipes)
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "recipes", "window-measurement.toml")) as f:
    open(os.path.join(recipes, "window-measurement.toml"), "w").write(f.read())
open(os.path.join(recipes, "log-note.toml"), "w").write('id = "log-note"\ntitle = "Note"\nversion = "1"\n'
    '[[inputs]]\nname = "text"\ntype = "string"\n'
    '[[steps]]\nid = "note"\nkind = "action"\noperation = "addLogNote"\narguments = { text = "{{input.text}}" }\n')
open(os.path.join(recipes, "broken.toml"), "w").write('id = "broken"\ntitle = "x"\nversion = "0"\n[[steps]]\nid = "a"\nkind = "action"\noperation = "rmRf"\n')
tokfile = os.path.join(tmp, "tokens")
with open(tokfile, "w") as f:
    f.write("# name role token\n")
    for who, t in TOK.items():
        f.write(f"{who}-1 {ROLE.get(who, who)} {t}\n")

procs = []
try:
    menv = {**os.environ, "X9_MOCK_ADDR": MOCK, "X9_MOCK_SPEED": "20", "X9_MOCK_FALLBACK_MODE": "HS_L1",
            "X9_MOCK_CANCEL_LINGER": "40"}  # a cancelled task stays listed 2 s
    if FIX:
        menv["X9_MOCK_FIXTURES"] = FIX
    procs.append(subprocess.Popen([f"{BIN}/x9-mock"], env=menv,
                                  stderr=subprocess.DEVNULL))
    wait_up(MOCK, "/api/v1/status")
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": f"http://{MOCK}/api/v1",
                                  "BRIDGE_ADDR": BRIDGE, "BRIDGE_TOKENS": tokfile, "BRIDGE_POLL_MS": "200",
                                  "BRIDGE_DATA": os.path.join(tmp, "data"), "BRIDGE_RECIPES": recipes},
                                  stderr=subprocess.DEVNULL))
    wait_up(BRIDGE, "/v1/health")

    # Event stream in the background.
    events = []
    def listen():
        req = urllib.request.Request(f"http://{BRIDGE}/v1/events")
        req.add_header("Authorization", f"Bearer {TOK['viewer']}")
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                for line in r:
                    line = line.decode().strip()
                    if line.startswith("event:"):
                        events.append(line.split(":", 1)[1].strip())
        except Exception:
            pass
    threading.Thread(target=listen, daemon=True).start()

    s, b = call("GET", "/health")
    check("health needs no token", s == 200 and b["ok"])
    s, b = call("GET", "/status")
    check("status without a token is 401 unauthorized", s == 401 and b["code"] == "unauthorized", b)
    s, b = call("GET", "/status", "viewer")
    check("status maps the scanner", s == 200 and b["scanner"]["connected"] and b["scanner"]["battery_pct"] == 87
          and b["scanner"]["leveling"]["state"] == "ok", b)
    s, b = call("GET", "/whoami", "agent")
    check("whoami names the caller and role", s == 200 and b["role"] == "agent", b)

    def page(path):
        try:
            with urllib.request.urlopen(f"http://{BRIDGE}{path}", timeout=10) as r:
                return r.status, r.headers.get("content-type", ""), r.read().decode("utf-8", "replace"), r.url
        except urllib.error.HTTPError as e:
            return e.code, "", "", path
    st, ct, body, url = page("/")
    check("/ leads to the web UI without a token", st == 200 and url.endswith("/ui/") and "app.js" in body, (st, url))
    st, ct, body, _ = page("/ui/app.js")
    check("web UI script is served", st == 200 and "javascript" in ct and "/whoami" in body, (st, ct))
    st, ct, body, _ = page("/ui/nope.js")
    check("unknown web UI file is 404", st == 404, st)

    s, b = call("GET", "/scan-modes", "viewer")
    check("scan modes are translated", s == 200 and any(m["mode"] == "STD_L2" and m["fixed_seconds"] == 42 for m in b), b)

    s, b = call("POST", "/projects", "agent", {"name": "e2e-window"})
    check("agent creates a project", s == 201 and b["name"] == "e2e-window", b)
    pid = b["project_id"]
    s, b = call("POST", "/projects", "agent", {"name": "e2e-window"})
    check("duplicate project name is a conflict", s == 409 and b["code"] == "conflict", b)

    dome = {"project_id": pid, "kind": "full_dome", "mode": "STD_L0", "note": "station 1"}
    s, b = call("POST", "/scans", "viewer", dome)
    check("viewer can't scan (403 forbidden)", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", "/scans", "viewer", {"bogus": True})
    check("viewer's malformed scan request is 403 forbidden, not a validation error", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", "/scans", "agent", {**dome, "kind": "area"})
    check("area scan without a window is 400 invalid_request", s == 400 and b["code"] == "invalid_request", b)
    s, b = call("POST", "/scans", "operator", {**dome, "mode": "STD_L9"})
    check("unknown scan mode is refused before it reaches the scanner", s == 400 and "STD_L0" in b.get("detail", ""), b)
    # Another client starts a scan directly on the scanner.
    other = urllib.request.Request(f"http://{MOCK}/api/v1/scans", method="POST", headers={"Content-Type": "application/json"},
                                   data=json.dumps({"scanMode": "STD_L0", "projectName": "mock_project",
                                                    "projectUUID": "39187455-8278-428c-89ad-8a4835f95be6"}).encode())
    urllib.request.urlopen(other).read()
    s, jb = call("POST", "/scans", "operator", dome)
    jb = wait_job(jb["job_id"], ("failed", "succeeded"))
    check("busy scanner (another client's task) fails the job with scanner_busy",
          jb["state"] == "failed" and jb["problem"]["code"] == "scanner_busy", jb)
    for _ in range(200):
        if json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read()) == []:
            break
        time.sleep(0.1)
    s, jf = call("POST", "/scans", "operator", {**dome, "mode": "HS_L1"})
    jf = wait_job(jf["job_id"], ("failed", "succeeded"))
    tasks = json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read())
    check("a request the scanner echoes differently is cancelled at once",
          jf["state"] == "failed" and "did not accept" in jf["problem"].get("detail", ""), jf)
    check("the cancelled task stays listed for a while, exitResult none (as on a real X9)",
          len(tasks) == 1 and tasks[0]["exitResult"] == "none", tasks)
    s, job = call("POST", "/scans", "agent", dome)
    check("agent scan waits for approval", s == 202 and job["state"] == "awaiting_approval", job)
    check("a waiting scan shows its request and project", job.get("request", {}).get("mode") == dome["mode"]
          and job.get("project_name") == "e2e-window", job)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "agent")
    check("agent can't approve (403)", s == 403, b)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "operator")
    check("operator approves", s == 200 and b["state"] == "queued", b)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "operator")
    check("approving twice is a conflict naming the state in API spelling", s == 409 and b["code"] == "conflict"
          and re.fullmatch(r"job is (queued|running|succeeded), not awaiting approval", b.get("detail", "")), b)
    seen_phases = set()
    t0 = time.time()
    while time.time() - t0 < 60:
        _, j = call("GET", f"/jobs/{job['job_id']}", "viewer")
        if j.get("phase"):
            seen_phases.add(j["phase"])
        if j["state"] in ("succeeded", "failed", "cancelled"):
            break
        time.sleep(0.1)
    check("full-dome job succeeds", j["state"] == "succeeded" and j["result"]["scan_id"], j)
    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("queued while the bridge's cancelled task lingered: waited instead of scanner_busy",
          any("clear the cancelled task" in e["text"] and e["refs"].get("job_id") == job["job_id"] for e in log), [e["text"] for e in log])
    check("phases reported: waiting (for the cancelled task), leveling, sweeping",
          {"waiting", "leveling", "sweeping"} <= seen_phases, seen_phases)
    station_scan = j["result"]["scan_id"]
    s, sc = call("GET", f"/scans/{station_scan}", "viewer")
    check("scan record: full dome, station 1, timings", s == 200 and sc["kind"] == "full_dome" and sc["station"] == 1
          and sc["project_id"] == pid and sc["timings"].get("geometry_ready_s"), sc)

    s, g = call("POST", "/grants", "agent", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z"})
    check("agent can't create grants (403)", s == 403, g)
    s, g = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z",
                                                "max_scans": 1, "allowed_modes": ["STD_L2"]})
    check("operator grants one STD_L2 scan", s == 201 and g["scans_used"] == 0, g)
    window = {"start_ha": 5.163, "delta_ha": 0.455, "start_va": 1.261, "delta_va": 0.44}
    area = {"project_id": pid, "kind": "area", "mode": "STD_L2", "window": window}
    s, job2 = call("POST", "/scans", "agent", area)
    check("granted area scan is queued without asking", s == 202 and job2["state"] == "queued", job2)
    j2 = wait_job(job2["job_id"], ("succeeded", "failed", "cancelled"))
    check("area job succeeds", j2["state"] == "succeeded", j2)
    s, sc2 = call("GET", f"/scans/{j2['result']['scan_id']}", "viewer")
    check("area scan record: kind area, parent = station scan, window kept", s == 200 and sc2["kind"] == "area"
          and sc2["parent_scan_id"] == station_scan and abs(sc2["window"]["start_ha"] - 5.163) < 1e-9, sc2)
    if FIX:
        s, ctype, body = fetch(f"/scans/{station_scan}/preview", "viewer")
        check("preview comes through the bridge", s == 200 and ctype == "image/jpeg" and body[:2] == b"\xff\xd8", (s, ctype))
        s, _, body = fetch(f"/scans/{sc2['scan_id']}/tzf", "viewer")
        check("scan file comes through the bridge", s == 200 and len(body) == os.path.getsize(os.path.join(FIX, "area.tzf")), (s, len(body)))
        s, b = call("POST", "/aim", "agent", {"reference_scan_id": station_scan, "box": {"x0": 523, "y0": 418, "x1": 674, "y1": 564}, "margin_rad": 0})
        want = {"start_ha": 5.157144, "delta_ha": 0.45571, "start_va": 1.261974, "delta_va": 0.440785}
        check("aim reproduces the 2026-10-19 window shot", s == 200 and all(abs(b["window"][k] - v) < 1e-4 for k, v in want.items()), b)
        s, b = call("POST", "/aim", "agent", {"reference_scan_id": station_scan, "box": {"x0": 10, "y0": 500, "x1": 20, "y1": 400}})
        check("aim rejects a box with y0 > y1", s == 400 and b["code"] == "invalid_request", b)
        s, b = call("POST", f"/scans/{sc2['scan_id']}/exports", "viewer", {"format": "ply"})
        check("viewer can't export (403)", s == 403, b)
        s, ej = call("POST", f"/scans/{sc2['scan_id']}/exports", "agent", {"format": "ply"})
        ej = wait_job(ej["job_id"], ("succeeded", "failed"), timeout=120)
        s, ex = call("GET", f"/exports/{ej['result']['export_id']}", "viewer") if ej["state"] == "succeeded" else (0, ej)
        check("area export: every point, all checks pass, preview intensity", s == 200 and ex["point_count"] == 777580
              and all(c["ok"] for c in ex["checks"]) and any("preview" in c.get("detail", "") for c in ex["checks"]), ex)
        s, _, body = fetch(f"/exports/{ex.get('export_id')}/file", "viewer")
        check("PLY file is complete", s == 200 and body.startswith(b"ply") and len(body) == body.index(b"end_header\n") + 11 + 15 * 777580, (s, len(body)))
        t_full = time.time()
        s, ej2 = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "las", "intensity": "none"})
        _, st_ = call("GET", "/status", "viewer")
        check("a running export is not the scanner's current job", st_["scanner"]["current_job_id"] is None, st_)
        ej2 = wait_job(ej2["job_id"], ("succeeded", "failed"), timeout=180)
        t_full = time.time() - t_full
        s, ex2 = call("GET", f"/exports/{ej2['result']['export_id']}", "viewer") if ej2["state"] == "succeeded" else (0, ej2)
        s3, _, body = fetch(f"/exports/{ex2.get('export_id')}/file", "viewer")
        check("full-dome LAS export: 6,281,859 points, levelled", s == 200 and ex2["point_count"] == 6281859
              and next(c for c in ex2["checks"] if c["name"] == "levelled")["ok"] and len(body) == 227 + 20 * 6281859, (ex2, len(body)))
        exports_dir = os.path.join(tmp, "data", "exports")
        n_files = len(os.listdir(exports_dir))
        t_cancel = time.time()
        s, ej3 = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "las", "intensity": "none"})
        s, b = call("POST", f"/jobs/{ej3['job_id']}/cancel", "agent")
        ej3 = wait_job(ej3["job_id"], ("succeeded", "failed", "cancelled"), timeout=180)
        t_cancel = time.time() - t_cancel
        check("cancelling an export stops it before it finishes, leaving no file", ej3["state"] == "cancelled"
              and "result" not in ej3 and len(os.listdir(exports_dir)) == n_files and t_cancel < t_full, (ej3, t_cancel, t_full))
    # --- recipes and runs ---
    s, rl = call("GET", "/recipes", "viewer")
    check("recipes listed, the broken one skipped", s == 200 and sorted(r["recipe_id"] for r in rl) == ["log-note", "window-measurement"], rl)
    s, rc = call("GET", "/recipes/window-measurement", "viewer")
    check("recipe served as data", s == 200 and [st["step_id"] for st in rc["steps"]][:3] == ["overview", "box", "aim"], rc)
    s, b = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "nonsense"}})
    check("run rejects an input outside its choices", s == 400 and b["code"] == "invalid_request", b)
    s, b = call("POST", "/runs", "viewer", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
    check("viewer can't start runs (403)", s == 403, b)
    s, runc = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
    r = wait_run(runc["run_id"], lambda r: r["waiting_for"] == "approval")
    check("run waits for approval of its overview scan", r["state"] == "waiting_for_person" and r["current_step_id"] == "overview", r)
    s, b = call("POST", f"/runs/{runc['run_id']}/advance", "agent2", {"step_id": "box", "input": {}})
    check("another agent can't advance the run (403)", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", f"/runs/{runc['run_id']}/cancel", "agent2")
    check("another agent can't cancel the run (403)", s == 403 and b["code"] == "forbidden", b)
    _, r = call("GET", f"/runs/{runc['run_id']}", "viewer")
    check("... and the run carries on", r["state"] == "waiting_for_person", r)
    s, b = call("POST", f"/runs/{runc['run_id']}/cancel", "agent")
    r = wait_run(runc["run_id"], lambda r: False, timeout=10)
    _, oj = call("GET", f"/jobs/{r['steps'][0]['job_id']}", "viewer")
    check("cancelling the run cancels its pending scan", r["state"] == "cancelled" and oj["state"] == "cancelled", (r["state"], oj["state"]))
    s, runo = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "trim"}})
    wait_run(runo["run_id"], lambda r: r["waiting_for"] == "approval")
    s, b = call("POST", f"/runs/{runo['run_id']}/cancel", "operator")
    r = wait_run(runo["run_id"], lambda r: False, timeout=10)
    check("an operator may cancel an agent's run", s == 200 and r["state"] == "cancelled", (s, r["state"]))
    s, runn = call("POST", "/runs", "agent", {"recipe_id": "log-note", "project_id": pid, "inputs": {"text": "   "}})
    r = wait_run(runn["run_id"], lambda r: False, timeout=10)
    check("addLogNote with blank text fails the run with invalid_request", r["state"] == "failed"
          and r["problem"]["code"] == "invalid_request" and "1–4000" in r["problem"]["detail"], r)
    s, runn = call("POST", "/runs", "agent", {"recipe_id": "log-note", "project_id": pid, "inputs": {"text": "Door wedged open"}})
    r = wait_run(runn["run_id"], lambda r: False, timeout=10)
    _, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("addLogNote with text writes the note", r["state"] == "succeeded"
          and any(e["text"] == "Door wedged open" and e["refs"].get("run_id") == runn["run_id"] for e in log), r)

    if FIX:
        s, run = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
        rid = run["run_id"]
        r = wait_run(rid, lambda r: r["waiting_for"] == "approval")
        call("POST", f"/jobs/{r['steps'][0]['job_id']}/approve", "operator")
        r = wait_run(rid, lambda r: r["waiting_for"] == "input")
        check("after approval the run scans, then waits for the box", r["current_step_id"] == "box" and r["steps"][0]["state"] == "done", r)
        s, g = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 1, "run_id": rid})
        s, b = call("POST", f"/runs/{rid}/advance", "agent", {"step_id": "box", "input": {}})
        check("advance without the box is refused", s == 400 and "box" in b.get("detail", ""), b)
        box = {"x0": 523, "y0": 418, "x1": 674, "y1": 564}
        s, b = call("POST", f"/runs/{rid}/propose", "agent2", {"step_id": "box", "input": {"box": box}})
        check("another agent can't propose in the run (403)", s == 403 and b["code"] == "forbidden", b)
        s, b = call("POST", f"/runs/{rid}/propose", "agent", {"step_id": "box", "input": {}})
        check("a proposal without the box is refused", s == 400 and "box" in b.get("detail", ""), b)
        s, b = call("POST", f"/runs/{rid}/propose", "agent", {"step_id": "box", "input": {"box": box}, "note": "window found at the right of the overview"})
        prop = {x["step_id"]: x for x in b["steps"]}["box"].get("proposal", {}) if s == 200 else {}
        check("agent proposes the box; the step keeps waiting with the proposal", s == 200 and prop.get("input", {}).get("box") == box
              and prop.get("by") == b["started_by"] and b["waiting_for"] == "input", b)
        s, b = call("POST", f"/runs/{rid}/advance", "operator", {"step_id": "box", "input": {"box": box}})
        r = wait_run(rid, lambda r: r["current_step_id"] == "measure" and r["waiting_for"] == "input", timeout=240)
        steps = {x["step_id"]: x for x in r["steps"]}
        check("run aims, scans the close-up under its grant, exports and passes checks",
              r["current_step_id"] == "measure" and all(steps[k]["state"] == "done" for k in ("aim", "closeup", "export", "checks"))
              and steps["export"]["output"]["point_count"] == 777580, {k: v["state"] for k, v in steps.items()} if r else r)
        check("the person's confirmation is recorded as accepting the proposal", steps["box"]["output"].get("proposal_accepted") is True, steps["box"])
        _, cj = call("GET", f"/jobs/{steps['closeup']['job_id']}", "viewer")
        check("close-up job carries the run id and ran without approval", cj.get("run_id") == rid and cj["state"] == "succeeded", cj)
        s, b = call("POST", f"/runs/{rid}/advance", "agent", {"step_id": "measure", "input": {"notes": "opening 1196 x 1168 mm"}})
        r = wait_run(rid, lambda r: False, timeout=10)
        check("run finishes when the person hands in the measurement", r["state"] == "succeeded" and r["finished_at"], r)
        s, log = call("GET", f"/log?project_id={pid}", "viewer")
        check("run steps are in the field log", any(e["kind"] == "run" and "started" in e["text"] for e in log)
              and any("Waiting for a person" in e["text"] for e in log), [e["text"] for e in log][-6:])

    s, job3 = call("POST", "/scans", "agent", area)
    check("grant used up: next scan waits for approval", s == 202 and job3["state"] == "awaiting_approval", job3)
    s, b = call("POST", f"/jobs/{job3['job_id']}/cancel", "agent")
    check("agent cancels its own waiting job", s == 200 and b["state"] == "cancelled", b)

    s, job4 = call("POST", "/scans", "operator", {**dome, "color": True})
    check("operator scan is queued directly", s == 202 and job4["state"] == "queued", job4)
    t0 = time.time()
    while time.time() - t0 < 30:
        _, j4 = call("GET", f"/jobs/{job4['job_id']}", "viewer")
        if j4.get("phase") == "sweeping" or j4["state"] != "running" and j4["state"] != "queued":
            break
        time.sleep(0.05)
    # While job4 runs, a granted agent scan waits in the queue; cancelling it gives the grant use back.
    s, g2 = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z",
                                                 "max_scans": 1, "allowed_modes": ["STD_L0"]})
    s, jq = call("POST", "/scans", "agent", dome)
    _, gl = call("GET", "/grants", "viewer")
    used = next(x["scans_used"] for x in gl if x["grant_id"] == g2["grant_id"])
    s, b = call("POST", f"/jobs/{jq['job_id']}/cancel", "agent")
    _, gl = call("GET", "/grants", "viewer")
    check("a granted scan cancelled before it starts gives its grant use back", jq["state"] == "queued" and used == 1
          and b["state"] == "cancelled" and next(x["scans_used"] for x in gl if x["grant_id"] == g2["grant_id"]) == 0, (jq, used, gl))
    call("DELETE", f"/grants/{g2['grant_id']}", "operator")
    s, b = call("POST", f"/jobs/{job4['job_id']}/cancel", "operator")
    j4 = wait_job(job4["job_id"], ("cancelled", "failed", "succeeded"))
    check("scan cancelled mid-sweep reads cancelled at once", j4["state"] == "cancelled" and "partial_scan_id" not in j4, j4)
    # Queued while the cancelled task is still listed.
    s, job5 = call("POST", "/scans", "operator", dome)
    t0 = time.time()
    while time.time() - t0 < 15 and "partial_scan_id" not in j4:
        time.sleep(0.2)
        _, j4 = call("GET", f"/jobs/{job4['job_id']}", "viewer")
    s, ps = call("GET", f"/scans/{j4.get('partial_scan_id')}", "viewer")
    check("cancelled job points at the partial scan the scanner filed", s == 200 and 0 < ps["point_count"] < 6281859
          and ps["project_id"] == pid and ps["created_at"], (j4, ps))
    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("field log says the partial scan was kept", any(e["refs"].get("scan_id") == j4.get("partial_scan_id")
          and "partial" in e["text"] for e in log), [e["text"] for e in log][-4:])
    j5 = wait_job(job5["job_id"], ("succeeded", "failed", "cancelled"))
    check("scan queued right after a cancel runs once the scanner clears", j5["state"] == "succeeded"
          and j5["result"]["scan_id"] != j4.get("partial_scan_id"), j5)
    tasks = json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read())
    check("scanner has no task left", tasks == [], tasks)

    # Plant cached copies (unless real fixtures cached them already) to see what a delete does to them.
    cache = os.path.join(tmp, "data", "scans")
    os.makedirs(cache, exist_ok=True)
    cached = [os.path.join(cache, f"{sc2['scan_id']}.{ext}") for ext in ("tzf", "jpg")]
    for c in cached:
        if not os.path.exists(c):
            open(c, "wb").write(b"\xff\xd8 cached")
    s, _, body = fetch(f"/scans/{sc2['scan_id']}/preview", "viewer")
    check("a listed scan's cached preview is served", s == 200 and body == open(cached[1], "rb").read(), s)
    ghost = os.path.join(cache, f"{uuid.uuid4()}.jpg")
    open(ghost, "wb").write(b"\xff\xd8 ghost")
    s, _, body = fetch(f"/scans/{os.path.basename(ghost)[:-4]}/preview", "viewer")
    check("a cached file for a scan the scanner doesn't list is refused and evicted", s == 404 and not os.path.exists(ghost), s)
    s, b = call("DELETE", f"/scans/{sc2['scan_id']}", "agent")
    check("agent can't delete (403)", s == 403, b)
    s, b = call("DELETE", f"/scans/{sc2['scan_id']}", "operator")
    check("delete without X-Confirm is refused", s == 412 and b["code"] == "confirm_mismatch", b)
    s, _ = call("DELETE", f"/scans/{sc2['scan_id']}", "operator", headers={"X-Confirm": sc2["scan_id"]})
    s2, gone = call("GET", f"/scans/{sc2['scan_id']}", "viewer")
    check("confirmed delete removes the scan", s == 204 and s2 == 404, (s, s2))
    s3, _, _ = fetch(f"/scans/{sc2['scan_id']}/preview", "viewer")
    s4, _, _ = fetch(f"/scans/{sc2['scan_id']}/tzf", "viewer")
    check("delete evicts the cached file and preview; neither is served", s3 == 404 and s4 == 404
          and not any(os.path.exists(c) for c in cached), (s3, s4, [os.path.exists(c) for c in cached]))
    if FIX:
        s, _ = call("GET", f"/exports/{ex.get('export_id')}", "viewer")
        s2, _, _ = fetch(f"/exports/{ex.get('export_id')}/file", "viewer")
        check("delete also removes exports made from the scan", s == 404 and s2 == 404, (s, s2))
    else:
        s, b = call("GET", f"/scans/{station_scan}/tzf", "viewer")
        check("a scanner error status (no file on the mock) is 502 scanner_error", s == 502 and b["code"] == "scanner_error"
              and "404" in b["title"], b)
        s, ej = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "ply"})
        ej = wait_job(ej["job_id"], ("succeeded", "failed", "cancelled"))
        check("... and fails an export with scanner_error", ej["state"] == "failed" and ej["problem"]["code"] == "scanner_error"
              and ej["problem"]["status"] == 502, ej)
    s, sc1 = call("GET", f"/scans/{station_scan}", "viewer")
    check("other scans survive the renumbering", s == 200, sc1)

    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("field log records the project's scans", s == 200 and sum(e["kind"] == "scan" for e in log) >= 4, len(log or []))
    check("approvals and finished jobs are in the project's log", any(e["kind"] == "approval" for e in log)
          and any("finished" in e["text"] for e in log), [e["text"] for e in log])
    s, b = call("POST", "/log", "agent", {"text": "Plant moved out of the way", "project_id": pid})
    check("agent adds a log note", s == 201 and b["author"] == "agent-1", b)
    time.sleep(0.3)
    check("event stream delivered job and log events", "job" in events and "log" in events, sorted(set(events)))

    # A bridge whose scanner is unreachable.
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": "http://127.0.0.1:9/api/v1",
                                  "BRIDGE_ADDR": "127.0.0.1:8798", "BRIDGE_TOKENS": tokfile}, stderr=subprocess.DEVNULL))
    wait_up("127.0.0.1:8798", "/v1/health")
    s, b = call("GET", "/status", "viewer", base="127.0.0.1:8798")
    check("disconnected scanner shows as connected: false", s == 200 and b["scanner"]["connected"] is False, b)
    s, b = call("POST", "/scans", "operator", dome, base="127.0.0.1:8798")
    check("scan with no scanner is 503 scanner_disconnected", s == 503 and b["code"] == "scanner_disconnected", b)

    # A bridge that can't write its cache: `scans` is a file, not a directory.
    bad_data = os.path.join(tmp, "bad-data")
    os.makedirs(bad_data)
    open(os.path.join(bad_data, "scans"), "w").write("not a directory")
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": f"http://{MOCK}/api/v1",
                                  "BRIDGE_ADDR": "127.0.0.1:8797", "BRIDGE_TOKENS": tokfile, "BRIDGE_DATA": bad_data},
                                  stderr=subprocess.DEVNULL))
    wait_up("127.0.0.1:8797", "/v1/health")
    s, b = call("GET", f"/scans/{station_scan}/preview", "viewer", base="127.0.0.1:8797")
    check("a cache the bridge can't write is 500 cache_error", s == 500 and b["code"] == "cache_error", b)

    # The simulator serves the item routes the x9 CLI uses.
    for args, key in ((["scan", "0"], "scanUUID"), (["tzf", "0"], "nbPoints")):
        p = subprocess.run([f"{BIN}/x9", "--base", f"http://{MOCK}/api/v1", *args], capture_output=True, text=True)
        out = json.loads(p.stdout) if p.returncode == 0 else {}
        check(f"x9 {args[0]} works against the mock", p.returncode == 0 and out.get(key) is not None, (p.returncode, p.stderr))
finally:
    for p in procs:
        p.terminate()

print(f"\n{sum(results)}/{len(results)} checks passed")
sys.exit(0 if all(results) else 1)
