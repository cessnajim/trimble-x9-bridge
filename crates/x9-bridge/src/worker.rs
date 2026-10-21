//! Runs queued scan jobs one at a time. Re-checks the rules right before the
//! scanner starts, then follows the scanner's task through its phases:
//! (waiting, while a task the bridge cancelled clears) → leveling → sweeping →
//! (scan file appears) → imaging if color → done.

use crate::map::parse_file;
use crate::model::{JobState, ScanKind, Timings};
use crate::problem::ApiError;
use crate::rules::preflight;
use crate::state::{AppState, ScanMeta};
use crate::util::now_rfc3339;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x9_client::models;

const MAX_SCAN: Duration = Duration::from_secs(45 * 60);
const MAX_SCANNER_ERRORS: u32 = 10;
/// How long a task the bridge cancelled may stay in `/tasks` (13 s on a real X9).
const CANCEL_LINGER: Duration = Duration::from_secs(60);

pub async fn run(st: Arc<AppState>) {
    loop {
        if let Some(job_id) = claim_next(&st) {
            run_scan(&st, &job_id).await;
            continue;
        }
        tokio::select! {
            _ = st.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// Mark the oldest queued job running and return its id.
fn claim_next(st: &AppState) -> Option<String> {
    let job = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter_mut().find(|j| j.job.state == JobState::Queued && j.request.is_some())?;
        rec.job.state = JobState::Running;
        rec.job.started_at = Some(now_rfc3339());
        rec.job.phase = Some("leveling".into());
        rec.job.progress = Some(0.0);
        rec.job.clone()
    };
    st.emit_job(&job);
    Some(job.job_id)
}

/// Apply a change to a job, publish it, and return whether a cancel was asked for.
fn update(st: &AppState, job_id: &str, f: impl FnOnce(&mut crate::state::JobRecord)) -> bool {
    let (job, cancel) = {
        let mut inner = st.inner.lock().unwrap();
        let Some(rec) = inner.jobs.iter_mut().find(|j| j.job.job_id == job_id) else { return true };
        f(rec);
        (rec.job.clone(), rec.cancel_requested)
    };
    st.emit_job(&job);
    cancel
}

fn finish(st: &AppState, job_id: &str, state: JobState, result: Option<serde_json::Value>, problem: Option<&ApiError>) {
    let project_id = {
        let inner = st.inner.lock().unwrap();
        inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| (j.project_id.clone(), j.job.kind.clone()))
    };
    update(st, job_id, |r| {
        r.job.state = state;
        r.job.finished_at = Some(now_rfc3339());
        r.job.phase = None;
        if state == JobState::Succeeded {
            r.job.progress = Some(1.0);
        }
        r.job.result = result;
        r.job.problem = problem.map(|p| p.to_json());
    });
    let verb = match state {
        JobState::Succeeded => "finished".to_string(),
        JobState::Cancelled => "cancelled".to_string(),
        _ => format!("failed: {}", problem.map(|p| p.code).unwrap_or("error")),
    };
    let (project_id, job_kind) = project_id.unzip();
    let job_kind = job_kind.unwrap_or_else(|| "scan".into());
    let kind = if state == JobState::Failed { "error" } else { job_kind.as_str() };
    let label = if job_kind == "export" { "Export" } else { "Scan" };
    st.log(kind, format!("{label} job {job_id} {verb}"), "bridge", project_id.as_deref(), &[("job_id", job_id)]);
}

async fn run_scan(st: &AppState, job_id: &str) {
    let (req, project_name) = {
        let inner = st.inner.lock().unwrap();
        let Some(rec) = inner.jobs.iter().find(|j| j.job.job_id == job_id) else { return };
        let Some(req) = rec.request.clone() else { return };
        (req, rec.project_name.clone())
    };

    let (snap, _) = st.snapshot().await;
    if let Err(e) = preflight(&st.rules, &snap) {
        return finish(st, job_id, JobState::Failed, None, Some(&e));
    }
    match wait_idle(st, job_id, &req.project_id).await {
        Idle::Ready => {}
        Idle::Cancelled => {
            // The scanner never got this scan: give its grant use back.
            st.inner.lock().unwrap().refund_grant(job_id);
            st.save_grants();
            return finish(st, job_id, JobState::Cancelled, None, None);
        }
        Idle::Failed(e) => return finish(st, job_id, JobState::Failed, None, Some(&e)),
    }
    // Taken after the wait: a cancelled scan's partial sweep may be filed during it.
    let before: HashSet<String> = match st.scanner.scans().await {
        Ok(s) => s.into_iter().filter_map(|s| s.scan_uuid).collect(),
        Err(e) => return finish(st, job_id, JobState::Failed, None, Some(&e.into())),
    };

    let w = req.window;
    let body = models::ScanRequest {
        project_name: Some(project_name.clone()),
        project_uuid: Some(req.project_id.clone()),
        scan_mode: Some(req.mode.clone()),
        color_mode: Some(if req.color { "ON" } else { "OFF" }.into()),
        scan_type: Some(match req.kind {
            ScanKind::FullDome => "FullDome",
            ScanKind::Area => "AreaScan",
        }.into()),
        start_ha: w.map(|w| w.start_ha),
        delta_ha: w.map(|w| w.delta_ha),
        start_va: w.map(|w| w.start_va),
        delta_va: w.map(|w| w.delta_va),
        ..Default::default()
    };
    let t0 = Instant::now();
    let task = match st.scanner.launch_scan(&body).await {
        Ok(t) => t,
        Err(e) => return finish(st, job_id, JobState::Failed, None, Some(&e.into())),
    };
    let task_id = task.id;
    // The scanner answers 202 even when it can't parse a request, then runs a
    // default full-dome scan. Compare its echo with what was sent; on any
    // difference, stop the task at once.
    let sent = serde_json::to_value(&body).unwrap_or_default();
    let echo = task.scan_request.clone().unwrap_or_default();
    let diffs = echo_mismatch(&sent, &echo);
    if !diffs.is_empty() {
        // Stopped before any sweep, so there's no partial scan to wait for;
        // the next job waits out the lingering task in `wait_idle`.
        cancel_task(st, task_id).await;
        let e = ApiError::conflict(format!("the scanner did not accept the request as sent ({}); its task was cancelled", diffs.join(", ")));
        return finish(st, job_id, JobState::Failed, None, Some(&e));
    }
    update(st, job_id, |r| r.scanner_task_id = task_id);
    st.log("scan", format!("Scanner started {} ({})", req.mode, if req.color { "color on" } else { "color off" }), "bridge", Some(&req.project_id), &[("job_id", job_id)]);

    let mut timings = Timings::default();
    let mut new_scan: Option<String> = None;
    let mut idle_since: Option<Instant> = None;
    let mut errors = 0u32;
    let secs = || (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;

    loop {
        tokio::time::sleep(st.poll).await;
        if t0.elapsed() > MAX_SCAN {
            let e = ApiError::conflict("the scan did not finish within 45 minutes");
            return finish(st, job_id, JobState::Failed, None, Some(&e));
        }
        let (tasks, scans) = match (st.scanner.tasks().await, st.scanner.scans().await) {
            (Ok(t), Ok(s)) => {
                errors = 0;
                (t, s)
            }
            (Err(e), _) | (_, Err(e)) => {
                errors += 1;
                if errors >= MAX_SCANNER_ERRORS {
                    return finish(st, job_id, JobState::Failed, None, Some(&e.into()));
                }
                continue;
            }
        };
        let running = tasks.iter().find(|t| task_id.is_none() || t.id == task_id);

        // The scan file appears whole once the sweep is done (before color).
        if new_scan.is_none() {
            let fresh: Vec<&models::Scan> = scans.iter().filter(|s| s.scan_uuid.as_ref().is_some_and(|u| !before.contains(u))).collect();
            let want = running.and_then(|t| t.file.clone()).filter(|f| !f.is_empty());
            let pick = fresh
                .iter()
                .find(|s| match (&want, s.tzf.as_ref().and_then(|t| t.file.as_deref())) {
                    (Some(w), Some(f)) => f.ends_with(&format!("/{w}")),
                    _ => false,
                })
                .or(fresh.first());
            if let Some(s) = pick {
                new_scan = s.scan_uuid.clone();
                timings.geometry_ready_s = Some(secs());
                if let (Some(g), Some(l)) = (timings.geometry_ready_s, timings.leveling_s) {
                    timings.sweep_s = Some(((g - l) * 10.0).round() / 10.0);
                }
                let file = s.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
                let station = parse_file(&file).map(|(_, st, _)| st).unwrap_or(0);
                st.log("scan", format!("Geometry ready: {file} (station {station}), {} points", s.tzf.as_ref().and_then(|t| t.nb_points).unwrap_or(0)), "bridge", Some(&req.project_id), &[("job_id", job_id), ("scan_id", new_scan.as_deref().unwrap_or(""))]);
            }
        }

        let phase = match running {
            None => None,
            Some(t) if matches!(t.state.as_deref(), Some("running")) => {
                if timings.leveling_s.is_none() {
                    timings.leveling_s = Some(secs());
                }
                // With color off, the file appears a few seconds before the
                // task clears; that tail still counts as the sweep.
                Some(if new_scan.is_some() && req.color { "imaging" } else { "sweeping" })
            }
            Some(_) => Some("leveling"),
        };
        let progress = running.and_then(|t| t.percentage);
        let cancel = update(st, job_id, |r| {
            if let Some(p) = phase {
                r.job.phase = Some(p.into());
            }
            if progress.is_some() {
                r.job.progress = progress;
            }
        });

        if cancel {
            cancel_task(st, task_id).await;
            finish(st, job_id, JobState::Cancelled, None, None);
            return follow_cancelled(st, job_id, task_id, &before, new_scan, &req).await;
        }

        // Done once the scanner has been idle for a few polls with the file in
        // hand (it can start a short follow-up task right after a color scan).
        if running.is_none() && tasks.is_empty() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if new_scan.is_some() && since.elapsed() >= st.poll * 3 {
                break;
            }
            if new_scan.is_none() && since.elapsed() >= st.poll * 10 {
                let e = ApiError::conflict("the scanner finished without producing a scan file");
                return finish(st, job_id, JobState::Failed, None, Some(&e));
            }
        } else {
            idle_since = None;
        }
    }

    timings.done_s = Some(secs());
    let scan_id = new_scan.unwrap_or_default();
    let created_at = {
        let inner = st.inner.lock().unwrap();
        inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.job.started_at.clone()).unwrap_or_else(now_rfc3339)
    };
    st.inner.lock().unwrap().scan_meta.insert(scan_id.clone(), ScanMeta { created_at, timings, window: req.window });
    finish(st, job_id, JobState::Succeeded, Some(serde_json::json!({"scan_id": scan_id})), None);
}

enum Idle {
    Ready,
    Cancelled,
    Failed(ApiError),
}

#[derive(Debug, PartialEq)]
enum Busy {
    Idle,
    /// Only tasks the bridge cancelled, which the scanner still lists.
    Ours,
    /// Tasks someone else started.
    Other(usize),
}

fn busy(tasks: &[models::Task], ours: &HashSet<i64>) -> Busy {
    let other = tasks.iter().filter(|t| !t.id.is_some_and(|id| ours.contains(&id))).count();
    match (tasks.is_empty(), other) {
        (true, _) => Busy::Idle,
        (false, 0) => Busy::Ours,
        (false, n) => Busy::Other(n),
    }
}

fn scanner_busy(detail: String) -> ApiError {
    ApiError::new(axum::http::StatusCode::CONFLICT, "scanner_busy", "The scanner is busy with another task")
        .detail(detail)
        .hint("Wait for the other task to finish, then queue the scan again.")
}

fn cancel_requested(st: &AppState, job_id: &str) -> bool {
    st.inner.lock().unwrap().jobs.iter().find(|j| j.job.job_id == job_id).is_none_or(|j| j.cancel_requested)
}

/// Stop a scanner task and remember it was the bridge that did.
async fn cancel_task(st: &AppState, task_id: Option<i64>) {
    if let Some(id) = task_id {
        st.inner.lock().unwrap().cancelled_tasks.insert(id);
        let _ = st.scanner.cancel_task(id).await;
    }
}

/// Wait until the scanner has no task. A task the bridge cancelled stays listed
/// for a while after the cancel; wait that out (up to `CANCEL_LINGER`). Any
/// other task means another client (the vendor app, a script) is driving the
/// scanner: fail at once.
async fn wait_idle(st: &AppState, job_id: &str, project_id: &str) -> Idle {
    let t0 = Instant::now();
    let mut logged = false;
    loop {
        let tasks = match st.scanner.tasks().await {
            Ok(t) => t,
            Err(e) => return Idle::Failed(e.into()),
        };
        let ours = st.inner.lock().unwrap().cancelled_tasks.clone();
        match busy(&tasks, &ours) {
            Busy::Idle => {
                st.inner.lock().unwrap().cancelled_tasks.clear();
                if logged {
                    update(st, job_id, |r| r.job.phase = Some("leveling".into()));
                }
                return Idle::Ready;
            }
            Busy::Other(n) => return Idle::Failed(scanner_busy(format!("{n} task(s) running"))),
            Busy::Ours if t0.elapsed() >= CANCEL_LINGER => {
                return Idle::Failed(scanner_busy(format!("a task the bridge cancelled is still listed after {} s", CANCEL_LINGER.as_secs())));
            }
            Busy::Ours => {
                if !logged {
                    update(st, job_id, |r| r.job.phase = Some("waiting".into()));
                    st.log("scan", "Waiting for the scanner to clear the cancelled task", "bridge", Some(project_id), &[("job_id", job_id)]);
                    logged = true;
                }
            }
        }
        if cancel_requested(st, job_id) {
            return Idle::Cancelled;
        }
        tokio::time::sleep(st.poll).await;
    }
}

/// After a cancel the scanner keeps the task listed for a while, and may still
/// file what it had swept as a scan. Wait for that (up to `CANCEL_LINGER`) and
/// point the job at it, so a person can keep or delete it. `known` is a scan
/// already seen during the run.
async fn follow_cancelled(st: &AppState, job_id: &str, task_id: Option<i64>, before: &HashSet<String>, known: Option<String>, req: &crate::model::ScanRequest) {
    let t0 = Instant::now();
    let mut gone_since: Option<Instant> = None;
    let mut found: Option<models::Scan> = None;
    while found.is_none() && t0.elapsed() < CANCEL_LINGER {
        let (Ok(tasks), Ok(scans)) = (st.scanner.tasks().await, st.scanner.scans().await) else {
            tokio::time::sleep(st.poll).await;
            continue;
        };
        found = scans.into_iter().find(|s| match (&known, &s.scan_uuid) {
            (Some(k), Some(u)) => k == u,
            (None, Some(u)) => !before.contains(u),
            _ => false,
        });
        if task_id.is_none_or(|id| !tasks.iter().any(|t| t.id == Some(id))) {
            if let Some(id) = task_id {
                st.inner.lock().unwrap().cancelled_tasks.remove(&id);
            }
            // The scan may be filed just after the task clears.
            if gone_since.get_or_insert_with(Instant::now).elapsed() >= st.poll * 5 {
                break;
            }
        }
        if found.is_none() {
            tokio::time::sleep(st.poll).await;
        }
    }
    let Some(scan) = found else { return };
    let Some(scan_id) = scan.scan_uuid.clone() else { return };
    {
        let mut inner = st.inner.lock().unwrap();
        let created_at = inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.job.started_at.clone()).unwrap_or_else(now_rfc3339);
        inner.scan_meta.entry(scan_id.clone()).or_insert_with(|| ScanMeta { created_at, timings: Timings::default(), window: req.window });
    }
    update(st, job_id, |r| r.job.partial_scan_id = Some(scan_id.clone()));
    let file = scan.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
    let points = scan.tzf.as_ref().and_then(|t| t.nb_points).unwrap_or(0);
    st.log(
        "scan",
        format!("The scanner kept a partial scan from cancelled job {job_id}: {file}, {points} points. Keep it or delete it."),
        "bridge",
        Some(&req.project_id),
        &[("job_id", job_id), ("scan_id", &scan_id)],
    );
}

/// Resolves once someone asks the job to stop.
async fn until_cancelled(st: &AppState, job_id: &str) {
    while !cancel_requested(st, job_id) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run `fut` unless the job is cancelled first (then `None`; `fut` is dropped).
async fn unless_cancelled<T>(st: &AppState, job_id: &str, fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::select! {
        v = fut => Some(v),
        _ = until_cancelled(st, job_id) => None,
    }
}

/// Decode a scan into a point file. Runs alongside scans (it never touches the
/// scanner beyond downloading the file once). A cancel stops it at the next
/// check: during the download, between decoding and writing, or while writing.
pub async fn run_export(st: Arc<AppState>, job_id: String, scan_id: String, req: crate::export::ExportRequest) {
    use crate::export::Intensity;
    use crate::files::{scan_file, FileKind};
    let cancelled = |st: &AppState| finish(st, &job_id, JobState::Cancelled, None, None);
    let tzf = match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Tzf)).await {
        None => return cancelled(&st),
        Some(Ok(p)) => p,
        Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    };
    let preview = if req.intensity == Intensity::None {
        None
    } else {
        match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Preview)).await {
            None => return cancelled(&st),
            Some(Ok(p)) => tokio::fs::read(p).await.ok(),
            Some(Err(_)) => None,
        }
    };
    if update(&st, &job_id, |r| r.job.phase = Some("decoding".into())) {
        return cancelled(&st);
    }
    let export_id = crate::util::new_id("exp");
    let out = st.data_dir.join("exports").join(format!("{export_id}.{}", req.format.ext()));
    if let Some(dir) = out.parent() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("exports directory: {e}"))));
        }
    }
    let (out2, req2, st2, job2) = (out.clone(), req.clone(), st.clone(), job_id.clone());
    let res = tokio::task::spawn_blocking(move || {
        let stop = || cancel_requested(&st2, &job2);
        crate::export::run(&tzf, preview.as_deref(), &req2, &out2, &stop)
    })
    .await;
    if cancel_requested(&st, &job_id) {
        let _ = tokio::fs::remove_file(&out).await;
        return cancelled(&st);
    }
    match res {
        Ok(Ok(mut v)) => {
            v["export_id"] = serde_json::json!(export_id);
            v["scan_id"] = serde_json::json!(scan_id);
            let failed: Vec<String> = v["checks"]
                .as_array()
                .map(|cs| cs.iter().filter(|c| c["ok"] == false).filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let project_id = {
                let mut inner = st.inner.lock().unwrap();
                inner.exports.insert(export_id.clone(), v.clone());
                inner.export_files.insert(export_id.clone(), out);
                inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.project_id.clone())
            };
            if !failed.is_empty() {
                st.log("check", format!("Export {export_id}: checks failed: {}", failed.join(", ")), "bridge", project_id.as_deref(), &[("export_id", &export_id)]);
            }
            finish(&st, &job_id, JobState::Succeeded, Some(serde_json::json!({"export_id": export_id})), None);
        }
        Ok(Err(crate::export::Failure::Write(e))) => {
            let _ = tokio::fs::remove_file(&out).await;
            finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("writing the export: {e}"))));
        }
        Ok(Err(crate::export::Failure::Decode(e))) => {
            let _ = tokio::fs::remove_file(&out).await;
            let p = ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not decode the scan").detail(e);
            finish(&st, &job_id, JobState::Failed, None, Some(&p));
        }
        // Only returned when a cancel was asked for, which is handled above.
        Ok(Err(crate::export::Failure::Cancelled)) => cancelled(&st),
        Err(e) => finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::conflict(e.to_string()))),
    }
}

/// Fields of `sent` the scanner's echo doesn't repeat (floats within 1e-9).
pub fn echo_mismatch(sent: &serde_json::Value, echo: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(sent) = sent.as_object() else { return out };
    for (k, v) in sent {
        let got = echo.get(k);
        let same = match (v, got) {
            (serde_json::Value::Number(a), Some(serde_json::Value::Number(b))) => {
                (a.as_f64().unwrap_or(f64::NAN) - b.as_f64().unwrap_or(f64::NAN)).abs() < 1e-9
            }
            (a, Some(b)) => a == b,
            (_, None) => false,
        };
        if !same {
            out.push(format!("{k}: sent {v}, echoed {}", got.map(|g| g.to_string()).unwrap_or_else(|| "nothing".into())));
        }
    }
    out
}

/// Measure a scan (blocking work off the runtime): download its file, decode
/// levelled points, find the opening, store the measurement and evidence.
/// Grid cells inside a preview box (pixels, y down; `x1 < x0` wraps across the
/// seam), padded by `margin` radians: rows `r0..=r1`, columns `c0..=c1`
/// (wrapping when `c1 < c0`).
pub struct Cells {
    pub r0: u32,
    pub r1: u32,
    pub c0: u32,
    pub c1: u32,
}

impl Cells {
    pub fn contains(&self, r: u32, c: u32) -> bool {
        r >= self.r0 && r <= self.r1 && if self.c0 <= self.c1 { c >= self.c0 && c <= self.c1 } else { c >= self.c0 || c <= self.c1 }
    }
}

pub fn crop_cells(m: &trimble_tzf::ScanMeta, pw: usize, ph: usize, b: [i64; 4], margin: f64) -> Result<Cells, String> {
    let [x0, y0, x1, y1] = b;
    if !(0..=pw as i64).contains(&x0) || !(0..=pw as i64).contains(&x1) || !(0 <= y0 && y0 < y1 && y1 <= ph as i64) || x0 == x1 {
        return Err(format!("box must lie within the {pw}x{ph} preview, with y0 < y1 and some width"));
    }
    let (cols, rows) = (m.grid_cols as i64, m.grid_rows as i64);
    let (sx, sy) = (cols as f64 / pw as f64, rows as f64 / ph as f64);
    let pad_c = (margin / m.angle_step_rad[0].abs().max(1e-9)).round() as i64;
    let pad_r = (margin / m.angle_step_rad[1].abs().max(1e-9)).round() as i64;
    let r0 = ((y0 as f64 * sy) as i64 - pad_r).clamp(0, rows - 1) as u32;
    let r1 = ((y1 as f64 * sy) as i64 + pad_r).clamp(0, rows - 1) as u32;
    let c0 = ((x0 as f64 * sx) as i64 - pad_c).rem_euclid(cols) as u32;
    let c1 = ((x1 as f64 * sx) as i64 + pad_c).rem_euclid(cols) as u32;
    Ok(Cells { r0, r1, c0, c1 })
}

pub async fn run_measure(st: Arc<AppState>, job_id: String, scan_id: String, crop: Option<([i64; 4], f64)>) {
    use crate::files::{scan_file, FileKind};
    let cancelled = |st: &AppState| finish(st, &job_id, JobState::Cancelled, None, None);
    let tzf = match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Tzf)).await {
        None => return cancelled(&st),
        Some(Ok(p)) => p,
        Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    };
    // A crop is a box on the preview: its size maps the box onto the scan grid.
    let preview_size = match crop {
        None => None,
        Some(_) => match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Preview)).await {
            None => return cancelled(&st),
            Some(Ok(p)) => match tokio::fs::read(&p).await.map_err(|e| e.to_string()).and_then(|b| crate::aim::jpeg_size(&b)) {
                Ok(wh) => Some(wh),
                Err(e) => return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::invalid(format!("preview: {e}")))),
            },
            Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
        },
    };
    if update(&st, &job_id, |r| r.job.phase = Some("measuring".into())) {
        return cancelled(&st);
    }
    let (st2, job2) = (st.clone(), job_id.clone());
    let res = tokio::task::spawn_blocking(move || -> Result<(serde_json::Value, Vec<u8>), String> {
        let t = trimble_tzf::Tzf::open(&tzf).map_err(|e| format!("open: {e}"))?;
        let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
        let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;
        if cancel_requested(&st2, &job2) {
            return Err("cancelled".into());
        }
        let cells = match (crop, preview_size) {
            (Some((b, margin)), Some((pw, ph))) => Some(crop_cells(&meta, pw, ph, b, margin)?),
            _ => None,
        };
        let keep = |p: &trimble_tzf::GridPoint| cells.as_ref().is_none_or(|c| c.contains(p.row, p.col));
        let xyz: Vec<[f64; 3]> = pts.iter().filter(|p| keep(p)).map(|p| [p.point.x as f64, p.point.y as f64, p.point.z as f64]).collect();
        drop(pts);
        let o = crate::measure::opening(&xyz)?;
        let (mut v, png) = crate::measure::report(&o);
        if let (Some(c), Some((b, margin))) = (&cells, crop) {
            v["crop"] = serde_json::json!({"box": {"x0": b[0], "y0": b[1], "x1": b[2], "y1": b[3]}, "margin_rad": margin,
                                           "rows": [c.r0, c.r1], "cols": [c.c0, c.c1], "points": xyz.len(),
                                           "angle_step_mrad": (meta.angle_step_rad[0].abs().max(meta.angle_step_rad[1].abs()) * 1e4).round() / 10.0});
        }
        Ok((v, png))
    })
    .await;
    if cancel_requested(&st, &job_id) {
        return cancelled(&st);
    }
    match res {
        Ok(Ok((mut v, png))) => {
            let id = crate::util::new_id("msr");
            let path = st.data_dir.join("measurements").join(format!("{id}.png"));
            if let Some(dir) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(dir).await {
                    return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("measurements directory: {e}"))));
                }
            }
            if let Err(e) = tokio::fs::write(&path, &png).await {
                return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("evidence image: {e}"))));
            }
            v["measurement_id"] = serde_json::json!(id);
            v["scan_id"] = serde_json::json!(scan_id);
            v["evidence_url"] = serde_json::json!(format!("/v1/measurements/{id}/evidence"));
            let failed: Vec<String> = v["checks"]
                .as_array()
                .map(|cs| cs.iter().filter(|c| c["ok"] == false).filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let project_id = {
                let mut inner = st.inner.lock().unwrap();
                inner.measurements.insert(id.clone(), v.clone());
                inner.measurement_files.insert(id.clone(), path);
                inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.project_id.clone())
            };
            let text = format!("Opening {} × {} mm (± {} mm) in scan {scan_id}", v["opening_width_mm"], v["opening_height_mm"], v["uncertainty_mm"]);
            st.log("measure", text, "bridge", project_id.as_deref(), &[("measurement_id", &id), ("scan_id", &scan_id)]);
            if !failed.is_empty() {
                st.log("check", format!("Measurement {id}: checks failed: {}", failed.join(", ")), "bridge", project_id.as_deref(), &[("measurement_id", &id)]);
            }
            finish(&st, &job_id, JobState::Succeeded, Some(serde_json::json!({"measurement_id": id})), None);
        }
        Ok(Err(e)) => {
            let p = ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "measure_failed", "Could not measure the opening").detail(e);
            finish(&st, &job_id, JobState::Failed, None, Some(&p));
        }
        Err(e) => finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::conflict(e.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::{busy, echo_mismatch, Busy};
    use serde_json::json;
    use std::collections::HashSet;
    use x9_client::models::Task;

    #[test]
    fn busy_tells_our_cancelled_task_from_another_client() {
        let task = |id: Option<i64>| Task { id, ..Default::default() };
        let ours: HashSet<i64> = [4].into();
        assert_eq!(busy(&[], &ours), Busy::Idle);
        // The task we cancelled is still listed: wait for it.
        assert_eq!(busy(&[task(Some(4))], &ours), Busy::Ours);
        // Anything else is another client.
        assert_eq!(busy(&[task(Some(5))], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(Some(4)), task(Some(5))], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(None)], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(Some(4))], &HashSet::new()), Busy::Other(1));
    }

    #[test]
    fn echo_check() {
        let sent = json!({"scanMode": "STD_L2", "scanType": "AreaScan", "startHA": 5.163, "colorMode": "OFF"});
        let good = json!({"scanMode": "STD_L2", "scanType": "AreaScan", "startHA": 5.163000000000001, "colorMode": "OFF", "autoLeveling": true});
        assert!(echo_mismatch(&sent, &good).is_empty());
        // The failure mode: the scanner fell back to a default full dome.
        let fallback = json!({"scanMode": "STD_L0", "scanType": "FullDome", "startHA": -1, "colorMode": "ON"});
        assert_eq!(echo_mismatch(&sent, &fallback).len(), 4);
        assert_eq!(echo_mismatch(&sent, &json!({})).len(), 4);
    }
}
