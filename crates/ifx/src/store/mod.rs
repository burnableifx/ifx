//! Persistence. The engine writes through a [`Store`] after every operation; `ifxd`
//! reads health and drift history back out. SurrealDB is the database layer —
//! embedded (`surrealkv://`, `mem://`) or a shared server (`ws://`). A plain JSON
//! file store remains for export/import and tests.

mod file;
mod surreal;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control::StackLease;
use crate::model::Urn;
use crate::state::State;
use crate::{ExecutionEvent, ExecutionRun, ProgramRevision};
pub use file::FileStore;
pub use surreal::SurrealStore;

pub type Result<T> = anyhow::Result<T>;

/// Kind of a recorded run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Plan,
    Apply,
    Destroy,
    Refresh,
    Check,
    Drift,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub stack: String,
    pub kind: RunKind,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub summary: Value,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub run_id: String,
    pub stack: String,
    pub urn: Urn,
    pub action: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    pub at: DateTime<Utc>,
}

/// Outcome of one health check or drift observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Healthy,
    Degraded,
    Unhealthy,
    /// The resource exists but differs from the program.
    Drifted,
    /// Could not be evaluated (transport error, missing dependency).
    Unknown,
}

impl Health {
    pub fn symbol(&self) -> &'static str {
        match self {
            Health::Healthy => "✓",
            Health::Degraded => "~",
            Health::Unhealthy => "✗",
            Health::Drifted => "±",
            Health::Unknown => "?",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthRecord {
    pub stack: String,
    pub urn: Urn,
    pub status: Health,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    pub at: DateTime<Utc>,
}

#[async_trait]
pub trait Store: Send + Sync {
    /// Human label (the endpoint) for logs.
    fn describe(&self) -> String;

    async fn load_state(&self, stack: &str) -> Result<State>;

    /// Make the stored resources equal to `state.resources` (upsert present, delete
    /// missing). Called after every engine operation, so it must be cheap-ish.
    async fn save_state(&self, state: &State) -> Result<()>;

    /// Persist replacement intent before invoking the provider. File-backed stores
    /// use the atomically written state document; database stores may override this
    /// with a targeted transaction.
    async fn prepare_replacement(&self, state: &State, _urn: &Urn) -> Result<()> {
        self.save_state(state).await
    }

    /// Replace the exact approval identity on an existing journal after a fresh grant.
    async fn reauthorize_replacement(
        &self,
        state: &State,
        _urn: &Urn,
        _previous_operation_id: &str,
    ) -> Result<()> {
        self.save_state(state).await
    }

    /// Atomically publish the successor state and clear its replacement intent.
    async fn commit_replacement(
        &self,
        state: &State,
        _urn: &Urn,
        _operation_id: &str,
    ) -> Result<()> {
        self.save_state(state).await
    }

    /// Clear a journal after the provider confirms that its old resource was restored.
    async fn abort_replacement(
        &self,
        state: &State,
        _urn: &Urn,
        _operation_id: &str,
    ) -> Result<()> {
        self.save_state(state).await
    }

    async fn begin_run(&self, stack: &str, kind: RunKind) -> Result<String>;
    async fn finish_run(
        &self,
        run_id: &str,
        ok: bool,
        summary: Value,
        error: Option<String>,
    ) -> Result<()>;
    async fn runs(&self, stack: &str, limit: usize) -> Result<Vec<RunRecord>>;

    async fn record_event(&self, ev: &EventRecord) -> Result<()>;
    async fn events(&self, run_id: &str) -> Result<Vec<EventRecord>>;

    async fn record_health(&self, h: &HealthRecord) -> Result<()>;
    /// Most recent record per resource for a stack.
    async fn latest_health(&self, stack: &str) -> Result<Vec<HealthRecord>>;
    /// History for one resource, newest first.
    async fn health_history(
        &self,
        stack: &str,
        urn: &Urn,
        limit: usize,
    ) -> Result<Vec<HealthRecord>>;

    /// Stacks that have any stored resources or runs.
    async fn stacks(&self) -> Result<Vec<String>>;

    /// Persist a content-addressed program revision and optionally make it active.
    async fn put_program_revision(
        &self,
        _revision: &ProgramRevision,
        _activate: bool,
    ) -> Result<()> {
        anyhow::bail!("program revisions are unsupported by {}", self.describe())
    }

    async fn program_revision(
        &self,
        _stack: &str,
        _revision: &str,
    ) -> Result<Option<ProgramRevision>> {
        anyhow::bail!("program revisions are unsupported by {}", self.describe())
    }

    async fn active_program_revision(&self, _stack: &str) -> Result<Option<ProgramRevision>> {
        anyhow::bail!("program revisions are unsupported by {}", self.describe())
    }

    async fn save_execution(&self, _run: &ExecutionRun) -> Result<()> {
        anyhow::bail!("daemon executions are unsupported by {}", self.describe())
    }

    async fn execution(&self, _run_id: &str) -> Result<Option<ExecutionRun>> {
        anyhow::bail!("daemon executions are unsupported by {}", self.describe())
    }

    async fn executions(&self, _stack: &str, _limit: usize) -> Result<Vec<ExecutionRun>> {
        anyhow::bail!("daemon executions are unsupported by {}", self.describe())
    }

    async fn record_execution_event(&self, _event: &ExecutionEvent) -> Result<()> {
        anyhow::bail!(
            "daemon execution events are unsupported by {}",
            self.describe()
        )
    }

    async fn execution_events(&self, _run_id: &str) -> Result<Vec<ExecutionEvent>> {
        anyhow::bail!(
            "daemon execution events are unsupported by {}",
            self.describe()
        )
    }

    async fn lease(&self, _stack: &str) -> Result<Option<StackLease>> {
        anyhow::bail!("stack leases are unsupported by {}", self.describe())
    }

    async fn put_lease(&self, _lease: &StackLease) -> Result<()> {
        anyhow::bail!("stack leases are unsupported by {}", self.describe())
    }

    async fn clear_lease(&self, _stack: &str) -> Result<()> {
        anyhow::bail!("stack leases are unsupported by {}", self.describe())
    }
}

/// Open a store from an endpoint string:
/// - `mem://` — in-memory SurrealDB (tests)
/// - `surrealkv://<path>` — embedded SurrealDB on disk (single process at a time)
/// - `ws://host:port` / `wss://` — SurrealDB server, shared by `ifx` and `ifxd`;
///   credentials from `IFX_DB_USER` / `IFX_DB_PASS` (default `root`/`root`)
/// - `file://<path.json>` — plain JSON file (state only; no history)
pub async fn open(endpoint: &str, namespace: &str, database: &str) -> Result<Arc<dyn Store>> {
    if let Some(path) = endpoint.strip_prefix("file://") {
        return Ok(Arc::new(FileStore::new(path)));
    }
    Ok(Arc::new(
        SurrealStore::connect(endpoint, namespace, database).await?,
    ))
}

/// A store that keeps nothing (engine tests without persistence).
pub struct NullStore;

#[async_trait]
impl Store for NullStore {
    fn describe(&self) -> String {
        "null".into()
    }
    async fn load_state(&self, _stack: &str) -> Result<State> {
        Ok(State::default())
    }
    async fn save_state(&self, _state: &State) -> Result<()> {
        Ok(())
    }
    async fn begin_run(&self, _stack: &str, _kind: RunKind) -> Result<String> {
        Ok(String::new())
    }
    async fn finish_run(
        &self,
        _run_id: &str,
        _ok: bool,
        _summary: Value,
        _error: Option<String>,
    ) -> Result<()> {
        Ok(())
    }
    async fn runs(&self, _stack: &str, _limit: usize) -> Result<Vec<RunRecord>> {
        Ok(vec![])
    }
    async fn record_event(&self, _ev: &EventRecord) -> Result<()> {
        Ok(())
    }
    async fn events(&self, _run_id: &str) -> Result<Vec<EventRecord>> {
        Ok(vec![])
    }
    async fn record_health(&self, _h: &HealthRecord) -> Result<()> {
        Ok(())
    }
    async fn latest_health(&self, _stack: &str) -> Result<Vec<HealthRecord>> {
        Ok(vec![])
    }
    async fn health_history(
        &self,
        _stack: &str,
        _urn: &Urn,
        _limit: usize,
    ) -> Result<Vec<HealthRecord>> {
        Ok(vec![])
    }
    async fn stacks(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
}
