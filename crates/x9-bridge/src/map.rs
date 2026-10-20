//! Translate the scanner's HTTP API shapes into the bridge API's.

use crate::state::ScanMeta;
use serde_json::{json, Value};
use std::collections::HashMap;
use x9_client::models;

pub fn status(st: Option<&models::Status>, busy_job: Option<&str>, queue_len: usize, version: &str, started_at: &str) -> Value {
    let scanner = match st {
        None => json!({"connected": false}),
        Some(s) => {
            let lev = s.leveling.as_ref();
            let state = match lev.and_then(|l| l.leveling_status.as_deref()) {
                Some("ok") => "ok",
                Some("none") | None => "none",
                Some("error") => "error",
                Some(_) => "in_progress",
            };
            json!({
                "connected": true,
                "busy": s.current_task_id.unwrap_or(-1) >= 0,
                "current_job_id": busy_job,
                "battery_pct": s.battery_level,
                "on_ac_power": s.plugged_to_ac.unwrap_or(false),
                "storage_free_bytes": s.storage.as_ref().and_then(|x| x.available_size),
                "moved_since_last_scan": s.instrument_moved_since_last_scan.unwrap_or(false),
                "leveling": {
                    "state": state,
                    "compensator_on": lev.and_then(|l| l.compensator_status.as_deref()) == Some("on"),
                    "tilt_rad": [lev.and_then(|l| l.sighting).unwrap_or(0.0), lev.and_then(|l| l.trunnion).unwrap_or(0.0)],
                }
            })
        }
    };
    json!({
        "bridge": {"version": version, "started_at": started_at, "queue_length": queue_len, "worker_ready": false},
        "scanner": scanner,
    })
}

pub fn scan_mode(m: &models::ScanMode) -> Value {
    json!({
        "mode": m.scan_mode,
        "range_m": m.range,
        "angular_step_rad": m.spacing,
        "points_full_dome": m.nb_points,
        "fixed_seconds": m.area_scan_time.as_ref().and_then(|a| a.constant_time),
        "full_dome_seconds": m.scan_time,
    })
}

pub fn project(p: &models::Project) -> Value {
    json!({
        "project_id": p.project_uuid,
        "name": p.project_name,
        "scan_count": p.next_project_scan_id.map(|n| (n - 1).max(0)),
    })
}

/// `project/3_2.tzf` → (`project`, station 3, Some(2) for an area scan).
pub fn parse_file(file: &str) -> Option<(String, i64, Option<i64>)> {
    let (proj, base) = file.rsplit_once('/')?;
    let stem = base.strip_suffix(".tzf")?;
    match stem.split_once('_') {
        Some((st, k)) => Some((proj.to_string(), st.parse().ok()?, Some(k.parse().ok()?))),
        None => Some((proj.to_string(), stem.parse().ok()?, None)),
    }
}

/// One scanner scan as an API `Scan`. `all` resolves area scans' parents and
/// `projects` turns project names into ids.
pub fn scan(s: &models::Scan, all: &[models::Scan], projects: &[models::Project], meta: &HashMap<String, ScanMeta>) -> Option<Value> {
    let uuid = s.scan_uuid.clone()?;
    let file = s.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
    let (proj_name, station, area_k) = parse_file(&file).unwrap_or_default();
    let project_id = projects
        .iter()
        .find(|p| p.project_name.as_deref() == Some(proj_name.as_str()))
        .and_then(|p| p.project_uuid.clone())
        .unwrap_or(proj_name.clone());
    let parent = area_k.and_then(|_| {
        let want = format!("{proj_name}/{station}.tzf");
        all.iter()
            .find(|o| o.tzf.as_ref().and_then(|t| t.file.as_deref()) == Some(want.as_str()))
            .and_then(|o| o.scan_uuid.clone())
    });
    let m = meta.get(&uuid);
    let mut v = json!({
        "scan_id": uuid,
        "project_id": project_id,
        "parent_scan_id": parent,
        "station": station,
        "kind": if area_k.is_some() { "area" } else { "full_dome" },
        "mode": s.scan_mode,
        "color": s.image.as_ref().and_then(|i| i.color_mode.as_deref()) == Some("ON"),
        "created_at": m.map(|m| m.created_at.clone()),
        "point_count": s.tzf.as_ref().and_then(|t| t.nb_points),
        "tzf_bytes": s.tzf.as_ref().and_then(|t| t.file_size),
    });
    if let Some(m) = m {
        v["timings"] = serde_json::to_value(&m.timings).unwrap_or_default();
        if let Some(w) = m.window {
            v["window"] = serde_json::to_value(w).unwrap_or_default();
        }
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files() {
        assert_eq!(parse_file("2026-10-19_live-probe/3_2.tzf"), Some(("2026-10-19_live-probe".into(), 3, Some(2))));
        assert_eq!(parse_file("p/1.tzf"), Some(("p".into(), 1, None)));
        assert_eq!(parse_file("nonsense"), None);
    }
}
