//! `x9-bridge worker`: do the bridge's heavy work on this computer (see
//! `work`). Claims registration jobs, fetches the scans over the bridge,
//! decodes, registers and audits them here, and sends the result back.
//! Decoded stations are kept in a cache directory, and prepared stations and
//! pair checks in memory, so each new station costs about one station's work.
//!
//!   x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR]
//!
//! Defaults: the bridge over the laptop cable (http://10.55.0.1:8795), the
//! token from `X9_WORKER_TOKEN` or ~/.config/x9-worker/token (an agent or
//! operator token), this computer's name, and ~/.cache/x9-worker.

use crate::register::{audit, prepare, register_all, walls, AuditCache, Cloud, Merged, Pair, Prepared};
use crate::work::WorkResult;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Worker {
    http: reqwest::Client,
    base: String,
    token: String,
    name: String,
    cache: PathBuf,
    prepared: HashMap<String, Arc<Prepared>>,
    checks: AuditCache,
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mut base = "http://10.55.0.1:8795".to_string();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    let mut token_file = PathBuf::from(&home).join(".config/x9-worker/token");
    let mut name = hostname();
    let mut cache = PathBuf::from(&home).join(".cache/x9-worker");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--bridge" => base = val()?.trim_end_matches('/').to_string(),
            "--token-file" => token_file = PathBuf::from(val()?),
            "--name" => name = val()?.clone(),
            "--cache" => cache = PathBuf::from(val()?),
            _ => bail!("usage: x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR]"),
        }
    }
    let token = match std::env::var("X9_WORKER_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => std::fs::read_to_string(&token_file).with_context(|| format!("reading the token from {}", token_file.display()))?.trim().to_string(),
    };
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build()?;
    let mut w = Worker { http, base, token, name, cache, prepared: HashMap::new(), checks: AuditCache::new() };
    eprintln!("x9-bridge worker {}: {} as {}, cache {}", crate::routes::VERSION, w.base, w.name, w.cache.display());
    let mut said_down = false;
    loop {
        match w.claim().await {
            Ok(Some(job)) => {
                said_down = false;
                let id = job["job_id"].as_str().unwrap_or("").to_string();
                let t0 = std::time::Instant::now();
                match w.register(&job).await {
                    Ok(summary) => eprintln!("{id}: done in {:.1} s: {summary}", t0.elapsed().as_secs_f64()),
                    Err(e) => {
                        eprintln!("{id}: failed: {e:#}");
                        let _ = w.post(&format!("/v1/work/{id}/fail"), &json!({"error": format!("{e:#}")})).await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(e) => {
                if !said_down {
                    eprintln!("bridge not reachable ({e:#}); retrying");
                    said_down = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "worker".into())
}

impl Worker {
    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<reqwest::Response> {
        let r = self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(body).send().await?;
        if !r.status().is_success() {
            bail!("{path}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(r)
    }

    async fn claim(&self) -> anyhow::Result<Option<Value>> {
        let r = self.post("/v1/work/claim", &json!({"worker": self.name})).await?;
        if r.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(r.json().await?))
    }

    async fn progress(&self, job: &str, phase: &str, progress: Option<f64>) {
        let _ = self.post(&format!("/v1/work/{job}/progress"), &json!({"phase": phase, "progress": progress})).await;
    }

    /// The station's 5 cm cloud: from the cache, or fetched and decoded.
    async fn cloud(&self, scan_id: &str) -> anyhow::Result<Cloud> {
        let path = self.cache.join(crate::worker::cloud_name(scan_id));
        if let Ok(b) = tokio::fs::read(&path).await {
            if let Ok(c) = Cloud::from_bytes(&b) {
                return Ok(c);
            }
        }
        let r = self.http.get(format!("{}/v1/scans/{scan_id}/tzf", self.base)).bearer_auth(&self.token).timeout(Duration::from_secs(600)).send().await?;
        if !r.status().is_success() {
            bail!("fetching the scan: {}", r.status());
        }
        let tzf = self.cache.join(format!("{scan_id}.tzf.part"));
        tokio::fs::write(&tzf, r.bytes().await?).await?;
        let t = tzf.clone();
        let c = tokio::task::spawn_blocking(move || -> anyhow::Result<Cloud> {
            let f = trimble_tzf::Tzf::open(&t)?;
            let meta = f.scan_meta()?;
            let pts = f.grid_points()?;
            Ok(Cloud::from_grid(&pts, crate::view::step_rad(&meta)))
        })
        .await??;
        let _ = tokio::fs::remove_file(&tzf).await;
        tokio::fs::write(&path, c.to_bytes()).await?;
        Ok(c)
    }

    async fn register(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let stations: Vec<(u16, String)> = job["stations"]
            .as_array()
            .context("stations")?
            .iter()
            .filter_map(|s| Some((s["station"].as_u64()? as u16, s["scan_id"].as_str()?.to_string())))
            .collect();
        let pairs0: Vec<Pair> = serde_json::from_value(job["pairs"].clone()).unwrap_or_default();
        let (mut kept, mut skipped) = (Vec::new(), Vec::new());
        for (k, (n, scan)) in stations.iter().enumerate() {
            if self.prepared.contains_key(scan) {
                kept.push((*n, scan.clone()));
                continue;
            }
            self.progress(&id, &format!("decoding station {n} on {}", self.name), Some(k as f64 / stations.len().max(1) as f64 * 0.4)).await;
            match self.cloud(scan).await {
                Ok(c) => {
                    let p = tokio::task::spawn_blocking(move || prepare(&c)).await?;
                    self.prepared.insert(scan.clone(), Arc::new(p));
                    kept.push((*n, scan.clone()));
                }
                Err(e) => skipped.push((*n, format!("{e:#}"))),
            }
        }
        self.progress(&id, &format!("matching on {}", self.name), Some(0.5)).await;
        let ids: Vec<String> = kept.iter().map(|s| s.1.clone()).collect();
        let numbers: Vec<u16> = kept.iter().map(|s| s.0).collect();
        let prep: Vec<Arc<Prepared>> = ids.iter().map(|i| self.prepared[i].clone()).collect();
        let mut checks = std::mem::take(&mut self.checks);
        let (ids2, numbers2) = (ids.clone(), numbers.clone());
        let (result, merged, checks) = tokio::task::spawn_blocking(move || {
            let refs: Vec<&Prepared> = prep.iter().map(|p| p.as_ref()).collect();
            let (pairs, groups) = register_all(&ids2, &refs, pairs0, &|_, _| {});
            let poses = groups.poses();
            let frame = ids2.get(groups.frame()).cloned();
            let names: Vec<String> = numbers2.iter().map(u16::to_string).collect();
            let audits = audit(&ids2, &names, &refs, &poses, &pairs, &mut checks);
            let mut merged = Merged::default();
            for (k, p) in poses.iter().enumerate() {
                if let Some(p) = p {
                    merged.add(refs[k], p, numbers2[k]);
                }
            }
            let ws = walls(&merged);
            (WorkResult { pairs, poses, audits, walls: ws, skipped: vec![], frame }, merged.to_bytes(), checks)
        })
        .await?;
        self.checks = checks;
        let result = WorkResult { skipped, ..result };
        let r = self.http.put(format!("{}/v1/work/{id}/merged", self.base)).bearer_auth(&self.token).body(merged).send().await?;
        if !r.status().is_success() {
            bail!("uploading merged points: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        let placed = result.poses.iter().flatten().count();
        let trusted = result.audits.iter().filter(|a| a.trusted()).count();
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(&result)?).await?;
        Ok(format!("{placed} of {} placed, {trusted} trusted", numbers.len()))
    }
}
