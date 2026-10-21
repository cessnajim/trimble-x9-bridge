//! Runs: execute a recipe's steps in order, under the role of whoever started
//! the run (so approvals and grants apply exactly as for a direct request).

use crate::export::ExportRequest;
use crate::model::{Caller, JobState, ScanRequest};
use crate::problem::ApiError;
use crate::recipe::{lookup, resolve, Recipe, RecipeStep};
use crate::routes::{check_note, compute_aim, submit_export, submit_measure, submit_scan, AimReq, MeasureRequest};
use crate::state::AppState;
use crate::util::{new_id, now_rfc3339};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize)]
pub struct StepState {
    pub step_id: String,
    pub state: String,
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// A person step's input as proposed by someone else (usually the agent),
    /// for the person to confirm or change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Proposal {
    pub input: Value,
    pub by: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub run_id: String,
    pub recipe_id: String,
    pub recipe_version: String,
    pub project_id: String,
    /// running | waiting_for_person | succeeded | failed | cancelled
    pub state: String,
    /// When waiting: `approval` (a scan needs an operator) or `input` (a person step).
    pub waiting_for: Option<String>,
    pub current_step_id: Option<String>,
    pub started_by: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub steps: Vec<StepState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Value>,
    /// How often a person had to step in. AI-first aims for one: the final
    /// confirmation.
    pub interventions: Interventions,
    /// Grants the run's scans ran under (`grant_id`, `granted_by`): standing
    /// permission given before or during the run, which `interventions` does
    /// not count as a per-run approval.
    pub authorized_by: Vec<Value>,
}

/// A run's person interventions. An agent completing its own run's step is
/// not one; an operator or admin (or a person-held token) acting on it is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Interventions {
    pub total: u32,
    /// A person approved one of the run's scans.
    pub approvals: u32,
    /// A person accepted a proposal unchanged.
    pub confirmed: u32,
    /// A person changed a proposal; see `corrections`.
    pub corrected: u32,
    /// A person answered a step that had no proposal.
    pub answered: u32,
    /// Of `total`, how many the agent relayed from the chat (the person said
    /// it to the agent; the chat client couldn't ask them directly). Marked
    /// by `confirmed_via` starting with "relayed" in the step's input.
    pub relayed: u32,
    /// What was changed: `{step_id, field, proposed, given, delta?}`, with
    /// `delta` = given − proposed for numbers.
    pub corrections: Vec<Value>,
}

impl Interventions {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, one, many) in [(self.approvals, "approval", "approvals"), (self.confirmed, "confirmation", "confirmations"),
                               (self.corrected, "correction", "corrections"), (self.answered, "answer", "answers")] {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        }
        if self.relayed > 0 {
            parts.push(format!("{} relayed by the agent", self.relayed));
        }
        let what = if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) };
        format!("{} person intervention{}{what}", self.total, if self.total == 1 { "" } else { "s" })
    }
}

/// Record the grant a run's scan job was admitted under, if any (once per grant).
fn note_grant(st: &AppState, run_id: &str, job_id: &str) {
    let g = {
        let inner = st.inner.lock().unwrap();
        let gid = inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.grant_id.clone());
        gid.and_then(|gid| {
            inner.grants.iter().find(|g| g.grant_id == gid).map(|g| {
                let mut v = json!({"grant_id": g.grant_id, "granted_by": g.granted_by});
                if let Some(n) = &g.req.note {
                    v["note"] = json!(n);
                }
                v
            })
        })
    };
    if let Some(g) = g {
        with_run(st, run_id, |rec| {
            if !rec.run.authorized_by.iter().any(|x| x["grant_id"] == g["grant_id"]) {
                rec.run.authorized_by.push(g);
            }
        });
    }
}

/// Scans the run may still start from its current step on: one per
/// `startScan` step, plus each `measureOpening` step's `rescans`.
pub fn scans_left(st: &AppState, run_id: &str) -> u32 {
    with_run(st, run_id, |rec| {
        let from = rec.run.current_step_id.as_ref().and_then(|c| rec.recipe.steps.iter().position(|s| &s.step_id == c)).unwrap_or(0);
        rec.recipe.steps[from..]
            .iter()
            .map(|s| match s.operation_id.as_deref() {
                Some("startScan") => 1,
                Some("measureOpening") => s.arguments.as_ref().and_then(|a| a.get("rescans")).and_then(Value::as_u64).unwrap_or(0) as u32,
                _ => 0,
            })
            .sum()
    })
    .unwrap_or(0)
}

/// Count a person's approval of one of the run's scans.
pub fn note_approval(st: &AppState, run_id: &str) {
    with_run(st, run_id, |rec| {
        rec.run.interventions.approvals += 1;
        rec.run.interventions.total += 1;
    });
}

pub struct RunRecord {
    pub run: Run,
    pub recipe: Recipe,
    pub ctx: Value,
    pub caller: Caller,
    pub advance: Option<(String, Value)>,
    pub cancel: bool,
    pub wake: Arc<Notify>,
}

fn cancelled() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "cancelled", "The run was cancelled")
}

/// Apply a change to a run and publish it.
fn with_run<R>(st: &AppState, run_id: &str, f: impl FnOnce(&mut RunRecord) -> R) -> Option<R> {
    let (r, run) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.runs.get_mut(run_id)?;
        let r = f(rec);
        (r, rec.run.clone())
    };
    st.emit("run", serde_json::to_value(&run).unwrap_or_default());
    Some(r)
}

pub fn snapshot(st: &AppState, run_id: &str) -> Option<Run> {
    st.inner.lock().unwrap().runs.get(run_id).map(|r| r.run.clone())
}

/// Validate inputs, record the run and start driving it.
pub fn start(st: &Arc<AppState>, caller: Caller, recipe: Recipe, project_id: String, inputs: Value) -> Result<Run, ApiError> {
    let inputs = if inputs.is_null() { json!({}) } else { inputs };
    for i in &recipe.inputs {
        let v = inputs.get(&i.name).ok_or_else(|| ApiError::invalid(format!("input {:?} is required", i.name)))?;
        if !i.choices.is_empty() && !v.as_str().is_some_and(|s| i.choices.iter().any(|c| c == s)) {
            return Err(ApiError::invalid(format!("input {:?} must be one of: {}", i.name, i.choices.join(", "))));
        }
    }
    let run = Run {
        run_id: new_id("run"),
        recipe_id: recipe.recipe_id.clone(),
        recipe_version: recipe.version.clone(),
        project_id: project_id.clone(),
        state: "running".into(),
        waiting_for: None,
        current_step_id: None,
        started_by: caller.name.clone(),
        created_at: now_rfc3339(),
        finished_at: None,
        steps: recipe.steps.iter().map(|s| StepState { step_id: s.step_id.clone(), state: "pending".into(), job_id: None, output: None, proposal: None }).collect(),
        problem: None,
        interventions: Interventions::default(),
        authorized_by: Vec::new(),
    };
    let ctx = json!({"project_id": project_id, "input": inputs, "steps": {}});
    let name = caller.name.clone();
    st.inner.lock().unwrap().runs.insert(
        run.run_id.clone(),
        RunRecord { run: run.clone(), recipe: recipe.clone(), ctx, caller, advance: None, cancel: false, wake: Arc::new(Notify::new()) },
    );
    st.log("run", format!("Recipe {:?} v{} started", recipe.title, recipe.version), &name, Some(&project_id), &[("run_id", &run.run_id)]);
    tokio::spawn(drive(st.clone(), run.run_id.clone()));
    Ok(run)
}

/// Only whoever started a run, or an operator or admin, may advance or cancel it.
pub fn may_steer(st: &AppState, run_id: &str, c: &Caller, what: &str) -> Result<(), ApiError> {
    let inner = st.inner.lock().unwrap();
    let rec = inner.runs.get(run_id).ok_or_else(|| ApiError::not_found("run"))?;
    if may_steer_run(&rec.run, c) {
        Ok(())
    } else {
        Err(ApiError::forbidden(what))
    }
}

fn may_steer_run(run: &Run, c: &Caller) -> bool {
    run.started_by == c.name || c.role.is_operator()
}

/// The waiting person step `step_id`, with `input` holding every field it needs.
fn waiting_step(rec: &RunRecord, step_id: &str, input: &Value) -> Result<usize, ApiError> {
    let i = rec.run.steps.iter().position(|s| s.step_id == step_id).ok_or_else(|| ApiError::not_found("step"))?;
    if rec.run.steps[i].state != "waiting" || rec.run.waiting_for.as_deref() != Some("input") {
        return Err(ApiError::conflict(format!("step {step_id:?} is not waiting for input")));
    }
    if !input.is_object() {
        return Err(ApiError::invalid("input must be an object"));
    }
    let missing: Vec<&String> = rec.recipe.steps[i].needs.iter().filter(|n| input.get(n.as_str()).is_none()).collect();
    if !missing.is_empty() {
        return Err(ApiError::invalid(format!("missing: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
    }
    Ok(i)
}

/// Supply a person step's input. Errors if the run isn't waiting there. When
/// the step had a proposal, its output records whether it was accepted as is.
pub fn advance(st: &AppState, run_id: &str, step_id: &str, mut input: Value, by_person: bool) -> Result<Run, ApiError> {
    let res = with_run(st, run_id, |rec| -> Result<(Run, Option<(String, bool)>), ApiError> {
        let i = waiting_step(rec, step_id, &input)?;
        if by_person {
            let iv = &mut rec.run.interventions;
            iv.total += 1;
            if input.get("confirmed_via").and_then(Value::as_str).is_some_and(|v| v.starts_with("relayed")) {
                iv.relayed += 1;
            }
            match &rec.run.steps[i].proposal {
                None => iv.answered += 1,
                Some(p) => {
                    let mut changed = Vec::new();
                    for n in &rec.recipe.steps[i].needs {
                        let (given, proposed) = (input.get(n.as_str()), p.input.get(n.as_str()));
                        let same = match (given, proposed) {
                            (Some(Value::Number(a)), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
                            _ => given == proposed,
                        };
                        if !same {
                            let mut c = json!({"step_id": step_id, "field": n, "proposed": proposed, "given": given});
                            if let (Some(a), Some(b)) = (given.and_then(Value::as_f64), proposed.and_then(Value::as_f64)) {
                                c["delta"] = json!(((a - b) * 1000.0).round() / 1000.0);
                            }
                            changed.push(c);
                        }
                    }
                    if changed.is_empty() {
                        iv.confirmed += 1;
                    } else {
                        iv.corrected += 1;
                        iv.corrections.extend(changed);
                    }
                }
            }
        }
        let verdict = rec.run.steps[i].proposal.as_ref().map(|p| {
            // Numbers compare by value (1185 and 1185.0 are the same answer).
            let eq = |a: Option<&Value>, b: Option<&Value>| match (a, b) {
                (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
                _ => a == b,
            };
            let same = rec.recipe.steps[i].needs.iter().all(|n| eq(input.get(n.as_str()), p.input.get(n.as_str())));
            (p.by.clone(), same)
        });
        if let Some((_, same)) = &verdict {
            input["proposal_accepted"] = json!(same);
        }
        rec.advance = Some((step_id.to_string(), input));
        rec.wake.notify_one();
        Ok((rec.run.clone(), verdict))
    });
    let (run, verdict) = res.ok_or_else(|| ApiError::not_found("run"))??;
    if let Some((by, same)) = verdict {
        let text = if same { format!("Step {step_id:?}: {by}'s proposal accepted") } else { format!("Step {step_id:?}: {by}'s proposal changed by a person") };
        st.log("run", text, "bridge", Some(&run.project_id), &[("run_id", run_id), ("step_id", step_id)]);
    }
    Ok(run)
}

/// Propose a waiting person step's input (an agent's box, a measurement) for
/// a person to confirm or change. The step keeps waiting; a newer proposal
/// replaces an older one.
pub fn propose(st: &AppState, run_id: &str, step_id: &str, input: Value, note: Option<String>, by: &str) -> Result<Run, ApiError> {
    let res = with_run(st, run_id, |rec| -> Result<Run, ApiError> {
        let i = waiting_step(rec, step_id, &input)?;
        rec.run.steps[i].proposal = Some(Proposal { input, by: by.to_string(), at: now_rfc3339(), note });
        Ok(rec.run.clone())
    });
    let run = res.ok_or_else(|| ApiError::not_found("run"))??;
    st.log("run", format!("{by} proposed an answer for step {step_id:?}; waiting for a person to confirm"), by, Some(&run.project_id), &[("run_id", run_id), ("step_id", step_id)]);
    Ok(run)
}

/// Ask a run to stop. Its job in flight is cancelled too: a scan or an export
/// stops at its next check, and the run settles once the job does.
pub fn cancel(st: &AppState, run_id: &str) -> Result<Run, ApiError> {
    let job_id = with_run(st, run_id, |rec| {
        if !matches!(rec.run.state.as_str(), "succeeded" | "failed" | "cancelled") {
            rec.cancel = true;
            rec.wake.notify_one();
        }
        rec.run.steps.iter().find(|s| s.state == "running" || s.state == "waiting").and_then(|s| s.job_id.clone())
    })
    .ok_or_else(|| ApiError::not_found("run"))?;
    if let Some(job_id) = job_id {
        let job = st.inner.lock().unwrap().cancel_job(&job_id);
        st.save_grants();
        if let Some(j) = job {
            st.emit_job(&j);
        }
    }
    snapshot(st, run_id).ok_or_else(|| ApiError::not_found("run"))
}

fn finish(st: &AppState, run_id: &str, state: &str, problem: Option<&ApiError>) {
    let project = with_run(st, run_id, |rec| {
        rec.run.state = state.into();
        rec.run.waiting_for = None;
        rec.run.finished_at = Some(now_rfc3339());
        rec.run.problem = problem.map(|p| p.to_json());
        for s in rec.run.steps.iter_mut() {
            if s.state == "running" || s.state == "waiting" {
                s.state = if state == "cancelled" { "skipped".into() } else { "failed".into() };
            }
        }
        rec.run.project_id.clone()
    });
    let iv = snapshot(st, run_id)
        .map(|r| {
            let by: Vec<String> = r
                .authorized_by
                .iter()
                .filter_map(|g| g["granted_by"].as_str().map(|n| format!("a grant from {n}{}", g["note"].as_str().map(|x| format!(", {x}")).unwrap_or_default())))
                .collect();
            format!("{}{}", r.interventions.summary(), if by.is_empty() { String::new() } else { format!("; scans under {}", by.join(", ")) })
        })
        .unwrap_or_default();
    let text = match (state, problem) {
        ("failed", Some(p)) => format!("Run {run_id} failed: {} ({iv})", p.detail.as_deref().unwrap_or(&p.title)),
        _ => format!("Run {run_id} {state} after {iv}"),
    };
    st.log(if state == "failed" { "error" } else { "run" }, text, "bridge", project.as_deref(), &[("run_id", run_id)]);
}

async fn drive(st: Arc<AppState>, run_id: String) {
    let n = snapshot(&st, &run_id).map(|r| r.steps.len()).unwrap_or(0);
    for i in 0..n {
        let Some(Ok((step, ctx, caller))) = with_run(&st, &run_id, |rec| {
            if rec.cancel {
                return Err(());
            }
            rec.run.current_step_id = Some(rec.recipe.steps[i].step_id.clone());
            rec.run.steps[i].state = "running".into();
            rec.run.state = "running".into();
            rec.run.waiting_for = None;
            Ok((rec.recipe.steps[i].clone(), rec.ctx.clone(), rec.caller.clone()))
        }) else {
            return finish(&st, &run_id, "cancelled", None);
        };
        let out = match step.kind.as_str() {
            "action" => action(&st, &run_id, i, &step, &ctx, &caller).await,
            "person" => person(&st, &run_id, i, &step).await,
            "check" => check(&step, &ctx),
            k => Err(ApiError::invalid(format!("unknown step kind {k:?}"))),
        };
        match out {
            Ok(v) => {
                with_run(&st, &run_id, |rec| {
                    rec.ctx["steps"][&step.step_id] = v.clone();
                    rec.run.steps[i].state = "done".into();
                    rec.run.steps[i].output = Some(v);
                });
            }
            Err(e) if e.code == "cancelled" => return finish(&st, &run_id, "cancelled", None),
            Err(e) => return finish(&st, &run_id, "failed", Some(&e)),
        }
    }
    with_run(&st, &run_id, |rec| rec.run.current_step_id = None);
    finish(&st, &run_id, "succeeded", None);
}

async fn action(st: &Arc<AppState>, run_id: &str, i: usize, step: &RecipeStep, ctx: &Value, caller: &Caller) -> Result<Value, ApiError> {
    let args = resolve(step.arguments.as_ref().unwrap_or(&json!({})), ctx).map_err(ApiError::invalid)?;
    let bad = |e: serde_json::Error| ApiError::invalid(format!("step {:?}: {e}", step.step_id));
    match step.operation_id.as_deref().unwrap_or("") {
        "startScan" => {
            let mut a = args;
            a["project_id"] = ctx["project_id"].clone();
            let req: ScanRequest = serde_json::from_value(a).map_err(bad)?;
            let job = submit_scan(st, caller, req, Some(run_id)).await?;
            note_grant(st, run_id, &job.job_id);
            let job = wait_job(st, run_id, i, &job.job_id).await?;
            Ok(json!({"job_id": job.job_id, "scan_id": job.result.as_ref().and_then(|r| r.get("scan_id")).cloned()}))
        }
        "exportPoints" => {
            let scan_id = args.get("scan_id").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("exportPoints needs scan_id"))?.to_string();
            let req: ExportRequest = serde_json::from_value(args.clone()).map_err(bad)?;
            let job = submit_export(st, caller, scan_id, req, Some(run_id)).await?;
            let job = wait_job(st, run_id, i, &job.job_id).await?;
            let export_id = job.result.as_ref().and_then(|r| r.get("export_id")).and_then(Value::as_str).unwrap_or("").to_string();
            st.inner.lock().unwrap().exports.get(&export_id).cloned().ok_or_else(|| ApiError::not_found("export"))
        }
        "measureOpening" => {
            let mut scan_id = args.get("scan_id").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("measureOpening needs scan_id"))?.to_string();
            // Site check: with `window` (the close-up's) and `rescans` > 0, a
            // close-up short of wall on a side is rescanned wider, under the
            // run, instead of asking a person.
            let rescans = args.get("rescans").and_then(Value::as_u64).unwrap_or(0);
            let mut window: Option<crate::model::Window> = match args.get("window") {
                Some(w) if !w.is_null() => Some(serde_json::from_value(w.clone()).map_err(bad)?),
                _ => None,
            };
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("STD_L2").to_string();
            let project_id = ctx["project_id"].as_str().unwrap_or("").to_string();
            let mut attempts = Vec::new();
            loop {
                let job = submit_measure(st, caller, scan_id.clone(), MeasureRequest::opening(), Some(run_id)).await?;
                let job = wait_job(st, run_id, i, &job.job_id).await?;
                let id = job.result.as_ref().and_then(|r| r.get("measurement_id")).and_then(Value::as_str).unwrap_or("").to_string();
                let mut m = st.inner.lock().unwrap().measurements.get(&id).cloned().ok_or_else(|| ApiError::not_found("measurement"))?;
                let short: Vec<String> = m["details"]["sides"]
                    .as_object()
                    .map(|o| o.iter().filter(|(_, v)| v["ok"] == false).map(|(k, _)| k.clone()).collect())
                    .unwrap_or_default();
                attempts.push(json!({"scan_id": scan_id, "measurement_id": id, "short_sides": short,
                                     "opening_width_mm": m["opening_width_mm"], "opening_height_mm": m["opening_height_mm"]}));
                let done = short.is_empty() || attempts.len() as u64 > rescans;
                let Some(w) = window.filter(|_| !done) else {
                    m["site_check"] = json!({"rescans": attempts.len() - 1, "attempts": attempts, "ok": short.is_empty()});
                    return Ok(m);
                };
                let d = m["details"]["distance_m"].as_f64().unwrap_or(1.0);
                let (nw, a) = widen(w, &short, d);
                st.log("check", format!("Site check: not enough wall at the {} of the opening; rescanning {:.1}° wider on each side of that axis", short.join(", "), a.to_degrees()),
                       "bridge", Some(&project_id), &[("run_id", run_id), ("measurement_id", &id)]);
                let req = ScanRequest { project_id: project_id.clone(), kind: crate::model::ScanKind::Area, mode: mode.clone(), color: false,
                                        window: Some(nw), note: Some("site check rescan, wider".into()) };
                let job = submit_scan(st, caller, req, Some(run_id)).await?;
                note_grant(st, run_id, &job.job_id);
                let job = wait_job(st, run_id, i, &job.job_id).await?;
                scan_id = job.result.as_ref().and_then(|r| r.get("scan_id")).and_then(Value::as_str).unwrap_or("").to_string();
                window = Some(nw);
            }
        }
        "aimFromPreview" => {
            let req: AimReq = serde_json::from_value(args).map_err(bad)?;
            compute_aim(st, req).await
        }
        "addLogNote" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            check_note(&text).map_err(|e| ApiError::invalid(format!("step {:?}: {}", step.step_id, e.detail.unwrap_or_default())))?;
            let project = ctx["project_id"].as_str().unwrap_or("").to_string();
            let e = st.log("note", text, &caller.name, Some(&project), &[("run_id", run_id)]);
            Ok(json!({"entry_id": e.entry_id}))
        }
        op => Err(ApiError::invalid(format!("unknown operation {op:?}"))),
    }
}

/// A close-up window widened on its short axis (left/right: horizontal,
/// top/bottom: vertical) by the angle that adds ~15 cm at the wall's distance
/// `d`, on both sides. Returns the window and that angle.
pub fn widen(w: crate::model::Window, short: &[String], d: f64) -> (crate::model::Window, f64) {
    use std::f64::consts::{PI, TAU};
    let a = (0.15 / d.max(0.3)).atan();
    let mut nw = w;
    if short.iter().any(|s| s == "left" || s == "right") {
        nw.start_ha = (w.start_ha - a).rem_euclid(TAU);
        nw.delta_ha = (w.delta_ha + 2.0 * a).min(TAU);
    }
    if short.iter().any(|s| s == "top" || s == "bottom") {
        let (lo, hi) = ((w.start_va - a).max(0.0), (w.start_va + w.delta_va + a).min(PI));
        nw.start_va = lo;
        nw.delta_va = hi - lo;
    }
    (nw, a)
}

/// Follow a job to the end, mirroring "waiting for approval" onto the run.
async fn wait_job(st: &AppState, run_id: &str, i: usize, job_id: &str) -> Result<crate::model::Job, ApiError> {
    with_run(st, run_id, |rec| rec.run.steps[i].job_id = Some(job_id.to_string()));
    let mut last_wait: Option<bool> = None;
    loop {
        let (job, cancel) = {
            let inner = st.inner.lock().unwrap();
            let job = inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.job.clone());
            let cancel = inner.runs.get(run_id).is_none_or(|r| r.cancel);
            (job, cancel)
        };
        let job = job.ok_or_else(|| ApiError::not_found("job"))?;
        match job.state {
            JobState::Succeeded => return Ok(job),
            JobState::Failed => {
                let why = job.problem.as_ref().and_then(|p| p.get("detail").or(p.get("title"))).and_then(Value::as_str).unwrap_or("failed");
                return Err(ApiError::conflict(format!("job {job_id} failed: {why}")));
            }
            JobState::Cancelled => return Err(if cancel { cancelled() } else { ApiError::conflict(format!("job {job_id} was cancelled")) }),
            s => {
                let waiting = s == JobState::AwaitingApproval;
                if last_wait != Some(waiting) {
                    last_wait = Some(waiting);
                    with_run(st, run_id, |rec| {
                        rec.run.state = if waiting { "waiting_for_person" } else { "running" }.into();
                        rec.run.waiting_for = waiting.then(|| "approval".into());
                    });
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn person(st: &AppState, run_id: &str, i: usize, step: &RecipeStep) -> Result<Value, ApiError> {
    let wake = with_run(st, run_id, |rec| {
        rec.run.steps[i].state = "waiting".into();
        rec.run.state = "waiting_for_person".into();
        rec.run.waiting_for = Some("input".into());
        rec.wake.clone()
    })
    .ok_or_else(cancelled)?;
    let project = snapshot(st, run_id).map(|r| r.project_id).unwrap_or_default();
    st.log("run", format!("Waiting for a person: {}", step.instructions.as_deref().unwrap_or("")), "bridge", Some(&project), &[("run_id", run_id), ("step_id", &step.step_id)]);
    if let Some(from) = &step.propose_from {
        // Fill the step's needs from the earlier step's output, by name.
        let source = with_run(st, run_id, |rec| rec.ctx["steps"][from.as_str()].clone()).unwrap_or(Value::Null);
        let mut input = serde_json::Map::new();
        for n in &step.needs {
            if let Some(v) = source.get(n.as_str()) {
                input.insert(n.clone(), v.clone());
            }
        }
        if input.len() == step.needs.len() {
            let note = match (source.get("opening_width_mm"), source.get("opening_height_mm")) {
                (Some(w), Some(h)) => Some(format!("Measured by the bridge from step {from:?}: {w} × {h} mm (± {} mm)", source.get("uncertainty_mm").cloned().unwrap_or(Value::Null))),
                _ => Some(format!("Taken from step {from:?}")),
            };
            let _ = propose(st, run_id, &step.step_id, Value::Object(input), note, "bridge");
        }
    }
    loop {
        wake.notified().await;
        let got = with_run(st, run_id, |rec| {
            if rec.cancel {
                return Err(cancelled());
            }
            match rec.advance.take() {
                Some((sid, input)) if sid == step.step_id => Ok(Some(input)),
                _ => Ok(None),
            }
        })
        .ok_or_else(cancelled)?;
        if let Some(input) = got? {
            return Ok(input);
        }
    }
}

fn check(step: &RecipeStep, ctx: &Value) -> Result<Value, ApiError> {
    let of = step.of.as_deref().unwrap_or("");
    let checks = lookup(ctx, &format!("steps.{of}.checks")).and_then(|c| c.as_array().cloned()).unwrap_or_default();
    let mut failed = Vec::new();
    for want in &step.expect {
        match checks.iter().find(|c| c["name"] == want.as_str()) {
            Some(c) if c["ok"] == true => {}
            Some(c) => failed.push(format!("{want} ({})", c["detail"].as_str().unwrap_or("not ok"))),
            None => failed.push(format!("{want} (missing)")),
        }
    }
    if failed.is_empty() {
        Ok(json!({"ok": true, "checked": step.expect}))
    } else {
        Err(ApiError::new(StatusCode::CONFLICT, "check_failed", "A recipe check failed").detail(failed.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn run(started_by: &str) -> Run {
        Run {
            run_id: "run_1".into(),
            recipe_id: "r".into(),
            recipe_version: "1".into(),
            project_id: "p".into(),
            state: "waiting_for_person".into(),
            waiting_for: Some("input".into()),
            current_step_id: None,
            started_by: started_by.into(),
            created_at: now_rfc3339(),
            finished_at: None,
            steps: vec![],
            problem: None,
            interventions: Interventions::default(),
            authorized_by: Vec::new(),
        }
    }

    #[test]
    fn only_the_starter_or_an_operator_steers_a_run() {
        let who = |name: &str, role| Caller { name: name.into(), role };
        let r = run("agent-1");
        assert!(may_steer_run(&r, &who("agent-1", Role::Agent)));
        assert!(!may_steer_run(&r, &who("agent-2", Role::Agent)));
        assert!(may_steer_run(&r, &who("op", Role::Operator)));
        assert!(may_steer_run(&r, &who("root", Role::Admin)));
    }

    #[test]
    fn interventions_summary_reads_well() {
        let mut iv = Interventions::default();
        assert_eq!(iv.summary(), "0 person interventions");
        iv.total = 3;
        iv.approvals = 1;
        iv.confirmed = 2;
        assert_eq!(iv.summary(), "3 person interventions (1 approval, 2 confirmations)");
        iv.total = 1;
        iv.relayed = 1;
        assert!(iv.summary().ends_with("1 relayed by the agent)"), "{}", iv.summary());
        iv.relayed = 0;
        iv.approvals = 0;
        iv.confirmed = 0;
        iv.corrected = 1;
        assert_eq!(iv.summary(), "1 person intervention (1 correction)");
    }

    #[test]
    fn widen_adds_about_15_cm_on_the_short_axis_only() {
        use crate::model::Window;
        let w = Window { start_ha: 0.05, delta_ha: 1.4, start_va: 0.7, delta_va: 1.35 };
        let (nw, a) = widen(w, &["left".into()], 0.85);
        assert!((a - (0.15f64 / 0.85).atan()).abs() < 1e-12);
        assert!((nw.delta_ha - (1.4 + 2.0 * a)).abs() < 1e-12);
        assert!((nw.start_ha - (0.05 - a).rem_euclid(std::f64::consts::TAU)).abs() < 1e-12, "wraps past zero");
        assert_eq!((nw.start_va, nw.delta_va), (0.7, 1.35));
        let (nw, _) = widen(w, &["top".into()], 0.85);
        assert_eq!((nw.start_ha, nw.delta_ha), (0.05, 1.4));
        assert!(nw.start_va < 0.7 && nw.start_va + nw.delta_va <= std::f64::consts::PI + 1e-12);
        nw.validate().unwrap();
    }
}
