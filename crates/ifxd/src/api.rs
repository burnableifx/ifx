use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Json, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use futures::stream;
use ifx::control::{
    ApprovalGrant, LeaseExtension, LeaseRequest, ProgramResolveRequest, RunRequest,
};
use ifx::model::Urn;
use ifx::monitor;
use serde_json::json;

use crate::watch::Watcher;

#[derive(Clone)]
pub struct AppState {
    pub registry: ifx::Registry,
    pub watchers: Vec<Arc<Watcher>>,
    pub token: Option<Arc<str>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/stacks", get(list_stacks))
        .route("/api/stacks/{stack}", get(stack_status))
        .route("/api/stacks/{stack}/topology", get(topology))
        .route("/api/stacks/{stack}/health", get(stack_health))
        .route("/api/stacks/{stack}/runs", get(stack_runs))
        .route("/api/stacks/{stack}/history/{urn}", get(history))
        .route("/api/stacks/{stack}/check", post(check_now))
        .route("/api/stacks/{stack}/drift", post(drift_now))
        .route("/api/schema", get(schema))
        .route(
            "/api/v1/stacks/{stack}/build",
            get(build_status).post(retry_build),
        )
        .route("/api/v1/stacks/{stack}/program", post(resolve_program))
        .route(
            "/api/v1/stacks/{stack}/revisions/active",
            get(active_revision),
        )
        .route(
            "/api/v1/stacks/{stack}/runs",
            post(start_run).get(executions),
        )
        .route(
            "/api/v1/stacks/{stack}/state",
            get(stack_state).put(replace_state),
        )
        .route("/api/v1/stacks/{stack}/state/{urn}", delete(forget_state))
        .route(
            "/api/v1/stacks/{stack}/lease",
            get(lease).put(set_lease).delete(clear_lease),
        )
        .route("/api/v1/stacks/{stack}/lease/extend", post(extend_lease))
        .route("/api/v1/runs/{run_id}", get(execution))
        .route("/api/v1/runs/{run_id}/events", get(execution_event_stream))
        .route("/api/v1/runs/{run_id}/events.json", get(execution_events))
        .route("/api/v1/runs/{run_id}/cancel", post(cancel_execution))
        .route("/api/v1/runs/{run_id}/retry", post(retry_execution))
        .route("/api/v1/runs/{run_id}/approve", post(approve_execution))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/", get(explorer))
        .route("/explorer", get(explorer))
        .route("/healthz", get(|| async { "ok" }))
        .merge(api)
        .with_state(state)
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(expected) = &state.token else {
        return next.run(request).await;
    };
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    if supplied == Some(expected.as_ref()) {
        return next.run(request).await;
    }
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "missing or invalid bearer token" })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let code = if msg.contains("not found")
            || msg.contains("unknown stack")
            || msg.contains("has no lease")
        {
            StatusCode::NOT_FOUND
        } else if msg.contains("not in the future")
            || msg.contains("needs a deadline")
            || msg.contains("not both")
            || msg.contains("must be 0s")
            || msg.contains("too far in the future")
        {
            StatusCode::BAD_REQUEST
        } else if msg.contains("not waiting")
            || msg.contains("does not match")
            || msg.contains("is not pending")
            || msg.contains("lease expired")
            || msg.contains("requires a lease")
        {
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

type ApiResult = Result<Response, ApiError>;

fn watcher<'a>(s: &'a AppState, stack: &str) -> anyhow::Result<&'a Arc<Watcher>> {
    s.watchers
        .iter()
        .find(|w| w.cfg.name == stack)
        .ok_or_else(|| anyhow::anyhow!("unknown stack `{stack}`"))
}

async fn list_stacks(State(s): State<AppState>) -> ApiResult {
    let mut out = Vec::new();
    for w in &s.watchers {
        let st = monitor::stack_status(&w.engine, &w.cfg.name).await?;
        out.push(json!({
            "stack": w.cfg.name,
            "dir": w.ctx.dir,
            "resources": st.resources,
            "checks": st.checks,
            "overall": st.overall,
            "build": w.build_status().await,
            "program_error": *w.last_error.read().await,
        }));
    }
    let mut stored = BTreeSet::new();
    for watcher in &s.watchers {
        stored.extend(watcher.engine.store().stacks().await?);
    }
    Ok(Json(json!({ "watched": out, "stored": stored })).into_response())
}

async fn explorer() -> Html<String> {
    Html(ifx::explorer::live_html())
}

#[derive(Default, serde::Deserialize)]
struct TopologyQuery {
    #[serde(default)]
    no_refresh: bool,
}

async fn topology(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Query(query): Query<TopologyQuery>,
) -> ApiResult {
    let w = watcher(&s, &stack)?;
    let snapshot = w.topology(query.no_refresh).await?;
    Ok(Json(snapshot).into_response())
}

async fn stack_status(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let w = watcher(&s, &stack)?;
    let mut st = monitor::stack_status(&w.engine, &stack).await?;
    st.build = Some(w.build_status().await);
    st.lease = w.lease().await?;
    Ok(Json(st).into_response())
}

async fn stack_health(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let w = watcher(&s, &stack)?;
    Ok(Json(w.engine.store().latest_health(&stack).await?).into_response())
}

async fn stack_runs(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let w = watcher(&s, &stack)?;
    Ok(Json(w.engine.store().runs(&stack, 50).await?).into_response())
}

async fn history(
    State(s): State<AppState>,
    Path((stack, urn)): Path<(String, String)>,
) -> ApiResult {
    let w = watcher(&s, &stack)?;
    let urn = Urn::parse(&urn)?;
    Ok(Json(w.engine.store().health_history(&stack, &urn, 200).await?).into_response())
}

async fn check_now(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let w = watcher(&s, &stack)?;
    Ok(Json(w.run_checks().await).into_response())
}

async fn drift_now(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let w = watcher(&s, &stack)?;
    Ok(Json(w.run_drift().await).into_response())
}

async fn schema(State(s): State<AppState>) -> ApiResult {
    Ok(Json(s.registry.schemas()).into_response())
}

async fn build_status(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.build_status().await).into_response())
}

async fn retry_build(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.retry_build().await).into_response())
}

async fn resolve_program(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Json(request): Json<ProgramResolveRequest>,
) -> ApiResult {
    let revision = watcher(&s, &stack)?.resolve_program(request).await?;
    Ok((StatusCode::CREATED, Json(revision)).into_response())
}

async fn active_revision(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.active_revision().await?).into_response())
}

async fn start_run(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Json(request): Json<RunRequest>,
) -> ApiResult {
    let run = watcher(&s, &stack)?.start_run(request).await?;
    Ok((StatusCode::ACCEPTED, Json(run)).into_response())
}

async fn executions(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.executions(50).await?).into_response())
}

async fn stack_state(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.state().await?).into_response())
}

async fn replace_state(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Json(state): Json<ifx::State>,
) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.replace_state(state).await?).into_response())
}

async fn forget_state(
    State(s): State<AppState>,
    Path((stack, urn)): Path<(String, String)>,
) -> ApiResult {
    let urn = Urn::parse(&urn)?;
    Ok(Json(watcher(&s, &stack)?.forget_state(&urn).await?).into_response())
}

async fn lease(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    let lease = watcher(&s, &stack)?
        .lease()
        .await?
        .ok_or_else(|| anyhow::anyhow!("stack `{stack}` has no lease"))?;
    Ok(Json(lease).into_response())
}

async fn set_lease(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Json(request): Json<LeaseRequest>,
) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.set_lease(request).await?).into_response())
}

async fn extend_lease(
    State(s): State<AppState>,
    Path(stack): Path<String>,
    Json(extension): Json<LeaseExtension>,
) -> ApiResult {
    Ok(Json(watcher(&s, &stack)?.extend_lease(extension).await?).into_response())
}

async fn clear_lease(State(s): State<AppState>, Path(stack): Path<String>) -> ApiResult {
    watcher(&s, &stack)?.clear_lease().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn execution_watcher(s: &AppState, run_id: &str) -> anyhow::Result<Arc<Watcher>> {
    for watcher in &s.watchers {
        if watcher.execution(run_id).await?.is_some() {
            return Ok(watcher.clone());
        }
    }
    anyhow::bail!("run `{run_id}` not found")
}

async fn execution(State(s): State<AppState>, Path(run_id): Path<String>) -> ApiResult {
    let watcher = execution_watcher(&s, &run_id).await?;
    let run = watcher
        .execution(&run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run `{run_id}` not found"))?;
    Ok(Json(run).into_response())
}

async fn execution_events(State(s): State<AppState>, Path(run_id): Path<String>) -> ApiResult {
    let watcher = execution_watcher(&s, &run_id).await?;
    Ok(Json(watcher.execution_events(&run_id).await?).into_response())
}

async fn cancel_execution(State(s): State<AppState>, Path(run_id): Path<String>) -> ApiResult {
    let watcher = execution_watcher(&s, &run_id).await?;
    Ok(Json(watcher.cancel(&run_id).await?).into_response())
}

async fn retry_execution(State(s): State<AppState>, Path(run_id): Path<String>) -> ApiResult {
    let watcher = execution_watcher(&s, &run_id).await?;
    Ok(Json(watcher.retry_now(&run_id).await?).into_response())
}

async fn approve_execution(
    State(s): State<AppState>,
    Path(run_id): Path<String>,
    Json(grant): Json<ApprovalGrant>,
) -> ApiResult {
    let watcher = execution_watcher(&s, &run_id).await?;
    Ok(Json(watcher.approve(&run_id, grant).await?).into_response())
}

struct EventStreamState {
    watcher: Arc<Watcher>,
    run_id: String,
    sent: usize,
    pending: VecDeque<ifx::ExecutionEvent>,
    done: bool,
}

async fn execution_event_stream(
    State(s): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let watcher = execution_watcher(&s, &run_id).await?;
    let state = EventStreamState {
        watcher,
        run_id,
        sent: 0,
        pending: VecDeque::new(),
        done: false,
    };
    let events = stream::unfold(state, |mut state| async move {
        loop {
            if state.done {
                return None;
            }
            if let Some(event) = state.pending.pop_front() {
                let data = serde_json::to_string(&event)
                    .unwrap_or_else(|error| json!({"error": error.to_string()}).to_string());
                return Some((Ok(Event::default().event(event.kind).data(data)), state));
            }
            match state.watcher.execution_events(&state.run_id).await {
                Ok(events) if events.len() > state.sent => {
                    state.pending.extend(events.into_iter().skip(state.sent));
                    state.sent += state.pending.len();
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    state.done = true;
                    return Some((
                        Ok(Event::default()
                            .event("error")
                            .data(json!({"error": format!("{error:#}")}).to_string())),
                        state,
                    ));
                }
            }
            match state.watcher.execution(&state.run_id).await {
                Ok(Some(run)) if run.status.terminal() => {
                    state.done = true;
                    let data = serde_json::to_string(&run)
                        .unwrap_or_else(|error| json!({"error": error.to_string()}).to_string());
                    return Some((Ok(Event::default().event("run_finished").data(data)), state));
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    state.done = true;
                    return Some((
                        Ok(Event::default()
                            .event("error")
                            .data(json!({"error": "run disappeared"}).to_string())),
                        state,
                    ));
                }
                Err(error) => {
                    state.done = true;
                    return Some((
                        Ok(Event::default()
                            .event("error")
                            .data(json!({"error": format!("{error:#}")}).to_string())),
                        state,
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ifx::control::{BuildPhase, ExecutionStatus, ProgramResolveRequest, RunRequest};
    use ifx::model::Urn;
    use ifx::store::{RunKind, Store, SurrealStore};

    #[tokio::test]
    async fn control_api_executes_and_mutates_state_through_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        write_test_stack(dir.path());
        let registry = ifx::Registry::builtin();
        let ctx = ifx::LoadCtx::from_dir(dir.path(), None, "api-test", &[]).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            SurrealStore::connect("mem://", "ifx", "api-test")
                .await
                .unwrap(),
        );
        let cfg = crate::config::StackConfig {
            dir: dir.path().to_path_buf(),
            name: "api-test".into(),
            file: None,
            check_interval: None,
            drift_interval: None,
            no_drift: true,
            require_lease: false,
        };
        let watcher = Arc::new(
            Watcher::start(ctx, cfg, registry.clone(), store)
                .await
                .unwrap(),
        );
        watcher
            .spawn(&crate::config::Config::default())
            .await
            .unwrap();
        let app = router(AppState {
            registry,
            watchers: vec![watcher],
            token: Some(Arc::from("test-token")),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base_url = format!("http://{address}");
        let http = reqwest::Client::new();
        assert_eq!(
            http.get(format!("{base_url}/healthz"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            http.get(format!("{base_url}/api/stacks"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            http.get(format!("{base_url}/api/stacks"))
                .bearer_auth("wrong-token")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let client = ifx::DaemonClient::new(&base_url)
            .unwrap()
            .with_token("test-token");

        assert_eq!(
            http.post(format!("{base_url}/api/v1/stacks/api-test/revisions"))
                .bearer_auth("test-token")
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "clients cannot submit arbitrary Program revisions"
        );

        let built = client.retry_build("api-test").await.unwrap();
        assert_eq!(built.phase, BuildPhase::Ready, "{:?}", built.error);
        let revision = client
            .resolve("api-test", &ProgramResolveRequest::default())
            .await
            .unwrap();
        assert_eq!(
            client.active_revision("api-test").await.unwrap().revision,
            revision.revision
        );
        let run = client
            .start_run("api-test", &RunRequest::new(RunKind::Apply))
            .await
            .unwrap();
        let run = client.wait_run(&run.run_id, |_| {}).await.unwrap();
        assert_eq!(run.status, ExecutionStatus::Succeeded);
        let topology = client.topology("api-test", true).await.unwrap();
        assert_eq!(
            topology.active_revision.as_deref(),
            Some(revision.revision.as_str())
        );
        assert!(
            topology
                .executions
                .iter()
                .any(|execution| execution.run_id == run.run_id)
        );

        let urn = Urn::new("memory.value", "from-api");
        assert!(
            client
                .state("api-test")
                .await
                .unwrap()
                .resources
                .contains_key(&urn)
        );
        client
            .forget_state("api-test", &urn.to_string())
            .await
            .unwrap();
        assert!(client.state("api-test").await.unwrap().resources.is_empty());

        let generation = client.build_status("api-test").await.unwrap().generation;
        std::fs::write(dir.path().join("src/main.rs"), "this is not Rust\n").unwrap();
        assert!(
            client
                .resolve("api-test", &ProgramResolveRequest::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("build is Failed"),
            "resolution must scan for edits without waiting for the polling tick"
        );
        let failed = wait_for_build(&client, BuildPhase::Failed, generation + 1).await;
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("could not compile")),
            "{:?}",
            failed.error
        );
        assert!(
            client
                .resolve("api-test", &ProgramResolveRequest::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("build is Failed"),
            "a failed source generation must not resolve the previous Program"
        );
        write_test_stack(dir.path());
        let recovered = wait_for_build(&client, BuildPhase::Ready, failed.generation + 1).await;
        assert!(recovered.revision.is_some());

        std::fs::remove_dir_all(dir.path()).unwrap();
        assert!(
            client
                .resolve("api-test", &ProgramResolveRequest::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("build is Failed"),
            "resolution must fail closed before the polling tick"
        );
        let scan_failed =
            wait_for_build(&client, BuildPhase::Failed, recovered.generation + 1).await;
        assert!(
            scan_failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("scanning Rust stack sources")),
            "{:?}",
            scan_failed.error
        );
        assert!(
            client
                .resolve("api-test", &ProgramResolveRequest::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("build is Failed"),
            "an unavailable source root must invalidate the active generation"
        );
        write_test_stack(dir.path());
        let scan_recovered =
            wait_for_build(&client, BuildPhase::Ready, scan_failed.generation + 1).await;
        assert!(scan_recovered.revision.is_some());
        server.abort();
    }

    #[tokio::test]
    async fn lease_api_round_trips_through_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ifx::Registry::builtin();
        let ctx = ifx::LoadCtx::from_dir(dir.path(), None, "lease-api", &[]).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            SurrealStore::connect("mem://", "ifx", "lease-api")
                .await
                .unwrap(),
        );
        let cfg = crate::config::StackConfig {
            dir: dir.path().to_path_buf(),
            name: "lease-api".into(),
            file: None,
            check_interval: None,
            drift_interval: None,
            no_drift: true,
            require_lease: false,
        };
        let watcher = Arc::new(
            Watcher::start(ctx, cfg, registry.clone(), store)
                .await
                .unwrap(),
        );
        let app = router(AppState {
            registry,
            watchers: vec![watcher],
            token: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ifx::DaemonClient::new(&format!("http://{address}")).unwrap();

        let err = client.lease("lease-api").await.unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
        let err = client
            .set_lease(
                "lease-api",
                &LeaseRequest {
                    deadline: Some(chrono::Utc::now() - chrono::TimeDelta::seconds(1)),
                    ..LeaseRequest::default()
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"), "{err}");
        let lease = client
            .set_lease(
                "lease-api",
                &LeaseRequest {
                    duration: Some(Duration::from_secs(600)),
                    ..LeaseRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(lease.stack, "lease-api");
        let extended = client
            .extend_lease(
                "lease-api",
                &LeaseExtension {
                    by: Duration::from_secs(60),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            extended.deadline,
            lease.deadline + chrono::TimeDelta::seconds(60)
        );
        assert_eq!(client.lease("lease-api").await.unwrap(), extended);
        assert_eq!(
            client.status("lease-api").await.unwrap().lease,
            Some(extended)
        );
        client.clear_lease("lease-api").await.unwrap();
        let err = client.lease("lease-api").await.unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
        server.abort();
    }

    async fn wait_for_build(
        client: &ifx::DaemonClient,
        phase: BuildPhase,
        minimum_generation: u64,
    ) -> ifx::StackBuildStatus {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = client.build_status("api-test").await.unwrap();
                if status.generation >= minimum_generation && status.phase == phase {
                    return status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("source watcher should publish a terminal build state")
    }

    fn write_test_stack(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"ifxd-api-test\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"ifxd-api-test\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            r##"fn main() { println!("{}", r#"{"resources":[{"urn":"memory.value:from-api","inputs":{"value":"ok"},"depends_on":[],"triggers":[],"protect":false}]}"#); }
"##,
        )
        .unwrap();
    }
}
