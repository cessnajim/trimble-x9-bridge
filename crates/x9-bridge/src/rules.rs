//! The rules every client is subject to. Pure functions over a snapshot of the
//! scanner, so they are tested without hardware.

use crate::model::{Grant, Role, Rules, ScanRequest};
use crate::problem::ApiError;
use axum::http::StatusCode;

/// What the rules need to know about the scanner right now.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub battery_pct: Option<u32>,
    pub on_ac: bool,
    pub storage_free: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Queue,
    AwaitApproval,
}

/// Checks that apply both at admission and again right before the scanner
/// starts: connected, battery, storage.
pub fn preflight(rules: &Rules, snap: &Snapshot) -> Result<(), ApiError> {
    if !snap.connected {
        return Err(ApiError::disconnected("no response from the scanner"));
    }
    if let Some(b) = snap.battery_pct {
        if b < rules.min_battery_pct_to_scan && !snap.on_ac {
            return Err(ApiError::new(StatusCode::CONFLICT, "battery_low", "Battery too low to scan")
                .detail(format!("battery {b}% is below {}%", rules.min_battery_pct_to_scan))
                .hint("Swap the battery or plug the scanner in."));
        }
    }
    if let Some(free) = snap.storage_free {
        if free < rules.min_storage_free_bytes {
            return Err(ApiError::new(StatusCode::CONFLICT, "storage_low", "Not enough space on the scanner")
                .detail(format!("{free} bytes free, need {}", rules.min_storage_free_bytes))
                .hint("Free space on the scanner's card."));
        }
    }
    Ok(())
}

/// Decide whether a scan request is refused, queued, or held for approval.
pub fn admit_scan(
    rules: &Rules,
    snap: &Snapshot,
    role: Role,
    queue_len: usize,
    covered_by_grant: bool,
) -> Result<Admission, ApiError> {
    if role == Role::Viewer {
        return Err(ApiError::forbidden("start scans"));
    }
    preflight(rules, snap)?;
    if queue_len >= rules.max_queue_length {
        return Err(ApiError::conflict(format!("the queue is full ({queue_len} jobs)")));
    }
    if role == Role::Agent && rules.require_approval_for_scans && !covered_by_grant {
        return Ok(Admission::AwaitApproval);
    }
    Ok(Admission::Queue)
}

/// Whether an active grant lets this request run without asking.
pub fn grant_covers(g: &Grant, req: &ScanRequest, now_unix: u64, run_id: Option<&str>) -> bool {
    now_unix < g.expires_unix
        && g.req.project_id == req.project_id
        && g.req.max_scans.is_none_or(|m| g.scans_used < m)
        && (g.req.allowed_modes.is_empty() || g.req.allowed_modes.iter().any(|m| m == &req.mode))
        && (!req.color || g.req.allow_color)
        && g.req.run_id.as_deref().is_none_or(|r| Some(r) == run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GrantRequest, ScanKind};

    fn snap() -> Snapshot {
        Snapshot { connected: true, battery_pct: Some(80), on_ac: false, storage_free: Some(20_000_000_000) }
    }
    fn req(mode: &str, color: bool) -> ScanRequest {
        ScanRequest { project_id: "p1".into(), kind: ScanKind::FullDome, mode: mode.into(), color, window: None, note: None }
    }
    fn grant(max: Option<u32>, used: u32, modes: &[&str], color: bool) -> Grant {
        Grant {
            req: GrantRequest {
                project_id: "p1".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                max_scans: max,
                allowed_modes: modes.iter().map(|m| m.to_string()).collect(),
                allow_color: color,
                run_id: None,
                note: None,
            },
            grant_id: "g1".into(),
            granted_by: "crew".into(),
            scans_used: used,
            expires_unix: 2_000_000_000,
        }
    }

    #[test]
    fn viewer_is_refused() {
        let e = admit_scan(&Rules::default(), &snap(), Role::Viewer, 0, false).unwrap_err();
        assert_eq!(e.code, "forbidden");
    }

    #[test]
    fn agent_waits_for_approval_unless_granted() {
        let r = Rules::default();
        assert_eq!(admit_scan(&r, &snap(), Role::Agent, 0, false).unwrap(), Admission::AwaitApproval);
        assert_eq!(admit_scan(&r, &snap(), Role::Agent, 0, true).unwrap(), Admission::Queue);
        assert_eq!(admit_scan(&r, &snap(), Role::Operator, 0, false).unwrap(), Admission::Queue);
    }

    #[test]
    fn battery_storage_connection_and_queue() {
        let r = Rules::default();
        let low = Snapshot { battery_pct: Some(10), ..snap() };
        assert_eq!(admit_scan(&r, &low, Role::Operator, 0, false).unwrap_err().code, "battery_low");
        let low_on_ac = Snapshot { on_ac: true, ..low };
        assert!(admit_scan(&r, &low_on_ac, Role::Operator, 0, false).is_ok());
        let full = Snapshot { storage_free: Some(1_000), ..snap() };
        assert_eq!(admit_scan(&r, &full, Role::Operator, 0, false).unwrap_err().code, "storage_low");
        let off = Snapshot { connected: false, ..snap() };
        assert_eq!(admit_scan(&r, &off, Role::Operator, 0, false).unwrap_err().code, "scanner_disconnected");
        assert_eq!(admit_scan(&r, &snap(), Role::Operator, 20, false).unwrap_err().code, "conflict");
    }

    #[test]
    fn grant_limits() {
        let now = 1_900_000_000;
        assert!(grant_covers(&grant(Some(3), 2, &[], false), &req("STD_L0", false), now, None));
        assert!(!grant_covers(&grant(Some(3), 3, &[], false), &req("STD_L0", false), now, None), "used up");
        assert!(!grant_covers(&grant(None, 0, &["STD_L0"], false), &req("STD_L2", false), now, None), "mode");
        assert!(!grant_covers(&grant(None, 0, &[], false), &req("STD_L0", true), now, None), "color");
        assert!(grant_covers(&grant(None, 0, &[], true), &req("STD_L0", true), now, None));
        assert!(!grant_covers(&grant(None, 0, &[], false), &req("STD_L0", false), 2_100_000_000, None), "expired");
    }
}
