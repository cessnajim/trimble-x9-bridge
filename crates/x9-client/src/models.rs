//! On-wire JSON models for the X9 scanner API REST API.
//!
//! GROUND TRUTH: these match live responses captured 2026-10-19 from a real
//! Trimble X9 over USB-CDC-EEM at
//! http://192.168.42.1:34567/api/v1 — see capture/live/*.json.
//! Every struct keeps `#[serde(default)]` and a flattened `extra` so firmware
//! variations never break deserialization.

use serde::{Deserialize, Serialize};

/// GET /status — aggregates leveling, storage, imu, laser, wifi (these are NOT
/// separate routes on the device; they are nested here).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
    pub battery_level: Option<u32>,
    #[serde(rename = "currentParentUUID")]
    pub current_parent_uuid: Option<String>,
    #[serde(rename = "currentTaskID")]
    pub current_task_id: Option<i64>,
    pub error_list: Vec<ErrorItem>,
    pub imu_data: Option<ImuData>,
    pub instrument_moved_since_last_scan: Option<bool>,
    pub laser_pointer_state: Option<LaserPointerState>,
    pub leveling: Option<Leveling>,
    #[serde(rename = "pluggedToAC")]
    pub plugged_to_ac: Option<bool>,
    pub storage: Option<Storage>,
    pub wifi_configuration: Option<WifiConfiguration>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ErrorItem {
    pub description: Option<String>,
    pub error_level: Option<String>, // "Information" | ...
    #[serde(rename = "type")]
    pub kind: Option<String>, // "NoError" | ...
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ImuData {
    pub confidence: Option<f64>,
    pub delta_altitude: Option<f64>,
    pub firmware_version: Option<String>,
    #[serde(rename = "linkedTZF")]
    pub linked_tzf: Option<String>,
    pub move_x: Option<f64>,
    pub move_y: Option<f64>,
    pub orientation: Option<f64>,
    pub step_stride_offset: Option<f64>,
    pub step_stride_slope: Option<f64>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LaserPointerState {
    pub laser_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Leveling {
    pub compensator_status: Option<String>, // "off" | "on"
    pub leveling_status: Option<String>,     // "none" | "ok" | ...
    pub sighting: Option<f64>,
    pub trunnion: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Storage {
    pub available_size: Option<u64>,
    pub file_system: Option<String>,
    pub is_present: Option<bool>,
    pub is_read_only: Option<bool>,
    pub total_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WifiConfiguration {
    #[serde(rename = "SSID")]
    pub ssid: Option<String>,
    pub channel: Option<u32>,
    pub current_channel: Option<u32>,
    pub indoor_only: Option<bool>,
    pub is_factory_key: Option<bool>,
    pub key: Option<String>, // OpenSSL-salted, not plaintext
    pub region_code: Option<String>,
    pub security_mode: Option<String>,
    #[serde(rename = "useDFS600s")]
    pub use_dfs600s: Option<bool>,
    #[serde(rename = "useDFS60s")]
    pub use_dfs60s: Option<bool>,
    pub wifi_band: Option<String>,
}

/// GET /projects — returns a bare JSON array of these.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Project {
    pub id: Option<i64>,
    #[serde(rename = "nextProjectScanID")]
    pub next_project_scan_id: Option<i64>,
    pub project_name: Option<String>,
    #[serde(rename = "projectUUID")]
    pub project_uuid: Option<String>,
}

/// GET /scans — returns a bare JSON array of these.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Scan {
    pub id: Option<i64>,
    pub fast_mode: Option<bool>,
    pub image: Option<ScanImage>,
    pub image_id: Option<i64>,
    pub leveling_status: Option<String>,
    pub locked_scan: Option<bool>,
    #[serde(rename = "parentScanUUID")]
    pub parent_scan_uuid: Option<String>,
    pub ppm: Option<f64>,
    pub scan_mode: Option<String>, // e.g. "STD_L1"
    #[serde(rename = "scanUUID")]
    pub scan_uuid: Option<String>,
    pub tzf: Option<Tzf>,
    pub tzf_id: Option<i64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanImage {
    pub color_mode: Option<String>, // "OFF" | ...
    pub downloaded: Option<bool>,
    pub file: Option<String>,
    pub file_size: Option<u64>,
    pub id: Option<i64>,
    pub nb_images: Option<u32>,
    pub white_balance_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Tzf {
    pub downloaded: Option<bool>,
    pub file: Option<String>,   // e.g. "2024-05-16_.../1.tzf"
    pub file_size: Option<u64>,
    pub id: Option<i64>,
    pub nb_points: Option<u64>,
}

/// A task object: returned by POST /scans (launchScan, HTTP 202) and listed by
/// GET /tasks (bare array) while running. Confirmed live 2026-10-19 from a real
/// STD_L0 scan (state idle->running, percentage 0->1, then the task clears).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Task {
    pub id: Option<i64>,
    #[serde(rename = "type")]
    pub kind: Option<String>, // "scan"
    pub state: Option<String>, // "idle" | "running"
    pub percentage: Option<f64>, // 0.0 .. 1.0
    pub exit_result: Option<String>, // "none" | ...
    pub file: Option<String>, // e.g. "20.tzf"
    #[serde(rename = "scanID")]
    pub scan_id: Option<i64>,
    #[serde(rename = "taskUUID")]
    pub task_uuid: Option<String>,
    /// The full scan request the device actually used (it fills many defaults:
    /// autoLeveling, levelingMethod, scanType "FullDome", colorMode, etc.).
    pub scan_request: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// GET /scans/modes — one entry per available scan mode (bare array).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanMode {
    pub scan_mode: Option<String>, // "STD_L0" | "HS_L1" | "HSP_L3" | ...
    pub range: Option<f64>,        // metres
    pub nb_points: Option<u64>,
    pub file_size: Option<u64>,
    pub scan_time: Option<f64>,    // seconds
    pub spacing: Option<f64>,
    pub edm_frequency: Option<u64>,
    pub area_scan_time: Option<AreaScanTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AreaScanTime {
    pub acquisition_time: Option<f64>,
    pub constant_time: Option<f64>,
}

/// Body for launchScan (POST). PROVISIONAL — actuating, not yet exercised.
/// Scan modes seen in real data: "STD_L1" (and family). Confirm before use.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanRequest {
    /// REQUIRED together with project_uuid to target a project; if omitted the
    /// device scans into its last/current project (confirmed 2026-10-19).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(rename = "projectUUID", skip_serializing_if = "Option::is_none")]
    pub project_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_mode: Option<String>, // "OFF" | ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indoor_only: Option<bool>,
    /// "FullDome" (default on the device) or "AreaScan". Confirmed live 2026-10-19.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_type: Option<String>,
    /// Area-scan window, radians; VA from the zenith. Confirmed live 2026-10-19.
    #[serde(rename = "startHA", skip_serializing_if = "Option::is_none")]
    pub start_ha: Option<f64>,
    #[serde(rename = "deltaHA", skip_serializing_if = "Option::is_none")]
    pub delta_ha: Option<f64>,
    #[serde(rename = "startVA", skip_serializing_if = "Option::is_none")]
    pub start_va: Option<f64>,
    #[serde(rename = "deltaVA", skip_serializing_if = "Option::is_none")]
    pub delta_va: Option<f64>,
}
