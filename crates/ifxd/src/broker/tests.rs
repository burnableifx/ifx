use std::sync::Mutex;

use super::*;

const CALLER: &str = "fixture-client-credential-000000000000";
const UPSTREAM: &str = "fixture-upstream-credential";

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct CapturedRequest {
    method: String,
    path: String,
    authorization: String,
    body: Value,
}

#[derive(Clone, Default)]
struct Upstream {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    reply: Arc<Mutex<(StatusCode, Value)>>,
    location: Arc<Mutex<Option<String>>>,
}

async fn receive(State(state): State<Upstream>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let token = request
        .headers()
        .get("Authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = to_bytes(request.into_body(), 8192).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    state.requests.lock().unwrap().push(CapturedRequest {
        method,
        path,
        authorization: token,
        body,
    });
    let (status, value) = state.reply.lock().unwrap().clone();
    let mut response = (status, axum::Json(value)).into_response();
    response
        .headers_mut()
        .insert("Set-Cookie", HeaderValue::from_static("sensitive=value"));
    if let Some(location) = state.location.lock().unwrap().as_ref() {
        response
            .headers_mut()
            .insert("Location", HeaderValue::from_str(location).unwrap());
    }
    response
}

fn config(url: &str) -> (TargetConfig, tempfile::NamedTempFile) {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), UPSTREAM).unwrap();
    (
        TargetConfig {
            name: "executor".into(),
            backend: Backend::Ifxd,
            url: url.into(),
            token_file: file.path().into(),
            account_id: None,
            stacks: vec![StackBinding {
                name: "lab".into(),
                remote: "tenant-owned-stack".into(),
                manifest: None,
            }],
            grants: vec![Grant {
                name: "laptop".into(),
                token_sha256: hex::encode(Sha256::digest(CALLER)),
                stacks: vec!["lab".into()],
                operations: vec![
                    Operation::Status,
                    Operation::Plan,
                    Operation::Ignite,
                    Operation::Extinguish,
                ],
                expires_at: Utc::now() + chrono::Duration::hours(1),
            }],
        },
        file,
    )
}

async fn send(server: &Server, action: &str) -> reqwest::Response {
    Client::new()
        .post(format!(
            "{}/api/v1/brokers/executor/stacks/lab/{action}",
            server.url
        ))
        .bearer_auth(CALLER)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn upstream_credential_stays_at_broker_and_status_is_projected() {
    let upstream = Upstream::default();
    *upstream.reply.lock().unwrap() = (
        StatusCode::OK,
        json!({"stack":"tenant-owned-stack","resources":2,"overall":ifx::store::Health::Drifted,"health":[{"message":UPSTREAM}],"build":{"error":"unrelated-private-material"}}),
    );
    let endpoint =
        Server::start(Router::new().fallback(receive).with_state(upstream.clone())).await;
    let (config, _file) = config(&endpoint.url);
    let broker = Server::start(router(Broker::load(&[config]).unwrap())).await;
    let response = send(&broker, "status").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("Set-Cookie"));
    assert_eq!(response.headers()["Cache-Control"], "no-store");
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["resources"], 2);
    assert_eq!(value["state"], "drifted");
    assert_eq!(value["stack"], "lab");
    assert!(!value.to_string().contains(UPSTREAM));
    assert!(!value.to_string().contains("unrelated-private-material"));
    assert!(value.get("build").is_none());
    let requests = upstream.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/stacks/tenant-owned-stack");
    assert_eq!(requests[0].authorization, format!("Bearer {UPSTREAM}"));
    assert_ne!(requests[0].authorization, format!("Bearer {CALLER}"));
    assert_eq!(requests[0].body, Value::Null);
}

#[tokio::test]
async fn grants_deny_wrong_credentials_scopes_expiry_and_executor_admin_routes() {
    let upstream = Upstream::default();
    let endpoint =
        Server::start(Router::new().fallback(receive).with_state(upstream.clone())).await;
    let (mut config, _file) = config(&endpoint.url);
    config.grants[0].operations = vec![Operation::Status];
    config.stacks.push(StackBinding {
        name: "another-stack".into(),
        remote: "another-tenant".into(),
        manifest: None,
    });
    let broker = Server::start(router(Broker::load(&[config.clone()]).unwrap())).await;
    let http = Client::new();
    let route = format!("{}/api/v1/brokers/executor/stacks/lab/status", broker.url);
    assert_eq!(
        http.post(&route).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(&route)
            .bearer_auth("incorrect-credential-0000000000000")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&broker, "ignite").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(route.replace("/lab/", "/another-stack/"))
            .bearer_auth(CALLER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(format!("{route}?url=http://127.0.0.1"))
            .bearer_auth(CALLER)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        http.post(&route)
            .bearer_auth(CALLER)
            .json(&json!({"command":"anything","token":"anything"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        http.post(format!(
            "{}/api/v1/stacks/tenant-owned-stack/runs",
            broker.url
        ))
        .bearer_auth(CALLER)
        .json(&json!({"kind":"apply"}))
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::NOT_FOUND
    );
    config.grants[0].expires_at = Utc::now() - chrono::Duration::seconds(1);
    let expired = Server::start(router(Broker::load(&[config]).unwrap())).await;
    assert_eq!(
        send(&expired, "status").await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(upstream.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lifecycle_calls_are_fixed_and_preserve_accepted_waiting_states() {
    let upstream = Upstream::default();
    let endpoint =
        Server::start(Router::new().fallback(receive).with_state(upstream.clone())).await;
    let (config, _file) = config(&endpoint.url);
    let broker = Server::start(router(Broker::load(&[config]).unwrap())).await;
    for (action, kind, state) in [
        ("plan", "plan", "queued"),
        ("ignite", "apply", "approval_wait"),
        ("extinguish", "destroy", "recovery_wait"),
    ] {
        *upstream.reply.lock().unwrap() = (
            StatusCode::ACCEPTED,
            json!({"stack":"tenant-owned-stack","run_id":"run-123","status":state,"error":UPSTREAM,"result":{"private":"material"}}),
        );
        let response = send(&broker, action).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["state"], state);
        assert_eq!(value["run_id"], "run-123");
        assert!(!value.to_string().contains(UPSTREAM));
        let requests = upstream.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/stacks/tenant-owned-stack/runs");
        assert_eq!(request.body, json!({"kind":kind}));
    }
    assert_eq!(upstream.requests.lock().unwrap().len(), 3);
    *upstream.reply.lock().unwrap() = (
        StatusCode::ACCEPTED,
        json!({"stack":"tenant-owned-stack","run_id":UPSTREAM,"status":"queued"}),
    );
    let response = send(&broker, "ignite").await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.text().await.unwrap().contains(UPSTREAM));
}

#[tokio::test]
async fn redirects_failures_and_oversized_responses_cannot_leak_or_trigger_retries() {
    let upstream = Upstream::default();
    let sink = Upstream::default();
    let sink_server = Server::start(Router::new().fallback(receive).with_state(sink.clone())).await;
    let endpoint =
        Server::start(Router::new().fallback(receive).with_state(upstream.clone())).await;
    let (config, _file) = config(&endpoint.url);
    let broker = Server::start(router(Broker::load(&[config]).unwrap())).await;
    *upstream.location.lock().unwrap() = Some(sink_server.url.clone());
    for (status, body) in [
        (StatusCode::TEMPORARY_REDIRECT, json!({"error":UPSTREAM})),
        (StatusCode::INTERNAL_SERVER_ERROR, json!({"error":UPSTREAM})),
        (StatusCode::OK, json!({"data":"x".repeat(1024 * 1024)})),
    ] {
        *upstream.reply.lock().unwrap() = (status, body);
        let response = send(&broker, "ignite").await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.headers().contains_key("Location"));
        assert!(!response.text().await.unwrap().contains(UPSTREAM));
    }
    assert!(sink.requests.lock().unwrap().is_empty());
    assert_eq!(upstream.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn sandbox_adapter_uses_pinned_manifest_and_checks_account_before_mutations() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = requests.clone();
    let endpoint = Server::start(Router::new().fallback(move |request: Request| {
        let observed = observed.clone();
        async move {
            assert_eq!(request.headers()["Authorization"], format!("Bearer {UPSTREAM}"));
            let path = request.uri().path().to_string();
            if path == "/api/account" { return axum::Json(json!({"id":"11111111-1111-4111-8111-111111111111","sandbox":true})); }
            let bytes = to_bytes(request.into_body(), 8192).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            observed.lock().unwrap().push(body);
            axum::Json(json!({"sandbox":true,"stack":{"id":"22222222-2222-4222-8222-222222222222","status":"running","resources":[{},{}],"private":UPSTREAM}}))
        }
    })).await;
    let (mut config, _file) = config(&endpoint.url);
    config.backend = Backend::BurnableSandbox;
    config.account_id = Some("11111111-1111-4111-8111-111111111111".into());
    config.stacks[0].remote = "22222222-2222-4222-8222-222222222222".into();
    config.stacks[0].manifest = Some(json!({"package":"operator-owned"}));
    let broker = Server::start(router(Broker::load(&[config.clone()]).unwrap())).await;
    let response = send(&broker, "ignite").await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["state"], "running");
    assert_eq!(value["resources"], 2);
    assert!(!value.to_string().contains(UPSTREAM));
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            json!({"request":"22222222-2222-4222-8222-222222222222","manifest":"{\"package\":\"operator-owned\"}"})
        ]
    );
    config.account_id = Some("33333333-3333-4333-8333-333333333333".into());
    let mismatch = Server::start(router(Broker::load(&[config]).unwrap())).await;
    assert_eq!(
        send(&mismatch, "ignite").await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn configuration_fails_closed_on_destinations_bindings_and_credential_reuse() {
    let (config, _file) = config("http://127.0.0.1:7433");
    for url in [
        "http://example.com",
        "http://localhost:7433",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/?token=x",
        "file:///tmp/token",
    ] {
        let mut invalid = config.clone();
        invalid.url = url.into();
        assert!(Broker::load(&[invalid]).is_err());
    }
    let mut invalid = config.clone();
    invalid.grants[0].stacks = vec!["not-registered".into()];
    assert!(Broker::load(&[invalid]).is_err());
    let mut invalid = config.clone();
    invalid.stacks[0].remote = "../another-stack".into();
    assert!(Broker::load(&[invalid]).is_err());
    let mut invalid = config.clone();
    invalid.grants[0].token_sha256 = hex::encode(Sha256::digest(UPSTREAM));
    assert!(Broker::load(&[invalid]).is_err());
    assert!(Broker::load(&[config.clone(), config]).is_err());
}
