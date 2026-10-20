//! Runs: execute a recipe's steps in order, under the role of whoever started
//! the run (so approvals and grants apply exactly as for a direct request).

use crate::export::ExportRequest;
use crate::model::{Caller, JobState, ScanRequest};
use crate::problem::ApiError;
use crate::recipe::{lookup, resolve, Recipe, RecipeStep};
use crate::routes::{check_note, compute_aim, submit_export, submit_scan, AimReq};
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
        steps: recipe.steps.iter().map(|s| StepState { step_id: s.step_id.clone(), state: "pending".into(), job_id: None, output: None }).collect(),
        problem: None,
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

/// Supply a person step's input. Errors if the run isn't waiting there.
pub fn advance(st: &AppState, run_id: &str, step_id: &str, input: Value) -> Result<Run, ApiError> {
    let res = with_run(st, run_id, |rec| -> Result<Run, ApiError> {
        let i = rec.run.steps.iter().position(|s| s.step_id == step_id).ok_or_else(|| ApiError::not_found("step"))?;
        if rec.run.steps[i].state != "waiting" || rec.run.waiting_for.as_deref() != Some("input") {
            return Err(ApiError::conflict(format!("step {step_id:?} is not waiting for input")));
        }
        let missing: Vec<&String> = rec.recipe.steps[i].needs.iter().filter(|n| input.get(n.as_str()).is_none()).collect();
        if !missing.is_empty() {
            return Err(ApiError::invalid(format!("missing: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
        }
        rec.advance = Some((step_id.to_string(), input));
        rec.wake.notify_one();
        Ok(rec.run.clone())
    });
    res.ok_or_else(|| ApiError::not_found("run"))?
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
    let text = match (state, problem) {
        ("failed", Some(p)) => format!("Run {run_id} failed: {}", p.detail.as_deref().unwrap_or(&p.title)),
        _ => format!("Run {run_id} {state}"),
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
}
