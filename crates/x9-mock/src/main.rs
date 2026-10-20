//! Mock X9 scanner API — serves a subset of `/api/v1` with shapes matching live
//! captures (capture/live/*.json) so x9-client, the bridge and the GUI can be
//! developed offline. Bind defaults to 127.0.0.1:34567 (the device's real port).
//!
//! Stateful: `POST /scans` starts a simulated scan that moves through the
//! phases seen on a real X9 (initializing → running sweep → scan file appears →
//! color imaging if requested → task clears), adds the scan to `/scans`, and can
//! be stopped with `DELETE /tasks/{id}`. As on a real X9, a cancelled task
//! stays listed in `/tasks` for a while (exitResult still "none"), and a scan
//! cancelled during its sweep is still filed, with the points swept so far.
//!
//! Environment:
//! - `X9_MOCK_ADDR` (default `127.0.0.1:34567`)
//! - `X9_MOCK_SPEED` time compression, default 10 (a 54 s scan takes ~5 s)
//! - `X9_MOCK_BATTERY` battery percent, default 87
//! - `X9_MOCK_ON_AC` `1` to report AC power, default 0
//! - `X9_MOCK_FALLBACK_MODE` a scan mode whose requests are echoed (and run)
//!   as a default color full dome, to test clients' echo checks
//! - `X9_MOCK_CANCEL_LINGER` seconds a cancelled task stays listed, default 13
//!   (scaled by the speed factor like every other duration)
//! - `X9_MOCK_FIXTURES` directory with `dome.tzf`, `dome.jpg`, `area.tzf`,
//!   `area.jpg`: served for full-dome and area scans' files and previews

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Mock {
    projects: Vec<Value>,
    scans: Vec<Value>,
    task: Option<Value>,
    /// Set while the current task is cancelled but still listed.
    cancelled: Option<i64>,
    next_task_id: i64,
    cancel_linger: f64,
    battery: u32,
    on_ac: bool,
    speed: f64,
}

type Shared = Arc<Mutex<Mock>>;

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mock = Mock {
        projects: vec![json!({"id": 0, "nextProjectScanID": 2, "projectName": "mock_project",
                              "projectUUID": "39187455-8278-428c-89ad-8a4835f95be6"})],
        scans: vec![json!({"id": 0, "fastMode": false, "scanMode": "STD_L1", "levelingStatus": "ok", "lockedScan": true,
             "scanUUID": "5b5314cf-76b8-4f59-a445-ceee1e6df4c3", "ppm": 0, "tzfId": 0,
             "parentScanUUID": "00000000-0000-0000-0000-000000000000",
             "image": {"colorMode": "OFF", "downloaded": false, "file": "", "fileSize": 0, "id": 0, "nbImages": 0, "whiteBalanceMode": "mode_1"},
             "tzf": {"downloaded": true, "file": "mock_project/1.tzf", "fileSize": 102108747u64, "id": 0, "nbPoints": 24844238u64}})],
        task: None,
        cancelled: None,
        next_task_id: 0,
        cancel_linger: env("X9_MOCK_CANCEL_LINGER", "13").parse().unwrap_or(13.0),
        battery: env("X9_MOCK_BATTERY", "87").parse().unwrap_or(87),
        on_ac: env("X9_MOCK_ON_AC", "0") == "1",
        speed: env("X9_MOCK_SPEED", "10").parse().unwrap_or(10.0),
    };
    let shared: Shared = Arc::new(Mutex::new(mock));

    let app = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/scans", get(scans).post(launch_scan))
        .route("/api/v1/scans/modes", get(scan_modes))
        .route("/api/v1/scans/:id", get(scan).delete(delete_scan))
        .route("/api/v1/projects", get(projects).post(create_project))
        .route("/api/v1/tasks", get(tasks))
        .route("/api/v1/tzfs/:id", get(tzf))
        .route("/api/v1/tzfs/:id/file", get(tzf_file))
        .route("/api/v1/tzfs/:id/preview", get(tzf_preview))
        .route("/api/v1/tasks/:id", delete(cancel_task))
        .route("/api/v1/errors", get(errors))
        .route("/api/v1/diagnostics", get(|| async { Json(json!([])) }))
        .with_state(shared);

    let addr = env("X9_MOCK_ADDR", "127.0.0.1:34567");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    eprintln!("x9-mock scanner API listening on http://{addr}/api/v1");
    axum::serve(listener, app).await.unwrap();
}

async fn status(State(m): State<Shared>) -> Json<Value> {
    let m = m.lock().unwrap();
    let task_id = m.task.as_ref().and_then(|t| t["id"].as_i64()).unwrap_or(-1);
    Json(json!({
        "batteryLevel": m.battery,
        "currentParentUUID": "00000000-0000-0000-0000-000000000000",
        "currentTaskID": task_id,
        "errorList": [{"description": "", "errorLevel": "Information", "type": "NoError"}],
        "imuData": {"confidence": 1, "firmwareVersion": "", "orientation": 0, "timestamp": 0},
        "instrumentMovedSinceLastScan": false,
        "laserPointerState": {"laserEnabled": false},
        "leveling": {"compensatorStatus": "on", "levelingStatus": "ok", "sighting": 0.0155, "trunnion": 0.0230},
        "pluggedToAC": m.on_ac,
        "storage": {"availableSize": 24356585472u64, "fileSystem": "FAT32", "isPresent": true, "isReadOnly": false, "totalSize": 31902400512u64},
        "wifiConfiguration": {"SSID": "TRIMBLE-X9-MOCK", "securityMode": "wpa2_psk", "regionCode": "NL", "wifiBand": "both"}
    }))
}

async fn projects(State(m): State<Shared>) -> Json<Value> {
    Json(Value::Array(m.lock().unwrap().projects.clone()))
}

async fn create_project(State(m): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut m = m.lock().unwrap();
    let p = json!({"id": m.projects.len(), "nextProjectScanID": 1,
                   "projectName": body["projectName"], "projectUUID": body["projectUUID"]});
    m.projects.push(p.clone());
    (StatusCode::OK, Json(p))
}

async fn scans(State(m): State<Shared>) -> Json<Value> {
    Json(Value::Array(m.lock().unwrap().scans.clone()))
}

/// GET /scans/{id}: one scan by its (positional) id.
async fn scan(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Json<Value>, StatusCode> {
    let g = m.lock().unwrap();
    g.scans.iter().find(|s| s["id"].as_i64() == Some(id)).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// GET /tzfs/{id}: a scan file's metadata (the `tzf` object `/scans` embeds).
async fn tzf(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Json<Value>, StatusCode> {
    let g = m.lock().unwrap();
    g.scans.iter().find(|s| s["tzfId"].as_i64() == Some(id)).map(|s| Json(s["tzf"].clone())).ok_or(StatusCode::NOT_FOUND)
}

async fn scan_modes() -> Json<Value> {
    Json(json!([
        {"scanMode": "STD_L0", "range": 30, "nbPoints": 6799768, "fileSize": 36700160, "scanTime": 50,
         "spacing": 0.0015079644737231006, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 17, "constantTime": 38}},
        {"scanMode": "STD_L1", "range": 150, "nbPoints": 27199074, "fileSize": 104857600, "scanTime": 123,
         "spacing": 0.000_753_982_236_861_550_3, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 69, "constantTime": 40}},
        {"scanMode": "STD_L2", "range": 150, "nbPoints": 61197916, "fileSize": 225443840, "scanTime": 213,
         "spacing": 0.000_502_654_824_574_366_9, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 156, "constantTime": 42}},
        {"scanMode": "HS_L1", "range": 150, "nbPoints": 12088380, "fileSize": 41943040, "scanTime": 147,
         "spacing": 0.0011309778792038423, "edmFrequency": 166666, "areaScanTime": {"acquisitionTime": 51, "constantTime": 42}}
    ]))
}

async fn tasks(State(m): State<Shared>) -> Json<Value> {
    Json(match &m.lock().unwrap().task {
        Some(t) => json!([t]),
        None => json!([]),
    })
}

async fn cancel_task(State(m): State<Shared>, Path(id): Path<i64>) -> StatusCode {
    let linger = {
        let mut g = m.lock().unwrap();
        if g.task.as_ref().and_then(|t| t["id"].as_i64()) != Some(id) {
            return StatusCode::NOT_FOUND;
        }
        if g.cancelled == Some(id) {
            return StatusCode::OK;
        }
        g.cancelled = Some(id);
        g.cancel_linger / g.speed
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(linger)).await;
        let mut g = m.lock().unwrap();
        let Some(t) = g.task.clone().filter(|t| t["id"].as_i64() == Some(id)) else { return };
        // Cancelled mid-sweep: the scanner files what it swept so far.
        if t["state"] == "running" && t["file"] == "" {
            let full = full_points(&t["scanRequest"]) as f64;
            let frac = t["percentage"].as_f64().unwrap_or(0.0).max(0.01);
            file_scan(&mut g, &t["scanRequest"], (full * frac) as u64);
        }
        g.task = None;
        g.cancelled = None;
    });
    StatusCode::OK
}

fn full_points(req: &Value) -> u64 {
    if req["scanType"] == "AreaScan" { 777_580 } else { 6_281_859 }
}

/// Add a scan to `/scans` the way the device names it; returns (file, scan id).
fn file_scan(g: &mut Mock, req: &Value, points: u64) -> (String, usize) {
    let area = req["scanType"] == "AreaScan";
    let color = req["colorMode"] == "ON";
    let project_name = req["projectName"].as_str().unwrap_or("mock_project").to_string();
    let proj_idx = g.projects.iter().position(|p| p["projectName"] == project_name.as_str());
    let station = proj_idx.map(|i| g.projects[i]["nextProjectScanID"].as_i64().unwrap_or(1)).unwrap_or(1);
    let file = if area {
        let parent = (station - 1).max(1);
        let k = g.scans.iter()
            .filter(|s| s["tzf"]["file"].as_str().unwrap_or("").starts_with(&format!("{project_name}/{parent}_")))
            .count() + 1;
        format!("{project_name}/{parent}_{k}.tzf")
    } else {
        if let Some(i) = proj_idx {
            g.projects[i]["nextProjectScanID"] = json!(station + 1);
        }
        format!("{project_name}/{station}.tzf")
    };
    let n = g.scans.len();
    let mode = req["scanMode"].as_str().unwrap_or("STD_L0").to_string();
    let short = file.rsplit('/').next().unwrap_or("").to_string();
    g.scans.push(json!({"id": n, "fastMode": !area, "scanMode": mode, "levelingStatus": "ok", "lockedScan": false,
        "scanUUID": uuid::Uuid::new_v4().to_string(), "ppm": 0, "tzfId": n,
        "parentScanUUID": "00000000-0000-0000-0000-000000000000",
        "image": {"colorMode": if color { "ON" } else { "OFF" }, "downloaded": false, "file": "", "fileSize": 0, "id": n, "nbImages": if color { 30 } else { 0 }, "whiteBalanceMode": "mode_1"},
        "tzf": {"downloaded": false, "file": file, "fileSize": points * 9, "id": n, "nbPoints": points}}));
    (short, n)
}

async fn delete_scan(State(m): State<Shared>, Path(id): Path<i64>) -> StatusCode {
    let mut m = m.lock().unwrap();
    let Some(pos) = m.scans.iter().position(|s| s["id"].as_i64() == Some(id)) else {
        return StatusCode::NOT_FOUND;
    };
    m.scans.remove(pos);
    // Ids are positional on the real device: renumber.
    for (i, s) in m.scans.iter_mut().enumerate() {
        s["id"] = json!(i);
        s["tzfId"] = json!(i);
    }
    StatusCode::OK
}

async fn launch_scan(State(m): State<Shared>, Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    let task = {
        let mut g = m.lock().unwrap();
        if g.task.is_some() {
            return (StatusCode::CONFLICT, Json(json!({"error": "taskManager is busy"})));
        }
        let id = g.next_task_id;
        g.next_task_id += 1;
        let fallback = std::env::var("X9_MOCK_FALLBACK_MODE").ok().filter(|m| req["scanMode"] == m.as_str());
        let req = if fallback.is_some() {
            json!({"scanMode": "STD_L0", "scanType": "FullDome", "colorMode": "ON", "fastMode": false,
                   "projectName": req["projectName"], "projectUUID": req["projectUUID"],
                   "startHA": -1, "deltaHA": 0, "startVA": -1, "deltaVA": 0})
        } else {
            req
        };
        let t = json!({"id": id, "type": "scan", "state": "initializing", "percentage": 0.0,
                       "exitResult": "none", "file": "", "scanID": 0,
                       "taskUUID": uuid::Uuid::new_v4().to_string(), "scanRequest": req});
        g.task = Some(t.clone());
        t
    };
    tokio::spawn(simulate(m.clone(), task.clone()));
    (StatusCode::ACCEPTED, Json(task))
}

/// Walk one scan through its phases. Real-time durations (STD_L0, already
/// level) divided by the speed factor: leveling 18 s, sweep 22 s (area 8 s),
/// file ready 6 s later, color 150 s.
async fn simulate(m: Shared, task: Value) {
    let id = task["id"].as_i64().unwrap();
    let req = &task["scanRequest"];
    let area = req["scanType"] == "AreaScan";
    let color = req["colorMode"] == "ON";
    let speed = m.lock().unwrap().speed;
    let wait = |s: f64| tokio::time::sleep(Duration::from_secs_f64(s / speed));
    // Returns false once the task was cancelled.
    let set = |f: &dyn Fn(&mut Value)| -> bool {
        let mut g = m.lock().unwrap();
        if g.cancelled == Some(id) {
            return false;
        }
        match g.task.as_mut() {
            Some(t) if t["id"].as_i64() == Some(id) => {
                f(t);
                true
            }
            _ => false,
        }
    };

    wait(18.0).await;
    if !set(&|t| t["state"] = json!("running")) {
        return;
    }
    let sweep = if area { 8.0 } else { 22.0 };
    for i in 1..=10 {
        wait(sweep / 10.0).await;
        if !set(&|t| t["percentage"] = json!(i as f64 / 10.0)) {
            return;
        }
    }
    wait(6.0).await;
    {
        let mut g = m.lock().unwrap();
        if g.task.as_ref().and_then(|t| t["id"].as_i64()) != Some(id) || g.cancelled == Some(id) {
            return;
        }
        let (short, n) = file_scan(&mut g, req, full_points(req));
        if let Some(t) = g.task.as_mut() {
            t["file"] = json!(short);
            t["scanID"] = json!(n);
        }
    }
    if color {
        for i in 0..=30 {
            if !set(&|t| t["percentage"] = json!(i as f64 / 30.0)) {
                return;
            }
            wait(5.0).await;
        }
    }
    wait(2.0).await;
    let mut g = m.lock().unwrap();
    if g.task.as_ref().and_then(|t| t["id"].as_i64()) == Some(id) && g.cancelled != Some(id) {
        g.task = None;
    }
}

/// Fixture for a scan: `area` if its file name has a `_` (an area scan), else `dome`.
async fn fixture(m: &Shared, id: i64, ext: &str) -> Result<Vec<u8>, StatusCode> {
    let dir = std::env::var("X9_MOCK_FIXTURES").map_err(|_| StatusCode::NOT_FOUND)?;
    let file = {
        let g = m.lock().unwrap();
        let s = g.scans.iter().find(|s| s["tzfId"].as_i64() == Some(id)).ok_or(StatusCode::NOT_FOUND)?;
        s["tzf"]["file"].as_str().unwrap_or("").to_string()
    };
    let base = file.rsplit('/').next().unwrap_or("");
    let kind = if base.contains('_') { "area" } else { "dome" };
    tokio::fs::read(format!("{dir}/{kind}.{ext}")).await.map_err(|_| StatusCode::NOT_FOUND)
}

async fn tzf_file(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Vec<u8>, StatusCode> {
    fixture(&m, id, "tzf").await
}

async fn tzf_preview(State(m): State<Shared>, Path(id): Path<i64>) -> Result<([(axum::http::HeaderName, &'static str); 1], Vec<u8>), StatusCode> {
    Ok(([(axum::http::header::CONTENT_TYPE, "image/jpeg")], fixture(&m, id, "jpg").await?))
}

async fn errors() -> Json<Value> {
    Json(json!([{"description": "", "errorLevel": "Information", "type": "NoError"}]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock(task: Value) -> Shared {
        Arc::new(Mutex::new(Mock {
            projects: vec![json!({"id": 0, "nextProjectScanID": 1, "projectName": "p", "projectUUID": "u"})],
            scans: vec![],
            task: Some(task),
            cancelled: None,
            next_task_id: 1,
            cancel_linger: 13.0,
            battery: 87,
            on_ac: false,
            speed: 100.0,
        }))
    }

    fn task(state: &str, pct: f64) -> Value {
        json!({"id": 0, "type": "scan", "state": state, "percentage": pct, "exitResult": "none", "file": "",
               "scanRequest": {"scanMode": "HS_L1", "scanType": "FullDome", "colorMode": "OFF", "projectName": "p"}})
    }

    #[tokio::test]
    async fn cancelled_sweep_lingers_then_files_a_partial_scan() {
        let m = mock(task("running", 0.5));
        assert_eq!(cancel_task(State(m.clone()), Path(0)).await, StatusCode::OK);
        // Still listed right after the cancel, and a new scan is refused.
        assert!(m.lock().unwrap().task.is_some());
        let (code, _) = launch_scan(State(m.clone()), Json(json!({"scanMode": "STD_L0"}))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let g = m.lock().unwrap();
        assert!(g.task.is_none());
        assert_eq!(g.scans.len(), 1);
        assert_eq!(g.scans[0]["tzf"]["file"], "p/1.tzf");
        assert_eq!(g.scans[0]["tzf"]["nbPoints"], 6_281_859 / 2);
    }

    #[tokio::test]
    async fn scan_and_tzf_items_follow_the_positional_ids() {
        let m = mock(task("running", 0.5));
        {
            let mut g = m.lock().unwrap();
            g.task = None;
            let req = json!({"scanMode": "STD_L0", "scanType": "FullDome", "projectName": "p"});
            file_scan(&mut g, &req, 10);
            file_scan(&mut g, &req, 20);
        }
        let Json(s) = scan(State(m.clone()), Path(1)).await.unwrap();
        assert_eq!(s["tzf"]["file"], "p/2.tzf");
        let Json(t) = tzf(State(m.clone()), Path(1)).await.unwrap();
        assert_eq!((t["id"].as_i64(), t["nbPoints"].as_u64()), (Some(1), Some(20)));
        assert_eq!(scan(State(m.clone()), Path(2)).await.unwrap_err(), StatusCode::NOT_FOUND);
        assert_eq!(tzf(State(m.clone()), Path(2)).await.unwrap_err(), StatusCode::NOT_FOUND);
        // After a delete the survivor is renumbered to 0.
        assert_eq!(delete_scan(State(m.clone()), Path(0)).await, StatusCode::OK);
        let Json(s) = scan(State(m.clone()), Path(0)).await.unwrap();
        assert_eq!(s["tzf"]["file"], "p/2.tzf");
        assert_eq!(tzf(State(m.clone()), Path(1)).await.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_while_leveling_files_nothing() {
        let m = mock(task("initializing", 0.0));
        assert_eq!(cancel_task(State(m.clone()), Path(0)).await, StatusCode::OK);
        assert_eq!(cancel_task(State(m.clone()), Path(9)).await, StatusCode::NOT_FOUND);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let g = m.lock().unwrap();
        assert!(g.task.is_none() && g.scans.is_empty());
    }
}
