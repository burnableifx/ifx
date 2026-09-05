//! One daemon-owned executor per stack: serialized runs, immutable program revisions,
//! recurring health/drift observation, cancellation, and retry wake-ups.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{TimeDelta, Utc};
use ifx::cli::LoadCtx;
use ifx::control::{
    ApprovalGrant, ApprovalRequirement, BuildPhase, ExecutionEvent, ExecutionRun, ExecutionStatus,
    LeaseChange, LeaseExtension, LeaseFire, LeaseRequest, ProgramResolveRequest, ProgramRevision,
    ProgramSubmission, RunRequest, StackBuildStatus, StackLease,
};
use ifx::engine::{Action, Engine, Event, Options, Plan, Report};
use ifx::model::Program;
use ifx::monitor;
use ifx::rust::{CompiledStack, RustCompiler, SourcesChanged, source_fingerprint};
use ifx::store::{HealthRecord, RunKind, Store};
use ifx::transport::TransportPool;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, OwnedMutexGuard, RwLock, mpsc};
use tokio::task::AbortHandle;

use crate::config::{Config, StackConfig};

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// How often each stack's lease deadline is checked.
const LEASE_POLL: Duration = Duration::from_secs(1);
/// Minimum wait before a failed lease destroy is queued again.
const LEASE_REFIRE: Duration = Duration::from_secs(60);

struct BuildSlot {
    status: StackBuildStatus,
    artifact: Option<CompiledStack>,
    context: Option<LoadCtx>,
    fingerprint: Option<u64>,
}

pub struct Watcher {
    pub cfg: StackConfig,
    pub engine: Engine,
    pub ctx: LoadCtx,
    registry: ifx::Registry,
    store: Arc<dyn Store>,
    transports: Arc<TransportPool>,
    run_lock: Arc<Mutex<()>>,
    observation_lock: RwLock<()>,
    tasks: Mutex<BTreeMap<String, AbortHandle>>,
    retry_wakes: Mutex<BTreeMap<String, Arc<Notify>>>,
    execution_writes: Mutex<()>,
    /// Serializes every read-modify-write of the stack lease, including enforcement.
    lease_lock: Mutex<()>,
    compiler: RustCompiler,
    build_lock: Mutex<()>,
    build: RwLock<BuildSlot>,
    /// Last program or monitoring error shown in the API.
    pub last_error: RwLock<Option<String>>,
}

impl Watcher {
    pub async fn start(
        ctx: LoadCtx,
        cfg: StackConfig,
        registry: ifx::Registry,
        store: Arc<dyn Store>,
    ) -> anyhow::Result<Self> {
        let transports = Arc::new(TransportPool::with_control_dir(ifx::cli::control_dir(
            &ctx.dir,
        )?));
        let engine = Engine::new(registry.clone())
            .with_transports(transports.clone())
            .with_store(store.clone());
        let watcher = Self {
            cfg,
            engine,
            ctx,
            registry,
            store,
            transports,
            run_lock: Arc::new(Mutex::new(())),
            observation_lock: RwLock::new(()),
            tasks: Mutex::new(BTreeMap::new()),
            retry_wakes: Mutex::new(BTreeMap::new()),
            execution_writes: Mutex::new(()),
            lease_lock: Mutex::new(()),
            compiler: RustCompiler::from_environment()?,
            build_lock: Mutex::new(()),
            build: RwLock::new(BuildSlot {
                status: StackBuildStatus::dirty(),
                artifact: None,
                context: None,
                fingerprint: None,
            }),
            last_error: RwLock::new(None),
        };
        watcher.recover_interrupted().await?;
        Ok(watcher)
    }

    /// A running task cannot survive its owning daemon process. Seal any durable
    /// execution-scoped access first. Approval waits remain resumable, while a run
    /// owning a replacement journal waits for an explicit retry or cancellation.
    async fn recover_interrupted(&self) -> anyhow::Result<()> {
        let state = self.store.load_state(&self.cfg.name).await?;
        for urn in self.engine.recover_execution_access(&state).await? {
            tracing::warn!(stack = %self.cfg.name, %urn, "sealed management left open by an interrupted execution");
        }
        if let Some(revision) = self.store.active_program_revision(&self.cfg.name).await? {
            for urn in self
                .engine
                .recover_program_execution_access(&revision.program, &state)
                .await?
            {
                tracing::warn!(stack = %self.cfg.name, %urn, "recovered interrupted provider access from the active program");
            }
        }
        for mut run in self.store.executions(&self.cfg.name, 10_000).await? {
            if run.status == ExecutionStatus::ApprovalWait {
                continue;
            }
            let recovery_resources = state
                .pending_replacements
                .iter()
                .filter(|(_, replacement)| replacement.execution_run_id == run.run_id)
                .map(|(urn, _)| urn.clone())
                .collect::<Vec<_>>();
            if !recovery_resources.is_empty() {
                let message = "ifxd restarted during a replacement; trigger retry to recover or cancel to roll back";
                run.status = ExecutionStatus::RecoveryWait;
                run.finished_at = None;
                run.next_retry_at = None;
                run.pending_approvals.clear();
                run.recovery_resources = recovery_resources;
                run.error = Some(message.to_string());
                self.store.save_execution(&run).await?;
                self.record_control_event(&run.run_id, "recovery_wait", None, None, message)
                    .await?;
                tracing::warn!(run = %run.run_id, stack = %self.cfg.name, "replacement requires explicit recovery");
                continue;
            }
            if run.status.terminal() {
                continue;
            }
            let message = "ifxd restarted before this run completed; start a new run to retry";
            run.status = ExecutionStatus::Failed;
            run.finished_at = Some(Utc::now());
            run.next_retry_at = None;
            run.recovery_resources.clear();
            run.error = Some(message.to_string());
            self.store.save_execution(&run).await?;
            self.record_control_event(&run.run_id, "interrupted", None, None, message)
                .await?;
            tracing::warn!(run = %run.run_id, stack = %self.cfg.name, "recovered interrupted execution");
        }
        Ok(())
    }

    pub async fn spawn(self: &Arc<Self>, global: &Config) -> anyhow::Result<()> {
        self.resume_approval_waits().await?;
        let compiler = self.clone();
        tokio::spawn(async move {
            compiler.ensure_built(false).await;
        });
        let sources = self.clone();
        tokio::spawn(async move {
            sources.watch_sources().await;
        });
        let check_every = self.cfg.check_interval.unwrap_or(global.check_interval);
        let drift_every = self.cfg.drift_interval.unwrap_or(global.drift_interval);
        let this = self.clone();
        tokio::spawn(async move {
            let mut check = tokio::time::interval(check_every.max(Duration::from_secs(1)));
            let mut drift = tokio::time::interval(drift_every.max(Duration::from_secs(5)));
            let mut lease = tokio::time::interval(LEASE_POLL);
            check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            drift.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            lease.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = check.tick() => { this.run_checks().await; }
                    _ = drift.tick() => { if !this.cfg.no_drift { this.run_drift().await; } }
                    _ = lease.tick() => {
                        if let Err(error) = this.enforce_lease().await {
                            tracing::warn!(stack = %this.cfg.name, "lease enforcement: {error:#}");
                        }
                    }
                }
            }
        });
        Ok(())
    }

    pub async fn lease(&self) -> anyhow::Result<Option<StackLease>> {
        self.store.lease(&self.cfg.name).await
    }

    /// Set or replace the stack lease. An existing lease keeps its creation time and
    /// history; the replaced deadline is recorded as one more change.
    pub async fn set_lease(&self, request: LeaseRequest) -> anyhow::Result<StackLease> {
        let _lease_guard = self.lease_lock.lock().await;
        anyhow::ensure!(request.grace.is_zero(), "grace is reserved and must be 0s");
        let now = Utc::now();
        let deadline = match (request.deadline, request.duration) {
            (Some(_), Some(_)) => {
                anyhow::bail!("a lease takes either a deadline or a duration, not both")
            }
            (Some(deadline), None) => deadline,
            (None, Some(duration)) => now
                .checked_add_signed(to_delta(duration)?)
                .context("lease duration is too far in the future")?,
            (None, None) => anyhow::bail!("a lease needs a deadline or a duration"),
        };
        anyhow::ensure!(
            deadline > now,
            "lease deadline {deadline} is not in the future"
        );
        let mut lease = StackLease {
            stack: self.cfg.name.clone(),
            deadline,
            grace: request.grace,
            created_at: now,
            history: Vec::new(),
            fired: None,
        };
        if let Some(previous) = self.store.lease(&self.cfg.name).await? {
            lease.created_at = previous.created_at;
            lease.history = previous.history;
            lease.history.push(LeaseChange {
                at: now,
                from: previous.deadline,
                to: deadline,
            });
            // A destroy already queued by the old deadline is still going to happen.
            if let Some(fired) = previous.fired
                && self.run_in_flight(&fired.run_id).await?
            {
                lease.fired = Some(fired);
            }
        }
        self.store.put_lease(&lease).await?;
        tracing::info!(stack = %self.cfg.name, deadline = %lease.deadline, "lease set");
        Ok(lease)
    }

    /// Push an unexpired deadline later. An expired lease must be set anew.
    pub async fn extend_lease(&self, extension: LeaseExtension) -> anyhow::Result<StackLease> {
        let _lease_guard = self.lease_lock.lock().await;
        let mut lease = self.require_current_lease().await?;
        let now = Utc::now();
        let to = lease
            .deadline
            .checked_add_signed(to_delta(extension.by)?)
            .context("lease extension is too far in the future")?;
        lease.history.push(LeaseChange {
            at: now,
            from: lease.deadline,
            to,
        });
        lease.deadline = to;
        self.store.put_lease(&lease).await?;
        tracing::info!(stack = %self.cfg.name, deadline = %lease.deadline, "lease extended");
        Ok(lease)
    }

    pub async fn clear_lease(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.cfg.require_lease,
            "stack `{}` requires a lease; set a new deadline instead of clearing it",
            self.cfg.name
        );
        let _lease_guard = self.lease_lock.lock().await;
        self.store.clear_lease(&self.cfg.name).await
    }

    async fn run_in_flight(&self, run_id: &str) -> anyhow::Result<bool> {
        Ok(matches!(
            self.store.execution(run_id).await?,
            Some(run) if !run.status.terminal()
        ))
    }

    /// A deadline never waits for a human. Runs parked on approval hold the stack
    /// mutation lock indefinitely, so an expired lease cancels them.
    async fn cancel_approval_waits(&self) -> anyhow::Result<()> {
        for run in self.store.executions(&self.cfg.name, 10_000).await? {
            if run.status != ExecutionStatus::ApprovalWait {
                continue;
            }
            tracing::warn!(
                stack = %self.cfg.name,
                run = %run.run_id,
                kind = ?run.request.kind,
                "cancelled by the expired lease: an approval wait cannot outlast the deadline"
            );
            self.cancel(&run.run_id).await?;
        }
        Ok(())
    }

    async fn require_current_lease(&self) -> anyhow::Result<StackLease> {
        let lease = self
            .store
            .lease(&self.cfg.name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("stack `{}` has no lease", self.cfg.name))?;
        anyhow::ensure!(
            !lease.expired(Utc::now()),
            "stack `{}` lease expired at {}; set a new lease",
            self.cfg.name,
            lease.expires_at()
        );
        Ok(lease)
    }

    /// Queue a state-only destroy once the lease deadline has passed. Safe to call every
    /// tick: a queued destroy is left alone while it runs, one parked on approval is
    /// cancelled, a finished-but-failed one is retried no sooner than [`LEASE_REFIRE`],
    /// and an already-empty stack queues nothing. A destroy in replacement recovery needs
    /// an operator; the lease waits for that decision.
    pub async fn enforce_lease(self: &Arc<Self>) -> anyhow::Result<Option<ExecutionRun>> {
        let _lease_guard = self.lease_lock.lock().await;
        let Some(mut lease) = self.store.lease(&self.cfg.name).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        if !lease.expired(now) {
            return Ok(None);
        }
        if let Some(fired) = &lease.fired {
            if let Some(run) = self.store.execution(&fired.run_id).await? {
                match run.status {
                    ExecutionStatus::Queued
                    | ExecutionStatus::Running
                    | ExecutionStatus::RetryWait => return Ok(None),
                    ExecutionStatus::RecoveryWait => {
                        tracing::debug!(stack = %self.cfg.name, run = %run.run_id, "lease destroy is waiting for replacement recovery");
                        return Ok(None);
                    }
                    ExecutionStatus::ApprovalWait => {
                        let pending = run
                            .pending_approvals
                            .iter()
                            .map(ApprovalRequirement::selector)
                            .collect::<Vec<_>>();
                        tracing::warn!(stack = %self.cfg.name, run = %run.run_id, ?pending, "lease destroy needs approval; cancelling it and retrying later");
                        self.cancel(&run.run_id).await?;
                    }
                    ExecutionStatus::Succeeded
                    | ExecutionStatus::Failed
                    | ExecutionStatus::Cancelled => {}
                }
            }
            if now.signed_duration_since(fired.at) < to_delta(LEASE_REFIRE)? {
                return Ok(None);
            }
        }
        if self
            .store
            .load_state(&self.cfg.name)
            .await?
            .resources
            .is_empty()
        {
            return Ok(None);
        }
        self.cancel_approval_waits().await?;
        let mut request = RunRequest::new(RunKind::Destroy);
        request.retry.max_attempts = 1;
        let run = self.start_run(request).await?;
        // If recording the fire fails, the next tick queues a second destroy; it
        // serializes behind this one and finds nothing left to delete.
        lease.fired = Some(LeaseFire {
            run_id: run.run_id.clone(),
            at: now,
        });
        self.store.put_lease(&lease).await?;
        tracing::info!(stack = %self.cfg.name, run = %run.run_id, deadline = %lease.deadline, "lease expired; destroy queued");
        Ok(Some(run))
    }

    pub async fn build_status(&self) -> StackBuildStatus {
        self.build.read().await.status.clone()
    }

    async fn mark_source_dirty(&self) {
        let mut build = self.build.write().await;
        build.status.generation += 1;
        build.status.phase = BuildPhase::Dirty;
        build.status.changed_at = Utc::now();
        build.status.started_at = None;
        build.status.finished_at = None;
        build.status.duration_ms = None;
        build.status.revision = None;
        build.status.error = None;
        *self.last_error.write().await = None;
    }

    async fn mark_source_scan_error(&self, error: anyhow::Error) {
        let error = format!("scanning Rust stack sources: {error:#}");
        let mut build = self.build.write().await;
        if build.status.phase == BuildPhase::Failed
            && build.status.error.as_deref() == Some(error.as_str())
        {
            return;
        }
        build.status.generation += 1;
        build.status.phase = BuildPhase::Failed;
        build.status.changed_at = Utc::now();
        build.status.started_at = None;
        build.status.finished_at = Some(Utc::now());
        build.status.duration_ms = None;
        build.status.revision = None;
        build.status.error = Some(error.clone());
        *self.last_error.write().await = Some(error);
    }

    pub async fn retry_build(&self) -> StackBuildStatus {
        self.mark_source_dirty().await;
        self.ensure_built(true).await
    }

    async fn ensure_built(&self, force: bool) -> StackBuildStatus {
        let _serial = self.build_lock.lock().await;
        loop {
            let generation = {
                let mut build = self.build.write().await;
                if !force && build.status.phase == BuildPhase::Ready {
                    return build.status.clone();
                }
                build.status.phase = BuildPhase::Building;
                build.status.started_at = Some(Utc::now());
                build.status.finished_at = None;
                build.status.duration_ms = None;
                build.status.error = None;
                build.status.generation
            };
            let started = Instant::now();
            let context = match self.load_context() {
                Ok(context) => context,
                Err(error) => {
                    return self.finish_build_error(generation, 0, error).await;
                }
            };
            let compiler = self.compiler.clone();
            let manifest = Self::stack_manifest(&context);
            let current_dir = context.dir.clone();
            let emit_context = context.clone();
            let result = tokio::task::spawn_blocking(move || {
                let artifact = compiler.build(&manifest, &current_dir)?;
                let program = artifact.emit(&emit_context)?;
                let fingerprint = artifact.source_fingerprint();
                anyhow::Ok((artifact, program, fingerprint))
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

            let current_generation = self.build.read().await.status.generation;
            if current_generation != generation {
                continue;
            }

            match result {
                Ok((artifact, program, fingerprint)) => {
                    let revision = match self.store_program(program).await {
                        Ok(revision) => revision,
                        Err(error) => {
                            return self
                                .finish_build_error(generation, duration_ms, error)
                                .await;
                        }
                    };
                    let mut build = self.build.write().await;
                    if build.status.generation != generation {
                        continue;
                    }
                    build.artifact = Some(artifact);
                    build.context = Some(context);
                    build.fingerprint = Some(fingerprint);
                    build.status.phase = BuildPhase::Ready;
                    build.status.finished_at = Some(Utc::now());
                    build.status.duration_ms = Some(duration_ms);
                    build.status.revision = Some(revision.revision);
                    build.status.error = None;
                    *self.last_error.write().await = None;
                    return build.status.clone();
                }
                Err(error) => {
                    if error.downcast_ref::<SourcesChanged>().is_some() {
                        self.mark_source_dirty().await;
                        continue;
                    }
                    return self
                        .finish_build_error(generation, duration_ms, error)
                        .await;
                }
            }
        }
    }

    async fn finish_build_error(
        &self,
        generation: u64,
        duration_ms: u64,
        error: anyhow::Error,
    ) -> StackBuildStatus {
        let error = format!("{error:#}");
        let mut build = self.build.write().await;
        if build.status.generation == generation {
            build.status.phase = BuildPhase::Failed;
            build.status.finished_at = Some(Utc::now());
            build.status.duration_ms = Some(duration_ms);
            build.status.revision = None;
            build.status.error = Some(error.clone());
            *self.last_error.write().await = Some(error);
        }
        build.status.clone()
    }

    fn load_context(&self) -> anyhow::Result<LoadCtx> {
        let mut context =
            LoadCtx::from_dir(&self.ctx.dir, self.cfg.file.clone(), &self.ctx.stack, &[])?;
        // The store is selected when ifxd starts. Stack configuration is hot-reloaded,
        // but changing that persistence boundary requires a daemon restart.
        context.project = self.ctx.project.clone();
        Ok(context)
    }

    fn stack_manifest(context: &LoadCtx) -> PathBuf {
        context
            .file
            .clone()
            .unwrap_or_else(|| context.dir.join("Cargo.toml"))
    }

    async fn store_program(&self, program: Program) -> anyhow::Result<ProgramRevision> {
        let submission = ProgramSubmission::new(self.ctx.project.clone(), program);
        self.submit_program(submission).await
    }

    pub async fn resolve_program(
        &self,
        request: ProgramResolveRequest,
    ) -> anyhow::Result<ProgramRevision> {
        let status = self.ensure_fresh_build().await;
        anyhow::ensure!(
            status.phase == BuildPhase::Ready,
            "stack `{}` build is {:?}: {}",
            self.cfg.name,
            status.phase,
            status.error.as_deref().unwrap_or("build is not ready")
        );
        let (artifact, mut context, generation) = {
            let build = self.build.read().await;
            let artifact = build
                .artifact
                .clone()
                .context("ready stack has no compiled artifact")?;
            let context = build
                .context
                .clone()
                .context("ready stack has no build context")?;
            (artifact, context, build.status.generation)
        };
        if let Some(config) = request.config {
            context.config = config;
        }
        let program = tokio::task::spawn_blocking(move || artifact.emit(&context)).await??;
        let current = self.build_status().await;
        anyhow::ensure!(
            current.phase == BuildPhase::Ready && current.generation == generation,
            "stack `{}` changed while its Program was being emitted; retry",
            self.cfg.name
        );
        let revision = self.store_program(program).await?;
        let mut build = self.build.write().await;
        anyhow::ensure!(
            build.status.phase == BuildPhase::Ready && build.status.generation == generation,
            "stack `{}` changed while its Program revision was being stored; retry",
            self.cfg.name
        );
        build.status.revision = Some(revision.revision.clone());
        Ok(revision)
    }

    async fn ensure_fresh_build(&self) -> StackBuildStatus {
        loop {
            let status = self.ensure_built(false).await;
            if status.phase != BuildPhase::Ready {
                return status;
            }
            let (artifact, expected, generation) = {
                let build = self.build.read().await;
                let Some(artifact) = build.artifact.clone() else {
                    return build.status.clone();
                };
                (artifact, build.fingerprint, build.status.generation)
            };
            let roots = artifact.source_roots().to_vec();
            let inputs = artifact.source_inputs().to_vec();
            let actual = tokio::task::spawn_blocking(move || source_fingerprint(&roots, &inputs))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let current = self.build_status().await;
            if current.phase != BuildPhase::Ready || current.generation != generation {
                continue;
            }
            let actual = match actual {
                Ok(actual) => actual,
                Err(error) => {
                    self.mark_source_scan_error(error).await;
                    return self.build_status().await;
                }
            };
            if expected == Some(actual) {
                return current;
            }
            self.mark_source_dirty().await;
        }
    }

    async fn watch_sources(self: Arc<Self>) {
        let mut previous_roots = Vec::new();
        let mut previous_inputs = Vec::new();
        let mut previous = None;
        let mut scan_failed = false;
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let (roots, inputs, expected) = {
                let build = self.build.read().await;
                build
                    .artifact
                    .as_ref()
                    .map(|artifact| {
                        (
                            artifact.source_roots().to_vec(),
                            artifact.source_inputs().to_vec(),
                            build.fingerprint,
                        )
                    })
                    .unwrap_or_else(|| (vec![self.ctx.dir.clone()], Vec::new(), None))
            };
            let scan_roots = roots.clone();
            let scan_inputs = inputs.clone();
            let fingerprint = match tokio::task::spawn_blocking(move || {
                source_fingerprint(&scan_roots, &scan_inputs)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    tracing::warn!(stack = %self.cfg.name, "scanning Rust stack sources: {error:#}");
                    if !scan_failed {
                        self.mark_source_scan_error(error).await;
                        scan_failed = true;
                    }
                    continue;
                }
            };
            if scan_failed {
                scan_failed = false;
                previous_roots = roots;
                previous_inputs = inputs;
                previous = Some(fingerprint);
                self.mark_source_dirty().await;
                let watcher = self.clone();
                tokio::spawn(async move {
                    watcher.ensure_built(false).await;
                });
                continue;
            }
            if roots != previous_roots || inputs != previous_inputs {
                previous_roots = roots;
                previous_inputs = inputs;
                match expected {
                    Some(expected) => previous = Some(expected),
                    None => {
                        previous = Some(fingerprint);
                        continue;
                    }
                }
            }
            if previous == Some(fingerprint) {
                continue;
            }
            previous = Some(fingerprint);
            self.mark_source_dirty().await;
            let watcher = self.clone();
            tokio::spawn(async move {
                watcher.ensure_built(false).await;
            });
        }
    }

    pub async fn submit_program(
        &self,
        mut submission: ProgramSubmission,
    ) -> anyhow::Result<ProgramRevision> {
        if submission.project.is_empty() {
            submission.project = self.ctx.project.clone();
        }
        anyhow::ensure!(
            submission.project == self.ctx.project,
            "program project `{}` does not match daemon project `{}`",
            submission.project,
            self.ctx.project
        );
        submission.ensure_supported()?;
        validate_program(&self.registry, &submission.program)?;
        let revision = ProgramRevision::from_submission(&self.cfg.name, submission)?;
        self.store.put_program_revision(&revision, true).await?;
        let mut build = self.build.write().await;
        if build.status.phase != BuildPhase::Building {
            build.status.phase = BuildPhase::Ready;
            build.status.finished_at = Some(Utc::now());
            build.status.revision = Some(revision.revision.clone());
            build.status.error = None;
        }
        *self.last_error.write().await = None;
        Ok(revision)
    }

    pub async fn active_revision(&self) -> anyhow::Result<ProgramRevision> {
        let status = self.ensure_fresh_build().await;
        anyhow::ensure!(
            status.phase == BuildPhase::Ready,
            "stack `{}` build is {:?}: {}",
            self.cfg.name,
            status.phase,
            status.error.as_deref().unwrap_or("build is not ready")
        );
        let revision = status
            .revision
            .context("ready stack has no current Program revision")?;
        self.store
            .program_revision(&self.cfg.name, &revision)
            .await?
            .ok_or_else(|| anyhow::anyhow!("current revision `{revision}` not found"))
    }

    /// An expired lease blocks apply everywhere; a missing one blocks it only when the
    /// stack requires leases.
    async fn ensure_apply_leased(&self) -> anyhow::Result<()> {
        match self.store.lease(&self.cfg.name).await? {
            Some(lease) if lease.expired(Utc::now()) => anyhow::bail!(
                "stack `{}` lease expired at {}; set a new lease before applying",
                self.cfg.name,
                lease.expires_at()
            ),
            None if self.cfg.require_lease => anyhow::bail!(
                "stack `{}` requires a lease before apply; run `ifx lease set --for <duration>`",
                self.cfg.name
            ),
            _ => Ok(()),
        }
    }

    pub async fn start_run(self: &Arc<Self>, request: RunRequest) -> anyhow::Result<ExecutionRun> {
        anyhow::ensure!(
            request.parallelism > 0,
            "parallelism must be greater than zero"
        );
        anyhow::ensure!(
            request.retry.max_attempts > 0,
            "retry.max_attempts must be greater than zero"
        );
        anyhow::ensure!(
            !request.plan_only || request.kind == RunKind::Destroy,
            "plan_only is only valid for destroy runs"
        );
        if request.kind == RunKind::Apply {
            self.ensure_apply_leased().await?;
        }
        let needs_program = matches!(
            request.kind,
            RunKind::Plan | RunKind::Apply | RunKind::Refresh
        );
        let revision = if needs_program {
            let current = self.active_revision().await?;
            if let Some(requested) = request.revision.as_deref() {
                anyhow::ensure!(
                    requested == current.revision,
                    "revision `{requested}` is not the current source generation `{}`",
                    current.revision
                );
            }
            current
        } else {
            ProgramRevision {
                api_version: ifx::API_VERSION.into(),
                revision: "state-only".into(),
                stack: self.cfg.name.clone(),
                project: self.ctx.project.clone(),
                source_digest: None,
                program: Program::default(),
                submitted_at: Utc::now(),
            }
        };
        let run_id = next_run_id();
        let run = ExecutionRun::queued(&run_id, &self.cfg.name, &revision.revision, request);
        self.store.save_execution(&run).await?;
        self.record_control_event(&run_id, "queued", None, None, "run queued")
            .await?;
        self.launch_run(run.clone(), revision, None).await;
        Ok(run)
    }

    async fn launch_run(
        self: &Arc<Self>,
        run: ExecutionRun,
        revision: ProgramRevision,
        reserved_run: Option<OwnedMutexGuard<()>>,
    ) {
        let run_id = run.run_id.clone();
        let wake = Arc::new(Notify::new());
        self.retry_wakes
            .lock()
            .await
            .insert(run_id.clone(), wake.clone());
        let registered = Arc::new(Notify::new());
        let this = self.clone();
        let task_id = run_id.clone();
        let task_registered = registered.clone();
        let handle = tokio::spawn(async move {
            task_registered.notified().await;
            this.execute_run(run, revision, wake, reserved_run).await;
            this.tasks.lock().await.remove(&task_id);
            this.retry_wakes.lock().await.remove(&task_id);
        });
        self.tasks
            .lock()
            .await
            .insert(run_id, handle.abort_handle());
        registered.notify_one();
    }

    async fn resume_approval_waits(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut waits: Vec<_> = self
            .store
            .executions(&self.cfg.name, 10_000)
            .await?
            .into_iter()
            .filter(|run| run.status == ExecutionStatus::ApprovalWait)
            .collect();
        waits.sort_by_key(|run| run.requested_at);
        for (index, run) in waits.into_iter().enumerate() {
            let revision = self.revision_for_run(&run).await?;
            let reserved_run = if index == 0 {
                Some(self.run_lock.clone().lock_owned().await)
            } else {
                None
            };
            self.launch_run(run, revision, reserved_run).await;
        }
        Ok(())
    }

    async fn revision_for_run(&self, run: &ExecutionRun) -> anyhow::Result<ProgramRevision> {
        if run.revision == "state-only" {
            return Ok(ProgramRevision {
                api_version: ifx::API_VERSION.into(),
                revision: run.revision.clone(),
                stack: run.stack.clone(),
                project: self.ctx.project.clone(),
                source_digest: None,
                program: Program::default(),
                submitted_at: run.requested_at,
            });
        }
        self.store
            .program_revision(&run.stack, &run.revision)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "run `{}` references missing revision `{}`",
                    run.run_id,
                    run.revision
                )
            })
    }

    async fn save_execution_serialized(&self, run: &ExecutionRun) -> anyhow::Result<()> {
        let _write = self.execution_writes.lock().await;
        self.store.save_execution(run).await
    }

    async fn wait_for_approvals(
        &self,
        run_id: &str,
        wake: &Notify,
    ) -> anyhow::Result<Option<ExecutionRun>> {
        loop {
            let run = {
                let _write = self.execution_writes.lock().await;
                self.store.execution(run_id).await?
            }
            .ok_or_else(|| anyhow::anyhow!("run `{run_id}` disappeared during approval wait"))?;
            if run.status.terminal() {
                return Ok(None);
            }
            anyhow::ensure!(
                run.status == ExecutionStatus::ApprovalWait,
                "run `{run_id}` left approval_wait as {:?}",
                run.status
            );
            if pending_approvals_granted(&run) {
                return Ok(Some(run));
            }
            wake.notified().await;
        }
    }

    async fn execute_run(
        self: &Arc<Self>,
        mut run: ExecutionRun,
        revision: ProgramRevision,
        retry_wake: Arc<Notify>,
        reserved_run: Option<OwnedMutexGuard<()>>,
    ) {
        let mutation = matches!(run.request.kind, RunKind::Apply | RunKind::Refresh)
            || (run.request.kind == RunKind::Destroy && !run.request.plan_only);
        let _run = if mutation {
            match reserved_run {
                Some(run) => Some(run),
                None => Some(self.run_lock.clone().lock_owned().await),
            }
        } else {
            debug_assert!(reserved_run.is_none());
            None
        };
        if run.status == ExecutionStatus::ApprovalWait {
            match self
                .wait_for_approvals(&run.run_id, retry_wake.as_ref())
                .await
            {
                Ok(Some(saved)) => run = saved,
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(run = %run.run_id, "reloading durable approval wait: {error:#}");
                    return;
                }
            };
            let _ = self
                .record_control_event(
                    &run.run_id,
                    "approval_resumed",
                    None,
                    None,
                    "all pending approvals granted; resuming after daemon restart",
                )
                .await;
        }

        let first_attempt = run.attempt.max(1);
        for attempt in first_attempt..=run.request.retry.max_attempts {
            run.attempt = attempt;
            run.status = ExecutionStatus::Running;
            run.started_at.get_or_insert_with(Utc::now);
            run.next_retry_at = None;
            run.error = None;
            run.pending_approvals.clear();
            if let Err(error) = self.save_execution_serialized(&run).await {
                tracing::error!(run = %run.run_id, "saving running execution: {error:#}");
                return;
            }
            let _ = self
                .record_control_event(
                    &run.run_id,
                    "attempt_started",
                    None,
                    None,
                    &format!("attempt {attempt} started"),
                )
                .await;

            loop {
                let execution = if mutation {
                    let _observation = self.observation_lock.write().await;
                    self.execute_once(&run, &revision).await
                } else {
                    let _observation = self.observation_lock.read().await;
                    self.execute_once(&run, &revision).await
                };
                let (result, failure, pending) = match execution {
                    Ok(outcome) => outcome,
                    Err(error) => (Value::Null, Some(format!("{error:#}")), Vec::new()),
                };
                checkpoint_completed_triggers(&mut run, &result);
                run.result = result;
                if !pending.is_empty() {
                    run.status = ExecutionStatus::ApprovalWait;
                    run.pending_approvals = pending;
                    run.error = None;
                    if let Err(error) = self.save_execution_serialized(&run).await {
                        tracing::error!(run = %run.run_id, "saving approval wait: {error:#}");
                        return;
                    }
                    for approval in &run.pending_approvals {
                        let _ = self
                            .record_control_event(
                                &run.run_id,
                                "approval_required",
                                Some(approval.urn.clone()),
                                Some(approval.risk.clone()),
                                &format!(
                                    "{} [fingerprint {}]",
                                    approval.reason,
                                    approval.fingerprint.as_deref().unwrap_or("deferred")
                                ),
                            )
                            .await;
                    }
                    match self
                        .wait_for_approvals(&run.run_id, retry_wake.as_ref())
                        .await
                    {
                        Ok(Some(saved)) => run = saved,
                        Ok(None) => return,
                        Err(error) => {
                            tracing::error!(run = %run.run_id, "reloading approved run: {error:#}");
                            return;
                        }
                    };
                    let _ = self
                        .record_control_event(
                            &run.run_id,
                            "approval_resumed",
                            None,
                            None,
                            "all pending approvals granted; resuming the same attempt",
                        )
                        .await;
                    run.status = ExecutionStatus::Running;
                    run.pending_approvals.clear();
                    let _ = self.save_execution_serialized(&run).await;
                    continue;
                }

                match failure {
                    None => {
                        run.status = ExecutionStatus::Succeeded;
                        run.finished_at = Some(Utc::now());
                        run.next_retry_at = None;
                        run.pending_approvals.clear();
                        run.recovery_resources.clear();
                        run.error = None;
                        let _ = self.save_execution_serialized(&run).await;
                        let _ = self
                            .record_control_event(
                                &run.run_id,
                                "finished",
                                None,
                                None,
                                "run succeeded",
                            )
                            .await;
                        return;
                    }
                    Some(message) => {
                        if mutation {
                            match self.recovery_resources(&run.run_id).await {
                                Ok(resources) if !resources.is_empty() => {
                                    run.status = ExecutionStatus::RecoveryWait;
                                    run.finished_at = None;
                                    run.next_retry_at = None;
                                    run.pending_approvals.clear();
                                    run.recovery_resources = resources;
                                    run.error = Some(message.clone());
                                    let _ = self.save_execution_serialized(&run).await;
                                    let _ = self
                                        .record_control_event(
                                            &run.run_id,
                                            "recovery_wait",
                                            None,
                                            None,
                                            &message,
                                        )
                                        .await;
                                    return;
                                }
                                Err(error) => {
                                    let message = format!(
                                        "{message}; replacement state could not be verified: {error:#}"
                                    );
                                    run.status = ExecutionStatus::RecoveryWait;
                                    run.finished_at = None;
                                    run.next_retry_at = None;
                                    run.pending_approvals.clear();
                                    run.recovery_resources.clear();
                                    run.error = Some(message.clone());
                                    let _ = self.save_execution_serialized(&run).await;
                                    let _ = self
                                        .record_control_event(
                                            &run.run_id,
                                            "recovery_wait",
                                            None,
                                            None,
                                            &message,
                                        )
                                        .await;
                                    return;
                                }
                                Ok(_) => {}
                            }
                        }
                        run.error = Some(message.clone());
                        if attempt >= run.request.retry.max_attempts {
                            run.status = ExecutionStatus::Failed;
                            run.finished_at = Some(Utc::now());
                            let _ = self.save_execution_serialized(&run).await;
                            let _ = self
                                .record_control_event(&run.run_id, "failed", None, None, &message)
                                .await;
                            return;
                        }
                        let wait = run.request.retry.backoff_secs(attempt);
                        let retry_at = Utc::now() + chrono::Duration::seconds(wait as i64);
                        run.status = ExecutionStatus::RetryWait;
                        run.next_retry_at = Some(retry_at);
                        let _ = self.save_execution_serialized(&run).await;
                        let _ = self
                            .record_control_event(
                                &run.run_id,
                                "retry_scheduled",
                                None,
                                None,
                                &format!(
                                    "attempt {attempt} failed; retrying in {wait}s: {message}"
                                ),
                            )
                            .await;
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_secs(wait)) => {}
                            () = retry_wake.notified() => {
                                let _ = self.record_control_event(
                                    &run.run_id,
                                    "retry_triggered",
                                    None,
                                    None,
                                    "operator triggered retry now",
                                ).await;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    async fn recovery_resources(&self, run_id: &str) -> anyhow::Result<Vec<ifx::model::Urn>> {
        let state = self.store.load_state(&self.cfg.name).await?;
        Ok(state
            .pending_replacements
            .iter()
            .filter(|(_, replacement)| replacement.execution_run_id == run_id)
            .map(|(urn, _)| urn.clone())
            .collect())
    }

    async fn execute_once(
        &self,
        run: &ExecutionRun,
        revision: &ProgramRevision,
    ) -> anyhow::Result<(Value, Option<String>, Vec<ApprovalRequirement>)> {
        let mutation = matches!(run.request.kind, RunKind::Apply | RunKind::Refresh)
            || (run.request.kind == RunKind::Destroy && !run.request.plan_only);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(move |event: Event| {
            let _ = event_tx.send(event);
        });
        let event_store = self.store.clone();
        let event_run = run.run_id.clone();
        let event_stack = run.stack.clone();
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let event = engine_event(&event_run, &event_stack, event);
                if let Err(error) = event_store.record_execution_event(&event).await {
                    tracing::warn!(run = %event_run, "saving execution event: {error:#}");
                }
            }
        });

        let engine = Engine::new(self.registry.clone())
            .with_transports(self.transports.clone())
            .with_parallelism(run.request.parallelism)
            .with_store(self.store.clone())
            .with_execution_identity(run.run_id.clone(), run.revision.clone())
            .with_events(sink);
        let mut state = self.store.load_state(&run.stack).await?;
        state.stack = run.stack.clone();
        if mutation {
            for urn in engine.recover_execution_access(&state).await? {
                tracing::warn!(stack = %self.cfg.name, run = %run.run_id, %urn, "sealed management before execution");
            }
            for urn in engine
                .recover_program_execution_access(&revision.program, &state)
                .await?
            {
                tracing::warn!(stack = %self.cfg.name, run = %run.run_id, %urn, "recovered provider access before execution");
            }
        }
        if matches!(run.request.kind, RunKind::Apply | RunKind::Refresh)
            || (run.request.kind == RunKind::Destroy && !run.request.plan_only)
        {
            let approvals = granted_requirements(run);
            let recovery = engine
                .recover_pending_replacements(&mut state, &run.run_id, &approvals)
                .await?;
            for urn in &recovery.recovered {
                tracing::info!(stack = %self.cfg.name, run = %run.run_id, %urn, "recovered interrupted replacement");
            }
            if !recovery.pending_approvals.is_empty() {
                let pending = recovery.pending_approvals.clone();
                return Ok((json!({"recovery": recovery}), None, pending));
            }
        }
        let options = Options {
            targets: run.request.targets.clone(),
            no_refresh: run.request.no_refresh,
        };
        let program = &revision.program;

        let outcome = match run.request.kind {
            RunKind::Plan => {
                let plan = engine.plan(program, &state, &options).await?;
                (json!({"plan": plan}), None, Vec::new())
            }
            RunKind::Apply => {
                let plan = match saved_plan(run)? {
                    Some(plan) => plan,
                    None => engine.plan(program, &state, &options).await?,
                };
                let execution_plan = plan_for_execution(run, &plan);
                let approvals = granted_requirements(run);
                let report = engine
                    .apply_approved(program, &mut state, &execution_plan, &approvals)
                    .await?;
                let pending = report.pending_approvals.clone();
                let failure = (!report.failed.is_empty())
                    .then(|| format!("apply failed: {:?}", report.failed));
                let checks = if failure.is_none() && pending.is_empty() {
                    monitor::run_checks_with_management(&engine, &state).await?
                } else {
                    Vec::new()
                };
                (
                    json!({"plan": plan, "report": report, "checks": checks}),
                    failure,
                    pending,
                )
            }
            RunKind::Destroy => {
                let empty = Program::default();
                let plan = match saved_plan(run)? {
                    Some(plan) => plan,
                    None => engine.plan(&empty, &state, &options).await?,
                };
                if run.request.plan_only {
                    (json!({"plan": plan}), None, Vec::new())
                } else {
                    let approvals = granted_requirements(run);
                    let report = engine
                        .apply_approved(&empty, &mut state, &plan, &approvals)
                        .await?;
                    let pending = report.pending_approvals.clone();
                    let failure = (!report.failed.is_empty())
                        .then(|| format!("destroy failed: {:?}", report.failed));
                    (json!({"plan": plan, "report": report}), failure, pending)
                }
            }
            RunKind::Refresh => {
                let changed = engine.refresh(program, &mut state).await?;
                (json!({"changed": changed}), None, Vec::new())
            }
            RunKind::Check => {
                let checks = monitor::run_checks(&engine, &state).await?;
                (json!({"checks": checks}), None, Vec::new())
            }
            RunKind::Drift => {
                let records = monitor::detect_drift(&engine, program, &state).await?;
                (json!({"records": records}), None, Vec::new())
            }
        };
        drop(engine);
        let _ = event_task.await;
        Ok(outcome)
    }

    pub async fn retry_now(self: &Arc<Self>, run_id: &str) -> anyhow::Result<ExecutionRun> {
        let mut run = self
            .store
            .execution(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run `{run_id}` not found"))?;
        match run.status {
            ExecutionStatus::RetryWait => {
                let wake = self
                    .retry_wakes
                    .lock()
                    .await
                    .get(run_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("run `{run_id}` has no active retry waiter"))?;
                wake.notify_one();
                Ok(run)
            }
            ExecutionStatus::RecoveryWait => {
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if !self.tasks.lock().await.contains_key(run_id) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                anyhow::ensure!(
                    !self.tasks.lock().await.contains_key(run_id),
                    "run `{run_id}` is still entering recovery wait; retry shortly"
                );
                let revision = self.revision_for_run(&run).await?;
                run.status = ExecutionStatus::Queued;
                run.finished_at = None;
                run.next_retry_at = None;
                run.error = None;
                run.pending_approvals.clear();
                self.save_execution_serialized(&run).await?;
                self.record_control_event(
                    run_id,
                    "recovery_retry",
                    None,
                    None,
                    "operator triggered replacement recovery",
                )
                .await?;
                self.launch_run(run.clone(), revision, None).await;
                Ok(run)
            }
            status => {
                anyhow::bail!("run `{run_id}` is {status:?}, not waiting to retry or recover")
            }
        }
    }

    pub async fn approve(
        &self,
        run_id: &str,
        grant: ApprovalGrant,
    ) -> anyhow::Result<ExecutionRun> {
        let _write = self.execution_writes.lock().await;
        let mut run = self
            .store
            .execution(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run `{run_id}` not found"))?;
        anyhow::ensure!(
            run.status == ExecutionStatus::ApprovalWait,
            "run `{run_id}` is {:?}, not waiting for approval",
            run.status
        );
        anyhow::ensure!(
            grant.revision == run.revision,
            "approval revision `{}` does not match run revision `{}`",
            grant.revision,
            run.revision
        );
        let pending = run.pending_approvals.iter().any(|requirement| {
            requirement.urn == grant.urn
                && requirement.risk == grant.risk
                && requirement.fingerprint.as_deref() == Some(grant.fingerprint.as_str())
        });
        anyhow::ensure!(
            pending,
            "approval `{}/{}` with fingerprint {} is not pending on run `{run_id}`",
            grant.urn,
            grant.risk,
            grant.fingerprint
        );
        if !run.request.approvals.contains(&grant) {
            run.request.approvals.push(grant.clone());
        }
        self.store.save_execution(&run).await?;
        if pending_approvals_granted(&run) {
            if let Some(wake) = self.retry_wakes.lock().await.get(run_id).cloned() {
                wake.notify_one();
            }
        }
        if let Err(error) = self
            .record_control_event(
                run_id,
                "approval_granted",
                Some(grant.urn),
                Some(grant.risk),
                &format!("operator granted fingerprint {}", grant.fingerprint),
            )
            .await
        {
            tracing::warn!(run = %run_id, "recording durable approval grant event: {error:#}");
        }
        Ok(run)
    }

    pub async fn execution(&self, run_id: &str) -> anyhow::Result<Option<ExecutionRun>> {
        self.store.execution(run_id).await
    }

    pub async fn executions(&self, limit: usize) -> anyhow::Result<Vec<ExecutionRun>> {
        self.store.executions(&self.cfg.name, limit).await
    }

    pub async fn execution_events(&self, run_id: &str) -> anyhow::Result<Vec<ExecutionEvent>> {
        self.store.execution_events(run_id).await
    }

    /// Capture topology without racing provider observation against an active execution.
    /// When the stack lease is busy, state supplies the observed side of the snapshot.
    pub async fn topology(
        &self,
        no_refresh: bool,
    ) -> anyhow::Result<ifx::explorer::TopologySnapshot> {
        let lease = if no_refresh {
            None
        } else {
            self.observation_lock.try_read().ok()
        };
        let build = self.build_status().await;
        let revision = if build.phase == BuildPhase::Ready {
            Some(self.active_revision().await?)
        } else {
            self.store.active_program_revision(&self.cfg.name).await?
        };
        let mut state = self.store.load_state(&self.cfg.name).await?;
        state.stack = self.cfg.name.clone();
        let empty = Program::default();
        let program = revision
            .as_ref()
            .map(|revision| &revision.program)
            .unwrap_or(&empty);
        let mut snapshot = ifx::explorer::capture(
            &self.engine,
            &self.cfg.name,
            program,
            &state,
            &Options {
                no_refresh: no_refresh || lease.is_none() || build.phase != BuildPhase::Ready,
                ..Options::default()
            },
        )
        .await?;
        snapshot.active_revision = build.revision.clone();
        snapshot.build = Some(build);
        drop(lease);
        Ok(snapshot)
    }

    pub async fn state(&self) -> anyhow::Result<ifx::state::State> {
        self.store.load_state(&self.cfg.name).await
    }

    pub async fn replace_state(
        &self,
        mut state: ifx::state::State,
    ) -> anyhow::Result<ifx::state::State> {
        let _run = self.run_lock.lock().await;
        let _observation = self.observation_lock.write().await;
        let current = self.store.load_state(&self.cfg.name).await?;
        anyhow::ensure!(
            current.pending_replacements.is_empty() && state.pending_replacements.is_empty(),
            "state replacement is disabled while an interrupted replacement journal exists; recover or abort it explicitly"
        );
        state.stack = self.cfg.name.clone();
        self.store.save_state(&state).await?;
        Ok(state)
    }

    pub async fn forget_state(&self, urn: &ifx::model::Urn) -> anyhow::Result<ifx::state::State> {
        let _run = self.run_lock.lock().await;
        let _observation = self.observation_lock.write().await;
        let mut state = self.store.load_state(&self.cfg.name).await?;
        anyhow::ensure!(
            !state.pending_replacements.contains_key(urn),
            "cannot forget {urn} while its interrupted replacement journal exists; recover or abort it explicitly"
        );
        anyhow::ensure!(state.remove(urn).is_some(), "{urn} not in state");
        self.store.save_state(&state).await?;
        Ok(state)
    }

    pub async fn cancel(&self, run_id: &str) -> anyhow::Result<ExecutionRun> {
        if let Some(handle) = self.tasks.lock().await.remove(run_id) {
            handle.abort();
        }
        self.retry_wakes.lock().await.remove(run_id);

        // Ensure an aborted provider future has released the stack before rollback.
        let _run = self.run_lock.lock().await;
        let _observation = self.observation_lock.write().await;
        let _write = self.execution_writes.lock().await;
        let mut run = self
            .store
            .execution(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run `{run_id}` not found"))?;
        anyhow::ensure!(!run.status.terminal(), "run `{run_id}` is already terminal");
        let mut state = self.store.load_state(&run.stack).await?;
        state.stack = run.stack.clone();
        for urn in self.engine.recover_execution_access(&state).await? {
            tracing::warn!(stack = %self.cfg.name, run = %run.run_id, %urn, "sealed management while cancelling execution");
        }
        let revision = self.revision_for_run(&run).await?;
        for urn in self
            .engine
            .recover_program_execution_access(&revision.program, &state)
            .await?
        {
            tracing::warn!(stack = %self.cfg.name, run = %run.run_id, %urn, "recovered provider access while cancelling execution");
        }
        let recovery_resources = state
            .pending_replacements
            .iter()
            .filter(|(_, replacement)| replacement.execution_run_id == run.run_id)
            .map(|(urn, _)| urn.clone())
            .collect::<Vec<_>>();
        if recovery_resources.is_empty() {
            run.status = ExecutionStatus::Cancelled;
            run.finished_at = Some(Utc::now());
            run.next_retry_at = None;
            run.recovery_resources.clear();
            run.error = None;
            self.store.save_execution(&run).await?;
            self.record_control_event(run_id, "cancelled", None, None, "run cancelled")
                .await?;
            return Ok(run);
        }

        match self
            .engine
            .abort_pending_replacements(&mut state, &run.run_id)
            .await
        {
            Ok(aborted) => {
                run.status = ExecutionStatus::Cancelled;
                run.finished_at = Some(Utc::now());
                run.next_retry_at = None;
                run.recovery_resources.clear();
                run.error = None;
                self.store.save_execution(&run).await?;
                self.record_control_event(
                    run_id,
                    "recovery_aborted",
                    None,
                    None,
                    &format!("rolled back {} interrupted replacement(s)", aborted.len()),
                )
                .await?;
            }
            Err(error) => {
                let message = format!("replacement rollback is incomplete: {error:#}");
                run.status = ExecutionStatus::RecoveryWait;
                run.finished_at = None;
                run.next_retry_at = None;
                run.recovery_resources = recovery_resources;
                run.error = Some(message.clone());
                self.store.save_execution(&run).await?;
                self.record_control_event(run_id, "recovery_rollback_failed", None, None, &message)
                    .await?;
            }
        }
        Ok(run)
    }

    async fn record_control_event(
        &self,
        run_id: &str,
        kind: &str,
        urn: Option<ifx::model::Urn>,
        action: Option<String>,
        message: &str,
    ) -> anyhow::Result<()> {
        self.store
            .record_execution_event(&ExecutionEvent {
                run_id: run_id.to_string(),
                stack: self.cfg.name.clone(),
                kind: kind.to_string(),
                urn,
                action,
                message: message.to_string(),
                at: Utc::now(),
            })
            .await
    }

    pub async fn run_checks(&self) -> Vec<HealthRecord> {
        let Ok(_observation) = self.observation_lock.try_read() else {
            tracing::debug!(stack = %self.cfg.name, "skipping scheduled checks while a run owns the stack");
            return Vec::new();
        };
        let state = match self.store.load_state(&self.cfg.name).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(stack = %self.cfg.name, "loading state: {error:#}");
                return Vec::new();
            }
        };
        match monitor::run_checks(&self.engine, &state).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(stack = %self.cfg.name, "checks: {error:#}");
                Vec::new()
            }
        }
    }

    pub async fn run_drift(&self) -> Vec<HealthRecord> {
        let Ok(_observation) = self.observation_lock.try_read() else {
            tracing::debug!(stack = %self.cfg.name, "skipping scheduled drift while a run owns the stack");
            return Vec::new();
        };
        let revision = match self.active_revision().await {
            Ok(revision) => revision,
            Err(error) => {
                *self.last_error.write().await = Some(format!("{error:#}"));
                return Vec::new();
            }
        };
        let state = match self.store.load_state(&self.cfg.name).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(stack = %self.cfg.name, "loading state: {error:#}");
                return Vec::new();
            }
        };
        match monitor::detect_drift(&self.engine, &revision.program, &state).await {
            Ok(records) => {
                *self.last_error.write().await = None;
                records
            }
            Err(error) => {
                *self.last_error.write().await = Some(format!("{error:#}"));
                Vec::new()
            }
        }
    }
}

fn validate_program(registry: &ifx::Registry, program: &Program) -> anyhow::Result<()> {
    ifx::graph::Graph::build(program)?;
    for declaration in &program.resources {
        let handler = registry.get(declaration.type_name()).ok_or_else(|| {
            anyhow::anyhow!(
                "{}: unknown resource type `{}`",
                declaration.urn,
                declaration.type_name()
            )
        })?;
        let schema = handler.schema();
        let mut inputs = declaration.inputs.clone();
        schema.apply_defaults(&mut inputs);
        schema.validate(&inputs).map_err(|errors| {
            anyhow::anyhow!(
                "{}: invalid inputs:\n  {}",
                declaration.urn,
                errors.join("\n  ")
            )
        })?;
    }
    Ok(())
}

fn saved_plan(run: &ExecutionRun) -> anyhow::Result<Option<Plan>> {
    run.result
        .get("plan")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

fn plan_for_execution(run: &ExecutionRun, plan: &Plan) -> Plan {
    let mut execution = plan.clone();
    for operation in &mut execution.ops {
        if matches!(operation.action, Action::Trigger)
            && run.completed_triggers.contains(&operation.urn)
        {
            operation.action = Action::NoOp;
        }
    }
    execution
}

fn checkpoint_completed_triggers(run: &mut ExecutionRun, result: &Value) {
    let (Some(plan), Some(report)) = (result.get("plan"), result.get("report")) else {
        return;
    };
    let (Ok(plan), Ok(report)) = (
        serde_json::from_value::<Plan>(plan.clone()),
        serde_json::from_value::<Report>(report.clone()),
    ) else {
        return;
    };
    for (urn, _) in report.applied {
        if plan
            .get(&urn)
            .is_some_and(|operation| matches!(operation.action, Action::Trigger))
            && !run.completed_triggers.contains(&urn)
        {
            run.completed_triggers.push(urn);
        }
    }
}

fn granted_requirements(run: &ExecutionRun) -> Vec<ApprovalRequirement> {
    run.request
        .approvals
        .iter()
        .filter(|grant| grant.revision == run.revision)
        .map(|grant| ApprovalRequirement {
            urn: grant.urn.clone(),
            risk: grant.risk.clone(),
            reason: String::new(),
            fingerprint: Some(grant.fingerprint.clone()),
        })
        .collect()
}

fn pending_approvals_granted(run: &ExecutionRun) -> bool {
    run.pending_approvals.iter().all(|requirement| {
        let Some(fingerprint) = requirement.fingerprint.as_deref() else {
            return false;
        };
        run.request.approvals.iter().any(|grant| {
            grant.revision == run.revision
                && grant.urn == requirement.urn
                && grant.risk == requirement.risk
                && grant.fingerprint == fingerprint
        })
    })
}

fn to_delta(duration: Duration) -> anyhow::Result<TimeDelta> {
    TimeDelta::from_std(duration).map_err(|_| anyhow::anyhow!("duration {duration:?} is too large"))
}

fn next_run_id() -> String {
    let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("run-{}-{sequence}", Utc::now().format("%Y%m%dT%H%M%S%3f"))
}

fn action_name(action: &Action) -> String {
    match action {
        Action::Create => "create",
        Action::Update(_) => "update",
        Action::Replace(_) => "replace",
        Action::Delete => "delete",
        Action::Trigger => "trigger",
        Action::Adopt => "adopt",
        Action::NoOp => "unchanged",
    }
    .to_string()
}

fn engine_event(run_id: &str, stack: &str, event: Event) -> ExecutionEvent {
    let (kind, urn, action, message) = match event {
        Event::Started { urn, action } => (
            "resource_started",
            urn,
            Some(action_name(&action)),
            String::new(),
        ),
        Event::Finished {
            urn,
            action,
            outputs: _,
        } => (
            "resource_finished",
            urn,
            Some(action_name(&action)),
            String::new(),
        ),
        Event::Failed { urn, action, error } => {
            ("resource_failed", urn, Some(action_name(&action)), error)
        }
        Event::ApprovalAccepted { approval } => (
            "approval_accepted",
            approval.urn,
            Some(approval.risk),
            format!(
                "live operation matched approved fingerprint {}",
                approval.fingerprint.as_deref().unwrap_or("deferred")
            ),
        ),
    };
    ExecutionEvent {
        run_id: run_id.to_string(),
        stack: stack.to_string(),
        kind: kind.to_string(),
        urn: Some(urn),
        action,
        message,
        at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ifx::control::{ApprovalGrant, ProgramSubmission, RetryPolicy};
    use ifx::model::ResourceDecl;
    use ifx::provider::{Actual, Applied, Ctx, Handler, OperationKind, OperationRisk, Result};
    use ifx::providers::memory::Memory;
    use ifx::state::{Entry, PendingReplacement, State};
    use ifx::store::SurrealStore;
    use serde_json::json;
    use tempfile::TempDir;

    async fn watcher() -> (TempDir, Arc<Watcher>) {
        watcher_with_registry(ifx::Registry::builtin()).await
    }

    async fn watcher_with_registry(registry: ifx::Registry) -> (TempDir, Arc<Watcher>) {
        watcher_with(registry, false).await
    }

    async fn watcher_with(registry: ifx::Registry, require_lease: bool) -> (TempDir, Arc<Watcher>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LoadCtx::from_dir(dir.path(), None, "test", &[]).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            SurrealStore::connect("mem://", "ifx", "watcher-test")
                .await
                .unwrap(),
        );
        let cfg = StackConfig {
            dir: dir.path().to_path_buf(),
            name: "test".into(),
            file: None,
            check_interval: None,
            drift_interval: None,
            no_drift: true,
            require_lease,
        };
        let watcher = Arc::new(Watcher::start(ctx, cfg, registry, store).await.unwrap());
        (dir, watcher)
    }

    #[tokio::test]
    async fn stack_configuration_is_reloaded_for_each_generation() {
        let (dir, watcher) = watcher().await;
        std::fs::write(
            dir.path().join("ifx.toml"),
            "file = \"stack/Cargo.toml\"\n[config]\nanswer = 42\n",
        )
        .unwrap();

        let configured = watcher.load_context().unwrap();
        assert_eq!(
            configured.file.as_deref(),
            Some(dir.path().join("stack/Cargo.toml").as_path())
        );
        assert_eq!(configured.config["answer"], json!(42));

        std::fs::write(dir.path().join("ifx.toml"), "[config]\n").unwrap();
        let cleared = watcher.load_context().unwrap();
        assert!(cleared.config.is_empty());
        assert!(cleared.file.is_none());
    }

    #[test]
    fn source_fingerprint_tracks_optional_inputs_but_requires_source_roots() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let optional = dir.path().join("ancestor/.cargo/config.toml");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("main.rs"), "fn main() {}\n").unwrap();
        let roots = vec![source.clone()];
        let inputs = vec![optional.clone()];
        let absent = source_fingerprint(&roots, &inputs).unwrap();

        std::fs::create_dir_all(optional.parent().unwrap()).unwrap();
        std::fs::write(&optional, "[build]\n").unwrap();
        let present = source_fingerprint(&roots, &inputs).unwrap();
        assert_ne!(absent, present);

        std::fs::remove_dir_all(&source).unwrap();
        assert!(source_fingerprint(&roots, &inputs).is_err());
    }

    #[derive(Clone)]
    struct FailingReplacementMemory {
        memory: Memory,
        fail_replace: Arc<std::sync::atomic::AtomicBool>,
        fail_abort: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Handler for FailingReplacementMemory {
        fn schema(&self) -> ifx::schema::ResourceSchema {
            self.memory.schema()
        }

        async fn read(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            inputs: &Value,
        ) -> Result<Option<Actual>> {
            self.memory.read(cx, id, inputs).await
        }

        async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
            self.memory.create(cx, inputs).await
        }

        async fn replace(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            old_inputs: &Value,
            inputs: &Value,
            _actual: &Actual,
        ) -> Result<Applied> {
            if self.fail_replace.load(Ordering::SeqCst) {
                anyhow::bail!("simulated replacement interruption");
            }
            self.memory.delete(cx, id, old_inputs).await?;
            self.memory.create(cx, inputs).await
        }

        async fn abort_replace(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            old_inputs: &Value,
            _inputs: &Value,
        ) -> Result<()> {
            if self.fail_abort.load(Ordering::SeqCst) {
                anyhow::bail!("simulated replacement rollback failure");
            }
            let actual = self
                .memory
                .read(cx, id, old_inputs)
                .await?
                .ok_or_else(|| anyhow::anyhow!("the previous resource no longer exists"))?;
            anyhow::ensure!(
                self.memory.diff(old_inputs, &actual)?.is_empty(),
                "the previous resource no longer matches its applied state"
            );
            Ok(())
        }

        async fn update(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            inputs: &Value,
            actual: &Actual,
        ) -> Result<Applied> {
            self.memory.update(cx, id, inputs, actual).await
        }

        async fn delete(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<()> {
            self.memory.delete(cx, id, inputs).await
        }
    }

    #[test]
    fn approvals_are_bound_to_revision_resource_risk_and_fingerprint() {
        let urn = ifx::model::Urn::new("qemu.instance", "node");
        let requirement = ApprovalRequirement {
            urn: urn.clone(),
            risk: "restart".into(),
            reason: "attachment change requires a restart".into(),
            fingerprint: Some("operation-a".into()),
        };
        let mut request = RunRequest::new(RunKind::Apply);
        request.approvals.push(ApprovalGrant {
            revision: "revision-b".into(),
            urn: urn.clone(),
            risk: "restart".into(),
            fingerprint: "operation-a".into(),
        });
        let mut run = ExecutionRun::queued("run", "test", "revision-a", request);
        run.pending_approvals = vec![requirement];
        assert!(!pending_approvals_granted(&run));

        run.request.approvals[0].revision = "revision-a".into();
        assert!(pending_approvals_granted(&run));

        run.request.approvals[0].fingerprint = "operation-b".into();
        assert!(!pending_approvals_granted(&run));
    }

    #[test]
    fn completed_planned_triggers_are_checkpointed_across_partial_runs() {
        let urn = ifx::model::Urn::new("memory.value", "runner");
        let plan = Plan {
            ops: vec![ifx::engine::PlannedOp {
                urn: urn.clone(),
                action: Action::Trigger,
                desired: json!({"value": "same"}),
                secrets: Vec::new(),
                actual: None,
                outputs: json!({"value": "same"}),
                skipped: false,
                approvals: Vec::new(),
            }],
            warnings: Vec::new(),
        };
        let result = json!({
            "plan": plan,
            "report": Report {
                applied: vec![(urn.clone(), Action::Update(ifx::provider::Diff::default()))],
                ..Report::default()
            }
        });
        let mut run = ExecutionRun::queued(
            "trigger-checkpoint",
            "test",
            "revision",
            RunRequest::new(RunKind::Apply),
        );

        checkpoint_completed_triggers(&mut run, &result);
        checkpoint_completed_triggers(&mut run, &result);
        assert_eq!(run.completed_triggers, [urn]);
        assert!(matches!(
            plan_for_execution(&run, &plan).ops[0].action,
            Action::NoOp
        ));
        assert!(matches!(plan.ops[0].action, Action::Trigger));
    }

    async fn wait_for(watcher: &Watcher, run_id: &str, wanted: ExecutionStatus) -> ExecutionRun {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let run = watcher.execution(run_id).await.unwrap().unwrap();
                if run.status == wanted || run.status.terminal() {
                    return run;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn interrupted_replacement(
        fail_abort: bool,
    ) -> (
        TempDir,
        Arc<Watcher>,
        Arc<std::sync::atomic::AtomicBool>,
        ExecutionRun,
    ) {
        let fail_replace = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = ifx::Registry::new();
        registry.register(FailingReplacementMemory {
            memory: Memory::default(),
            fail_replace: fail_replace.clone(),
            fail_abort: Arc::new(std::sync::atomic::AtomicBool::new(fail_abort)),
        });
        let (dir, watcher) = watcher_with_registry(registry).await;
        watcher
            .submit_program(ProgramSubmission::new(
                watcher.ctx.project.clone(),
                Program {
                    resources: vec![ResourceDecl::new(
                        "memory.value",
                        "answer",
                        json!({"value": 1, "key": "old"}),
                    )],
                },
            ))
            .await
            .unwrap();
        let initial = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap();
        assert_eq!(
            wait_for(&watcher, &initial.run_id, ExecutionStatus::Succeeded)
                .await
                .status,
            ExecutionStatus::Succeeded
        );

        fail_replace.store(true, Ordering::SeqCst);
        watcher
            .submit_program(ProgramSubmission::new(
                watcher.ctx.project.clone(),
                Program {
                    resources: vec![ResourceDecl::new(
                        "memory.value",
                        "answer",
                        json!({"value": 2, "key": "new"}),
                    )],
                },
            ))
            .await
            .unwrap();
        let mut request = RunRequest::new(RunKind::Apply);
        request.retry.max_attempts = 3;
        let interrupted = watcher.start_run(request).await.unwrap();
        let waiting = wait_for(&watcher, &interrupted.run_id, ExecutionStatus::RecoveryWait).await;
        assert_eq!(waiting.status, ExecutionStatus::RecoveryWait);
        assert_eq!(waiting.attempt, 1);
        assert_eq!(waiting.recovery_resources.len(), 1);
        (dir, watcher, fail_replace, interrupted)
    }

    async fn seed_approval_wait(
        watcher: &Watcher,
        run_id: &str,
    ) -> (ProgramRevision, ApprovalRequirement) {
        let revision = watcher
            .submit_program(ProgramSubmission::new(
                watcher.ctx.project.clone(),
                Program {
                    resources: vec![ResourceDecl::new(
                        "memory.value",
                        "answer",
                        json!({"value": 42}),
                    )],
                },
            ))
            .await
            .unwrap();
        let requirement = ApprovalRequirement {
            urn: ifx::model::Urn::new("memory.value", "answer"),
            risk: "restart".into(),
            reason: "test restart approval".into(),
            fingerprint: Some("exact-operation".into()),
        };
        let mut run = ExecutionRun::queued(
            run_id,
            &watcher.cfg.name,
            &revision.revision,
            RunRequest::new(RunKind::Apply),
        );
        run.status = ExecutionStatus::ApprovalWait;
        run.attempt = 1;
        run.pending_approvals = vec![requirement.clone()];
        watcher.store.save_execution(&run).await.unwrap();
        (revision, requirement)
    }

    async fn grant(
        watcher: &Watcher,
        run_id: &str,
        revision: &ProgramRevision,
        requirement: &ApprovalRequirement,
    ) -> anyhow::Result<ExecutionRun> {
        watcher
            .approve(
                run_id,
                ApprovalGrant {
                    revision: revision.revision.clone(),
                    urn: requirement.urn.clone(),
                    risk: requirement.risk.clone(),
                    fingerprint: requirement.fingerprint.clone().unwrap(),
                },
            )
            .await
    }

    #[tokio::test]
    async fn submission_and_apply_are_daemon_owned_and_persisted() {
        let (_dir, watcher) = watcher().await;
        let program = Program {
            resources: vec![ResourceDecl::new(
                "memory.value",
                "answer",
                json!({"value": 42}),
            )],
        };
        let revision = watcher
            .submit_program(ProgramSubmission::new(watcher.ctx.project.clone(), program))
            .await
            .unwrap();
        assert_eq!(
            watcher.active_revision().await.unwrap().revision,
            revision.revision
        );

        let run = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap();
        let finished = wait_for(&watcher, &run.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(finished.status, ExecutionStatus::Succeeded);
        assert!(
            finished.result["report"]["failed"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            watcher
                .state()
                .await
                .unwrap()
                .resources
                .contains_key(&ifx::model::Urn::new("memory.value", "answer"))
        );
        let events = watcher.execution_events(&run.run_id).await.unwrap();
        assert!(events.iter().any(|event| event.kind == "queued"));
        assert!(events.iter().any(|event| event.kind == "resource_finished"));
        assert!(events.iter().any(|event| event.kind == "finished"));
    }

    #[tokio::test]
    async fn approval_granted_before_recovery_waiter_is_not_lost() {
        let (_dir, watcher) = watcher().await;
        let run_id = "approval-before-recovery";
        let (revision, requirement) = seed_approval_wait(&watcher, run_id).await;
        grant(&watcher, run_id, &revision, &requirement)
            .await
            .unwrap();
        watcher.resume_approval_waits().await.unwrap();
        let finished = wait_for(&watcher, run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(finished.status, ExecutionStatus::Succeeded);
        assert_eq!(finished.attempt, 1);
    }

    #[tokio::test]
    async fn approval_wait_allows_observation_and_keeps_later_mutations_queued() {
        let (_dir, watcher) = watcher().await;
        let run_id = "durable-approval-wait";
        let (revision, requirement) = seed_approval_wait(&watcher, run_id).await;
        watcher.resume_approval_waits().await.unwrap();
        let waiting = wait_for(&watcher, run_id, ExecutionStatus::ApprovalWait).await;
        assert_eq!(waiting.status, ExecutionStatus::ApprovalWait);

        tokio::time::timeout(Duration::from_secs(1), watcher.topology(false))
            .await
            .expect("topology should remain available during approval wait")
            .unwrap();
        let observed = watcher
            .start_run(RunRequest::new(RunKind::Check))
            .await
            .unwrap();
        assert_eq!(
            wait_for(&watcher, &observed.run_id, ExecutionStatus::Succeeded)
                .await
                .status,
            ExecutionStatus::Succeeded
        );
        let queued = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            watcher
                .execution(&queued.run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ExecutionStatus::Queued
        );

        grant(&watcher, run_id, &revision, &requirement)
            .await
            .unwrap();
        let finished = wait_for(&watcher, run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(finished.status, ExecutionStatus::Succeeded);
        assert_eq!(finished.attempt, 1);
        assert_eq!(
            wait_for(&watcher, &queued.run_id, ExecutionStatus::Succeeded)
                .await
                .status,
            ExecutionStatus::Succeeded
        );
        let events = watcher.execution_events(run_id).await.unwrap();
        assert!(events.iter().any(|event| event.kind == "approval_granted"));
        assert!(events.iter().any(|event| event.kind == "approval_resumed"));
    }

    #[tokio::test]
    async fn cancellation_racing_approval_cannot_resurrect_a_run() {
        let (_dir, watcher) = watcher().await;
        for sequence in 0..8 {
            let run_id = format!("approval-cancel-race-{sequence}");
            let (revision, requirement) = seed_approval_wait(&watcher, &run_id).await;
            watcher.resume_approval_waits().await.unwrap();
            let waiting = wait_for(&watcher, &run_id, ExecutionStatus::ApprovalWait).await;
            assert_eq!(waiting.status, ExecutionStatus::ApprovalWait);

            let approving = watcher.clone();
            let approval_run = run_id.clone();
            let cancelling = watcher.clone();
            let cancel_run = run_id.clone();
            let (approved, cancelled) = tokio::join!(
                async move { grant(&approving, &approval_run, &revision, &requirement).await },
                async move { cancelling.cancel(&cancel_run).await }
            );
            assert!(approved.is_ok() || cancelled.is_ok());

            let final_run = wait_for(&watcher, &run_id, ExecutionStatus::Succeeded).await;
            assert!(final_run.status.terminal());
            if cancelled.is_ok() {
                assert_eq!(final_run.status, ExecutionStatus::Cancelled);
            }
        }
    }

    #[tokio::test]
    async fn retry_now_interrupts_exponential_backoff() {
        let (_dir, watcher) = watcher().await;
        let program = Program {
            resources: vec![ResourceDecl::new(
                "host.exec",
                "always_fails",
                json!({"on": {"kind": "local"}, "command": "exit 23"}),
            )],
        };
        watcher
            .submit_program(ProgramSubmission::new(watcher.ctx.project.clone(), program))
            .await
            .unwrap();
        let mut request = RunRequest::new(RunKind::Apply);
        request.retry = RetryPolicy {
            max_attempts: 2,
            initial_backoff_secs: 60,
            max_backoff_secs: 60,
        };
        let run = watcher.start_run(request).await.unwrap();
        let waiting = wait_for(&watcher, &run.run_id, ExecutionStatus::RetryWait).await;
        assert_eq!(waiting.status, ExecutionStatus::RetryWait);

        watcher.retry_now(&run.run_id).await.unwrap();
        let finished = wait_for(&watcher, &run.run_id, ExecutionStatus::Failed).await;
        assert_eq!(finished.status, ExecutionStatus::Failed);
        assert_eq!(finished.attempt, 2);
        let events = watcher.execution_events(&run.run_id).await.unwrap();
        assert!(events.iter().any(|event| event.kind == "retry_scheduled"));
        assert!(events.iter().any(|event| event.kind == "retry_triggered"));
    }

    #[tokio::test]
    async fn retry_now_resumes_the_original_run_from_recovery_wait() {
        let (_dir, watcher, fail_replace, interrupted) = interrupted_replacement(false).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while watcher.tasks.lock().await.contains_key(&interrupted.run_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        fail_replace.store(false, Ordering::SeqCst);
        let resumed = watcher.retry_now(&interrupted.run_id).await.unwrap();
        assert_eq!(resumed.status, ExecutionStatus::Queued);
        let finished = wait_for(&watcher, &interrupted.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(finished.status, ExecutionStatus::Succeeded);
        assert_eq!(finished.attempt, 1, "recovery must not consume an attempt");
        assert!(finished.recovery_resources.is_empty());
        assert!(finished.error.is_none());
        let state = watcher.state().await.unwrap();
        assert!(state.pending_replacements.is_empty());
        assert_eq!(
            state.resources[&ifx::model::Urn::new("memory.value", "answer")].inputs["key"],
            "new"
        );
        let events = watcher.execution_events(&interrupted.run_id).await.unwrap();
        assert!(events.iter().any(|event| event.kind == "recovery_wait"));
        assert!(events.iter().any(|event| event.kind == "recovery_retry"));
    }

    #[tokio::test]
    async fn cancellation_is_terminal_only_after_replacement_rollback() {
        let (_dir, watcher, _fail_replace, interrupted) = interrupted_replacement(false).await;
        let cancelled = watcher.cancel(&interrupted.run_id).await.unwrap();
        assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
        assert!(cancelled.finished_at.is_some());
        assert!(cancelled.recovery_resources.is_empty());
        assert!(
            watcher
                .state()
                .await
                .unwrap()
                .pending_replacements
                .is_empty()
        );
        let events = watcher.execution_events(&interrupted.run_id).await.unwrap();
        assert!(events.iter().any(|event| event.kind == "recovery_aborted"));
    }

    #[tokio::test]
    async fn failed_replacement_rollback_remains_in_recovery_wait() {
        let (_dir, watcher, _fail_replace, interrupted) = interrupted_replacement(true).await;
        let waiting = watcher.cancel(&interrupted.run_id).await.unwrap();
        assert_eq!(waiting.status, ExecutionStatus::RecoveryWait);
        assert!(waiting.finished_at.is_none());
        assert_eq!(waiting.recovery_resources.len(), 1);
        assert!(
            waiting
                .error
                .as_deref()
                .unwrap()
                .contains("rollback is incomplete")
        );
        assert_eq!(watcher.state().await.unwrap().pending_replacements.len(), 1);
        let events = watcher.execution_events(&interrupted.run_id).await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == "recovery_rollback_failed")
        );
    }

    #[tokio::test]
    async fn topology_stays_available_without_observing_during_an_active_run() {
        let (_dir, watcher) = watcher().await;
        let program = Program {
            resources: vec![ResourceDecl::new(
                "host.exec",
                "slow",
                json!({"on": {"kind": "local"}, "command": "sleep 60"}),
            )],
        };
        watcher
            .submit_program(ProgramSubmission::new(watcher.ctx.project.clone(), program))
            .await
            .unwrap();
        let mut request = RunRequest::new(RunKind::Apply);
        request.retry.max_attempts = 1;
        let run = watcher.start_run(request).await.unwrap();
        let running = wait_for(&watcher, &run.run_id, ExecutionStatus::Running).await;
        assert_eq!(running.status, ExecutionStatus::Running);

        let snapshot = tokio::time::timeout(Duration::from_secs(1), watcher.topology(false))
            .await
            .expect("topology should not wait for the execution lease")
            .unwrap();
        assert!(
            snapshot
                .executions
                .iter()
                .any(|execution| execution.run_id == run.run_id)
        );
        assert_eq!(
            watcher.cancel(&run.run_id).await.unwrap().status,
            ExecutionStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn restart_marks_interrupted_runs_without_mutating_replacement_journals() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ifx::Registry::builtin();
        let ctx = LoadCtx::from_dir(dir.path(), None, "restart", &[]).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            SurrealStore::connect("mem://", "ifx", "restart-test")
                .await
                .unwrap(),
        );
        for (run_id, status) in [
            ("queued-before-restart", ExecutionStatus::Queued),
            ("running-before-restart", ExecutionStatus::Running),
            ("waiting-before-restart", ExecutionStatus::RetryWait),
        ] {
            let mut run = ExecutionRun::queued(
                run_id,
                "restart",
                "revision",
                RunRequest::new(RunKind::Apply),
            );
            run.status = status;
            store.save_execution(&run).await.unwrap();
        }
        let mut completed = ExecutionRun::queued(
            "completed-before-restart",
            "restart",
            "revision",
            RunRequest::new(RunKind::Apply),
        );
        completed.status = ExecutionStatus::Succeeded;
        store.save_execution(&completed).await.unwrap();
        let mut approval_wait = ExecutionRun::queued(
            "approval-before-restart",
            "restart",
            "revision",
            RunRequest::new(RunKind::Apply),
        );
        approval_wait.status = ExecutionStatus::ApprovalWait;
        store.save_execution(&approval_wait).await.unwrap();

        let recovery_urn = ifx::model::Urn::new("memory.value", "recovering");
        let old = Entry {
            id: Some("old-id".into()),
            inputs: json!({"value": 1, "key": "old"}),
            outputs: json!({"value": 1}),
            depends_on: Vec::new(),
            protect: false,
        };
        let mut state = State {
            stack: "restart".into(),
            ..State::default()
        };
        state.upsert(recovery_urn.clone(), old.clone());
        store.save_state(&state).await.unwrap();
        state.pending_replacements.insert(
            recovery_urn.clone(),
            PendingReplacement {
                operation_id: "prepared-operation".into(),
                execution_run_id: "running-before-restart".into(),
                revision: "revision".into(),
                intent_digest: "intent".into(),
                old,
                old_in_state: true,
                desired_inputs: json!({"value": 2, "key": "new"}),
                depends_on: Vec::new(),
                protect: false,
                approval_fingerprints: BTreeMap::new(),
                prepared_at: Utc::now(),
            },
        );
        store
            .prepare_replacement(&state, &recovery_urn)
            .await
            .unwrap();

        let cfg = StackConfig {
            dir: dir.path().to_path_buf(),
            name: "restart".into(),
            file: None,
            check_interval: None,
            drift_interval: None,
            no_drift: true,
            require_lease: false,
        };
        let watcher = Watcher::start(ctx, cfg, registry, store).await.unwrap();

        for run_id in ["queued-before-restart", "waiting-before-restart"] {
            let run = watcher.execution(run_id).await.unwrap().unwrap();
            assert_eq!(run.status, ExecutionStatus::Failed);
            assert!(run.finished_at.is_some());
            assert!(run.next_retry_at.is_none());
            assert!(run.error.unwrap().contains("ifxd restarted"));
            assert!(
                watcher
                    .execution_events(run_id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| event.kind == "interrupted")
            );
        }
        let recovery = watcher
            .execution("running-before-restart")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovery.status, ExecutionStatus::RecoveryWait);
        assert_eq!(
            recovery.recovery_resources,
            std::slice::from_ref(&recovery_urn)
        );
        assert!(recovery.finished_at.is_none());
        assert!(
            watcher
                .state()
                .await
                .unwrap()
                .pending_replacements
                .contains_key(&recovery_urn)
        );
        assert_eq!(
            watcher
                .execution("completed-before-restart")
                .await
                .unwrap()
                .unwrap()
                .status,
            ExecutionStatus::Succeeded
        );
        assert_eq!(
            watcher
                .execution("approval-before-restart")
                .await
                .unwrap()
                .unwrap()
                .status,
            ExecutionStatus::ApprovalWait
        );
    }

    fn lease_for(duration: Duration) -> LeaseRequest {
        LeaseRequest {
            duration: Some(duration),
            ..LeaseRequest::default()
        }
    }

    async fn submit_answer(watcher: &Watcher, protect: bool) {
        let mut answer = ResourceDecl::new("memory.value", "answer", json!({"value": 42}));
        answer.protect = protect;
        let program = Program {
            resources: vec![answer],
        };
        watcher
            .submit_program(ProgramSubmission::new(watcher.ctx.project.clone(), program))
            .await
            .unwrap();
    }

    async fn apply_answer(watcher: &Arc<Watcher>, protect: bool) {
        submit_answer(watcher, protect).await;
        let run = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap();
        let finished = wait_for(watcher, &run.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(
            finished.status,
            ExecutionStatus::Succeeded,
            "{:?}",
            finished.error
        );
        assert!(!watcher.state().await.unwrap().resources.is_empty());
    }

    #[tokio::test]
    async fn expired_lease_destroys_the_stack_once_and_blocks_reapply() {
        let (_dir, watcher) = watcher().await;
        apply_answer(&watcher, false).await;
        let lease = watcher
            .set_lease(lease_for(Duration::from_millis(50)))
            .await
            .unwrap();
        assert!(!lease.expired(Utc::now()));
        assert!(
            watcher.enforce_lease().await.unwrap().is_none(),
            "an unexpired lease queues nothing"
        );
        tokio::time::sleep(Duration::from_millis(80)).await;

        let run = watcher
            .enforce_lease()
            .await
            .unwrap()
            .expect("expired lease queues a destroy");
        assert_eq!(run.request.kind, RunKind::Destroy);
        let finished = wait_for(&watcher, &run.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(
            finished.status,
            ExecutionStatus::Succeeded,
            "{:?}",
            finished.error
        );
        assert!(watcher.state().await.unwrap().resources.is_empty());
        let lease = watcher.lease().await.unwrap().unwrap();
        assert_eq!(
            lease.fired.as_ref().map(|fired| fired.run_id.as_str()),
            Some(run.run_id.as_str())
        );
        assert!(
            watcher.enforce_lease().await.unwrap().is_none(),
            "an empty stack queues nothing"
        );

        let err = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lease expired"), "{err}");
        let err = watcher
            .extend_lease(LeaseExtension {
                by: Duration::from_secs(60),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lease expired"), "{err}");

        let renewed = watcher
            .set_lease(lease_for(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(renewed.history.len(), 1);
        assert!(renewed.fired.is_none());
        apply_answer(&watcher, false).await;
    }

    #[tokio::test]
    async fn required_lease_gates_apply_and_cannot_be_cleared() {
        let (_dir, watcher) = watcher_with(ifx::Registry::builtin(), true).await;
        submit_answer(&watcher, false).await;
        let err = watcher
            .start_run(RunRequest::new(RunKind::Apply))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a lease"), "{err}");
        assert!(watcher.state().await.unwrap().resources.is_empty());
        let plan = watcher
            .start_run(RunRequest::new(RunKind::Plan))
            .await
            .unwrap();
        let plan = wait_for(&watcher, &plan.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(
            plan.status,
            ExecutionStatus::Succeeded,
            "plans need no lease"
        );

        watcher
            .set_lease(lease_for(Duration::from_secs(3600)))
            .await
            .unwrap();
        apply_answer(&watcher, false).await;
        let err = watcher.clear_lease().await.unwrap_err();
        assert!(err.to_string().contains("requires a lease"), "{err}");
        assert!(watcher.lease().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn extension_moves_the_deadline_and_records_history() {
        let (_dir, watcher) = watcher().await;
        assert!(watcher.lease().await.unwrap().is_none());
        let err = watcher
            .extend_lease(LeaseExtension {
                by: Duration::from_secs(1),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("has no lease"), "{err}");
        let err = watcher
            .set_lease(LeaseRequest::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("needs a deadline"), "{err}");
        let err = watcher
            .set_lease(LeaseRequest {
                deadline: Some(Utc::now() - TimeDelta::seconds(1)),
                ..LeaseRequest::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not in the future"), "{err}");

        let lease = watcher
            .set_lease(lease_for(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert!(lease.history.is_empty());
        let extended = watcher
            .extend_lease(LeaseExtension {
                by: Duration::from_secs(1800),
            })
            .await
            .unwrap();
        assert_eq!(extended.deadline, lease.deadline + TimeDelta::seconds(1800));
        assert_eq!(extended.created_at, lease.created_at);
        assert_eq!(extended.history.len(), 1);
        assert_eq!(extended.history[0].from, lease.deadline);
        assert_eq!(extended.history[0].to, extended.deadline);
        assert_eq!(watcher.lease().await.unwrap(), Some(extended.clone()));

        let shortened = watcher
            .set_lease(lease_for(Duration::from_secs(60)))
            .await
            .unwrap();
        assert!(shortened.deadline < extended.deadline);
        assert_eq!(shortened.created_at, lease.created_at);
        assert_eq!(shortened.history.len(), 2);
        watcher.clear_lease().await.unwrap();
        assert!(watcher.lease().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn restarted_daemon_enforces_a_stored_lease() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LoadCtx::from_dir(dir.path(), None, "restart-lease", &[]).unwrap();
        let store: Arc<dyn Store> = Arc::new(
            SurrealStore::connect("mem://", "ifx", "restart-lease")
                .await
                .unwrap(),
        );
        let cfg = StackConfig {
            dir: dir.path().to_path_buf(),
            name: "restart-lease".into(),
            file: None,
            check_interval: None,
            drift_interval: None,
            no_drift: true,
            require_lease: false,
        };
        let registry = ifx::Registry::builtin();
        let first = Arc::new(
            Watcher::start(ctx.clone(), cfg.clone(), registry.clone(), store.clone())
                .await
                .unwrap(),
        );
        apply_answer(&first, false).await;
        first
            .set_lease(lease_for(Duration::from_secs(3600)))
            .await
            .unwrap();
        drop(first);

        // The daemon is down while the deadline passes.
        let mut lease = store.lease("restart-lease").await.unwrap().unwrap();
        lease.deadline = Utc::now() - TimeDelta::seconds(1);
        store.put_lease(&lease).await.unwrap();

        let second = Arc::new(
            Watcher::start(ctx, cfg, registry, store.clone())
                .await
                .unwrap(),
        );
        let run = second
            .enforce_lease()
            .await
            .unwrap()
            .expect("a stored expired lease destroys on the first tick after restart");
        let finished = wait_for(&second, &run.run_id, ExecutionStatus::Succeeded).await;
        assert_eq!(
            finished.status,
            ExecutionStatus::Succeeded,
            "{:?}",
            finished.error
        );
        assert!(
            store
                .load_state("restart-lease")
                .await
                .unwrap()
                .resources
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_lease_destroy_waits_for_the_refire_window() {
        let (_dir, watcher) = watcher().await;
        apply_answer(&watcher, true).await;
        watcher
            .set_lease(lease_for(Duration::from_millis(20)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let run = watcher
            .enforce_lease()
            .await
            .unwrap()
            .expect("expired lease queues a destroy");
        let finished = wait_for(&watcher, &run.run_id, ExecutionStatus::Failed).await;
        assert_eq!(finished.status, ExecutionStatus::Failed);
        assert!(
            finished
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("protected"),
            "{:?}",
            finished.error
        );
        assert!(
            watcher.enforce_lease().await.unwrap().is_none(),
            "a failed destroy is not queued again inside the refire window"
        );
        assert!(!watcher.state().await.unwrap().resources.is_empty());
    }

    /// `memory.value` whose deletion carries a named risk, so destroys park on approval.
    struct RiskyDeleteMemory {
        memory: Memory,
    }

    #[async_trait::async_trait]
    impl Handler for RiskyDeleteMemory {
        fn schema(&self) -> ifx::schema::ResourceSchema {
            self.memory.schema()
        }

        fn risks(
            &self,
            operation: OperationKind,
            _desired: &Value,
            _actual: Option<&Actual>,
        ) -> Result<Vec<OperationRisk>> {
            Ok(match operation {
                OperationKind::Delete => vec![OperationRisk {
                    name: "wipe".into(),
                    reason: "deletes the value".into(),
                }],
                _ => Vec::new(),
            })
        }

        async fn read(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            inputs: &Value,
        ) -> Result<Option<Actual>> {
            self.memory.read(cx, id, inputs).await
        }

        async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
            self.memory.create(cx, inputs).await
        }

        async fn update(
            &self,
            cx: &Ctx<'_>,
            id: Option<&str>,
            inputs: &Value,
            actual: &Actual,
        ) -> Result<Applied> {
            self.memory.update(cx, id, inputs, actual).await
        }

        async fn delete(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<()> {
            self.memory.delete(cx, id, inputs).await
        }
    }

    #[tokio::test]
    async fn expired_lease_cancels_a_destroy_parked_on_approval_and_refires() {
        let mut registry = ifx::Registry::new();
        registry.register(RiskyDeleteMemory {
            memory: Memory::default(),
        });
        let (_dir, watcher) = watcher_with_registry(registry).await;
        apply_answer(&watcher, false).await;
        watcher
            .set_lease(lease_for(Duration::from_millis(20)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let run = watcher
            .enforce_lease()
            .await
            .unwrap()
            .expect("expired lease queues a destroy");
        let parked = wait_for(&watcher, &run.run_id, ExecutionStatus::ApprovalWait).await;
        assert_eq!(parked.status, ExecutionStatus::ApprovalWait);

        // Replacing the lease while that destroy is parked keeps it visible.
        let renewed = watcher
            .set_lease(lease_for(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(
            renewed.fired.as_ref().map(|fired| fired.run_id.as_str()),
            Some(run.run_id.as_str())
        );
        assert!(watcher.enforce_lease().await.unwrap().is_none());

        // Expire it again: the parked destroy is cancelled rather than blocking forever.
        let mut lease = watcher.lease().await.unwrap().unwrap();
        lease.deadline = Utc::now() - TimeDelta::seconds(1);
        watcher.store.put_lease(&lease).await.unwrap();
        assert!(
            watcher.enforce_lease().await.unwrap().is_none(),
            "no new destroy inside the refire window"
        );
        let cancelled = watcher.execution(&run.run_id).await.unwrap().unwrap();
        assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
        assert!(!watcher.state().await.unwrap().resources.is_empty());

        // Past the refire window a fresh destroy is queued.
        let mut lease = watcher.lease().await.unwrap().unwrap();
        lease.fired.as_mut().unwrap().at = Utc::now() - to_delta(LEASE_REFIRE).unwrap();
        watcher.store.put_lease(&lease).await.unwrap();
        let again = watcher
            .enforce_lease()
            .await
            .unwrap()
            .expect("refired after the window");
        assert_ne!(again.run_id, run.run_id);
        assert_eq!(
            watcher
                .lease()
                .await
                .unwrap()
                .unwrap()
                .fired
                .map(|fired| fired.run_id),
            Some(again.run_id)
        );
    }

    #[tokio::test]
    async fn grace_is_reserved_and_must_be_zero() {
        let (_dir, watcher) = watcher().await;
        let err = watcher
            .set_lease(LeaseRequest {
                duration: Some(Duration::from_secs(3600)),
                deadline: None,
                grace: Duration::from_secs(1),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be 0s"), "{err}");
        assert!(watcher.lease().await.unwrap().is_none());
    }
}
