//! Workers: a faster computer (the field laptop, a desk machine) can do the
//! heavy part of registration for the bridge. A worker polls
//! `POST /v1/work/claim`; when a registration job is waiting, the bridge hands
//! it the stations and the pairs already matched. The worker fetches the scans
//! (`GET /v1/scans/{id}/tzf`), registers and audits them, uploads the merged
//! points (`PUT /v1/work/{job}/merged`) and posts the result
//! (`POST /v1/work/{job}/result`). The bridge stays the record: it stores,
//! logs and re-judges the mission exactly as when it registers itself.
//!
//! The bridge registers on its own when no worker has been seen for 15 s,
//! when none claims the job within 10 s, or when the worker fails or takes
//! longer than 10 minutes. A job never waits on a laptop that went away.

use crate::register::{Audit, Pair, Pose, Wall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// A worker counts as connected this long after its last claim.
pub const WORKER_SEEN: Duration = Duration::from_secs(15);
/// How long a job is offered before the bridge does it itself.
const CLAIM_WAIT: Duration = Duration::from_secs(10);
/// How long a claimed job may take.
const RUN_LIMIT: Duration = Duration::from_secs(600);

/// What a worker sends back. `pairs` cover every pair it matched; `poses`
/// and `audits` follow the offered stations in order, less `skipped`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkResult {
    pub pairs: Vec<Pair>,
    pub poses: Vec<Option<Pose>>,
    pub audits: Vec<Audit>,
    pub walls: Vec<Wall>,
    /// Stations it couldn't decode: (station number, why).
    #[serde(default)]
    pub skipped: Vec<(u16, String)>,
    /// The scan whose frame is the project's (the largest group's first
    /// station); the first station when not given.
    #[serde(default)]
    pub frame: Option<String>,
}

type Delivery = Result<(WorkResult, Vec<u8>), String>;

struct Offer {
    project_id: String,
    stations: Vec<(u16, String)>,
    pairs: Vec<Pair>,
    /// (worker name, the caller who claimed it).
    claimed_by: Option<(String, String)>,
    merged: Option<Vec<u8>>,
    done: Option<oneshot::Sender<Delivery>>,
}

#[derive(Default)]
pub struct WorkState {
    /// Worker name → last claim, and the caller behind it.
    workers: HashMap<String, (Instant, String)>,
    /// Offered registration jobs, by job id.
    offers: HashMap<String, Offer>,
}

pub enum Offered {
    /// The worker's result, the merged points (bytes) and the worker's name.
    Done(WorkResult, Vec<u8>, String),
    NoWorker,
    Failed(String),
    Cancelled,
}

impl WorkState {
    /// Workers seen recently: name, seconds since, the job each holds.
    pub fn workers(&self) -> Vec<Value> {
        let mut v: Vec<Value> = self
            .workers
            .iter()
            .filter(|(_, (t, _))| t.elapsed() < WORKER_SEEN)
            .map(|(name, (t, by))| {
                let job = self.offers.iter().find(|(_, o)| o.claimed_by.as_ref().is_some_and(|c| &c.0 == name)).map(|(j, _)| j.clone());
                json!({"worker": name, "token": by, "seen_s_ago": (t.elapsed().as_secs_f64() * 10.0).round() / 10.0, "job_id": job})
            })
            .collect();
        v.sort_by(|a, b| a["worker"].as_str().cmp(&b["worker"].as_str()));
        v
    }

    /// A worker asks for work: note it, and hand it the oldest unclaimed job.
    pub fn claim(&mut self, worker: &str, caller: &str) -> Option<Value> {
        self.workers.insert(worker.to_string(), (Instant::now(), caller.to_string()));
        let (job_id, o) = self.offers.iter_mut().find(|(_, o)| o.claimed_by.is_none())?;
        o.claimed_by = Some((worker.to_string(), caller.to_string()));
        Some(json!({
            "job_id": job_id,
            "kind": "register",
            "project_id": o.project_id,
            "stations": o.stations.iter().map(|(n, id)| json!({"station": n, "scan_id": id})).collect::<Vec<_>>(),
            "pairs": o.pairs,
        }))
    }

    /// The offer `job_id`, if `caller` claimed it.
    fn mine(&mut self, job_id: &str, caller: &str) -> Result<&mut Offer, String> {
        match self.offers.get_mut(job_id) {
            None => Err("no such job is offered (it finished, was cancelled, or the bridge took it back)".into()),
            Some(o) if o.claimed_by.as_ref().is_none_or(|c| c.1 != caller) => Err("this job was not claimed with your token".into()),
            Some(o) => Ok(o),
        }
    }

    /// Whether `caller` holds the offered job (for progress reports).
    pub fn holds(&mut self, job_id: &str, caller: &str) -> Result<(), String> {
        self.mine(job_id, caller).map(|_| ())
    }

    /// Keep the worker's merged points (already checked) for its result.
    pub fn put_merged(&mut self, job_id: &str, caller: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.mine(job_id, caller)?.merged = Some(bytes);
        Ok(())
    }

    pub fn deliver(&mut self, job_id: &str, caller: &str, r: Result<WorkResult, String>) -> Result<(), String> {
        let o = self.mine(job_id, caller)?;
        let d = match r {
            Ok(res) => {
                let kept = o.stations.len() - res.skipped.len().min(o.stations.len());
                if res.poses.len() != kept || res.audits.len() != kept {
                    return Err(format!("poses and audits must list the {kept} stations offered, less those skipped"));
                }
                Ok((res, o.merged.take().ok_or("upload the merged points (PUT …/merged) before the result")?))
            }
            Err(e) => Err(e),
        };
        if let Some(tx) = o.done.take() {
            let _ = tx.send(d);
        }
        Ok(())
    }
}

/// Offer a registration job to the workers and wait for the result.
/// `cancelled` is polled while waiting; `phase` reports where it stands.
pub async fn offer(
    st: &crate::state::AppState,
    job_id: &str,
    project_id: &str,
    stations: &[(u16, String)],
    pairs: &[Pair],
    cancelled: &(dyn Fn() -> bool + Sync),
    phase: &(dyn Fn(String) + Sync),
) -> Offered {
    let mut rx = {
        let mut w = st.work.lock().unwrap();
        w.workers.retain(|_, (t, _)| t.elapsed() < WORKER_SEEN);
        if w.workers.is_empty() {
            return Offered::NoWorker;
        }
        let (tx, rx) = oneshot::channel();
        w.offers.insert(job_id.to_string(), Offer {
            project_id: project_id.to_string(),
            stations: stations.to_vec(),
            pairs: pairs.to_vec(),
            claimed_by: None,
            merged: None,
            done: Some(tx),
        });
        rx
    };
    phase("waiting for a worker".into());
    let take_back = || st.work.lock().unwrap().offers.remove(job_id);
    let t0 = Instant::now();
    let worker = loop {
        if cancelled() {
            take_back();
            return Offered::Cancelled;
        }
        if let Some((name, _)) = st.work.lock().unwrap().offers.get(job_id).and_then(|o| o.claimed_by.clone()) {
            break name;
        }
        if t0.elapsed() > CLAIM_WAIT {
            take_back();
            return Offered::NoWorker;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    phase(format!("registering on {worker}"));
    let t1 = Instant::now();
    let got = loop {
        if cancelled() {
            take_back();
            return Offered::Cancelled;
        }
        if t1.elapsed() > RUN_LIMIT {
            take_back();
            return Offered::Failed(format!("{worker} took longer than {} minutes", RUN_LIMIT.as_secs() / 60));
        }
        match tokio::time::timeout(Duration::from_millis(250), &mut rx).await {
            Ok(r) => break r,
            Err(_) => continue,
        }
    };
    take_back();
    match got {
        Ok(Ok((res, merged))) => Offered::Done(res, merged, worker),
        Ok(Err(e)) => Offered::Failed(format!("{worker}: {e}")),
        Err(_) => Offered::Failed(format!("{worker} went away")),
    }
}
