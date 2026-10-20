//! x9-client — client for the Trimble X9's local HTTP API (`/api/v1`).
//!
//! Verified 2026-10-19 against a real X9 over USB-CDC-EEM at
//! `http://192.168.42.1:34567/api/v1`. Reach the instrument there (its USB
//! gadget hands the host a 192.168.42.0/24 lease and serves on :34567), or over
//! its WiFi AP, or via the bundled `x9-mock`.
//!
//! Every route here was confirmed against a real X9 (see PROVENANCE.md).
//! `launch_scan`, `cancel_task` and `delete_scan` move or change the hardware.

pub mod error;
pub mod models;

pub use error::{Result, X9Error};

use futures_util::StreamExt;
use std::path::Path;
use url::Url;

/// Default base: the X9's own HTTP API over the USB-RNDIS/EEM link.
pub const DEFAULT_BASE: &str = "http://192.168.42.1:34567/api/v1";

#[derive(Clone)]
pub struct Client {
    base: Url,
    http: reqwest::Client,
}

impl Client {
    pub fn connect(base: &str) -> Result<Self> {
        let base = if base.ends_with('/') { base.to_string() } else { format!("{base}/") };
        let http = reqwest::Client::builder()
            .user_agent(concat!("x9-client/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        Ok(Self { base: Url::parse(&base)?, http })
    }

    fn url(&self, rel: &str) -> Result<Url> {
        Ok(self.base.join(rel.trim_start_matches('/'))?)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, rel: &str) -> Result<T> {
        let u = self.url(rel)?;
        let resp = self.http.get(u.clone()).send().await?;
        Self::parse(resp, u.path()).await
    }

    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self, rel: &str, body: &B,
    ) -> Result<T> {
        let u = self.url(rel)?;
        let resp = self.http.post(u.clone()).json(body).send().await?;
        Self::parse(resp, u.path()).await
    }

    async fn parse<T: serde::de::DeserializeOwned>(resp: reqwest::Response, path: &str) -> Result<T> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(X9Error::Api { status: status.as_u16(), path: path.to_string(), body });
        }
        Ok(resp.json::<T>().await?)
    }

    // ---- Read-only command surface (confirmed against a real X9) ----

    /// GET /status — full instrument status (battery, leveling, storage, imu,
    /// laser, wifi, errors). The leveling/storage/etc. sub-objects live here;
    /// there are no standalone /leveling, /storage, /status-lights routes.
    pub async fn status(&self) -> Result<models::Status> {
        self.get_json("status").await
    }

    /// Convenience: leveling state, read from /status.
    pub async fn leveling(&self) -> Result<Option<models::Leveling>> {
        Ok(self.status().await?.leveling)
    }

    /// GET /scans — all scans (bare array).
    pub async fn scans(&self) -> Result<Vec<models::Scan>> {
        self.get_json("scans").await
    }

    /// GET /scans/{id} — a single scan. (Item route; provisional.)
    pub async fn scan(&self, id: i64) -> Result<models::Scan> {
        self.get_json(&format!("scans/{id}")).await
    }

    /// GET /scans/modes — the available scan modes (density/range/time menu).
    pub async fn scan_modes(&self) -> Result<Vec<models::ScanMode>> {
        self.get_json("scans/modes").await
    }

    /// GET /projects — all projects (bare array).
    pub async fn projects(&self) -> Result<Vec<models::Project>> {
        self.get_json("projects").await
    }

    /// POST /projects — create a project. Confirmed: body {projectName,
    /// projectUUID} (the device honors the supplied UUID), returns the Project.
    pub async fn create_project(&self, name: &str, uuid: &str) -> Result<models::Project> {
        self.post_json(
            "projects",
            &serde_json::json!({ "projectName": name, "projectUUID": uuid }),
        )
        .await
    }

    /// GET /tasks — active/queued tasks (bare array).
    pub async fn tasks(&self) -> Result<Vec<models::Task>> {
        self.get_json("tasks").await
    }

    /// GET /errors — current error list (bare array).
    pub async fn errors(&self) -> Result<Vec<models::ErrorItem>> {
        self.get_json("errors").await
    }

    /// GET /diagnostics — diagnostics list (bare array; schema permissive).
    pub async fn diagnostics(&self) -> Result<Vec<serde_json::Value>> {
        self.get_json("diagnostics").await
    }

    /// GET /tzfs/{id} — metadata for one TZF (file path, size, point count).
    pub async fn tzf(&self, tzf_id: i64) -> Result<models::Tzf> {
        self.get_json(&format!("tzfs/{tzf_id}")).await
    }

    /// Download a scan's TZF point cloud: GET /tzfs/{id}/file (octet-stream).
    /// Returns bytes written. Confirmed route (2026-10-19).
    pub async fn download_tzf(&self, tzf_id: i64, dest: &Path) -> Result<u64> {
        self.download(&format!("tzfs/{tzf_id}/file"), dest).await
    }

    /// Download a TZF preview JPEG: GET /tzfs/{id}/preview.
    pub async fn download_tzf_preview(&self, tzf_id: i64, dest: &Path) -> Result<u64> {
        self.download(&format!("tzfs/{tzf_id}/preview"), dest).await
    }

    /// Download a TZF thumbnail JPEG: GET /tzfs/{id}/thumbnail.
    pub async fn download_tzf_thumbnail(&self, tzf_id: i64, dest: &Path) -> Result<u64> {
        self.download(&format!("tzfs/{tzf_id}/thumbnail"), dest).await
    }

    /// Stream any binary resource to disk by relative path (used by the
    /// download_tzf* helpers). The X9 does not honor HTTP Range, so this
    /// streams the full response to `dest`.
    pub async fn download(&self, rel: &str, dest: &Path) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let u = self.url(rel)?;
        let resp = self.http.get(u.clone()).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(X9Error::Api { status, path: u.path().to_string(), body });
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(total)
    }

    /// Poll /tasks until nothing is running (or `max` polls reached), sleeping
    /// `interval` between checks. Returns the last running task, or None if idle.
    /// Use after launch_scan() to wait for a scan to finish.
    pub async fn wait_until_idle(
        &self,
        interval: std::time::Duration,
        max: u32,
    ) -> Result<Option<models::Task>> {
        let mut last = None;
        for _ in 0..max {
            let tasks = self.tasks().await?;
            if tasks.is_empty() {
                return Ok(None);
            }
            last = tasks.into_iter().next();
            tokio::time::sleep(interval).await;
        }
        Ok(last)
    }

    // ---- Actuating surface (moves hardware) ----

    /// POST /scans — launch a scan (HTTP 202, returns a Task). ACTUATING:
    /// rotates the head and fires the laser. CONFIRMED live 2026-10-19.
    /// Targeting: set BOTH `project_name` and `project_uuid` in `req` to choose
    /// the project; with only the UUID (or neither) the device uses its last
    /// project. The device fills many defaults (autoLeveling, levelingMethod
    /// "full", scanType "FullDome"). Poll `wait_until_idle` / `tasks` to follow.
    pub async fn launch_scan(&self, req: &models::ScanRequest) -> Result<models::Task> {
        self.post_json("scans", req).await
    }

    /// Launch a scan into a specific project (fills name+uuid for targeting).
    pub async fn launch_scan_in(
        &self,
        project: &models::Project,
        scan_mode: &str,
    ) -> Result<models::Task> {
        let req = models::ScanRequest {
            project_name: project.project_name.clone(),
            project_uuid: project.project_uuid.clone(),
            scan_mode: Some(scan_mode.to_string()),
            ..Default::default()
        };
        self.launch_scan(&req).await
    }

    /// DELETE /tasks/{id} — stop a running task (e.g. a scan). ACTUATING.
    pub async fn cancel_task(&self, task_id: i64) -> Result<()> {
        self.delete(&format!("tasks/{task_id}")).await
    }

    /// DELETE /scans/{id} — permanently delete a scan and its file. DESTRUCTIVE.
    /// Scan ids are positional: resolve the id from the scan UUID immediately
    /// before calling this.
    pub async fn delete_scan(&self, scan_id: i64) -> Result<()> {
        self.delete(&format!("scans/{scan_id}")).await
    }

    async fn delete(&self, rel: &str) -> Result<()> {
        let u = self.url(rel)?;
        let resp = self.http.delete(u.clone()).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(X9Error::Api { status, path: u.path().to_string(), body });
        }
        Ok(())
    }

}
