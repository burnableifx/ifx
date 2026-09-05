//! Plan and apply. Planning observes the world and computes the minimal set of
//! operations; applying walks the dependency graph in parallel, re-resolving each
//! resource's inputs from live outputs as upstream resources land. Every operation is
//! persisted through the [`Store`] as it completes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::control::ApprovalRequirement;
use crate::graph::Graph;
use crate::model::{
    Connection, OutputRef, Program, ResourceDecl, SshActivation, Urn, contains_unknown, get_path,
    resolve_refs, unknown,
};
use crate::provider::{Actual, Ctx, Diff, Handler, OperationKind, Registry};
use crate::state::{Entry, PendingReplacement, State};
use crate::store::{EventRecord, FileStore, NullStore, RunKind, Store};
use crate::transport::{TransportPool, close_ssh_activation, ssh_activation_pending};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Create,
    Update(Diff),
    Replace(Diff),
    Delete,
    /// Nothing about the resource changed, but a declared trigger did.
    Trigger,
    /// Exists and matches the program but is not in state yet: apply records it.
    Adopt,
    NoOp,
}

impl Action {
    pub fn is_change(&self) -> bool {
        !matches!(self, Action::NoOp)
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            Action::Create => "+",
            Action::Update(_) => "~",
            Action::Replace(_) => "±",
            Action::Delete => "-",
            Action::Trigger => "!",
            Action::Adopt => "=",
            Action::NoOp => " ",
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update(_) => "update",
            Action::Replace(_) => "replace",
            Action::Delete => "delete",
            Action::Trigger => "trigger",
            Action::Adopt => "adopt",
            Action::NoOp => "no-op",
        }
    }

    fn operation_kind(&self) -> Option<OperationKind> {
        match self {
            Action::Create => Some(OperationKind::Create),
            Action::Update(_) => Some(OperationKind::Update),
            Action::Replace(_) => Some(OperationKind::Replace),
            Action::Delete => Some(OperationKind::Delete),
            Action::Trigger => Some(OperationKind::Trigger),
            Action::Adopt | Action::NoOp => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedOp {
    pub urn: Urn,
    pub action: Action,
    /// Resolved desired inputs; may contain unknown markers. Secret markers are
    /// stripped; the affected fields are listed in `secrets`.
    #[serde(default)]
    pub desired: Value,
    /// Top-level input names the program marked with `secret(..)`.
    pub secrets: Vec<String>,
    /// What was observed (absent for creates / unknown-input resources).
    pub actual: Option<Actual>,
    /// Outputs known at plan time (from observation or state).
    #[serde(default)]
    pub outputs: Value,
    /// Excluded by `--target`.
    pub skipped: bool,
    /// Named operation risks that must be approved before apply.
    #[serde(default)]
    pub approvals: Vec<ApprovalRequirement>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    /// Deletes first (reverse dependency order), then the program in dependency order.
    pub ops: Vec<PlannedOp>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub create: usize,
    pub update: usize,
    pub replace: usize,
    pub delete: usize,
    pub trigger: usize,
    pub adopt: usize,
    pub unchanged: usize,
}

impl Plan {
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for op in self.ops.iter().filter(|o| !o.skipped) {
            match op.action {
                Action::Create => s.create += 1,
                Action::Update(_) => s.update += 1,
                Action::Replace(_) => s.replace += 1,
                Action::Delete => s.delete += 1,
                Action::Trigger => s.trigger += 1,
                Action::Adopt => s.adopt += 1,
                Action::NoOp => s.unchanged += 1,
            }
        }
        s
    }

    pub fn has_changes(&self) -> bool {
        self.ops.iter().any(|o| !o.skipped && o.action.is_change())
    }

    pub fn get(&self, urn: &Urn) -> Option<&PlannedOp> {
        self.ops.iter().find(|o| &o.urn == urn)
    }

    pub fn approvals(&self) -> impl Iterator<Item = &ApprovalRequirement> {
        self.ops.iter().flat_map(|op| op.approvals.iter())
    }

    /// Resources that exist but differ from the program (drift), with their diffs.
    pub fn drifted(&self) -> impl Iterator<Item = &PlannedOp> {
        self.ops
            .iter()
            .filter(|o| !o.skipped && matches!(o.action, Action::Update(_) | Action::Replace(_)))
    }
}

/// Progress notifications during apply.
#[derive(Clone, Debug)]
pub enum Event {
    Started {
        urn: Urn,
        action: Action,
    },
    Finished {
        urn: Urn,
        action: Action,
        outputs: Value,
    },
    Failed {
        urn: Urn,
        action: Action,
        error: String,
    },
    ApprovalAccepted {
        approval: ApprovalRequirement,
    },
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Restrict operations to these resources and their dependencies.
    pub targets: Vec<Urn>,
    /// Skip observation; trust state (faster, drift-blind).
    pub no_refresh: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub applied: Vec<(Urn, Action)>,
    pub failed: Vec<(Urn, String)>,
    pub outputs: BTreeMap<Urn, Value>,
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalRequirement>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.failed.is_empty() && self.pending_approvals.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub recovered: Vec<Urn>,
    pub pending_approvals: Vec<ApprovalRequirement>,
}

pub struct Engine {
    registry: Registry,
    transports: Arc<TransportPool>,
    parallelism: usize,
    store: Arc<dyn Store>,
    events: EventSink,
    execution_identity: Option<(String, String)>,
}

impl Engine {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            transports: Arc::new(TransportPool::new()),
            parallelism: 8,
            store: Arc::new(NullStore),
            events: Arc::new(|_| {}),
            execution_identity: None,
        }
    }

    pub fn with_transports(mut self, pool: Arc<TransportPool>) -> Self {
        self.transports = pool;
        self
    }

    pub fn with_parallelism(mut self, n: usize) -> Self {
        self.parallelism = n.max(1);
        self
    }

    /// Persist through this store after every operation; record runs and events.
    pub fn with_store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = store;
        self
    }

    /// Persist to a plain JSON file after every operation.
    pub fn with_state_path(self, path: impl Into<std::path::PathBuf>) -> Self {
        self.with_store(Arc::new(FileStore::new(path.into())))
    }

    pub fn with_events(mut self, sink: EventSink) -> Self {
        self.events = sink;
        self
    }

    /// Associate provider mutations with the durable daemon execution that owns them.
    pub fn with_execution_identity(
        mut self,
        run_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Self {
        self.execution_identity = Some((run_id.into(), revision.into()));
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    pub fn transports(&self) -> &Arc<TransportPool> {
        &self.transports
    }

    /// Seal durable execution-scoped connections left open by an interrupted executor.
    pub async fn recover_execution_access(&self, state: &State) -> anyhow::Result<Vec<Urn>> {
        let mut seen = HashSet::new();
        let mut recovered = Vec::new();
        for (urn, entry) in &state.resources {
            let mut activations = Vec::new();
            collect_ssh_activations(&entry.outputs, &mut activations);
            collect_ssh_activations(&entry.inputs, &mut activations);
            let mut recovered_resource = false;
            for activation in activations {
                if seen.insert(activation.clone()) && ssh_activation_pending(&activation) {
                    close_ssh_activation(&activation)
                        .await
                        .with_context(|| format!("{urn}: recovering interrupted management"))?;
                    recovered_resource = true;
                }
            }
            if recovered_resource {
                recovered.push(urn.clone());
            }
        }
        Ok(recovered)
    }

    /// Recover interrupted provider access from the durable Program, including a
    /// resource whose create was interrupted before it entered state.
    pub async fn recover_program_execution_access(
        &self,
        program: &Program,
        state: &State,
    ) -> anyhow::Result<Vec<Urn>> {
        let graph = Graph::build(program)?;
        let mut known = BTreeMap::new();
        let mut recovered = Vec::new();
        for urn in graph.order() {
            let declaration = program.get(urn).expect("in graph");
            let desired = self.desired_inputs(declaration, &known)?;
            let entry = state.get(urn);
            let cx = Ctx {
                urn,
                transports: &self.transports,
                triggered: false,
            };
            if self
                .handler(urn)?
                .recover_execution_access(
                    &cx,
                    entry.and_then(|entry| entry.id.as_deref()),
                    &desired,
                )
                .await
                .with_context(|| format!("{urn}: recovering interrupted provider access"))?
            {
                recovered.push(urn.clone());
            }
            known.insert(
                urn.clone(),
                entry
                    .map(|entry| entry.outputs.clone())
                    .unwrap_or_else(unknown),
            );
        }
        Ok(recovered)
    }

    fn handler(&self, urn: &Urn) -> anyhow::Result<Arc<dyn Handler>> {
        self.registry
            .get(urn.type_name())
            .ok_or_else(|| anyhow!("{urn}: unknown resource type `{}`", urn.type_name()))
    }

    /// Resolve references, apply schema defaults, validate.
    fn desired_inputs(
        &self,
        decl: &ResourceDecl,
        known: &BTreeMap<Urn, Value>,
    ) -> anyhow::Result<Value> {
        let handler = self.handler(&decl.urn)?;
        let schema = handler.schema();
        let mut desired = resolve_refs(&decl.inputs, &|r: &OutputRef| {
            known
                .get(&r.urn)
                .and_then(|o| get_path(o, &r.path))
                .cloned()
        });
        schema.apply_defaults(&mut desired);
        schema
            .validate(&desired)
            .map_err(|errs| anyhow!("{}: invalid inputs:\n  {}", decl.urn, errs.join("\n  ")))?;
        Ok(desired)
    }

    /// Which program resources participate given `--target`.
    fn target_set(&self, graph: &Graph, opts: &Options) -> Option<BTreeSet<Urn>> {
        if opts.targets.is_empty() {
            return None;
        }
        let mut set = BTreeSet::new();
        for t in &opts.targets {
            set.insert(t.clone());
            set.extend(graph.all_deps(t));
        }
        Some(set)
    }

    pub async fn plan(
        &self,
        program: &Program,
        state: &State,
        opts: &Options,
    ) -> anyhow::Result<Plan> {
        let run = self.store.begin_run(&state.stack, RunKind::Plan).await?;
        let result = self.plan_inner(program, state, opts).await;
        match &result {
            Ok(plan) => {
                let summary = serde_json::to_value(plan.summary())?;
                self.store.finish_run(&run, true, summary, None).await?;
            }
            Err(e) => {
                self.store
                    .finish_run(&run, false, Value::Null, Some(format!("{e:#}")))
                    .await?;
            }
        }
        result
    }

    /// Reconcile replacements that were durably prepared but not atomically committed
    /// to state before the previous executor stopped.
    pub async fn recover_pending_replacements(
        &self,
        state: &mut State,
        execution_run_id: &str,
        approvals: &[ApprovalRequirement],
    ) -> anyhow::Result<RecoveryReport> {
        let pending: Vec<_> = state
            .pending_replacements
            .iter()
            .filter(|(_, replacement)| replacement.execution_run_id == execution_run_id)
            .map(|(urn, replacement)| (urn.clone(), replacement.clone()))
            .collect();
        let mut report = RecoveryReport::default();
        for (urn, mut replacement) in pending {
            let expected_operation_id = replacement_operation_id(
                &urn,
                &replacement.execution_run_id,
                &replacement.revision,
                &replacement.intent_digest,
                &replacement.old,
                &replacement.desired_inputs,
                &replacement.approval_fingerprints,
            )?;
            anyhow::ensure!(
                replacement.operation_id == expected_operation_id,
                "{urn}: replacement journal fingerprint does not match its persisted intent"
            );
            let handler = self.handler(&urn)?;
            let cx = Ctx {
                urn: &urn,
                transports: &self.transports,
                triggered: false,
            };
            let old_actual = handler
                .read_for_recovery(&cx, replacement.old.id.as_deref(), &replacement.old.inputs)
                .await
                .with_context(|| format!("{urn}: observing replacement source during recovery"))?;
            let desired_actual = if old_actual.is_none() {
                handler
                    .read_for_recovery(&cx, None, &replacement.desired_inputs)
                    .await
                    .with_context(|| {
                        format!("{urn}: observing replacement destination during recovery")
                    })?
            } else {
                None
            };
            if let Some(actual) = &old_actual {
                let diff = handler.diff(&replacement.desired_inputs, actual)?;
                if !diff.is_empty() {
                    let required = approval_requirements_for_intent_digest(
                        handler.as_ref(),
                        &urn,
                        &Action::Replace(diff),
                        &replacement.intent_digest,
                        &replacement.desired_inputs,
                        Some(actual),
                    )?;
                    let current: BTreeMap<_, _> = required
                        .iter()
                        .filter_map(|approval| {
                            approval
                                .fingerprint
                                .as_ref()
                                .map(|fingerprint| (approval.risk.clone(), fingerprint.clone()))
                        })
                        .collect();
                    if current != replacement.approval_fingerprints {
                        let accepted = match ensure_approved(&required, approvals) {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                if let Some(required) = error.downcast_ref::<ApprovalRequired>() {
                                    report
                                        .pending_approvals
                                        .extend(required.requirements.clone());
                                    return Ok(report);
                                }
                                return Err(error);
                            }
                        };
                        let previous_operation_id = replacement.operation_id.clone();
                        replacement.approval_fingerprints = accepted
                            .into_iter()
                            .filter_map(|approval| {
                                approval
                                    .fingerprint
                                    .map(|fingerprint| (approval.risk, fingerprint))
                            })
                            .collect();
                        replacement.operation_id = replacement_operation_id(
                            &urn,
                            &replacement.execution_run_id,
                            &replacement.revision,
                            &replacement.intent_digest,
                            &replacement.old,
                            &replacement.desired_inputs,
                            &replacement.approval_fingerprints,
                        )?;
                        let mut reauthorized = state.clone();
                        reauthorized
                            .pending_replacements
                            .insert(urn.clone(), replacement.clone());
                        self.store
                            .reauthorize_replacement(&reauthorized, &urn, &previous_operation_id)
                            .await?;
                        *state = reauthorized;
                    }
                }
            }
            let applied = handler
                .recover_replace(
                    &cx,
                    replacement.old.id.as_deref(),
                    &replacement.old.inputs,
                    &replacement.desired_inputs,
                    old_actual.as_ref(),
                    desired_actual.as_ref(),
                )
                .await
                .with_context(|| {
                    format!("{urn}: recovering replacement {}", replacement.operation_id)
                })?;
            let mut committed = state.clone();
            committed.pending_replacements.remove(&urn);
            committed.upsert(
                urn.clone(),
                Entry {
                    id: applied.id.clone(),
                    inputs: replacement.desired_inputs.clone(),
                    outputs: applied.outputs.clone(),
                    depends_on: replacement.depends_on.clone(),
                    protect: replacement.protect,
                },
            );
            self.store
                .commit_replacement(&committed, &urn, &replacement.operation_id)
                .await?;
            *state = committed;
            handler
                .finalize_replace(
                    &cx,
                    replacement.old.id.as_deref(),
                    &replacement.old.inputs,
                    applied.id.as_deref(),
                    &replacement.desired_inputs,
                )
                .await
                .with_context(|| format!("{urn}: finalizing recovered replacement"))?;
            report.recovered.push(urn);
        }
        Ok(report)
    }

    /// Roll back every interrupted replacement owned by an execution. Journals are
    /// cleared only after the provider confirms the old resource is restored.
    pub async fn abort_pending_replacements(
        &self,
        state: &mut State,
        execution_run_id: &str,
    ) -> anyhow::Result<Vec<Urn>> {
        let pending: Vec<_> = state
            .pending_replacements
            .iter()
            .filter(|(_, replacement)| replacement.execution_run_id == execution_run_id)
            .map(|(urn, replacement)| (urn.clone(), replacement.clone()))
            .collect();
        let mut aborted = Vec::with_capacity(pending.len());
        for (urn, replacement) in pending {
            let expected_operation_id = replacement_operation_id(
                &urn,
                &replacement.execution_run_id,
                &replacement.revision,
                &replacement.intent_digest,
                &replacement.old,
                &replacement.desired_inputs,
                &replacement.approval_fingerprints,
            )?;
            anyhow::ensure!(
                replacement.operation_id == expected_operation_id,
                "{urn}: replacement journal fingerprint does not match its persisted intent"
            );
            let handler = self.handler(&urn)?;
            let cx = Ctx {
                urn: &urn,
                transports: &self.transports,
                triggered: false,
            };
            handler
                .abort_replace(
                    &cx,
                    replacement.old.id.as_deref(),
                    &replacement.old.inputs,
                    &replacement.desired_inputs,
                )
                .await
                .with_context(|| {
                    format!(
                        "{urn}: rolling back replacement {}",
                        replacement.operation_id
                    )
                })?;
            let mut restored = state.clone();
            restored.pending_replacements.remove(&urn);
            if replacement.old_in_state {
                restored.upsert(urn.clone(), replacement.old);
            } else {
                restored.remove(&urn);
            }
            self.store
                .abort_replacement(&restored, &urn, &replacement.operation_id)
                .await?;
            *state = restored;
            aborted.push(urn);
        }
        Ok(aborted)
    }

    /// Observe and diff a program without recording a `plan` run. Used by read-only
    /// inspection surfaces such as the deployment explorer, where polling must not
    /// create history entries of its own.
    pub async fn preview(
        &self,
        program: &Program,
        state: &State,
        opts: &Options,
    ) -> anyhow::Result<Plan> {
        self.plan_inner(program, state, opts).await
    }

    async fn plan_inner(
        &self,
        program: &Program,
        state: &State,
        opts: &Options,
    ) -> anyhow::Result<Plan> {
        anyhow::ensure!(
            state.pending_replacements.is_empty(),
            "state contains interrupted replacements; recover them before planning"
        );
        let graph = Graph::build(program)?;
        for t in &opts.targets {
            anyhow::ensure!(
                graph.contains(t) || state.get(t).is_some(),
                "unknown target `{t}`"
            );
        }
        let targets = self.target_set(&graph, opts);
        let mut plan = Plan::default();

        // Deletions: in state but not in program.
        let mut delete_deps: BTreeMap<Urn, BTreeSet<Urn>> = BTreeMap::new();
        for (urn, entry) in &state.resources {
            if graph.contains(urn) {
                continue;
            }
            if targets.as_ref().is_some_and(|t| !t.contains(urn)) {
                continue;
            }
            delete_deps.insert(urn.clone(), entry.depends_on.iter().cloned().collect());
        }
        let dgraph = Graph::from_deps(delete_deps)?;
        for urn in dgraph.order().iter().rev() {
            let entry = state.get(urn).expect("from state");
            if entry.protect {
                anyhow::bail!("{urn} is protected and cannot be deleted");
            }
            let handler = self.handler(urn)?;
            let cx = Ctx {
                urn,
                transports: &self.transports,
                triggered: false,
            };
            let actual = if opts.no_refresh || uses_execution_scoped_access(&entry.inputs) {
                Some(Actual {
                    id: entry.id.clone(),
                    props: entry.inputs.clone(),
                    outputs: entry.outputs.clone(),
                })
            } else {
                handler
                    .read(&cx, entry.id.as_deref(), &entry.inputs)
                    .await
                    .with_context(|| format!("{urn}: read before delete"))?
            };
            let approvals = approval_requirements(
                handler.as_ref(),
                urn,
                &Action::Delete,
                &entry.inputs,
                &entry.inputs,
                actual.as_ref(),
            )?;
            plan.ops.push(PlannedOp {
                urn: urn.clone(),
                action: Action::Delete,
                desired: Value::Null,
                secrets: Vec::new(),
                outputs: actual
                    .as_ref()
                    .map(|a| a.outputs.clone())
                    .unwrap_or_else(|| entry.outputs.clone()),
                actual,
                skipped: false,
                approvals,
            });
        }

        // Program resources in dependency order.
        let mut known: BTreeMap<Urn, Value> = BTreeMap::new();
        let mut changed: BTreeSet<Urn> = BTreeSet::new();
        for urn in graph.order() {
            let decl = program.get(urn).expect("in graph");
            let handler = self.handler(urn)?;
            let (desired, secrets) =
                crate::model::strip_secrets(self.desired_inputs(decl, &known)?);
            let entry = state.get(urn);
            let skipped = targets.as_ref().is_some_and(|t| !t.contains(urn));
            let has_unknown = contains_unknown(&desired);

            let (mut action, actual, outputs) = if skipped {
                (
                    Action::NoOp,
                    None,
                    entry.map(|e| e.outputs.clone()).unwrap_or(Value::Null),
                )
            } else if opts.no_refresh || uses_execution_scoped_access(&desired) {
                match entry {
                    Some(e) => {
                        let actual = Actual {
                            id: e.id.clone(),
                            props: e.inputs.clone(),
                            outputs: e.outputs.clone(),
                        };
                        let diff = handler.diff(&desired, &actual)?;
                        (classify(diff), Some(actual), e.outputs.clone())
                    }
                    None => (Action::Create, None, Value::Null),
                }
            } else {
                let read_inputs = entry.map(|e| &e.inputs).unwrap_or(&desired);
                if contains_unknown(read_inputs) {
                    (
                        if entry.is_some() {
                            Action::Update(Diff::default())
                        } else {
                            Action::Create
                        },
                        None,
                        entry.map(|e| e.outputs.clone()).unwrap_or(Value::Null),
                    )
                } else {
                    let cx = Ctx {
                        urn,
                        transports: &self.transports,
                        triggered: false,
                    };
                    let id = entry.and_then(|e| e.id.as_deref());
                    match handler
                        .read(&cx, id, read_inputs)
                        .await
                        .with_context(|| format!("{urn}: read"))?
                    {
                        None => (Action::Create, None, Value::Null),
                        Some(actual) => {
                            let diff = handler.diff(&desired, &actual)?;
                            let outputs = actual.outputs.clone();
                            (classify(diff), Some(actual), outputs)
                        }
                    }
                }
            };

            if has_unknown && matches!(action, Action::NoOp) {
                action = Action::Update(Diff::default());
            }
            // Matches the program but nothing in state knows it: adopt, so apply records
            // it (and later deletes can find it).
            if matches!(action, Action::NoOp) && entry.is_none() && !skipped {
                action = Action::Adopt;
            }
            if matches!(action, Action::NoOp) && decl.triggers.iter().any(|t| changed.contains(t)) {
                action = Action::Trigger;
            }
            if decl.protect && matches!(action, Action::Replace(_)) {
                anyhow::bail!("{urn} is protected and cannot be replaced");
            }
            // A change to `protect` alone is still a change: it must reach state.
            if matches!(action, Action::NoOp)
                && let Some(e) = entry
                && e.protect != decl.protect
            {
                action = Action::Update(protect_diff(e.protect, decl.protect));
            }
            if action.is_change() {
                changed.insert(urn.clone());
            }
            let approvals = if skipped {
                Vec::new()
            } else {
                approval_requirements(
                    handler.as_ref(),
                    urn,
                    &action,
                    &decl.inputs,
                    &desired,
                    actual.as_ref(),
                )?
            };
            // What downstream references can rely on at plan time: nothing for resources
            // being (re)created; for updates, outputs sharing a name with a changed input
            // are assumed to change too (`label` in -> `label` out); everything else is
            // taken from the observation.
            match &action {
                Action::Create | Action::Replace(_) => {}
                Action::Update(diff) => {
                    let mut o = outputs.clone();
                    if let Some(obj) = o.as_object_mut() {
                        for c in &diff.changes {
                            if obj.contains_key(&c.field) {
                                obj.insert(c.field.clone(), unknown());
                            }
                        }
                        if diff.changes.is_empty() {
                            for v in obj.values_mut() {
                                *v = unknown();
                            }
                        }
                    }
                    known.insert(urn.clone(), o);
                }
                _ => {
                    known.insert(urn.clone(), outputs.clone());
                }
            }
            plan.ops.push(PlannedOp {
                urn: urn.clone(),
                action,
                desired,
                secrets,
                actual,
                outputs,
                skipped,
                approvals,
            });
        }
        Ok(plan)
    }

    /// Execute a plan. Each program resource is re-observed with fully known inputs
    /// right before it is acted on, so the plan is a statement of intent, not a script.
    pub async fn apply(
        &self,
        program: &Program,
        state: &mut State,
        plan: &Plan,
    ) -> anyhow::Result<Report> {
        self.apply_approved(program, state, plan, &[]).await
    }

    /// Execute a plan with exact, plan-derived approvals. Every risky operation is
    /// re-observed and re-fingerprinted immediately before mutation.
    pub async fn apply_approved(
        &self,
        program: &Program,
        state: &mut State,
        plan: &Plan,
        approvals: &[ApprovalRequirement],
    ) -> anyhow::Result<Report> {
        let result = self
            .apply_approved_inner(program, state, plan, approvals)
            .await;
        let cleanup = self.transports.finish_execution().await;
        match (result, cleanup) {
            (Ok((report, _)), Ok(())) => Ok(report),
            (Ok((report, run_id)), Err(cleanup)) => {
                let error = format!("{cleanup:#}");
                let summary = serde_json::json!({
                    "applied": report.applied.len(),
                    "failed": report.failed.len(),
                    "pending_approvals": report.pending_approvals.len(),
                    "management_cleanup_failed": true,
                });
                self.store
                    .finish_run(&run_id, false, summary, Some(error.clone()))
                    .await?;
                anyhow::bail!(error)
            }
            (Err(operation), Ok(())) => Err(operation),
            (Err(operation), Err(cleanup)) => Err(anyhow!(
                "{operation:#}; closing execution-scoped management also failed: {cleanup:#}"
            )),
        }
    }

    async fn apply_approved_inner(
        &self,
        program: &Program,
        state: &mut State,
        plan: &Plan,
        approvals: &[ApprovalRequirement],
    ) -> anyhow::Result<(Report, String)> {
        anyhow::ensure!(
            state.pending_replacements.is_empty(),
            "state contains interrupted replacements; recover and re-plan before applying"
        );
        let graph = Graph::build(program)?;
        let kind = if program.resources.is_empty() {
            RunKind::Destroy
        } else {
            RunKind::Apply
        };
        let stack = state.stack.clone();
        let run_id = self.store.begin_run(&stack, kind).await?;
        let shared = Arc::new(Mutex::new(state.clone()));
        let report = Arc::new(Mutex::new(Report::default()));
        let mut failed = false;
        let this = ApplyCtx {
            registry: self.registry.clone(),
            transports: self.transports.clone(),
            events: self.events.clone(),
            store: self.store.clone(),
            run_id: run_id.clone(),
            stack,
            approvals: Arc::new(approvals.to_vec()),
            execution_run_id: self
                .execution_identity
                .as_ref()
                .map(|identity| identity.0.clone())
                .filter(|run_id| !run_id.is_empty())
                .unwrap_or_else(|| {
                    if run_id.is_empty() {
                        "direct".into()
                    } else {
                        run_id.clone()
                    }
                }),
            revision: self
                .execution_identity
                .as_ref()
                .map(|identity| identity.1.clone())
                .unwrap_or_else(|| "unversioned".into()),
        };

        // Phase 1: deletions, sequential in the order the plan lists them.
        let mut waiting_for_approval = false;
        for op in plan
            .ops
            .iter()
            .filter(|o| matches!(o.action, Action::Delete) && !o.skipped)
        {
            let entry = {
                let s = shared.lock().await;
                s.get(&op.urn).cloned()
            };
            let Some(entry) = entry else { continue };
            let outcome = self
                .delete_one(&op.urn, &entry, this.approvals.as_ref())
                .await;
            match outcome {
                Ok(()) => {
                    this.record(&op.urn, &Action::Delete, None).await;
                    let mut s = shared.lock().await;
                    s.remove(&op.urn);
                    self.store.save_state(&s).await?;
                    report
                        .lock()
                        .await
                        .applied
                        .push((op.urn.clone(), Action::Delete));
                }
                Err(error) => {
                    if let Some(required) = error.downcast_ref::<ApprovalRequired>() {
                        report
                            .lock()
                            .await
                            .pending_approvals
                            .extend(required.requirements.clone());
                        waiting_for_approval = true;
                    } else {
                        let error = format!("{error:#}");
                        this.record(&op.urn, &Action::Delete, Some(error.clone()))
                            .await;
                        report.lock().await.failed.push((op.urn.clone(), error));
                        failed = true;
                    }
                    break;
                }
            }
        }

        // Phase 2: program resources, parallel over the dependency graph.
        if !failed && !waiting_for_approval {
            let planned: HashMap<Urn, &PlannedOp> =
                plan.ops.iter().map(|o| (o.urn.clone(), o)).collect();
            let mut remaining: BTreeMap<Urn, usize> = graph
                .order()
                .iter()
                .map(|u| (u.clone(), graph.deps(u).count()))
                .collect();
            let mut ready: Vec<Urn> = remaining
                .iter()
                .filter(|(_, n)| **n == 0)
                .map(|(u, _)| u.clone())
                .collect();
            remaining.retain(|_, n| *n > 0);
            let changed: Arc<Mutex<BTreeSet<Urn>>> = Arc::new(Mutex::new(BTreeSet::new()));
            let mut set: JoinSet<(Urn, anyhow::Result<Option<Action>>)> = JoinSet::new();

            loop {
                while !failed && !waiting_for_approval && set.len() < self.parallelism {
                    let Some(urn) = ready.pop() else { break };
                    let decl = program.get(&urn).expect("in program").clone();
                    let op = planned.get(&urn).map(|o| (*o).clone());
                    let shared = shared.clone();
                    let changed = changed.clone();
                    let this = this.clone();
                    set.spawn(async move {
                        let r = this.apply_one(&decl, op, &shared, &changed).await;
                        (urn, r)
                    });
                }
                let Some(joined) = set.join_next().await else {
                    break;
                };
                let (urn, result) = joined.map_err(|e| anyhow!("task panicked: {e}"))?;
                match result {
                    Ok(Some(action)) => {
                        report.lock().await.applied.push((urn.clone(), action));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if let Some(required) = e.downcast_ref::<ApprovalRequired>() {
                            report
                                .lock()
                                .await
                                .pending_approvals
                                .extend(required.requirements.clone());
                            waiting_for_approval = true;
                        } else {
                            report
                                .lock()
                                .await
                                .failed
                                .push((urn.clone(), format!("{e:#}")));
                            failed = true;
                        }
                    }
                }
                for d in graph.dependents(&urn) {
                    if let Some(n) = remaining.get_mut(d) {
                        *n -= 1;
                        if *n == 0 {
                            remaining.remove(d);
                            ready.push(d.clone());
                        }
                    }
                }
            }
        }

        let final_state = Arc::try_unwrap(shared)
            .map_err(|_| anyhow!("state still shared"))?
            .into_inner();
        *state = final_state;
        let mut report = Arc::try_unwrap(report)
            .map_err(|_| anyhow!("report still shared"))?
            .into_inner();
        for (urn, entry) in &state.resources {
            if graph.contains(urn) {
                report.outputs.insert(urn.clone(), entry.outputs.clone());
            }
        }
        let summary = serde_json::json!({
            "applied": report.applied.len(),
            "failed": report.failed.len(),
            "pending_approvals": report.pending_approvals.len(),
        });
        let error = if !report.failed.is_empty() {
            Some(
                report
                    .failed
                    .iter()
                    .map(|(u, e)| format!("{u}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        } else if !report.pending_approvals.is_empty() {
            Some(
                report
                    .pending_approvals
                    .iter()
                    .map(ApprovalRequirement::selector)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        } else {
            None
        };
        let ok = report.ok();
        self.store
            .finish_run(&run_id, ok, summary, error.clone())
            .await?;
        Ok((report, run_id))
    }

    async fn delete_one(
        &self,
        urn: &Urn,
        entry: &Entry,
        approvals: &[ApprovalRequirement],
    ) -> anyhow::Result<()> {
        let handler = self.handler(urn)?;
        let cx = Ctx {
            urn,
            transports: &self.transports,
            triggered: false,
        };
        let actual = handler
            .read(&cx, entry.id.as_deref(), &entry.inputs)
            .await
            .with_context(|| format!("{urn}: read before delete"))?;
        let required = approval_requirements(
            handler.as_ref(),
            urn,
            &Action::Delete,
            &entry.inputs,
            &entry.inputs,
            actual.as_ref(),
        )?;
        for approval in ensure_approved(&required, approvals)? {
            (self.events)(Event::ApprovalAccepted { approval });
        }
        (self.events)(Event::Started {
            urn: urn.clone(),
            action: Action::Delete,
        });
        match handler
            .delete(&cx, entry.id.as_deref(), &entry.inputs)
            .await
        {
            Ok(()) => {
                (self.events)(Event::Finished {
                    urn: urn.clone(),
                    action: Action::Delete,
                    outputs: Value::Null,
                });
                Ok(())
            }
            Err(e) => {
                (self.events)(Event::Failed {
                    urn: urn.clone(),
                    action: Action::Delete,
                    error: format!("{e:#}"),
                });
                Err(e)
            }
        }
    }

    /// Plan against an empty program: delete everything in state.
    pub async fn plan_destroy(&self, state: &State, opts: &Options) -> anyhow::Result<Plan> {
        self.plan(&Program::default(), state, opts).await
    }

    /// Re-observe every resource in state and store what was found.
    pub async fn refresh(&self, program: &Program, state: &mut State) -> anyhow::Result<Vec<Urn>> {
        let result = self.refresh_inner(program, state).await;
        let cleanup = self.transports.finish_execution().await;
        match (result, cleanup) {
            (Ok((changed, _)), Ok(())) => Ok(changed),
            (Ok((changed, run)), Err(cleanup)) => {
                let error = format!("{cleanup:#}");
                self.store
                    .finish_run(
                        &run,
                        false,
                        serde_json::json!({
                            "changed": changed.len(),
                            "management_cleanup_failed": true,
                        }),
                        Some(error.clone()),
                    )
                    .await?;
                anyhow::bail!(error)
            }
            (Err(observation), Ok(())) => Err(observation),
            (Err(observation), Err(cleanup)) => Err(anyhow!(
                "{observation:#}; closing execution-scoped management also failed: {cleanup:#}"
            )),
        }
    }

    async fn refresh_inner(
        &self,
        program: &Program,
        state: &mut State,
    ) -> anyhow::Result<(Vec<Urn>, String)> {
        anyhow::ensure!(
            state.pending_replacements.is_empty(),
            "state contains interrupted replacements; recover them before refreshing"
        );
        let run = self.store.begin_run(&state.stack, RunKind::Refresh).await?;
        let mut changed = Vec::new();
        let urns: Vec<Urn> = state.resources.keys().cloned().collect();
        for urn in urns {
            if program.get(&urn).is_none() {
                continue;
            }
            let handler = self.handler(&urn)?;
            let entry = state.get(&urn).expect("present").clone();
            let cx = Ctx {
                urn: &urn,
                transports: &self.transports,
                triggered: false,
            };
            match handler
                .read(&cx, entry.id.as_deref(), &entry.inputs)
                .await?
            {
                None => {
                    state.remove(&urn);
                    changed.push(urn);
                }
                Some(actual) => {
                    if actual.outputs != entry.outputs || actual.id != entry.id {
                        changed.push(urn.clone());
                    }
                    state.upsert(
                        urn,
                        Entry {
                            id: actual.id,
                            outputs: actual.outputs,
                            ..entry
                        },
                    );
                }
            }
        }
        self.store.save_state(state).await?;
        let summary = serde_json::json!({ "changed": changed.len() });
        self.store.finish_run(&run, true, summary, None).await?;
        Ok((changed, run))
    }
}

#[derive(Serialize)]
struct OperationFingerprint<'a> {
    urn: &'a Urn,
    operation: OperationKind,
    intent_digest: &'a str,
    desired: &'a Value,
    actual: Option<&'a Actual>,
    risk: &'a str,
}

fn approval_requirements(
    handler: &dyn Handler,
    urn: &Urn,
    action: &Action,
    intent: &Value,
    desired: &Value,
    actual: Option<&Actual>,
) -> anyhow::Result<Vec<ApprovalRequirement>> {
    let intent_digest = intent_digest(intent)?;
    approval_requirements_for_intent_digest(handler, urn, action, &intent_digest, desired, actual)
}

fn intent_digest(intent: &Value) -> anyhow::Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(intent)?)))
}

pub(crate) fn uses_execution_scoped_access(inputs: &Value) -> bool {
    inputs
        .get("on")
        .and_then(|connection| serde_json::from_value::<crate::Connection>(connection.clone()).ok())
        .is_some_and(|connection| connection.has_execution_scoped_access())
}

fn collect_ssh_activations(value: &Value, activations: &mut Vec<SshActivation>) {
    if value.get("kind").and_then(Value::as_str).is_some()
        && let Ok(connection) = serde_json::from_value::<Connection>(value.clone())
        && let Some(activation) = connection.activation()
    {
        activations.push(activation.clone());
    }
    match value {
        Value::Object(object) => object
            .values()
            .for_each(|value| collect_ssh_activations(value, activations)),
        Value::Array(array) => array
            .iter()
            .for_each(|value| collect_ssh_activations(value, activations)),
        _ => {}
    }
}

fn approval_requirements_for_intent_digest(
    handler: &dyn Handler,
    urn: &Urn,
    action: &Action,
    intent_digest: &str,
    desired: &Value,
    actual: Option<&Actual>,
) -> anyhow::Result<Vec<ApprovalRequirement>> {
    let Some(operation) = action.operation_kind() else {
        return Ok(Vec::new());
    };
    let risks = handler.risks(operation, desired, actual)?;
    let mut names = BTreeSet::new();
    let mut requirements = Vec::with_capacity(risks.len());
    for risk in risks {
        anyhow::ensure!(
            !risk.name.is_empty()
                && risk
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "{urn}: invalid operation risk name `{}`; use letters, numbers, `.`, `-`, or `_`",
            risk.name
        );
        anyhow::ensure!(
            names.insert(risk.name.clone()),
            "{urn}: duplicate operation risk `{}`",
            risk.name
        );
        let fingerprint = if contains_unknown(desired) {
            None
        } else {
            let input = OperationFingerprint {
                urn,
                operation,
                intent_digest,
                desired,
                actual,
                risk: &risk.name,
            };
            Some(hex::encode(Sha256::digest(serde_json::to_vec(&input)?)))
        };
        requirements.push(ApprovalRequirement {
            urn: urn.clone(),
            risk: risk.name,
            reason: risk.reason,
            fingerprint,
        });
    }
    Ok(requirements)
}

fn replacement_operation_id(
    urn: &Urn,
    execution_run_id: &str,
    revision: &str,
    intent_digest: &str,
    old: &Entry,
    desired: &Value,
    approval_fingerprints: &BTreeMap<String, String>,
) -> anyhow::Result<String> {
    let encoded = serde_json::to_vec(&serde_json::json!({
        "urn": urn,
        "execution_run_id": execution_run_id,
        "revision": revision,
        "intent_digest": intent_digest,
        "old": old,
        "desired": desired,
        "approvals": approval_fingerprints,
    }))?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ApprovalRequired {
    requirements: Vec<ApprovalRequirement>,
    message: String,
}

fn ensure_approved(
    required: &[ApprovalRequirement],
    approved: &[ApprovalRequirement],
) -> anyhow::Result<Vec<ApprovalRequirement>> {
    let missing: Vec<_> = required
        .iter()
        .filter(|requirement| {
            !approved.iter().any(|approval| {
                approval.urn == requirement.urn
                    && approval.risk == requirement.risk
                    && approval.fingerprint == requirement.fingerprint
                    && approval.fingerprint.is_some()
            })
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(required.to_vec());
    }
    let message = missing
        .iter()
        .map(|requirement| {
            format!(
                "{}: approval required for risk `{}` ({}) [fingerprint {}]",
                requirement.urn,
                requirement.risk,
                requirement.reason,
                requirement.fingerprint.as_deref().unwrap_or("deferred")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(ApprovalRequired {
        requirements: missing,
        message,
    }
    .into())
}

fn protect_diff(from: bool, to: bool) -> Diff {
    Diff {
        changes: vec![crate::provider::FieldChange {
            field: "protect".into(),
            from: Some(Value::Bool(from)),
            to: Some(Value::Bool(to)),
            forces_replace: false,
        }],
    }
}

fn classify(diff: Diff) -> Action {
    if diff.is_empty() {
        Action::NoOp
    } else if diff.requires_replace() {
        Action::Replace(diff)
    } else {
        Action::Update(diff)
    }
}

#[derive(Clone)]
struct ApplyCtx {
    registry: Registry,
    transports: Arc<TransportPool>,
    events: EventSink,
    store: Arc<dyn Store>,
    run_id: String,
    stack: String,
    approvals: Arc<Vec<ApprovalRequirement>>,
    execution_run_id: String,
    revision: String,
}

impl ApplyCtx {
    /// Record an operation outcome; store failures are logged, never fatal.
    async fn record(&self, urn: &Urn, action: &Action, error: Option<String>) {
        let ev = EventRecord {
            run_id: self.run_id.clone(),
            stack: self.stack.clone(),
            urn: urn.clone(),
            action: action.verb().to_string(),
            ok: error.is_none(),
            error,
            at: chrono::Utc::now(),
        };
        if let Err(e) = self.store.record_event(&ev).await {
            tracing::warn!(target: "ifx::store", "recording event for {urn}: {e:#}");
        }
    }

    async fn apply_one(
        &self,
        decl: &ResourceDecl,
        planned: Option<PlannedOp>,
        shared: &Mutex<State>,
        changed: &Mutex<BTreeSet<Urn>>,
    ) -> anyhow::Result<Option<Action>> {
        let urn = &decl.urn;
        if planned.as_ref().is_some_and(|p| p.skipped) {
            return Ok(None);
        }
        let handler = self
            .registry
            .get(urn.type_name())
            .ok_or_else(|| anyhow!("{urn}: unknown resource type"))?;
        let schema = handler.schema();

        // Resolve against live outputs.
        let (entry, desired) = {
            let s = shared.lock().await;
            let desired = resolve_refs(&decl.inputs, &|r: &OutputRef| {
                s.get(&r.urn)
                    .and_then(|e| get_path(&e.outputs, &r.path))
                    .cloned()
            });
            (s.get(urn).cloned(), desired)
        };
        let mut desired = desired;
        schema.apply_defaults(&mut desired);
        if contains_unknown(&desired) {
            anyhow::bail!("{urn}: inputs still unknown after dependencies applied: {desired}");
        }
        schema
            .validate(&desired)
            .map_err(|errs| anyhow!("{urn}: invalid inputs:\n  {}", errs.join("\n  ")))?;
        let (desired, _secrets) = crate::model::strip_secrets(desired);

        let triggered = {
            let c = changed.lock().await;
            decl.triggers.iter().any(|t| c.contains(t))
                || planned
                    .as_ref()
                    .is_some_and(|op| matches!(op.action, Action::Trigger))
        };
        let cx = Ctx {
            urn,
            transports: &self.transports,
            triggered,
        };

        if !triggered
            && planned
                .as_ref()
                .is_some_and(|operation| matches!(operation.action, Action::NoOp))
            && entry.is_some()
            && uses_execution_scoped_access(&desired)
        {
            return Ok(None);
        }

        let read_inputs = entry.as_ref().map(|e| &e.inputs).unwrap_or(&desired);
        let id = entry.as_ref().and_then(|e| e.id.as_deref());
        let actual = handler
            .read(&cx, id, read_inputs)
            .await
            .with_context(|| format!("{urn}: read"))?;
        let mut action = match &actual {
            None => Action::Create,
            Some(a) => classify(handler.diff(&desired, a)?),
        };
        if matches!(action, Action::NoOp) && triggered {
            action = Action::Trigger;
        }
        if decl.protect && matches!(action, Action::Replace(_)) {
            anyhow::bail!("{urn} is protected and cannot be replaced");
        }
        let required = approval_requirements(
            handler.as_ref(),
            urn,
            &action,
            &decl.inputs,
            &desired,
            actual.as_ref(),
        )?;
        let accepted = ensure_approved(&required, self.approvals.as_ref())?;
        for approval in &accepted {
            (self.events)(Event::ApprovalAccepted {
                approval: approval.clone(),
            });
        }

        let depends_on: Vec<Urn> = decl.dependencies().into_iter().collect();
        if matches!(action, Action::NoOp) {
            let adopted = entry.is_none();
            let protect_changed = entry.as_ref().is_some_and(|e| e.protect != decl.protect);
            // Still refresh outputs so downstream refs see live values.
            if let Some(a) = actual {
                let mut s = shared.lock().await;
                s.upsert(
                    urn.clone(),
                    Entry {
                        id: a.id,
                        inputs: desired,
                        outputs: a.outputs,
                        depends_on,
                        protect: decl.protect,
                    },
                );
                self.store.save_state(&s).await?;
            }
            if protect_changed {
                let was = entry.as_ref().map(|e| e.protect).unwrap_or(false);
                return Ok(Some(Action::Update(protect_diff(was, decl.protect))));
            }
            if adopted {
                self.record(urn, &Action::Adopt, None).await;
                return Ok(Some(Action::Adopt));
            }
            return Ok(None);
        }

        if matches!(action, Action::Replace(_)) {
            let old = entry.clone().unwrap_or_else(|| {
                let observed = actual.as_ref().expect("replacement exists");
                Entry {
                    id: observed.id.clone(),
                    inputs: observed.props.clone(),
                    outputs: observed.outputs.clone(),
                    depends_on: Vec::new(),
                    protect: false,
                }
            });
            let approval_fingerprints = accepted
                .iter()
                .filter_map(|approval| {
                    approval
                        .fingerprint
                        .as_ref()
                        .map(|fingerprint| (approval.risk.clone(), fingerprint.clone()))
                })
                .collect();
            let intent_digest = intent_digest(&decl.inputs)?;
            let pending = PendingReplacement {
                operation_id: replacement_operation_id(
                    urn,
                    &self.execution_run_id,
                    &self.revision,
                    &intent_digest,
                    &old,
                    &desired,
                    &approval_fingerprints,
                )?,
                execution_run_id: self.execution_run_id.clone(),
                revision: self.revision.clone(),
                intent_digest,
                old,
                old_in_state: entry.is_some(),
                desired_inputs: desired.clone(),
                depends_on: depends_on.clone(),
                protect: decl.protect,
                approval_fingerprints,
                prepared_at: chrono::Utc::now(),
            };
            let mut state = shared.lock().await;
            anyhow::ensure!(
                !state.pending_replacements.contains_key(urn),
                "{urn}: another replacement is already pending recovery"
            );
            let mut prepared = state.clone();
            prepared.pending_replacements.insert(urn.clone(), pending);
            self.store.prepare_replacement(&prepared, urn).await?;
            *state = prepared;
        }

        (self.events)(Event::Started {
            urn: urn.clone(),
            action: action.clone(),
        });
        let result: anyhow::Result<crate::provider::Applied> = async {
            match &action {
                Action::Create => handler.create(&cx, &desired).await,
                Action::Update(_) | Action::Trigger => {
                    handler
                        .update(&cx, id, &desired, actual.as_ref().expect("exists"))
                        .await
                }
                Action::Replace(_) => {
                    let journal = {
                        let state = shared.lock().await;
                        state
                            .pending_replacements
                            .get(urn)
                            .cloned()
                            .expect("replacement journal was prepared")
                    };
                    handler
                        .replace(
                            &cx,
                            journal.old.id.as_deref(),
                            &journal.old.inputs,
                            &desired,
                            actual.as_ref().expect("exists"),
                        )
                        .await
                        .context("replace")
                }
                Action::Delete | Action::NoOp | Action::Adopt => unreachable!(),
            }
        }
        .await;

        match result {
            Ok(applied) => {
                let replacement = {
                    let mut s = shared.lock().await;
                    let mut committed = s.clone();
                    committed.upsert(
                        urn.clone(),
                        Entry {
                            id: applied.id.clone(),
                            inputs: desired.clone(),
                            outputs: applied.outputs.clone(),
                            depends_on: depends_on.clone(),
                            protect: decl.protect,
                        },
                    );
                    let replacement = if matches!(action, Action::Replace(_)) {
                        let replacement = committed
                            .pending_replacements
                            .remove(urn)
                            .expect("replacement journal was prepared");
                        self.store
                            .commit_replacement(&committed, urn, &replacement.operation_id)
                            .await?;
                        Some(replacement)
                    } else {
                        self.store.save_state(&committed).await?;
                        None
                    };
                    *s = committed;
                    replacement
                };
                if let Some(replacement) = replacement {
                    handler
                        .finalize_replace(
                            &cx,
                            replacement.old.id.as_deref(),
                            &replacement.old.inputs,
                            applied.id.as_deref(),
                            &desired,
                        )
                        .await
                        .context("finalize replacement")?;
                }
                changed.lock().await.insert(urn.clone());
                self.record(urn, &action, None).await;
                (self.events)(Event::Finished {
                    urn: urn.clone(),
                    action: action.clone(),
                    outputs: applied.outputs,
                });
                Ok(Some(action))
            }
            Err(e) => {
                self.record(urn, &action, Some(format!("{e:#}"))).await;
                (self.events)(Event::Failed {
                    urn: urn.clone(),
                    action: action.clone(),
                    error: format!("{e:#}"),
                });
                Err(e.context(format!("{urn}: {}", action.verb())))
            }
        }
    }
}
