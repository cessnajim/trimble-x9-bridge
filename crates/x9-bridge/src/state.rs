//! Shared state: tokens, rules, jobs, grants, the field log and the event bus.
//! Everything is in memory for now; it is rebuilt from the scanner on restart.

use crate::model::{Caller, Grant, Job, LogEntry, Role, Rules, ScanRequest, Timings, Window};
use crate::problem::ApiError;
use crate::rules::Snapshot;
use crate::util::{new_id, now_rfc3339};
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: &'static str,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub job: Job,
    /// Set for scan jobs.
    pub request: Option<ScanRequest>,
    pub project_id: String,
    pub project_name: String,
    pub scanner_task_id: Option<i64>,
    pub cancel_requested: bool,
}

/// What the bridge remembers about scans it ran (the scanner keeps none of this).
#[derive(Debug, Clone, Default)]
pub struct ScanMeta {
    pub created_at: String,
    pub timings: Timings,
    pub window: Option<Window>,
}

#[derive(Default)]
pub struct Inner {
    pub jobs: Vec<JobRecord>,
    pub grants: Vec<Grant>,
    pub log: Vec<LogEntry>,
    pub scan_meta: HashMap<String, ScanMeta>,
    pub exports: HashMap<String, Value>,
    pub export_files: HashMap<String, std::path::PathBuf>,
    pub runs: HashMap<String, crate::runs::RunRecord>,
}

pub struct AppState {
    pub scanner: x9_client::Client,
    pub tokens: HashMap<String, Caller>,
    pub rules: Rules,
    pub inner: Mutex<Inner>,
    pub events: broadcast::Sender<Event>,
    pub wake: Notify,
    pub started_at: String,
    pub poll: Duration,
    /// Cached scan files, previews and exports.
    pub data_dir: std::path::PathBuf,
    /// Recipe files (`*.toml`), read on each request so new ones need no restart.
    pub recipes_dir: std::path::PathBuf,
}

impl AppState {
    pub fn new(scanner: x9_client::Client, tokens: HashMap<String, Caller>, rules: Rules, poll: Duration, data_dir: std::path::PathBuf, recipes_dir: std::path::PathBuf) -> Self {
        let (events, _) = broadcast::channel(256);
        AppState {
            scanner,
            tokens,
            rules,
            inner: Mutex::new(Inner::default()),
            events,
            wake: Notify::new(),
            started_at: now_rfc3339(),
            poll,
            data_dir,
            recipes_dir,
        }
    }

    /// Who is calling, from `Authorization: Bearer <token>`.
    pub fn caller(&self, headers: &HeaderMap) -> Result<Caller, ApiError> {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        self.tokens.get(token.trim()).cloned().ok_or_else(ApiError::unauthorized)
    }

    pub fn require(&self, headers: &HeaderMap, ok: impl Fn(Role) -> bool, what: &str) -> Result<Caller, ApiError> {
        let c = self.caller(headers)?;
        if ok(c.role) {
            Ok(c)
        } else {
            Err(ApiError::forbidden(what))
        }
    }

    pub fn emit(&self, kind: &'static str, data: Value) {
        let _ = self.events.send(Event { kind, data });
    }

    pub fn emit_job(&self, job: &Job) {
        self.emit("job", serde_json::to_value(job).unwrap_or_default());
    }

    /// Append to the field log and publish it. Must not be called while
    /// holding `inner`.
    pub fn log(&self, kind: &str, text: impl Into<String>, author: &str, project_id: Option<&str>, refs: &[(&str, &str)]) -> LogEntry {
        let entry = LogEntry {
            entry_id: new_id("log"),
            at: now_rfc3339(),
            kind: kind.into(),
            text: text.into(),
            author: author.into(),
            project_id: project_id.map(str::to_string),
            refs: refs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        };
        self.inner.lock().unwrap().log.push(entry.clone());
        self.emit("log", serde_json::to_value(&entry).unwrap_or_default());
        entry
    }

    /// A fresh read of the scanner for the rules. Never fails: an unreachable
    /// scanner is `connected: false`.
    pub async fn snapshot(&self) -> (Snapshot, Option<x9_client::models::Status>) {
        match self.scanner.status().await {
            Ok(st) => (
                Snapshot {
                    connected: true,
                    battery_pct: st.battery_level,
                    on_ac: st.plugged_to_ac.unwrap_or(false),
                    storage_free: st.storage.as_ref().and_then(|s| s.available_size),
                },
                Some(st),
            ),
            Err(_) => (Snapshot::default(), None),
        }
    }
}
