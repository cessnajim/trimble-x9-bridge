//! API types (see `api/bridge-v1.openapi.yaml`). JSON is `snake_case`;
//! lengths in metres, angles in radians, times RFC 3339 UTC.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Agent,
    Operator,
    Admin,
}

impl Role {
    /// Approve jobs, create grants, delete with confirmation.
    pub fn is_operator(self) -> bool {
        matches!(self, Role::Operator | Role::Admin)
    }
}

#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rules {
    pub min_battery_pct_to_scan: u32,
    pub min_storage_free_bytes: u64,
    pub require_approval_for_scans: bool,
    pub refuse_aim_after_move: bool,
    pub max_queue_length: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            min_battery_pct_to_scan: 15,
            min_storage_free_bytes: 2_000_000_000,
            require_approval_for_scans: true,
            refuse_aim_after_move: true,
            max_queue_length: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub start_ha: f64,
    pub delta_ha: f64,
    pub start_va: f64,
    pub delta_va: f64,
}

impl Window {
    pub fn validate(&self) -> Result<(), String> {
        let ok = (0.0..TAU).contains(&self.start_ha)
            && self.delta_ha > 0.0
            && self.delta_ha <= TAU
            && (0.0..=PI).contains(&self.start_va)
            && self.delta_va > 0.0
            && self.start_va + self.delta_va <= PI + 1e-9;
        if ok {
            Ok(())
        } else {
            Err("window out of range: start_ha in [0, 2π), 0 < delta_ha ≤ 2π, start_va in [0, π], 0 < delta_va, start_va + delta_va ≤ π".into())
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    FullDome,
    Area,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub project_id: String,
    pub kind: ScanKind,
    pub mode: String,
    #[serde(default)]
    pub color: bool,
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub note: Option<String>,
}

impl ScanRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode.trim().is_empty() {
            return Err("mode is required".into());
        }
        match (self.kind, &self.window) {
            (ScanKind::Area, None) => Err("an area scan needs a window".into()),
            (ScanKind::Area, Some(w)) => w.validate(),
            (ScanKind::FullDome, Some(_)) => Err("a full-dome scan takes no window".into()),
            (ScanKind::FullDome, None) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    AwaitingApproval,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_final(self) -> bool {
        matches!(self, JobState::Succeeded | JobState::Failed | JobState::Cancelled)
    }

    /// The API spelling, e.g. `awaiting_approval`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::AwaitingApproval => "awaiting_approval",
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub job_id: String,
    pub kind: String,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    pub requested_by: String,
    pub run_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Value>,
    /// Cancelled scan jobs: the scan the scanner filed from the partial sweep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_scan_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRequest {
    pub project_id: String,
    pub expires_at: String,
    #[serde(default)]
    pub max_scans: Option<u32>,
    #[serde(default)]
    pub allowed_modes: Vec<String>,
    #[serde(default)]
    pub allow_color: bool,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    #[serde(flatten)]
    pub req: GrantRequest,
    pub grant_id: String,
    pub granted_by: String,
    pub scans_used: u32,
    #[serde(skip)]
    pub expires_unix: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub entry_id: String,
    pub at: String,
    pub kind: String,
    pub text: String,
    pub author: String,
    pub project_id: Option<String>,
    pub refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leveling_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry_ready_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_s: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::JobState;

    #[test]
    fn job_state_spelling_matches_serde() {
        use JobState::*;
        for s in [AwaitingApproval, Queued, Running, Succeeded, Failed, Cancelled] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }
}
