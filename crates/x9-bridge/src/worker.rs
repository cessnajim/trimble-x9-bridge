//! Runs queued scan jobs one at a time. Re-checks the rules right before the
//! scanner starts, then follows the scanner's task through its phases:
//! leveling → sweeping → (scan file appears) → imaging if color → done.

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
    // Another client (the vendor app, a script) may be driving the scanner.
    match st.scanner.tasks().await {
        Ok(t) if !t.is_empty() => {
            let busy = ApiError::new(axum::http::StatusCode::CONFLICT, "scanner_busy", "The scanner is busy with another task")
                .detail(format!("{} task(s) running", t.len()))
                .hint("Wait for the other task to finish, then queue the scan again.");
            return finish(st, job_id, JobState::Failed, None, Some(&busy));
        }
        Ok(_) => {}
        Err(e) => return finish(st, job_id, JobState::Failed, None, Some(&e.into())),
    }
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
        if let Some(id) = task_id {
            let _ = st.scanner.cancel_task(id).await;
        }
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
            if let Some(id) = task_id {
                let _ = st.scanner.cancel_task(id).await;
            }
            return finish(st, job_id, JobState::Cancelled, None, None);
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

/// Decode a scan into a point file. Runs alongside scans (it never touches the
/// scanner beyond downloading the file once).
pub async fn run_export(st: Arc<AppState>, job_id: String, scan_id: String, req: crate::export::ExportRequest) {
    use crate::export::Intensity;
    use crate::files::{scan_file, FileKind};
    let tzf = match scan_file(&st, &scan_id, FileKind::Tzf).await {
        Ok(p) => p,
        Err(e) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    };
    let preview = if req.intensity == Intensity::None {
        None
    } else {
        match scan_file(&st, &scan_id, FileKind::Preview).await {
            Ok(p) => tokio::fs::read(p).await.ok(),
            Err(_) => None,
        }
    };
    update(&st, &job_id, |r| r.job.phase = Some("decoding".into()));
    let export_id = crate::util::new_id("exp");
    let out = st.data_dir.join("exports").join(format!("{export_id}.{}", req.format.ext()));
    if let Some(dir) = out.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    let (out2, req2) = (out.clone(), req.clone());
    let res = tokio::task::spawn_blocking(move || crate::export::run(&tzf, preview.as_deref(), &req2, &out2)).await;
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
        Ok(Err(e)) => {
            let p = ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not decode the scan").detail(e);
            finish(&st, &job_id, JobState::Failed, None, Some(&p));
        }
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

#[cfg(test)]
mod tests {
    use super::echo_mismatch;
    use serde_json::json;

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
