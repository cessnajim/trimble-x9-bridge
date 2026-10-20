//! HTTP handlers for the implemented part of API v1.

use crate::files::{scan_file, FileKind};
use crate::{aim, export, map};
use crate::model::{Grant, GrantRequest, Job, JobState, Role, ScanRequest};
use crate::problem::ApiError;
use crate::rules::{admit_scan, grant_covers, Admission};
use crate::state::{AppState, JobRecord};
use crate::util::{new_id, now_rfc3339, now_unix, parse_rfc3339};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;

type St = State<Arc<AppState>>;
type ApiResult<T> = Result<T, ApiError>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/rules", get(rules))
        .route("/v1/scan-modes", get(scan_modes))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/scans", get(list_scans).post(start_scan))
        .route("/v1/scans/:scan_id", get(get_scan).delete(delete_scan))
        .route("/v1/scans/:scan_id/preview", get(scan_preview))
        .route("/v1/scans/:scan_id/tzf", get(scan_tzf))
        .route("/v1/scans/:scan_id/exports", post(start_export))
        .route("/v1/exports/:export_id", get(get_export))
        .route("/v1/exports/:export_id/file", get(get_export_file))
        .route("/v1/aim", post(aim_from_preview))
        .route("/v1/recipes", get(list_recipes))
        .route("/v1/recipes/:recipe_id", get(get_recipe))
        .route("/v1/runs", get(list_runs).post(start_run))
        .route("/v1/runs/:run_id", get(get_run))
        .route("/v1/runs/:run_id/advance", post(advance_run))
        .route("/v1/runs/:run_id/cancel", post(cancel_run))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/:job_id", get(get_job))
        .route("/v1/jobs/:job_id/cancel", post(cancel_job))
        .route("/v1/jobs/:job_id/approve", post(approve_job))
        .route("/v1/grants", get(list_grants).post(create_grant))
        .route("/v1/grants/:grant_id", delete(revoke_grant))
        .route("/v1/log", get(get_log).post(add_log))
        .route("/v1/events", get(events))
        .with_state(st)
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::invalid(e.to_string()))
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": VERSION}))
}

async fn status(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (_, raw) = st.snapshot().await;
    let (busy_job, queue_len) = {
        let inner = st.inner.lock().unwrap();
        let running = inner.current_scan_job().map(|j| j.job_id.clone());
        let q = inner.jobs.iter().filter(|j| matches!(j.job.state, JobState::Queued | JobState::AwaitingApproval)).count();
        (running, q)
    };
    Ok(Json(map::status(raw.as_ref(), busy_job.as_deref(), queue_len, VERSION, &st.started_at)))
}

async fn rules(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    Ok(Json(serde_json::to_value(&st.rules).unwrap_or_default()))
}

async fn scan_modes(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let modes = st.scanner.scan_modes().await?;
    Ok(Json(Value::Array(modes.iter().map(map::scan_mode).collect())))
}

async fn list_projects(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let ps = st.scanner.projects().await?;
    Ok(Json(Value::Array(ps.iter().map(map::project).collect())))
}

async fn create_project(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "create projects")?;
    #[derive(Deserialize)]
    struct Req {
        name: String,
    }
    let req: Req = parse(&body)?;
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 || name.contains('/') {
        return Err(ApiError::invalid("name must be 1–64 characters with no '/'"));
    }
    if st.scanner.projects().await?.iter().any(|p| p.project_name.as_deref() == Some(name)) {
        return Err(ApiError::conflict(format!("a project named {name:?} already exists")));
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    let p = st.scanner.create_project(name, &uuid).await?;
    st.log("note", format!("Project {name:?} created"), &c.name, Some(&uuid), &[]);
    Ok((StatusCode::CREATED, Json(map::project(&p))).into_response())
}

#[derive(Deserialize)]
struct ScanFilter {
    project_id: Option<String>,
}

async fn all_scans(st: &AppState) -> ApiResult<Vec<Value>> {
    let (scans, projects) = (st.scanner.scans().await?, st.scanner.projects().await?);
    let meta = st.inner.lock().unwrap().scan_meta.clone();
    Ok(scans.iter().rev().filter_map(|s| map::scan(s, &scans, &projects, &meta)).collect())
}

async fn list_scans(State(st): St, h: HeaderMap, Query(f): Query<ScanFilter>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut v = all_scans(&st).await?;
    if let Some(p) = f.project_id {
        v.retain(|s| s["project_id"] == p.as_str());
    }
    Ok(Json(Value::Array(v)))
}

async fn get_scan(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    all_scans(&st).await?.into_iter().find(|s| s["scan_id"] == id.as_str()).map(Json).ok_or_else(|| ApiError::not_found("scan"))
}

async fn delete_scan(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let c = st.require(&h, Role::is_operator, "delete scans")?;
    let confirm = h.get("x-confirm").and_then(|v| v.to_str().ok()).unwrap_or("");
    if confirm != id {
        return Err(ApiError::new(StatusCode::PRECONDITION_FAILED, "confirm_mismatch", "Delete not confirmed")
            .hint("Repeat the scan id in the X-Confirm header."));
    }
    // Scanner ids are positional: resolve right before deleting.
    let scans = st.scanner.scans().await?;
    let numeric = scans
        .iter()
        .find(|s| s.scan_uuid.as_deref() == Some(id.as_str()))
        .and_then(|s| s.id)
        .ok_or_else(|| ApiError::not_found("scan"))?;
    st.scanner.delete_scan(numeric).await?;
    crate::files::evict(&st, &id).await;
    st.inner.lock().unwrap().scan_meta.remove(&id);
    st.log("note", format!("Scan {id} deleted"), &c.name, None, &[("scan_id", &id)]);
    Ok(StatusCode::NO_CONTENT)
}

async fn start_scan(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "start scans")?;
    let req: ScanRequest = parse(&body)?;
    let job = submit_scan(&st, &c, req, None).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// Admit a scan under the caller's role (shared by direct requests and runs).
pub async fn submit_scan(st: &Arc<AppState>, c: &crate::model::Caller, req: ScanRequest, run_id: Option<&str>) -> ApiResult<Job> {
    req.validate().map_err(ApiError::invalid)?;
    let modes = st.scanner.scan_modes().await?;
    if !modes.iter().any(|m| m.scan_mode.as_deref() == Some(req.mode.as_str())) {
        let valid: Vec<&str> = modes.iter().filter_map(|m| m.scan_mode.as_deref()).collect();
        return Err(ApiError::invalid(format!("unknown mode {:?}; valid: {}", req.mode, valid.join(", "))));
    }
    let project = st
        .scanner
        .projects()
        .await?
        .into_iter()
        .find(|p| p.project_uuid.as_deref() == Some(req.project_id.as_str()))
        .ok_or_else(|| ApiError::not_found("project"))?;
    let (snap, _) = st.snapshot().await;

    let job = {
        let mut inner = st.inner.lock().unwrap();
        let queue_len = inner.jobs.iter().filter(|j| !j.job.state.is_final() && j.job.state != JobState::Running).count();
        let now = now_unix();
        let grant_idx = inner.grants.iter().position(|g| grant_covers(g, &req, now, run_id));
        let admission = admit_scan(&st.rules, &snap, c.role, queue_len, grant_idx.is_some())?;
        let mut grant_id = None;
        if admission == Admission::Queue && c.role == Role::Agent {
            if let Some(i) = grant_idx {
                inner.grants[i].scans_used += 1;
                grant_id = Some(inner.grants[i].grant_id.clone());
            }
        }
        let job = Job {
            job_id: new_id("job"),
            kind: "scan".into(),
            state: if admission == Admission::Queue { JobState::Queued } else { JobState::AwaitingApproval },
            phase: None,
            progress: None,
            requested_by: c.name.clone(),
            run_id: run_id.map(str::to_string),
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            result: None,
            problem: None,
            partial_scan_id: None,
        };
        inner.jobs.push(JobRecord {
            job: job.clone(),
            request: Some(req.clone()),
            project_id: req.project_id.clone(),
            project_name: project.project_name.clone().unwrap_or_default(),
            scanner_task_id: None,
            cancel_requested: false,
            grant_id,
        });
        job
    };
    let what = match req.kind {
        crate::model::ScanKind::FullDome => "full-dome",
        crate::model::ScanKind::Area => "area",
    };
    let waiting = if job.state == JobState::AwaitingApproval { ", awaiting approval" } else { "" };
    let note = req.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default();
    st.log("scan", format!("{what} {} scan requested{waiting}{note}", req.mode), &c.name, Some(&req.project_id), &[("job_id", &job.job_id)]);
    st.emit_job(&job);
    st.wake.notify_one();
    Ok(job)
}

async fn list_jobs(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    Ok(Json(json!(inner.jobs.iter().rev().map(|j| &j.job).collect::<Vec<_>>())))
}

async fn get_job(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.jobs.iter().find(|j| j.job.job_id == id).map(|j| Json(j.job.clone())).ok_or_else(|| ApiError::not_found("job"))
}

async fn cancel_job(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    let c = st.require(&h, |r| r != Role::Viewer, "cancel jobs")?;
    let (job, project_id) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter().find(|j| j.job.job_id == id).ok_or_else(|| ApiError::not_found("job"))?;
        if c.role == Role::Agent && rec.job.requested_by != c.name {
            return Err(ApiError::forbidden("cancel other people's jobs"));
        }
        let project_id = rec.project_id.clone();
        // A running job is stopped by whoever runs it: the scan worker or the export.
        (inner.cancel_job(&id).ok_or_else(|| ApiError::not_found("job"))?, project_id)
    };
    st.log("scan", format!("Cancel requested for {id}"), &c.name, Some(&project_id), &[("job_id", &id)]);
    st.emit_job(&job);
    Ok(Json(job))
}

async fn approve_job(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    let c = st.require(&h, Role::is_operator, "approve jobs")?;
    let (job, project_id) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter_mut().find(|j| j.job.job_id == id).ok_or_else(|| ApiError::not_found("job"))?;
        if rec.job.state != JobState::AwaitingApproval {
            return Err(ApiError::conflict(format!("job is {}, not awaiting approval", rec.job.state.as_str())));
        }
        rec.job.state = JobState::Queued;
        (rec.job.clone(), rec.project_id.clone())
    };
    st.log("approval", format!("Job {id} approved"), &c.name, Some(&project_id), &[("job_id", &id)]);
    st.emit_job(&job);
    st.wake.notify_one();
    Ok(Json(job))
}

async fn list_grants(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let now = now_unix();
    let inner = st.inner.lock().unwrap();
    Ok(Json(json!(inner.grants.iter().filter(|g| g.expires_unix > now).collect::<Vec<_>>())))
}

async fn create_grant(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, Role::is_operator, "create grants")?;
    let req: GrantRequest = parse(&body)?;
    let expires_unix = parse_rfc3339(&req.expires_at).ok_or_else(|| ApiError::invalid("expires_at must be RFC 3339"))?;
    if expires_unix <= now_unix() {
        return Err(ApiError::invalid("expires_at is in the past"));
    }
    let g = Grant { req, grant_id: new_id("grant"), granted_by: c.name.clone(), scans_used: 0, expires_unix };
    st.inner.lock().unwrap().grants.push(g.clone());
    let limit = g.req.max_scans.map(|m| format!("up to {m} scans")).unwrap_or_else(|| "scans".into());
    st.log("approval", format!("Grant: {limit} until {}", g.req.expires_at), &c.name, Some(&g.req.project_id), &[("grant_id", &g.grant_id)]);
    st.wake.notify_one();
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

async fn revoke_grant(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let c = st.require(&h, Role::is_operator, "revoke grants")?;
    let removed = {
        let mut inner = st.inner.lock().unwrap();
        let before = inner.grants.len();
        inner.grants.retain(|g| g.grant_id != id);
        before != inner.grants.len()
    };
    if !removed {
        return Err(ApiError::not_found("grant"));
    }
    st.log("approval", format!("Grant {id} revoked"), &c.name, None, &[("grant_id", &id)]);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogFilter {
    since: Option<String>,
    project_id: Option<String>,
}

async fn get_log(State(st): St, h: HeaderMap, Query(f): Query<LogFilter>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let since = match f.since.as_deref() {
        Some(s) => Some(parse_rfc3339(s).ok_or_else(|| ApiError::invalid("since must be RFC 3339"))?),
        None => None,
    };
    let inner = st.inner.lock().unwrap();
    let v: Vec<_> = inner
        .log
        .iter()
        .filter(|e| since.is_none_or(|s| parse_rfc3339(&e.at).unwrap_or(0) >= s))
        .filter(|e| f.project_id.as_deref().is_none_or(|p| e.project_id.as_deref() == Some(p)))
        .collect();
    Ok(Json(json!(v)))
}

async fn add_log(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "write to the log")?;
    #[derive(Deserialize)]
    struct Req {
        text: String,
        project_id: Option<String>,
    }
    let req: Req = parse(&body)?;
    check_note(&req.text)?;
    let e = st.log("note", req.text, &c.name, req.project_id.as_deref(), &[]);
    Ok((StatusCode::CREATED, Json(e)).into_response())
}

/// A log note's text (shared by `POST /log` and `addLogNote` steps).
pub fn check_note(text: &str) -> ApiResult<()> {
    if text.trim().is_empty() || text.len() > 4000 {
        return Err(ApiError::invalid("text must be 1–4000 characters"));
    }
    Ok(())
}

async fn events(State(st): St, h: HeaderMap) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    st.caller(&h)?;
    let rx = st.events.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((Ok(SseEvent::default().event(ev.kind).data(ev.data.to_string())), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::cache(format!("cache: {e}"))
}

async fn scan_preview(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let p = scan_file(&st, &id, FileKind::Preview).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response())
}

async fn scan_tzf(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let p = scan_file(&st, &id, FileKind::Tzf).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    let disp = format!("attachment; filename=\"{id}.tzf\"");
    Ok(([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response())
}

fn default_margin() -> f64 {
    0.009
}

#[derive(Deserialize)]
pub struct AimBox {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

#[derive(Deserialize)]
pub struct AimReq {
    reference_scan_id: String,
    #[serde(rename = "box")]
    bx: AimBox,
    #[serde(default = "default_margin")]
    margin_rad: f64,
}

async fn aim_from_preview(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let req: AimReq = parse(&body)?;
    Ok(Json(compute_aim(&st, req).await?))
}

/// Box on a preview → area-scan window (shared by direct requests and runs).
pub async fn compute_aim(st: &AppState, req: AimReq) -> ApiResult<Value> {
    if st.rules.refuse_aim_after_move {
        if let (_, Some(s)) = st.snapshot().await {
            if s.instrument_moved_since_last_scan == Some(true) {
                return Err(ApiError::new(StatusCode::CONFLICT, "instrument_moved", "The scanner moved since its last scan")
                    .hint("Take a new overview scan at this station and aim from its preview."));
            }
        }
    }
    let tzf = scan_file(st, &req.reference_scan_id, FileKind::Tzf).await?;
    let jpg = tokio::fs::read(scan_file(st, &req.reference_scan_id, FileKind::Preview).await?).await.map_err(io_err)?;
    let (g, (pw, ph)) = tokio::task::spawn_blocking(move || -> Result<_, String> { Ok((aim::geo(&tzf)?, aim::jpeg_size(&jpg)?)) })
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not read the reference scan").detail(e))?;
    let b = &req.bx;
    let w = aim::window_from_box(&g, pw, ph, [b.x0, b.y0, b.x1, b.y1], req.margin_rad).map_err(ApiError::invalid)?;
    Ok(json!({
        "window": w,
        "preview_size": [pw, ph],
        "cells_per_pixel": [g.cols as f64 / pw as f64, g.rows as f64 / ph as f64],
    }))
}

async fn start_export(State(st): St, h: HeaderMap, Path(scan_id): Path<String>, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "export scans")?;
    let req: export::ExportRequest = parse(&body)?;
    let job = submit_export(&st, &c, scan_id, req, None).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// Start an export under the caller's role (shared by direct requests and runs).
pub async fn submit_export(st: &Arc<AppState>, c: &crate::model::Caller, scan_id: String, req: export::ExportRequest, run_id: Option<&str>) -> ApiResult<Job> {
    let scan = all_scans(st).await?.into_iter().find(|s| s["scan_id"] == scan_id.as_str()).ok_or_else(|| ApiError::not_found("scan"))?;
    let project_id = scan["project_id"].as_str().unwrap_or("").to_string();
    let now = now_rfc3339();
    let job = Job {
        job_id: new_id("job"),
        kind: "export".into(),
        state: JobState::Running,
        phase: Some("downloading".into()),
        progress: None,
        requested_by: c.name.clone(),
        run_id: run_id.map(str::to_string),
        created_at: now.clone(),
        started_at: Some(now),
        finished_at: None,
        result: None,
        problem: None,
        partial_scan_id: None,
    };
    st.inner.lock().unwrap().jobs.push(JobRecord {
        job: job.clone(),
        request: None,
        project_id: project_id.clone(),
        project_name: String::new(),
        scanner_task_id: None,
        cancel_requested: false,
        grant_id: None,
    });
    st.log("export", format!("{} export of scan {scan_id} requested", req.format.ext()), &c.name, Some(&project_id), &[("job_id", &job.job_id), ("scan_id", &scan_id)]);
    st.emit_job(&job);
    tokio::spawn(crate::worker::run_export(st.clone(), job.job_id.clone(), scan_id, req));
    Ok(job)
}

async fn get_export(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.exports.get(&id).cloned().map(Json).ok_or_else(|| ApiError::not_found("export"))
}

async fn get_export_file(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let path = st.inner.lock().unwrap().export_files.get(&id).cloned().ok_or_else(|| ApiError::not_found("export"))?;
    let bytes = tokio::fs::read(&path).await.map_err(io_err)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("export").to_string();
    let disp = format!("attachment; filename=\"{name}\"");
    Ok(([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response())
}

async fn list_recipes(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    Ok(Json(json!(rs
        .iter()
        .map(|r| json!({"recipe_id": r.recipe_id, "title": r.title, "version": r.version, "summary": r.summary}))
        .collect::<Vec<_>>())))
}

async fn get_recipe(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    rs.into_iter().find(|r| r.recipe_id == id).map(|r| Json(serde_json::to_value(r).unwrap_or_default())).ok_or_else(|| ApiError::not_found("recipe"))
}

#[derive(Deserialize)]
struct StartRun {
    recipe_id: String,
    project_id: String,
    #[serde(default)]
    inputs: Value,
}

async fn start_run(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "start runs")?;
    let req: StartRun = parse(&body)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    let recipe = rs.into_iter().find(|r| r.recipe_id == req.recipe_id).ok_or_else(|| ApiError::not_found("recipe"))?;
    if !st.scanner.projects().await?.iter().any(|p| p.project_uuid.as_deref() == Some(req.project_id.as_str())) {
        return Err(ApiError::not_found("project"));
    }
    let run = crate::runs::start(&st, c, recipe, req.project_id, req.inputs)?;
    Ok((StatusCode::CREATED, Json(run)).into_response())
}

async fn list_runs(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut runs: Vec<crate::runs::Run> = st.inner.lock().unwrap().runs.values().map(|r| r.run.clone()).collect();
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.run_id.cmp(&a.run_id)));
    Ok(Json(json!(runs)))
}

async fn get_run(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<crate::runs::Run>> {
    st.caller(&h)?;
    crate::runs::snapshot(&st, &id).map(Json).ok_or_else(|| ApiError::not_found("run"))
}

#[derive(Deserialize)]
struct Advance {
    step_id: String,
    #[serde(default)]
    input: Value,
}

async fn advance_run(State(st): St, h: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<crate::runs::Run>> {
    let c = st.require(&h, |r| r != Role::Viewer, "advance runs")?;
    crate::runs::may_steer(&st, &id, &c, "advance other people's runs")?;
    let a: Advance = parse(&body)?;
    Ok(Json(crate::runs::advance(&st, &id, &a.step_id, a.input)?))
}

async fn cancel_run(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<crate::runs::Run>> {
    let c = st.require(&h, |r| r != Role::Viewer, "cancel runs")?;
    crate::runs::may_steer(&st, &id, &c, "cancel other people's runs")?;
    Ok(Json(crate::runs::cancel(&st, &id)?))
}
