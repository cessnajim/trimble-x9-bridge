//! Missions: what the job is for, and when it is done. The person's sentence
//! ("scan the exterior of the house", "measure all the windows") becomes a
//! mission on the project with criteria in prose and a ledger of targets the
//! agent keeps (each facade, each window): open, covered or measured (by named
//! stations), or declined (with a reason). "Ready to leave" is refused while
//! any target is open; the person then signs off once, and the mission is
//! done. Kept in `missions.json` so a restart keeps them.

use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissionKind {
    Exterior,
    Windows,
    Room,
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    Covered,
    Measured,
    Declined,
}

impl Status {
    pub fn is_final(self) -> bool {
        self != Status::Open
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Work in progress.
    Active,
    /// Every target settled; waiting for the person's sign-off.
    Ready,
    /// The person signed off: the job is done.
    SignedOff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub target_id: String,
    pub name: String,
    /// facade, window, corner, room, … (free text, short).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: Status,
    /// Scans that covered or measured it.
    #[serde(default)]
    pub stations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_id: Option<String>,
    /// Why it was declined, or anything worth knowing.
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub mission_id: String,
    pub project_id: String,
    /// The person's sentence.
    pub goal: String,
    pub kind: MissionKind,
    /// When it is done, in prose (the agent's reading of the goal).
    pub done_when: String,
    pub state: State,
    pub created_by: String,
    pub created_at: String,
    pub targets: Vec<Target>,
    /// The agent's summary when it said ready to leave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_off_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_off_at: Option<String>,
    /// How the sign-off reached the bridge ("asked in the chat", "relayed by the agent: …", "web UI").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_via: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MissionRequest {
    pub goal: String,
    pub kind: MissionKind,
    pub done_when: String,
    #[serde(default)]
    pub targets: Vec<TargetUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetUpdate {
    pub target_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub stations: Option<Vec<String>>,
    #[serde(default)]
    pub measurement_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

pub const MAX_TARGETS: usize = 200;

fn short(what: &str, s: &str, max: usize, required: bool) -> Result<(), String> {
    let n = s.trim().chars().count();
    if (required && n == 0) || n > max {
        let range = if required { format!("1–{max}") } else { format!("up to {max}") };
        return Err(format!("{what}: {range} characters"));
    }
    Ok(())
}

impl MissionRequest {
    pub fn check(&self) -> Result<(), String> {
        short("goal", &self.goal, 300, true)?;
        short("done_when", &self.done_when, 600, true)?;
        Ok(())
    }
}

impl Mission {
    pub fn new(req: &MissionRequest, project_id: &str, by: &str) -> Mission {
        Mission {
            mission_id: crate::util::new_id("msn"),
            project_id: project_id.to_string(),
            goal: req.goal.trim().to_string(),
            kind: req.kind,
            done_when: req.done_when.trim().to_string(),
            state: State::Active,
            created_by: by.to_string(),
            created_at: now_rfc3339(),
            targets: Vec::new(),
            ready_summary: None,
            signed_off_by: None,
            signed_off_at: None,
            confirmed_via: None,
        }
    }

    /// Add or change targets (by id; unnamed fields keep their value). A
    /// covered or measured target names its stations, a declined one its
    /// reason. Any change sends a ready mission back to work; a signed-off
    /// mission doesn't change. `known_scans`: the project's scan ids.
    pub fn update(&mut self, ups: &[TargetUpdate], known_scans: &[String]) -> Result<(), String> {
        if self.state == State::SignedOff {
            return Err("the mission is signed off; start a new one to change it".into());
        }
        let mut next = self.targets.clone();
        for u in ups {
            let id = u.target_id.trim();
            if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(format!("target_id {:?}: 1–64 letters, digits, '-' or '_'", u.target_id));
            }
            let i = match next.iter().position(|t| t.target_id == id) {
                Some(i) => i,
                None => {
                    if next.len() >= MAX_TARGETS {
                        return Err(format!("at most {MAX_TARGETS} targets"));
                    }
                    next.push(Target { target_id: id.into(), name: String::new(), kind: String::new(), status: Status::Open,
                                       stations: vec![], measurement_id: None, note: String::new(), updated_at: String::new() });
                    next.len() - 1
                }
            };
            let t = &mut next[i];
            if let Some(v) = &u.name {
                t.name = v.trim().into();
            }
            if let Some(v) = &u.kind {
                t.kind = v.trim().into();
            }
            if let Some(v) = u.status {
                t.status = v;
            }
            if let Some(v) = &u.stations {
                t.stations = v.clone();
            }
            if let Some(v) = &u.measurement_id {
                t.measurement_id = Some(v.clone()).filter(|m| !m.is_empty());
            }
            if let Some(v) = &u.note {
                t.note = v.trim().into();
            }
            t.updated_at = now_rfc3339();
            short(&format!("target {id}: name"), &t.name, 80, true)?;
            short(&format!("target {id}: kind"), &t.kind, 30, false)?;
            short(&format!("target {id}: note"), &t.note, 300, false)?;
            if let Some(s) = t.stations.iter().find(|s| !known_scans.contains(s)) {
                return Err(format!("target {id}: scan {s} is not in this project"));
            }
            match t.status {
                Status::Covered | Status::Measured if t.stations.is_empty() => {
                    return Err(format!("target {id}: a {} target names the stations that did it", if t.status == Status::Covered { "covered" } else { "measured" }));
                }
                Status::Measured if t.measurement_id.is_none() => {
                    return Err(format!("target {id}: a measured target names its measurement_id"));
                }
                Status::Declined if t.note.is_empty() => return Err(format!("target {id}: a declined target says why, in note")),
                _ => {}
            }
        }
        self.targets = next;
        if self.state == State::Ready {
            self.state = State::Active;
            self.ready_summary = None;
        }
        Ok(())
    }

    /// Ready to leave: every target settled. Returns the open ones otherwise.
    pub fn ready(&mut self, summary: &str) -> Result<(), String> {
        if self.state == State::SignedOff {
            return Err("the mission is already signed off".into());
        }
        if self.targets.is_empty() {
            return Err("the ledger is empty: list what the mission needs (each facade, each window) first".into());
        }
        let open: Vec<&str> = self.targets.iter().filter(|t| !t.status.is_final()).map(|t| t.name.as_str()).collect();
        if !open.is_empty() {
            return Err(format!("{} target{} still open: {}", open.len(), if open.len() == 1 { " is" } else { "s are" }, open.join(", ")));
        }
        short("summary", summary, 1000, true)?;
        self.state = State::Ready;
        self.ready_summary = Some(summary.trim().into());
        Ok(())
    }

    pub fn sign_off(&mut self, by: &str, via: Option<String>) -> Result<(), String> {
        if self.state != State::Ready {
            return Err(format!("the mission is {}, not ready to leave", match self.state {
                State::Active => "still in progress",
                State::Ready => "ready",
                State::SignedOff => "already signed off",
            }));
        }
        self.state = State::SignedOff;
        self.signed_off_by = Some(by.into());
        self.signed_off_at = Some(now_rfc3339());
        self.confirmed_via = via.filter(|v| !v.trim().is_empty()).map(|v| v.chars().take(300).collect());
        Ok(())
    }

    /// "5 covered, 2 measured, 1 declined, 3 open"
    pub fn tally(&self) -> String {
        let n = |s: Status| self.targets.iter().filter(|t| t.status == s).count();
        let parts: Vec<String> = [("covered", Status::Covered), ("measured", Status::Measured), ("declined", Status::Declined), ("open", Status::Open)]
            .iter()
            .filter(|(_, s)| n(*s) > 0)
            .map(|(w, s)| format!("{} {w}", n(*s)))
            .collect();
        if parts.is_empty() { "no targets yet".into() } else { parts.join(", ") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> MissionRequest {
        MissionRequest { goal: "Scan the exterior of the house".into(), kind: MissionKind::Exterior,
                         done_when: "Every facade seen at ≤ 12 mm spacing, no shadow on the house's walls".into(), targets: vec![] }
    }
    fn up(id: &str, status: Option<Status>, stations: &[&str], note: &str) -> TargetUpdate {
        TargetUpdate { target_id: id.into(), name: Some(format!("{id} facade")), kind: Some("facade".into()), status,
                       stations: Some(stations.iter().map(|s| s.to_string()).collect()), measurement_id: None, note: Some(note.into()) }
    }

    #[test]
    fn the_ledger_must_be_settled_before_leaving_and_signed_once() {
        let scans = vec!["s1".to_string(), "s2".to_string()];
        let mut m = Mission::new(&req(), "p1", "claude");
        assert!(m.ready("all done").unwrap_err().contains("empty"));
        m.update(&[up("north", None, &[], ""), up("south", Some(Status::Covered), &["s1"], "")], &scans).unwrap();
        assert!(m.ready("all done").unwrap_err().contains("1 target is still open: north facade"));
        assert!(m.update(&[up("north", Some(Status::Covered), &[], "")], &scans).unwrap_err().contains("names the stations"));
        assert!(m.update(&[up("north", Some(Status::Covered), &["s9"], "")], &scans).unwrap_err().contains("not in this project"));
        assert!(m.update(&[up("north", Some(Status::Declined), &[], "")], &scans).unwrap_err().contains("says why"));
        m.update(&[up("north", Some(Status::Declined), &[], "behind a locked fence")], &scans).unwrap();
        assert_eq!(m.tally(), "1 covered, 1 declined");
        assert!(m.sign_off("jim", None).is_err());
        m.ready("South seen from s1; north behind a locked fence.").unwrap();
        assert_eq!(m.state, State::Ready);
        // A change sends it back to work.
        m.update(&[up("east", None, &[], "")], &scans).unwrap();
        assert_eq!(m.state, State::Active);
        m.update(&[up("east", Some(Status::Covered), &["s2"], "")], &scans).unwrap();
        m.ready("done").unwrap();
        m.sign_off("jim", Some("asked in the chat".into())).unwrap();
        assert_eq!(m.state, State::SignedOff);
        assert!(m.update(&[up("west", None, &[], "")], &scans).unwrap_err().contains("signed off"));
    }

    #[test]
    fn checks_the_request() {
        let mut r = req();
        r.goal = " ".into();
        assert!(r.check().unwrap_err().contains("goal"));
        let mut m = Mission::new(&req(), "p1", "claude");
        assert!(m.update(&[up("no spaces", None, &[], "")], &[]).unwrap_err().contains("target_id"));
    }
}
