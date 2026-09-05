//! Health and drift evaluation shared by `ifx check`, `ifx apply`, and `ifxd`.

use std::collections::BTreeMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{Action, Engine, Options, uses_execution_scoped_access};
use crate::model::{Program, Urn};
use crate::provider::Ctx;
use crate::state::State;
use crate::store::{Health, HealthRecord, RunKind};

/// Run every check-type resource in `state` once and record the results.
pub async fn run_checks(engine: &Engine, state: &State) -> anyhow::Result<Vec<HealthRecord>> {
    run_checks_inner(engine, state, false).await
}

/// Run checks during an explicit apply, allowing execution-scoped management access.
pub async fn run_checks_with_management(
    engine: &Engine,
    state: &State,
) -> anyhow::Result<Vec<HealthRecord>> {
    run_checks_inner(engine, state, true).await
}

async fn run_checks_inner(
    engine: &Engine,
    state: &State,
    allow_management: bool,
) -> anyhow::Result<Vec<HealthRecord>> {
    let result = run_checks_body(engine, state, allow_management).await;
    let cleanup = if allow_management {
        engine.transports().finish_execution().await
    } else {
        Ok(())
    };
    match (result, cleanup) {
        (Ok((records, _)), Ok(())) => Ok(records),
        (Ok((records, run)), Err(cleanup)) => {
            let error = format!("{cleanup:#}");
            engine
                .store()
                .finish_run(
                    &run,
                    false,
                    serde_json::json!({
                        "checks": records.len(),
                        "management_cleanup_failed": true,
                    }),
                    Some(error.clone()),
                )
                .await?;
            anyhow::bail!(error)
        }
        (Err(check), Ok(())) => Err(check),
        (Err(check), Err(cleanup)) => Err(anyhow::anyhow!(
            "{check:#}; closing execution-scoped management also failed: {cleanup:#}"
        )),
    }
}

async fn run_checks_body(
    engine: &Engine,
    state: &State,
    allow_management: bool,
) -> anyhow::Result<(Vec<HealthRecord>, String)> {
    let store = engine.store();
    let run = store.begin_run(&state.stack, RunKind::Check).await?;
    let mut out = Vec::new();
    for (urn, entry) in &state.resources {
        let Some(checker) = engine.registry().checker(urn.type_name()) else {
            continue;
        };
        let cx = Ctx {
            urn,
            transports: engine.transports(),
            triggered: false,
        };
        let outcome = if !allow_management && uses_execution_scoped_access(&entry.inputs) {
            crate::provider::CheckOutcome {
                status: Health::Unknown,
                message: "management is sealed; run an apply to check through temporary access"
                    .to_string(),
                latency_ms: None,
            }
        } else {
            match checker.check(&cx, &entry.inputs).await {
                Ok(outcome) => outcome,
                Err(error) => crate::provider::CheckOutcome {
                    status: Health::Unknown,
                    message: format!("{error:#}"),
                    latency_ms: None,
                },
            }
        };
        let rec = HealthRecord {
            stack: state.stack.clone(),
            urn: urn.clone(),
            status: outcome.status,
            message: outcome.message,
            latency_ms: outcome.latency_ms,
            at: Utc::now(),
        };
        store.record_health(&rec).await?;
        out.push(rec);
    }
    let worst = out.iter().map(|r| r.status).max();
    let summary = serde_json::json!({
        "checks": out.len(),
        "unhealthy": out.iter().filter(|r| r.status == Health::Unhealthy).count(),
        "worst": worst,
    });
    store.finish_run(&run, true, summary, None).await?;
    Ok((out, run))
}

/// Observe every managed (non-check) resource and record whether it still matches the
/// program. Uses the planner, so the result is exactly what `ifx plan` would show.
pub async fn detect_drift(
    engine: &Engine,
    program: &Program,
    state: &State,
) -> anyhow::Result<Vec<HealthRecord>> {
    let store = engine.store();
    let run = store.begin_run(&state.stack, RunKind::Drift).await?;
    let result = engine.plan(program, state, &Options::default()).await;
    let plan = match result {
        Ok(p) => p,
        Err(e) => {
            store
                .finish_run(&run, false, Value::Null, Some(format!("{e:#}")))
                .await?;
            return Err(e);
        }
    };
    let now = Utc::now();
    let mut out = Vec::new();
    for op in &plan.ops {
        if op.skipped || engine.registry().is_check(op.urn.type_name()) {
            continue;
        }
        let (status, message) = match &op.action {
            Action::NoOp | Action::Trigger | Action::Adopt => {
                (Health::Healthy, "matches program".to_string())
            }
            Action::Create => (Health::Unhealthy, "missing (would be created)".to_string()),
            Action::Delete => (
                Health::Drifted,
                "not in program (would be deleted)".to_string(),
            ),
            Action::Update(d) | Action::Replace(d) => {
                let fields: Vec<&str> = d.changes.iter().map(|c| c.field.as_str()).collect();
                let what = if fields.is_empty() {
                    "inputs unknown".to_string()
                } else {
                    fields.join(", ")
                };
                (Health::Drifted, format!("drifted: {what}"))
            }
        };
        let rec = HealthRecord {
            stack: state.stack.clone(),
            urn: op.urn.clone(),
            status,
            message,
            latency_ms: None,
            at: now,
        };
        store.record_health(&rec).await?;
        out.push(rec);
    }
    let summary = serde_json::json!({
        "resources": out.len(),
        "drifted": out.iter().filter(|r| r.status != Health::Healthy).count(),
    });
    store.finish_run(&run, true, summary, None).await?;
    Ok(out)
}

/// Snapshot of a stack for `ifx status` and the daemon API.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StackStatus {
    pub stack: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<crate::StackBuildStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<crate::StackLease>,
    pub resources: usize,
    pub checks: usize,
    pub overall: Option<Health>,
    pub health: Vec<HealthRecord>,
    pub runs: Vec<crate::store::RunRecord>,
}

pub async fn stack_status(engine: &Engine, stack: &str) -> anyhow::Result<StackStatus> {
    let store = engine.store();
    let state = store.load_state(stack).await?;
    let health = store.latest_health(stack).await?;
    let runs = store.runs(stack, 10).await?;
    let checks = state
        .resources
        .keys()
        .filter(|u| engine.registry().is_check(u.type_name()))
        .count();
    let overall = health.iter().map(|h| h.status).max();
    Ok(StackStatus {
        stack: stack.to_string(),
        build: None,
        lease: None,
        resources: state.resources.len(),
        checks,
        overall,
        health,
        runs,
    })
}

/// Health keyed by resource, for rendering.
pub fn by_urn(records: &[HealthRecord]) -> BTreeMap<Urn, &HealthRecord> {
    records.iter().map(|r| (r.urn.clone(), r)).collect()
}
