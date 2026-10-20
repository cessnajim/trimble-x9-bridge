//! Scan files and previews, fetched from the scanner once and cached on disk.
//! Both are immutable once the scan exists, so the cache never goes stale.

use crate::problem::ApiError;
use crate::state::AppState;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Tzf,
    Preview,
}

/// Scan ids are the scanner's UUIDs; refuse anything else so a path can't
/// escape the cache directory.
fn safe_id(id: &str) -> Result<&str, ApiError> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        Ok(id)
    } else {
        Err(ApiError::not_found("scan"))
    }
}

/// Local path of a scan's file or preview, downloading it on first use.
pub async fn scan_file(st: &AppState, scan_id: &str, kind: FileKind) -> Result<PathBuf, ApiError> {
    let id = safe_id(scan_id)?;
    let ext = match kind {
        FileKind::Tzf => "tzf",
        FileKind::Preview => "jpg",
    };
    let dir = st.data_dir.join("scans");
    let path = dir.join(format!("{id}.{ext}"));
    if path.exists() {
        return Ok(path);
    }
    let scans = st.scanner.scans().await?;
    let s = scans.iter().find(|s| s.scan_uuid.as_deref() == Some(id)).ok_or_else(|| ApiError::not_found("scan"))?;
    let tzf_id = s.tzf_id.or_else(|| s.tzf.as_ref().and_then(|t| t.id)).ok_or_else(|| ApiError::not_found("scan file"))?;
    let rel = match kind {
        FileKind::Tzf => format!("tzfs/{tzf_id}/file"),
        FileKind::Preview => format!("tzfs/{tzf_id}/preview"),
    };
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ApiError::conflict(format!("cache directory: {e}")))?;
    let tmp = dir.join(format!("{id}.{ext}.part-{}", crate::util::new_id("dl")));
    if let Err(e) = st.scanner.download(&rel, &tmp).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    tokio::fs::rename(&tmp, &path).await.map_err(|e| ApiError::conflict(format!("cache: {e}")))?;
    Ok(path)
}
