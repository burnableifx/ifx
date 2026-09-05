use std::sync::Arc;

use ifx::engine::{Engine, Options};
use ifx::model::{Connection, Program, ResourceDecl, SshActivation, Urn};
use ifx::monitor;
use ifx::provider::Registry;
use ifx::state::{Entry, State};
use ifx::store::{Health, RunKind, Store, SurrealStore};
use serde_json::{Value, json};

async fn engine() -> (Engine, Arc<dyn Store>) {
    let store: Arc<dyn Store> =
        Arc::new(SurrealStore::connect("mem://", "ifx", "t").await.unwrap());
    (
        Engine::new(Registry::builtin()).with_store(store.clone()),
        store,
    )
}

fn execution_scoped_connection() -> Connection {
    Connection::Ssh {
        host: "127.0.0.1".into(),
        user: Some("debian".into()),
        port: Some(9),
        identity: None,
        connect_timeout_secs: Some(1),
        sudo: true,
        extra_args: Vec::new(),
        via: None,
        activation: Some(Box::new(SshActivation {
            monitor: "/does/not/exist".into(),
            lease: String::new(),
            host_port: 9,
            guest_port: 22,
            netdev_id: "mgmt".into(),
            device_id: "ifx-mgmt".into(),
            mac: "52:54:00:12:34:56".into(),
            persistent_netdev: false,
            reopen: true,
        })),
    }
}

#[tokio::test]
async fn checks_apply_run_and_record() {
    let (engine, store) = engine().await;
    let mut state = State {
        stack: "s".into(),
        ..State::default()
    };
    let program = Program {
        resources: vec![
            ResourceDecl::new("memory.value", "a", json!({"value": 1})),
            ResourceDecl::new(
                "check.exec",
                "ok",
                json!({"on": {"kind": "local"}, "command": "echo fine"}),
            ),
            ResourceDecl::new(
                "check.exec",
                "bad",
                json!({"on": {"kind": "local"}, "command": "echo nope >&2; exit 3"}),
            ),
            ResourceDecl::new(
                "check.tcp",
                "closed",
                json!({"host": "127.0.0.1", "port": 1, "timeout_secs": 2}),
            ),
        ],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert_eq!(plan.summary().create, 4);
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok(), "{:?}", report.failed);
    let ok = state.get(&Urn::new("check.exec", "ok")).unwrap();
    assert_eq!(ok.outputs["status"], "healthy");
    assert_eq!(ok.outputs["message"], "fine");
    let bad = state.get(&Urn::new("check.exec", "bad")).unwrap();
    assert_eq!(bad.outputs["status"], "unhealthy");
    assert!(
        bad.outputs["message"].as_str().unwrap().contains("exit 3"),
        "{}",
        bad.outputs
    );
    assert_eq!(
        state.get(&Urn::new("check.tcp", "closed")).unwrap().outputs["status"],
        "unhealthy"
    );

    // Idempotent: checks are not re-run by plan.
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert!(!plan.has_changes());

    // Persisted through the store, with a run + events.
    let loaded = store.load_state("s").await.unwrap();
    assert_eq!(loaded.resources.len(), 4);
    let runs = store.runs("s", 10).await.unwrap();
    assert!(
        runs.iter()
            .any(|r| r.kind == RunKind::Apply && r.ok == Some(true))
    );
    let apply = runs.iter().find(|r| r.kind == RunKind::Apply).unwrap();
    assert_eq!(store.events(&apply.run_id).await.unwrap().len(), 4);

    // run_checks records health for the three checks only.
    let records = monitor::run_checks(&engine, &state).await.unwrap();
    assert_eq!(records.len(), 3);
    let latest = store.latest_health("s").await.unwrap();
    assert_eq!(latest.len(), 3);
    let by = monitor::by_urn(&latest);
    assert_eq!(by[&Urn::new("check.exec", "ok")].status, Health::Healthy);
    assert_eq!(by[&Urn::new("check.exec", "bad")].status, Health::Unhealthy);

    // Drift: everything matches; then a removed resource shows as drifted.
    let drift = monitor::detect_drift(&engine, &program, &state)
        .await
        .unwrap();
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].status, Health::Healthy);
    let drift = monitor::detect_drift(&engine, &Program::default(), &state)
        .await
        .unwrap();
    assert!(drift.iter().any(|r| r.status == Health::Drifted));

    let status = monitor::stack_status(&engine, "s").await.unwrap();
    assert_eq!(status.resources, 4);
    assert_eq!(status.checks, 3);
    assert_eq!(status.overall, Some(Health::Drifted));
}

#[tokio::test]
async fn required_check_fails_apply() {
    let (engine, _store) = engine().await;
    let mut state = State {
        stack: "s".into(),
        ..State::default()
    };
    let program = Program {
        resources: vec![ResourceDecl::new(
            "check.exec",
            "must",
            json!({"on": {"kind": "local"}, "command": "false", "required": true}),
        )],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(!report.ok());
    assert!(
        report.failed[0].1.contains("unhealthy"),
        "{}",
        report.failed[0].1
    );
}

#[tokio::test]
async fn scheduled_check_does_not_reopen_execution_scoped_management() {
    let (engine, _store) = engine().await;
    let connection = execution_scoped_connection();
    let mut state = State {
        stack: "sealed".into(),
        ..State::default()
    };
    state.upsert(
        Urn::new("check.exec", "sealed"),
        Entry {
            id: None,
            inputs: json!({"on": connection, "command": "true"}),
            outputs: Value::Null,
            depends_on: Vec::new(),
            protect: false,
        },
    );

    let records = monitor::run_checks(&engine, &state).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, Health::Unknown);
    assert!(records[0].message.contains("management is sealed"));
}

#[tokio::test]
async fn plan_and_noop_apply_do_not_reopen_execution_scoped_management() {
    let (engine, _store) = engine().await;
    let inputs = json!({
        "on": execution_scoped_connection(),
        "path": "/tmp/ifx-sealed-plan-test",
        "content": "unchanged",
        "state": "present",
        "directory": false,
        "privileged": false,
    });
    let urn = Urn::new("host.file", "sealed");
    let mut state = State {
        stack: "sealed-plan".into(),
        ..State::default()
    };
    state.upsert(
        urn.clone(),
        Entry {
            id: Some("/tmp/ifx-sealed-plan-test".into()),
            inputs: inputs.clone(),
            outputs: json!({
                "path": "/tmp/ifx-sealed-plan-test",
                "state": "file",
                "sha256": null,
            }),
            depends_on: Vec::new(),
            protect: false,
        },
    );
    let program = Program {
        resources: vec![ResourceDecl::new("host.file", "sealed", inputs)],
    };

    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert!(!plan.has_changes());
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok());
    assert!(report.applied.is_empty());
    assert!(state.get(&urn).is_some());
}

#[tokio::test]
async fn http_check_against_mock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200).set_body_string("all good"))
        .mount(&server)
        .await;
    let (engine, _store) = engine().await;
    let mut state = State {
        stack: "s".into(),
        ..State::default()
    };
    let program = Program {
        resources: vec![
            ResourceDecl::new(
                "check.http",
                "up",
                json!({"url": format!("{}/healthz", server.uri()), "expect_body": "good"}),
            ),
            ResourceDecl::new(
                "check.http",
                "wrong",
                json!({"url": format!("{}/healthz", server.uri()), "expect_status": 204}),
            ),
        ],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(
        state.get(&Urn::new("check.http", "up")).unwrap().outputs["status"],
        "healthy"
    );
    let wrong = state.get(&Urn::new("check.http", "wrong")).unwrap();
    assert_eq!(wrong.outputs["status"], "unhealthy");
    assert!(
        wrong.outputs["message"]
            .as_str()
            .unwrap()
            .contains("expected 204")
    );
}
