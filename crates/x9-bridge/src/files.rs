//! Scan files and previews, fetched from the scanner once and cached on disk.
//! Both are immutable once the scan exists, but a scan can be deleted: every
//! request asks the scanner whether it still lists the scan before serving
//! the cache, and a delete evicts the scan's cached files.

use crate::problem::ApiError;
use crate::state::AppState;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Tzf,
    Preview,
}

impl FileKind {
    const ALL: [FileKind; 2] = [FileKind::Tzf, FileKind::Preview];

    fn ext(self) -> &'static str {
        match self {
            FileKind::Tzf => "tzf",
            FileKind::Preview => "jpg",
        }
    }
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

fn cache_path(data_dir: &Path, id: &str, kind: FileKind) -> PathBuf {
    data_dir.join("scans").join(format!("{id}.{}", kind.ext()))
}

/// Remove a scan's cached file and preview, and any export or measurement made from it.
/// Missing files are fine.
pub async fn evict(st: &AppState, scan_id: &str) {
    let Ok(id) = safe_id(scan_id) else { return };
    for kind in FileKind::ALL {
        let _ = tokio::fs::remove_file(cache_path(&st.data_dir, id, kind)).await;
    }
    let exports: Vec<PathBuf> = {
        let mut inner = st.inner.lock().unwrap();
        let ids: Vec<String> = inner.exports.iter().filter(|(_, v)| v["scan_id"] == id).map(|(k, _)| k.clone()).collect();
        ids.iter()
            .filter_map(|e| {
                inner.exports.remove(e);
                inner.export_files.remove(e)
            })
            .collect()
    };
    let evidence: Vec<PathBuf> = {
        let mut inner = st.inner.lock().unwrap();
        let ids: Vec<String> = inner.measurements.iter().filter(|(_, v)| v["scan_id"] == id).map(|(k, _)| k.clone()).collect();
        ids.iter()
            .filter_map(|m| {
                inner.measurements.remove(m);
                inner.measurement_files.remove(m)
            })
            .collect()
    };
    for p in evidence {
        let _ = tokio::fs::remove_file(p).await;
    }
    for p in exports {
        let _ = tokio::fs::remove_file(p).await;
    }
}

/// Deletes a partial download unless it was moved into place.
struct PartFile(PathBuf, bool);

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Local path of a scan's file or preview, downloading it on first use. The
/// scanner must still list the scan, cached or not; if it doesn't, any cached
/// copy is evicted and the scan is `not_found`.
pub async fn scan_file(st: &AppState, scan_id: &str, kind: FileKind) -> Result<PathBuf, ApiError> {
    let id = safe_id(scan_id)?;
    let scans = st.scanner.scans().await?;
    let Some(s) = scans.iter().find(|s| s.scan_uuid.as_deref() == Some(id)) else {
        evict(st, id).await;
        return Err(ApiError::not_found("scan"));
    };
    let path = cache_path(&st.data_dir, id, kind);
    if path.exists() {
        return Ok(path);
    }
    let tzf_id = s.tzf_id.or_else(|| s.tzf.as_ref().and_then(|t| t.id)).ok_or_else(|| ApiError::not_found("scan file"))?;
    let rel = match kind {
        FileKind::Tzf => format!("tzfs/{tzf_id}/file"),
        FileKind::Preview => format!("tzfs/{tzf_id}/preview"),
    };
    let dir = st.data_dir.join("scans");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ApiError::cache(format!("cache directory: {e}")))?;
    // Dropped (and deleted) if the download fails or its caller gives up.
    let mut part = PartFile(dir.join(format!("{id}.{}.part-{}", kind.ext(), crate::util::new_id("dl"))), false);
    st.scanner.download(&rel, &part.0).await?;
    tokio::fs::rename(&part.0, &path).await.map_err(|e| ApiError::cache(format!("cache: {e}")))?;
    part.1 = true;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_cache() {
        assert!(safe_id("5b5314cf-76b8-4f59-a445-ceee1e6df4c3").is_ok());
        for bad in ["", "../etc/passwd", "a/b", "x".repeat(65).as_str()] {
            assert_eq!(safe_id(bad).unwrap_err().code, "not_found", "{bad:?}");
        }
    }

    #[test]
    fn part_file_is_removed_unless_kept() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-part"));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.part"), dir.join("b.part"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        drop(PartFile(a.clone(), false));
        drop(PartFile(b.clone(), true));
        assert!(!a.exists() && b.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
