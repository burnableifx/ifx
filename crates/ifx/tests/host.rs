//! Integration tests for the `host` provider over a local connection, in a temp dir,
//! without root. Package, service and user resources need root and are covered by
//! unit tests on their parsers instead.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ifx::engine::{Action, Engine, Options};
use ifx::model::{Program, ResourceDecl, Urn};
use ifx::provider::Registry;
use ifx::state::State;
use serde_json::{Value, json};

fn engine() -> Engine {
    let mut reg = Registry::new();
    ifx::providers::host::register(&mut reg);
    Engine::new(reg)
}

fn local() -> Value {
    json!({"kind": "local"})
}

fn file(name: &str, mut inputs: Value) -> ResourceDecl {
    inputs["on"] = local();
    ResourceDecl::new("host.file", name, inputs)
}

fn exec(name: &str, mut inputs: Value) -> ResourceDecl {
    inputs["on"] = local();
    ResourceDecl::new("host.exec", name, inputs)
}

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

fn lines(p: &Path) -> usize {
    std::fs::read_to_string(p)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Plan + apply, asserting the apply succeeded; returns the plan.
async fn converge(engine: &Engine, program: &Program, state: &mut State) -> ifx::Plan {
    let plan = engine
        .plan(program, state, &Options::default())
        .await
        .unwrap();
    let report = engine.apply(program, state, &plan).await.unwrap();
    assert!(report.ok(), "{:?}", report.failed);
    plan
}

async fn assert_stable(engine: &Engine, program: &Program, state: &State) {
    let plan = engine
        .plan(program, state, &Options::default())
        .await
        .unwrap();
    let changes: Vec<_> = plan
        .ops
        .iter()
        .filter(|o| o.action.is_change())
        .map(|o| (o.urn.clone(), o.action.clone()))
        .collect();
    assert!(changes.is_empty(), "expected no changes, got {changes:?}");
}

#[tokio::test]
async fn file_create_update_mode_and_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("motd");
    let ps = path.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();
    let urn = Urn::new("host.file", "motd");

    let program = Program {
        resources: vec![file(
            "motd",
            json!({"path": ps, "content": "hi\n", "mode": "0600"}),
        )],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Create));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\n");
    assert_eq!(mode_of(&path), 0o600);
    let entry = state.get(&urn).unwrap();
    assert_eq!(entry.outputs["exists"], true);
    assert_eq!(entry.outputs["sha256"].as_str().unwrap().len(), 64);
    assert_stable(&engine, &program, &state).await;

    // Content change is an in-place update.
    let program = Program {
        resources: vec![file(
            "motd",
            json!({"path": ps, "content": "bye\n", "mode": "0600"}),
        )],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    match &plan.get(&urn).unwrap().action {
        Action::Update(d) => {
            assert_eq!(d.changes.len(), 1);
            assert_eq!(d.changes[0].field, "content");
            assert_eq!(d.changes[0].from, Some(json!("hi\n")));
        }
        other => panic!("expected update, got {other:?}"),
    }
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "bye\n");

    // Unpadded mode is normalized; only the mode changes.
    let program = Program {
        resources: vec![file(
            "motd",
            json!({"path": ps, "content": "bye\n", "mode": "644"}),
        )],
    };
    let plan = converge(&engine, &program, &mut state).await;
    match &plan.get(&urn).unwrap().action {
        Action::Update(d) => assert_eq!(
            d.changes
                .iter()
                .map(|c| c.field.as_str())
                .collect::<Vec<_>>(),
            ["mode"]
        ),
        other => panic!("expected update, got {other:?}"),
    }
    assert_eq!(mode_of(&path), 0o644);
    assert_stable(&engine, &program, &state).await;

    // Drift: someone edits the file out of band.
    std::fs::write(&path, "tampered").unwrap();
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Update(_)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "bye\n");

    // Omitting `mode` keeps the existing permissions on rewrite.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let program = Program {
        resources: vec![file("motd", json!({"path": ps, "content": "again\n"}))],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(mode_of(&path), 0o640);

    // Absent removes it; a second plan is a no-op.
    let program = Program {
        resources: vec![file("motd", json!({"path": ps, "state": "absent"}))],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Update(_)));
    assert!(!path.exists());
    assert_eq!(state.get(&urn).unwrap().outputs["exists"], false);
    assert_stable(&engine, &program, &state).await;

    // Back to present from absent.
    let program = Program {
        resources: vec![file("motd", json!({"path": ps, "content": "back\n"}))],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "back\n");
    assert_eq!(mode_of(&path), 0o644);
}

#[tokio::test]
async fn file_directory_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a/b");
    let ps = path.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();

    let program = Program {
        resources: vec![file(
            "d",
            json!({"path": ps, "directory": true, "mode": "0700"}),
        )],
    };
    converge(&engine, &program, &mut state).await;
    assert!(path.is_dir());
    assert_eq!(mode_of(&path), 0o700);
    assert_eq!(
        state.get(&Urn::new("host.file", "d")).unwrap().outputs["sha256"],
        Value::Null
    );
    assert_stable(&engine, &program, &state).await;

    // Removing the resource deletes the directory (recursively).
    std::fs::write(path.join("inner"), "x").unwrap();
    let empty = Program::default();
    let plan = converge(&engine, &empty, &mut state).await;
    assert_eq!(plan.summary().delete, 1);
    assert!(!path.exists());
    assert!(state.resources.is_empty());
}

#[tokio::test]
async fn file_source_and_replace_on_path_change() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    std::fs::write(&src, [0u8, 159, 146, 150]).unwrap(); // not UTF-8
    let p1 = dir.path().join("one");
    let p2 = dir.path().join("two");
    let engine = engine();
    let mut state = State::default();
    let urn = Urn::new("host.file", "f");

    let program = Program {
        resources: vec![file(
            "f",
            json!({"path": p1.to_str().unwrap(), "source": src.to_str().unwrap()}),
        )],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(std::fs::read(&p1).unwrap(), [0u8, 159, 146, 150]);
    assert_stable(&engine, &program, &state).await;

    // Source bytes change -> hash diff on `source`.
    std::fs::write(&src, b"new").unwrap();
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    match &plan.get(&urn).unwrap().action {
        Action::Update(d) => assert_eq!(d.changes[0].field, "source"),
        other => panic!("{other:?}"),
    }
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(std::fs::read(&p1).unwrap(), b"new");

    // Path change replaces: old removed, new created.
    let program = Program {
        resources: vec![file(
            "f",
            json!({"path": p2.to_str().unwrap(), "source": src.to_str().unwrap()}),
        )],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Replace(_)));
    assert!(!p1.exists());
    assert_eq!(std::fs::read(&p2).unwrap(), b"new");
}

#[tokio::test]
async fn matching_resources_missing_from_state_are_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x");
    std::fs::write(&p, "hi").unwrap();
    let engine = engine();
    let mut state = State::default();
    let program = Program {
        resources: vec![file(
            "x",
            json!({"path": p.to_str().unwrap(), "content": "hi"}),
        )],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert!(
        matches!(plan.ops[0].action, Action::Adopt),
        "{:?}",
        plan.ops[0].action
    );
    assert!(plan.has_changes());
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok());
    assert!(state.get(&Urn::new("host.file", "x")).is_some());
    assert_eq!(std::fs::read(&p).unwrap(), b"hi");
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert!(!plan.has_changes());
    let destroy = engine
        .plan(&Program::default(), &state, &Options::default())
        .await
        .unwrap();
    assert_eq!(destroy.summary().delete, 1);
}

#[tokio::test]
async fn file_content_and_source_are_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x");
    let engine = engine();
    let mut state = State::default();
    let program = Program {
        resources: vec![file(
            "x",
            json!({"path": p.to_str().unwrap(), "content": "a", "source": "b"}),
        )],
    };
    let err = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("only one of `content`, `source`"), "{err}");
    let _ = &mut state;
}

#[tokio::test]
async fn exec_creates_guard() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let ms = marker.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();
    let urn = Urn::new("host.exec", "touch");

    let program = Program {
        resources: vec![exec(
            "touch",
            json!({"command": format!("touch {ms}"), "creates": ms}),
        )],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Create));
    assert!(marker.exists());
    assert_stable(&engine, &program, &state).await;

    // Guard wins over a command change.
    let program = Program {
        resources: vec![exec(
            "touch",
            json!({"command": format!("touch {ms} && echo changed"), "creates": ms}),
        )],
    };
    assert_stable(&engine, &program, &state).await;

    // Guard no longer satisfied -> runs again as a create.
    std::fs::remove_file(&marker).unwrap();
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.get(&urn).unwrap().action, Action::Create));
    assert!(marker.exists());
}

#[tokio::test]
async fn exec_unless_and_only_if() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let ls = log.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();

    // only_if fails -> never runs: the first plan only adopts it into state.
    let program = Program {
        resources: vec![exec(
            "never",
            json!({"command": format!("echo x >> {ls}"), "only_if": "false"}),
        )],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(plan.ops[0].action, Action::Adopt));
    assert_stable(&engine, &program, &state).await;
    assert_eq!(lines(&log), 0);

    // unless fails -> runs every time the guard is not satisfied.
    let program = Program {
        resources: vec![exec(
            "append",
            json!({"command": format!("echo x >> {ls}"), "unless": format!("test $(wc -l < {ls}) -ge 2")}),
        )],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(lines(&log), 1);
    converge(&engine, &program, &mut state).await;
    assert_eq!(lines(&log), 2);
    assert_stable(&engine, &program, &state).await;

    // only_if succeeds -> runs.
    let program = Program {
        resources: vec![exec(
            "cond",
            json!({"command": format!("echo y >> {ls}"), "only_if": "true"}),
        )],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(lines(&log), 3);
}

#[tokio::test]
async fn exec_without_guards_runs_once_and_on_change_or_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let ls = log.to_str().unwrap();
    let conf = dir.path().join("conf");
    let cs = conf.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();
    let exec_urn = Urn::new("host.exec", "run");
    let conf_urn = Urn::new("host.file", "conf");

    let mut runner = exec(
        "run",
        json!({"command": format!("echo ran >> {ls}; echo out; echo err >&2")}),
    );
    runner.triggers = vec![conf_urn.clone()];
    let program = Program {
        resources: vec![
            file("conf", json!({"path": cs, "content": "v1"})),
            runner.clone(),
        ],
    };
    converge(&engine, &program, &mut state).await;
    assert_eq!(lines(&log), 1);
    let entry = state.get(&exec_urn).unwrap();
    assert_eq!(entry.outputs["stdout"], "out\n");
    assert_eq!(entry.outputs["stderr"], "err\n");
    assert_eq!(entry.outputs["status"], 0);
    assert!(entry.outputs["ran_at"].as_str().unwrap().ends_with('Z'));
    assert!(entry.id.is_some());

    // Runs once: re-applying does nothing.
    assert_stable(&engine, &program, &state).await;
    converge(&engine, &program, &mut state).await;
    assert_eq!(lines(&log), 1);

    // Trigger: the file it depends on changes.
    let program = Program {
        resources: vec![
            file("conf", json!({"path": cs, "content": "v2"})),
            runner.clone(),
        ],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(
        plan.get(&exec_urn).unwrap().action,
        Action::Trigger
    ));
    assert_eq!(lines(&log), 2);
    assert_stable(&engine, &program, &state).await;

    // Command change re-runs.
    let mut runner2 = exec("run", json!({"command": format!("echo again >> {ls}")}));
    runner2.triggers = vec![conf_urn.clone()];
    let program = Program {
        resources: vec![file("conf", json!({"path": cs, "content": "v2"})), runner2],
    };
    let plan = converge(&engine, &program, &mut state).await;
    assert!(matches!(
        plan.get(&exec_urn).unwrap().action,
        Action::Update(_)
    ));
    assert_eq!(lines(&log), 3);
    assert_stable(&engine, &program, &state).await;
}

#[tokio::test]
async fn exec_cwd_env_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().canonicalize().unwrap();
    let engine = engine();
    let mut state = State::default();

    let program = Program {
        resources: vec![exec(
            "env",
            json!({"command": "pwd; echo $GREETING", "cwd": ds.to_str().unwrap(), "env": {"GREETING": "hello world"}}),
        )],
    };
    converge(&engine, &program, &mut state).await;
    let out = &state.get(&Urn::new("host.exec", "env")).unwrap().outputs;
    assert_eq!(out["stdout"], format!("{}\nhello world\n", ds.display()));

    let program = Program {
        resources: vec![exec("boom", json!({"command": "echo nope >&2; exit 3"}))],
    };
    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(!report.ok());
    let err = &report.failed[0].1;
    assert!(err.contains("exited 3") && err.contains("nope"), "{err}");
    // A failed exec is not recorded, so it is attempted again next time.
    assert!(state.get(&Urn::new("host.exec", "boom")).is_none());
}

#[tokio::test]
async fn full_stack_plan_apply_plan_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    let rs = root.to_str().unwrap();
    let conf = root.join("app.conf");
    let cs = conf.to_str().unwrap();
    let engine = engine();
    let mut state = State::default();

    let mut reload = exec(
        "reload",
        json!({"command": format!("cp {cs} {rs}/app.conf.loaded")}),
    );
    reload.triggers = vec![Urn::new("host.file", "conf")];
    reload.depends_on = vec![Urn::new("host.file", "root")];
    let mut conf_decl = file(
        "conf",
        json!({"path": cs, "content": "port=80\n", "mode": "0640"}),
    );
    conf_decl.depends_on = vec![Urn::new("host.file", "root")];
    let mut init = exec(
        "init",
        json!({"command": format!("touch {rs}/.initialized"), "creates": format!("{rs}/.initialized")}),
    );
    init.depends_on = vec![Urn::new("host.file", "root")];
    let program = Program {
        resources: vec![
            reload,
            conf_decl,
            file("root", json!({"path": rs, "directory": true})),
            init,
        ],
    };

    let plan = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap();
    assert_eq!(plan.summary().create, 4);
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok(), "{:?}", report.failed);
    assert!(root.join("app.conf.loaded").exists());
    assert!(root.join(".initialized").exists());
    assert_stable(&engine, &program, &state).await;

    // Second apply changes nothing on disk either.
    let before = std::fs::metadata(root.join("app.conf.loaded"))
        .unwrap()
        .modified()
        .unwrap();
    converge(&engine, &program, &mut state).await;
    assert_eq!(
        std::fs::metadata(root.join("app.conf.loaded"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    assert_stable(&engine, &program, &state).await;

    // Tear everything down.
    let plan = converge(&engine, &Program::default(), &mut state).await;
    assert_eq!(plan.summary().delete, 4);
    assert!(!root.exists());
    assert!(state.resources.is_empty());
}

#[tokio::test]
async fn typed_rust_api_declares_host_resources() {
    use ifx::providers::host::{File, ServiceState};
    use ifx::{Connection, Stack};

    let mut s = Stack::new();
    let f = s
        .host_file("motd")
        .on(Connection::local())
        .path("/etc/motd")
        .content("hi")
        .mode("0644")
        .add()
        .unwrap();
    let p = s
        .host_package("tools")
        .on(Connection::local())
        .names(["curl", "git"])
        .update_cache(true)
        .add()
        .unwrap();
    let svc = s
        .host_service("nginx")
        .on(Connection::local())
        .name("nginx")
        .enabled(true)
        .state(ServiceState::Running)
        .triggered_by(&f)
        .add()
        .unwrap();
    let e = s
        .host_exec("hash")
        .on(Connection::local())
        .command("sha256sum /etc/motd")
        .env([("A", "1")])
        .cwd("/")
        .add()
        .unwrap();
    let u = s
        .add(
            File::builder("standalone")
                .on(Connection::local())
                .path("/tmp/x")
                .source("local.txt")
                .build(),
        )
        .map(|_| ())
        .and_then(|_| {
            s.host_user("deploy")
                .on(Connection::local())
                .name("deploy")
                .groups(["wheel"])
                .authorized_keys(["ssh-ed25519 AAA"])
                .add()
        })
        .unwrap();
    let _: ifx::Input<String> = f.sha256();
    let _: ifx::Input<Vec<String>> = p.installed();
    let _: ifx::Input<String> = svc.active_state();
    let _: ifx::Input<String> = e.stdout();
    let _: ifx::Input<i64> = u.uid();

    let program = s.into_program();
    let names: Vec<&str> = program.resources.iter().map(|r| r.urn.as_str()).collect();
    assert_eq!(
        names,
        [
            "host.file:motd",
            "host.package:tools",
            "host.service:nginx",
            "host.exec:hash",
            "host.file:standalone",
            "host.user:deploy"
        ]
    );
    assert_eq!(program.resources[2].triggers, vec![f.urn().clone()]);
    assert_eq!(
        program.resources[1].inputs,
        json!({"on": {"kind": "local", "sudo": false}, "names": ["curl", "git"], "update_cache": true})
    );

    // Every declaration validates against its schema.
    let reg = engine();
    for r in &program.resources {
        let schema = reg.registry().get(r.type_name()).unwrap().schema();
        let mut inputs = r.inputs.clone();
        schema.apply_defaults(&mut inputs);
        schema.validate(&inputs).unwrap();
        for f in schema.inputs.iter().chain(schema.outputs.iter()) {
            assert!(
                !f.doc.is_empty(),
                "{}.{} has no doc",
                schema.type_name,
                f.name
            );
        }
    }
}
