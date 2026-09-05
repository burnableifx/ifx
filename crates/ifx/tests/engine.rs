use async_trait::async_trait;
use ifx::engine::{Action, Engine, Options};
use ifx::model::{OutputRef, Program, ResourceDecl, Urn};
use ifx::provider::{
    Actual, Applied, Ctx, Handler, OperationKind, OperationRisk, Registry, Result,
};
use ifx::providers::memory::Memory;
use ifx::schema::ResourceSchema;
use ifx::state::State;
use ifx::store::{EventRecord, FileStore, HealthRecord, RunKind, RunRecord, Store};
use ifx::transport::TransportPool;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn setup() -> (Engine, Memory) {
    let mem = Memory::default();
    let mut reg = Registry::new();
    reg.register(mem.clone());
    (Engine::new(reg), mem)
}

fn decl(name: &str, inputs: serde_json::Value) -> ResourceDecl {
    ResourceDecl::new("memory.value", name, inputs)
}

#[derive(Clone)]
struct RiskyMemory(Memory);

#[derive(Clone)]
struct TransactionalMemory {
    memory: Memory,
    replaced: Arc<AtomicBool>,
    finalized: Arc<AtomicBool>,
}

#[derive(Clone)]
struct InterruptibleMemory {
    memory: Memory,
    fail_replace: Arc<AtomicBool>,
    finalized: Arc<AtomicBool>,
    recovery_read: Arc<AtomicBool>,
}

struct CommitFailStore {
    inner: FileStore,
    fail_commit: AtomicBool,
}

impl CommitFailStore {
    fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            inner: FileStore::new(path),
            fail_commit: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Store for CommitFailStore {
    fn describe(&self) -> String {
        self.inner.describe()
    }

    async fn load_state(&self, stack: &str) -> anyhow::Result<State> {
        self.inner.load_state(stack).await
    }

    async fn save_state(&self, state: &State) -> anyhow::Result<()> {
        self.inner.save_state(state).await
    }

    async fn prepare_replacement(&self, state: &State, urn: &Urn) -> anyhow::Result<()> {
        self.inner.prepare_replacement(state, urn).await
    }

    async fn commit_replacement(
        &self,
        state: &State,
        urn: &Urn,
        operation_id: &str,
    ) -> anyhow::Result<()> {
        if self.fail_commit.load(Ordering::SeqCst) {
            anyhow::bail!("simulated durable state commit failure");
        }
        self.inner
            .commit_replacement(state, urn, operation_id)
            .await
    }

    async fn begin_run(&self, stack: &str, kind: RunKind) -> anyhow::Result<String> {
        self.inner.begin_run(stack, kind).await
    }

    async fn finish_run(
        &self,
        run_id: &str,
        ok: bool,
        summary: serde_json::Value,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        self.inner.finish_run(run_id, ok, summary, error).await
    }

    async fn runs(&self, stack: &str, limit: usize) -> anyhow::Result<Vec<RunRecord>> {
        self.inner.runs(stack, limit).await
    }

    async fn record_event(&self, event: &EventRecord) -> anyhow::Result<()> {
        self.inner.record_event(event).await
    }

    async fn events(&self, run_id: &str) -> anyhow::Result<Vec<EventRecord>> {
        self.inner.events(run_id).await
    }

    async fn record_health(&self, health: &HealthRecord) -> anyhow::Result<()> {
        self.inner.record_health(health).await
    }

    async fn latest_health(&self, stack: &str) -> anyhow::Result<Vec<HealthRecord>> {
        self.inner.latest_health(stack).await
    }

    async fn health_history(
        &self,
        stack: &str,
        urn: &Urn,
        limit: usize,
    ) -> anyhow::Result<Vec<HealthRecord>> {
        self.inner.health_history(stack, urn, limit).await
    }

    async fn stacks(&self) -> anyhow::Result<Vec<String>> {
        self.inner.stacks().await
    }
}

#[async_trait]
impl Handler for InterruptibleMemory {
    fn schema(&self) -> ResourceSchema {
        self.memory.schema()
    }

    async fn read(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<Option<Actual>> {
        let snapshot = self.memory.snapshot();
        let found = match id {
            Some(id) => snapshot
                .get(id)
                .map(|value| (id.to_string(), value.clone())),
            None => snapshot.iter().find_map(|(id, value)| {
                (value.get("key") == inputs.get("key")).then(|| (id.clone(), value.clone()))
            }),
        };
        Ok(found.map(|(id, value)| Actual {
            id: Some(id.clone()),
            props: json!({
                "value": value.get("value").cloned().unwrap_or_default(),
                "key": value.get("key").cloned().unwrap_or_default(),
            }),
            outputs: json!({
                "id": id,
                "value": value.get("value").cloned().unwrap_or_default(),
                "key": value.get("key").cloned().unwrap_or_default(),
            }),
        }))
    }

    async fn read_for_recovery(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<Option<Actual>> {
        self.recovery_read.store(true, Ordering::SeqCst);
        self.read(cx, id, inputs).await
    }

    fn risks(
        &self,
        operation: OperationKind,
        _desired: &serde_json::Value,
        actual: Option<&Actual>,
    ) -> Result<Vec<OperationRisk>> {
        Ok(if operation == OperationKind::Replace && actual.is_some() {
            vec![OperationRisk {
                name: "replacement-test".into(),
                reason: "test replacement must be approved".into(),
            }]
        } else {
            Vec::new()
        })
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &serde_json::Value) -> Result<Applied> {
        self.memory.create(cx, inputs).await
    }

    async fn replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &serde_json::Value,
        inputs: &serde_json::Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        if self.fail_replace.load(Ordering::SeqCst) {
            anyhow::bail!("simulated interruption after journal preparation");
        }
        self.memory.delete(cx, id, old_inputs).await?;
        self.memory.create(cx, inputs).await
    }

    async fn finalize_replace(
        &self,
        _cx: &Ctx<'_>,
        _old_id: Option<&str>,
        _old_inputs: &serde_json::Value,
        _id: Option<&str>,
        _inputs: &serde_json::Value,
    ) -> Result<()> {
        self.finalized.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
        actual: &Actual,
    ) -> Result<Applied> {
        self.memory.update(cx, id, inputs, actual).await
    }

    async fn delete(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<()> {
        self.memory.delete(cx, id, inputs).await
    }
}

#[async_trait]
impl Handler for TransactionalMemory {
    fn schema(&self) -> ResourceSchema {
        self.memory.schema()
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<Option<Actual>> {
        self.memory.read(cx, id, inputs).await
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &serde_json::Value) -> Result<Applied> {
        self.memory.create(cx, inputs).await
    }

    async fn replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &serde_json::Value,
        inputs: &serde_json::Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        self.replaced.store(true, Ordering::SeqCst);
        self.memory.delete(cx, id, old_inputs).await?;
        self.memory.create(cx, inputs).await
    }

    async fn finalize_replace(
        &self,
        _cx: &Ctx<'_>,
        _old_id: Option<&str>,
        _old_inputs: &serde_json::Value,
        _id: Option<&str>,
        _inputs: &serde_json::Value,
    ) -> Result<()> {
        self.finalized.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
        actual: &Actual,
    ) -> Result<Applied> {
        self.memory.update(cx, id, inputs, actual).await
    }

    async fn delete(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<()> {
        self.memory.delete(cx, id, inputs).await
    }
}

#[async_trait]
impl Handler for RiskyMemory {
    fn schema(&self) -> ResourceSchema {
        self.0.schema()
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<Option<Actual>> {
        self.0.read(cx, id, inputs).await
    }

    fn risks(
        &self,
        operation: OperationKind,
        _desired: &serde_json::Value,
        _actual: Option<&Actual>,
    ) -> Result<Vec<OperationRisk>> {
        Ok(
            if matches!(operation, OperationKind::Update | OperationKind::Trigger) {
                vec![OperationRisk {
                    name: "restart".into(),
                    reason: "the test resource must restart".into(),
                }]
            } else {
                Vec::new()
            },
        )
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &serde_json::Value) -> Result<Applied> {
        self.0.create(cx, inputs).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
        actual: &Actual,
    ) -> Result<Applied> {
        self.0.update(cx, id, inputs, actual).await
    }

    async fn delete(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &serde_json::Value,
    ) -> Result<()> {
        self.0.delete(cx, id, inputs).await
    }
}

#[tokio::test]
async fn create_update_delete_lifecycle() {
    let (engine, mem) = setup();
    let mut state = State::default();
    let opts = Options::default();

    let program = Program {
        resources: vec![decl("a", json!({"value": 1, "key": "k1"}))],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert_eq!(plan.summary().create, 1);
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok());
    assert_eq!(state.resources.len(), 1);
    let id = state
        .get(&Urn::new("memory.value", "a"))
        .unwrap()
        .id
        .clone()
        .unwrap();
    assert_eq!(mem.snapshot()[&id]["value"], 1);

    // Idempotent.
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(!plan.has_changes());

    // Update in place.
    let program = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "k1"}))],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(matches!(plan.ops[0].action, Action::Update(_)));
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(mem.snapshot()[&id]["value"], 2);

    // Replace on `key`.
    let program = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "k2"}))],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(
        matches!(plan.ops[0].action, Action::Replace(_)),
        "{:?}",
        plan.ops[0].action
    );
    engine.apply(&program, &mut state, &plan).await.unwrap();
    let new_id = state
        .get(&Urn::new("memory.value", "a"))
        .unwrap()
        .id
        .clone()
        .unwrap();
    assert_ne!(new_id, id);
    assert!(!mem.snapshot().contains_key(&id));

    // Delete when removed from program.
    let program = Program::default();
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert_eq!(plan.summary().delete, 1);
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(state.resources.is_empty());
    assert!(mem.snapshot().is_empty());
}

#[tokio::test]
async fn replacement_uses_the_provider_lifecycle_hook() {
    let memory = Memory::default();
    let replaced = Arc::new(AtomicBool::new(false));
    let finalized = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::new();
    registry.register(TransactionalMemory {
        memory,
        replaced: replaced.clone(),
        finalized: finalized.clone(),
    });
    let engine = Engine::new(registry);
    let mut state = State::default();
    let options = Options::default();

    let initial = Program {
        resources: vec![decl("a", json!({"value": 1, "key": "old"}))],
    };
    let plan = engine.plan(&initial, &state, &options).await.unwrap();
    engine.apply(&initial, &mut state, &plan).await.unwrap();

    let replacement = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "new"}))],
    };
    let plan = engine.plan(&replacement, &state, &options).await.unwrap();
    engine.apply(&replacement, &mut state, &plan).await.unwrap();

    assert!(replaced.load(Ordering::SeqCst));
    assert!(finalized.load(Ordering::SeqCst));
}

#[tokio::test]
async fn interrupted_replacement_recovers_from_the_durable_journal() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state.json");
    let memory = Memory::default();
    let fail_replace = Arc::new(AtomicBool::new(false));
    let finalized = Arc::new(AtomicBool::new(false));
    let recovery_read = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::new();
    registry.register(InterruptibleMemory {
        memory: memory.clone(),
        fail_replace: fail_replace.clone(),
        finalized: finalized.clone(),
        recovery_read: recovery_read.clone(),
    });
    let engine = Engine::new(registry).with_store(Arc::new(FileStore::new(&state_path)));
    let options = Options::default();
    let mut state = State {
        stack: "journal-test".into(),
        ..State::default()
    };
    let initial = Program {
        resources: vec![decl("a", json!({"value": 1, "key": "old"}))],
    };
    let plan = engine.plan(&initial, &state, &options).await.unwrap();
    engine.apply(&initial, &mut state, &plan).await.unwrap();

    fail_replace.store(true, Ordering::SeqCst);
    let desired = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "new"}))],
    };
    let plan = engine.plan(&desired, &state, &options).await.unwrap();
    let approvals = plan.approvals().cloned().collect::<Vec<_>>();
    let report = engine
        .apply_approved(&desired, &mut state, &plan, &approvals)
        .await
        .unwrap();
    assert_eq!(report.failed.len(), 1);

    let urn = Urn::new("memory.value", "a");
    let mut persisted = State::load(&state_path).unwrap();
    let pending = &persisted.pending_replacements[&urn];
    assert_eq!(
        pending.approval_fingerprints["replacement-test"],
        approvals[0].fingerprint.as_deref().unwrap()
    );
    assert_eq!(persisted.resources[&urn].inputs["key"], "old");

    fail_replace.store(false, Ordering::SeqCst);
    let transports = TransportPool::default();
    let cx = Ctx {
        urn: &urn,
        transports: &transports,
        triggered: false,
    };
    let old_id = persisted.resources[&urn].id.clone();
    memory
        .update(
            &cx,
            old_id.as_deref(),
            &json!({"value": "drifted", "key": "old"}),
            &Actual::default(),
        )
        .await
        .unwrap();
    let waiting = engine
        .recover_pending_replacements(&mut persisted, "direct", &[])
        .await
        .unwrap();
    assert!(waiting.recovered.is_empty());
    assert_eq!(waiting.pending_approvals.len(), 1);
    assert_ne!(
        waiting.pending_approvals[0].fingerprint,
        approvals[0].fingerprint
    );
    assert!(persisted.pending_replacements.contains_key(&urn));

    memory
        .update(
            &cx,
            old_id.as_deref(),
            &json!({"value": 1, "key": "old"}),
            &Actual::default(),
        )
        .await
        .unwrap();
    let recovered = engine
        .recover_pending_replacements(&mut persisted, "direct", &approvals)
        .await
        .unwrap();
    assert_eq!(recovered.recovered.as_slice(), std::slice::from_ref(&urn));
    assert!(recovered.pending_approvals.is_empty());
    assert!(persisted.pending_replacements.is_empty());
    assert_eq!(persisted.resources[&urn].inputs["key"], "new");
    assert!(recovery_read.load(Ordering::SeqCst));
    assert!(finalized.load(Ordering::SeqCst));

    let committed = State::load(&state_path).unwrap();
    assert!(committed.pending_replacements.is_empty());
    assert_eq!(committed.resources[&urn].inputs["key"], "new");
}

#[tokio::test]
async fn recovery_adopts_a_provider_commit_after_the_state_commit_failed() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(CommitFailStore::new(directory.path().join("state.json")));
    let memory = Memory::default();
    let finalized = Arc::new(AtomicBool::new(false));
    let recovery_read = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::new();
    registry.register(InterruptibleMemory {
        memory: memory.clone(),
        fail_replace: Arc::new(AtomicBool::new(false)),
        finalized: finalized.clone(),
        recovery_read: recovery_read.clone(),
    });
    let engine = Engine::new(registry)
        .with_store(store.clone())
        .with_execution_identity("daemon-run", "revision-2");
    let options = Options::default();
    let mut state = State {
        stack: "commit-failure".into(),
        ..State::default()
    };
    let initial = Program {
        resources: vec![decl("a", json!({"value": 1, "key": "old"}))],
    };
    let plan = engine.plan(&initial, &state, &options).await.unwrap();
    engine.apply(&initial, &mut state, &plan).await.unwrap();

    let desired = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "new"}))],
    };
    let plan = engine.plan(&desired, &state, &options).await.unwrap();
    let approvals = plan.approvals().cloned().collect::<Vec<_>>();
    store.fail_commit.store(true, Ordering::SeqCst);
    let report = engine
        .apply_approved(&desired, &mut state, &plan, &approvals)
        .await
        .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(state.pending_replacements.len(), 1);
    assert_eq!(memory.snapshot().len(), 1);
    assert_eq!(memory.snapshot().values().next().unwrap()["key"], "new");
    assert!(
        !finalized.load(Ordering::SeqCst),
        "finalization must wait for durable successor state"
    );

    store.fail_commit.store(false, Ordering::SeqCst);
    let recovery = engine
        .recover_pending_replacements(&mut state, "daemon-run", &approvals)
        .await
        .unwrap();
    assert_eq!(recovery.recovered, [Urn::new("memory.value", "a")]);
    assert!(recovery.pending_approvals.is_empty());
    assert!(state.pending_replacements.is_empty());
    assert_eq!(
        state.resources[&Urn::new("memory.value", "a")].inputs["key"],
        "new"
    );
    assert_eq!(
        memory.snapshot().len(),
        1,
        "recovery must adopt the already-created successor"
    );
    assert!(recovery_read.load(Ordering::SeqCst));
    assert!(finalized.load(Ordering::SeqCst));

    let persisted = store.load_state("commit-failure").await.unwrap();
    assert!(persisted.pending_replacements.is_empty());
    assert_eq!(
        persisted.resources[&Urn::new("memory.value", "a")].inputs["key"],
        "new"
    );
}

#[tokio::test]
async fn references_flow_through_outputs_and_order_deletes() {
    let (engine, mem) = setup();
    let mut state = State::default();
    let opts = Options::default();
    let a = Urn::new("memory.value", "a");
    let program = Program {
        resources: vec![
            decl(
                "b",
                json!({"value": {"from_a": OutputRef::new(a.clone(), "value").to_value()}}),
            ),
            decl("a", json!({"value": "hello"})),
        ],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert_eq!(plan.ops[0].urn, a);
    assert!(ifx::model::contains_unknown(&plan.ops[1].desired));
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok(), "{:?}", report.failed);
    let b = state.get(&Urn::new("memory.value", "b")).unwrap();
    assert_eq!(b.inputs["value"]["from_a"], "hello");
    assert_eq!(b.depends_on, vec![a.clone()]);

    // Changing a's value cascades into b.
    let program2 = Program {
        resources: vec![
            decl(
                "b",
                json!({"value": {"from_a": OutputRef::new(a.clone(), "value").to_value()}}),
            ),
            decl("a", json!({"value": "world"})),
        ],
    };
    let plan = engine.plan(&program2, &state, &opts).await.unwrap();
    assert_eq!(plan.summary().update, 2);
    engine.apply(&program2, &mut state, &plan).await.unwrap();
    assert_eq!(
        state.get(&Urn::new("memory.value", "b")).unwrap().inputs["value"]["from_a"],
        "world"
    );

    // Destroy deletes b before a.
    let plan = engine.plan_destroy(&state, &opts).await.unwrap();
    let order: Vec<&str> = plan.ops.iter().map(|o| o.urn.name()).collect();
    assert_eq!(order, ["b", "a"]);
    engine
        .apply(&Program::default(), &mut state, &plan)
        .await
        .unwrap();
    assert!(mem.snapshot().is_empty());
}

#[tokio::test]
async fn triggers_and_protect() {
    let (engine, _mem) = setup();
    let mut state = State::default();
    let opts = Options::default();
    let a = Urn::new("memory.value", "a");
    let mut runner = decl("runner", json!({"value": "static"}));
    runner.triggers = vec![a.clone()];
    let program = Program {
        resources: vec![decl("a", json!({"value": 1})), runner.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    engine.apply(&program, &mut state, &plan).await.unwrap();

    let program = Program {
        resources: vec![decl("a", json!({"value": 2})), runner.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(matches!(
        plan.get(&runner.urn).unwrap().action,
        Action::Trigger
    ));
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(
        report
            .applied
            .iter()
            .any(|(u, a)| u == &runner.urn && matches!(a, Action::Trigger))
    );

    let program = Program {
        resources: vec![decl("a", json!({"value": 2, "key": "x"})), runner.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(matches!(plan.ops[0].action, Action::Replace(_)));
    engine.apply(&program, &mut state, &plan).await.unwrap();
    // Applying with protect=true (a no-op) records protection in state.
    let mut protected = decl("a", json!({"value": 2, "key": "x"}));
    protected.protect = true;
    let program = Program {
        resources: vec![protected, runner.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(matches!(&plan.ops[0].action, Action::Update(d) if d.changes[0].field == "protect"));
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(state.get(&a).unwrap().protect);

    let mut protected = decl("a", json!({"value": 2, "key": "y"}));
    protected.protect = true;
    let program = Program {
        resources: vec![protected, runner.clone()],
    };
    let err = engine.plan(&program, &state, &opts).await.unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    let err = engine
        .plan(&Program::default(), &state, &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
}

#[tokio::test]
async fn validation_and_targets() {
    let (engine, _mem) = setup();
    let mut state = State::default();
    let program = Program {
        resources: vec![decl("a", json!({"valu": 1}))],
    };
    let err = engine
        .plan(&program, &state, &Options::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("did you mean `value`"), "{err}");

    let program = Program {
        resources: vec![
            decl("a", json!({"value": 1})),
            decl("b", json!({"value": 2})),
        ],
    };
    let opts = Options {
        targets: vec![Urn::new("memory.value", "a")],
        no_refresh: false,
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert_eq!(plan.summary().create, 1);
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(state.resources.len(), 1);
}

#[tokio::test]
async fn protect_toggle_is_a_planned_change() {
    let (engine, _mem) = setup();
    let mut state = State::default();
    let opts = Options::default();
    let mut d = decl("a", json!({"value": 1}));
    d.protect = true;
    let program = Program {
        resources: vec![d.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(state.get(&d.urn).unwrap().protect);

    d.protect = false;
    let program = Program {
        resources: vec![d.clone()],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(plan.has_changes());
    match &plan.ops[0].action {
        Action::Update(diff) => assert_eq!(diff.changes[0].field, "protect"),
        other => panic!("{other:?}"),
    }
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert_eq!(report.applied.len(), 1);
    assert!(!state.get(&d.urn).unwrap().protect);
    assert!(
        engine
            .plan(&Program::default(), &state, &opts)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn risky_operations_require_exact_fresh_approvals() {
    let mem = Memory::default();
    let mut registry = Registry::new();
    registry.register(RiskyMemory(mem.clone()));
    let engine = Engine::new(registry);
    let opts = Options::default();
    let urn = Urn::new("memory.value", "risky");
    let mut state = State::default();

    let initial = Program {
        resources: vec![decl("risky", json!({"value": 1}))],
    };
    let plan = engine.plan(&initial, &state, &opts).await.unwrap();
    assert!(plan.approvals().next().is_none());
    assert!(
        engine
            .apply(&initial, &mut state, &plan)
            .await
            .unwrap()
            .ok()
    );

    let changed = Program {
        resources: vec![decl("risky", json!({"value": 2}))],
    };
    let plan = engine.plan(&changed, &state, &opts).await.unwrap();
    let approval = plan.approvals().next().unwrap().clone();
    assert_eq!(approval.selector(), "memory.value:risky/restart");
    assert!(approval.fingerprint.is_some());

    let denied = engine.apply(&changed, &mut state, &plan).await.unwrap();
    assert!(!denied.ok());
    assert!(denied.failed.is_empty());
    assert_eq!(
        denied.pending_approvals.as_slice(),
        std::slice::from_ref(&approval)
    );

    let applied = engine
        .apply_approved(&changed, &mut state, &plan, &[approval])
        .await
        .unwrap();
    assert!(applied.ok(), "{:?}", applied.failed);

    let changed_again = Program {
        resources: vec![decl("risky", json!({"value": 4}))],
    };
    let stale_plan = engine.plan(&changed_again, &state, &opts).await.unwrap();
    let stale_approval = stale_plan.approvals().next().unwrap().clone();
    let id = state.get(&urn).unwrap().id.clone().unwrap();
    let transports = TransportPool::new();
    let cx = Ctx {
        urn: &urn,
        transports: &transports,
        triggered: false,
    };
    mem.update(&cx, Some(&id), &json!({"value": 3}), &Actual::default())
        .await
        .unwrap();

    let denied = engine
        .apply_approved(
            &changed_again,
            &mut state,
            &stale_plan,
            std::slice::from_ref(&stale_approval),
        )
        .await
        .unwrap();
    assert!(!denied.ok());
    assert!(denied.failed.is_empty());
    assert_eq!(denied.pending_approvals.len(), 1);
    assert_ne!(denied.pending_approvals[0], stale_approval);
    assert_eq!(mem.snapshot()[&id]["value"], 3);
}

#[tokio::test]
async fn risky_operation_fingerprint_waits_for_referenced_outputs() {
    let mem = Memory::default();
    let mut registry = Registry::new();
    registry.register(RiskyMemory(mem));
    let engine = Engine::new(registry);
    let opts = Options::default();
    let source = Urn::new("memory.value", "source");
    let mut state = State::default();
    let initial = Program {
        resources: vec![
            decl("source", json!({"value": "one"})),
            decl(
                "consumer",
                json!({"value": OutputRef::new(source.clone(), "value").to_value()}),
            ),
        ],
    };
    let plan = engine.plan(&initial, &state, &opts).await.unwrap();
    assert!(
        engine
            .apply(&initial, &mut state, &plan)
            .await
            .unwrap()
            .ok()
    );

    let changed = Program {
        resources: vec![
            decl("source", json!({"value": "two"})),
            decl(
                "consumer",
                json!({"value": OutputRef::new(source, "value").to_value()}),
            ),
        ],
    };
    let plan = engine.plan(&changed, &state, &opts).await.unwrap();
    let source_approval = plan
        .get(&Urn::new("memory.value", "source"))
        .unwrap()
        .approvals
        .first()
        .unwrap()
        .clone();
    let consumer_approval = plan
        .get(&Urn::new("memory.value", "consumer"))
        .unwrap()
        .approvals
        .first()
        .unwrap();
    assert!(source_approval.fingerprint.is_some());
    assert!(consumer_approval.fingerprint.is_none());

    let report = engine
        .apply_approved(&changed, &mut state, &plan, &[source_approval])
        .await
        .unwrap();
    assert!(report.failed.is_empty());
    assert_eq!(report.pending_approvals.len(), 1);
    assert_eq!(
        report.pending_approvals[0].urn,
        Urn::new("memory.value", "consumer")
    );
    assert!(report.pending_approvals[0].fingerprint.is_some());
}

#[tokio::test]
async fn approval_resume_preserves_a_planned_trigger() {
    let mem = Memory::default();
    let mut registry = Registry::new();
    registry.register(RiskyMemory(mem));
    let engine = Engine::new(registry);
    let opts = Options::default();
    let source = Urn::new("memory.value", "source");
    let runner = Urn::new("memory.value", "runner");
    let mut runner_decl = decl("runner", json!({"value": "static"}));
    runner_decl.triggers = vec![source.clone()];
    let mut state = State::default();
    let initial = Program {
        resources: vec![decl("source", json!({"value": 1})), runner_decl.clone()],
    };
    let plan = engine.plan(&initial, &state, &opts).await.unwrap();
    assert!(
        engine
            .apply(&initial, &mut state, &plan)
            .await
            .unwrap()
            .ok()
    );

    let changed = Program {
        resources: vec![decl("source", json!({"value": 2})), runner_decl],
    };
    let plan = engine.plan(&changed, &state, &opts).await.unwrap();
    let source_approval = plan.get(&source).unwrap().approvals[0].clone();
    let runner_approval = plan.get(&runner).unwrap().approvals[0].clone();
    let paused = engine
        .apply_approved(
            &changed,
            &mut state,
            &plan,
            std::slice::from_ref(&source_approval),
        )
        .await
        .unwrap();
    assert_eq!(
        paused.pending_approvals.as_slice(),
        std::slice::from_ref(&runner_approval)
    );

    let resumed = engine
        .apply_approved(
            &changed,
            &mut state,
            &plan,
            &[source_approval, runner_approval],
        )
        .await
        .unwrap();
    assert!(resumed.ok(), "{:?}", resumed.failed);
    assert!(
        resumed
            .applied
            .iter()
            .any(|(urn, action)| urn == &runner && matches!(action, Action::Trigger))
    );
}
