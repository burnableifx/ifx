//! Versioned control-plane types shared by `ifxd` and its clients.
//!
//! Rust stack emitters produce a [`Program`]. The daemon stores that program
//! as an immutable, content-addressed [`ProgramRevision`] and executes explicit
//! [`RunRequest`]s against it. These JSON types are the public API boundary; provider
//! schemas remain the source of truth for resource fields and validation.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{Program, Urn};
use crate::store::RunKind;

pub const API_VERSION: &str = "ifx/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildPhase {
    Dirty,
    Building,
    Ready,
    Failed,
}

/// Current compiler state for one daemon-owned Rust stack.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackBuildStatus {
    pub generation: u64,
    pub phase: BuildPhase,
    pub changed_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl StackBuildStatus {
    pub fn dirty() -> Self {
        Self {
            generation: 1,
            phase: BuildPhase::Dirty,
            changed_at: Utc::now(),
            started_at: None,
            finished_at: None,
            duration_ms: None,
            revision: None,
            error: None,
        }
    }
}

/// Runtime configuration for emitting a Program from the current compiled artifact.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProgramResolveRequest {
    #[serde(default)]
    pub config: Option<std::collections::BTreeMap<String, Value>>,
}

fn api_version() -> String {
    API_VERSION.to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgramSubmission {
    #[serde(default = "api_version")]
    pub api_version: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub source_digest: Option<String>,
    pub program: Program,
}

impl ProgramSubmission {
    pub fn new(project: impl Into<String>, program: Program) -> Self {
        Self {
            api_version: api_version(),
            project: project.into(),
            source_digest: None,
            program,
        }
    }

    pub fn ensure_supported(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.api_version == API_VERSION,
            "unsupported program API version `{}` (expected `{API_VERSION}`)",
            self.api_version
        );
        Ok(())
    }

    pub fn revision(&self) -> anyhow::Result<String> {
        self.ensure_supported()?;
        let canonical = serde_json::to_vec(&serde_json::json!({
            "api_version": &self.api_version,
            "project": &self.project,
            "program": &self.program,
        }))?;
        Ok(hex::encode(Sha256::digest(canonical)))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgramRevision {
    pub api_version: String,
    pub revision: String,
    pub stack: String,
    pub project: String,
    #[serde(default)]
    pub source_digest: Option<String>,
    pub program: Program,
    pub submitted_at: DateTime<Utc>,
}

impl ProgramRevision {
    pub fn from_submission(
        stack: impl Into<String>,
        submission: ProgramSubmission,
    ) -> anyhow::Result<Self> {
        let revision = submission.revision()?;
        Ok(Self {
            api_version: submission.api_version,
            revision,
            stack: stack.into(),
            project: submission.project,
            source_digest: submission.source_digest,
            program: submission.program,
            submitted_at: Utc::now(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Queued,
    Running,
    RetryWait,
    ApprovalWait,
    RecoveryWait,
    Succeeded,
    Failed,
    Cancelled,
}

impl ExecutionStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff")]
    pub initial_backoff_secs: u64,
    #[serde(default = "default_max_backoff")]
    pub max_backoff_secs: u64,
}

const fn default_attempts() -> u32 {
    3
}
const fn default_initial_backoff() -> u64 {
    2
}
const fn default_max_backoff() -> u64 {
    60
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_attempts(),
            initial_backoff_secs: default_initial_backoff(),
            max_backoff_secs: default_max_backoff(),
        }
    }
}

impl RetryPolicy {
    pub fn backoff_secs(&self, failed_attempt: u32) -> u64 {
        let shift = failed_attempt.saturating_sub(1).min(62);
        self.initial_backoff_secs
            .saturating_mul(1_u64 << shift)
            .min(self.max_backoff_secs)
    }
}

/// A durable destruction deadline for one stack. Leases are state, not source: only
/// the daemon API changes them, so an extension survives every later apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLease {
    pub stack: String,
    pub deadline: DateTime<Utc>,
    /// Reserved for a suspend-before-destroy window. Always zero today.
    #[serde(default, with = "humantime_serde")]
    pub grace: Duration,
    pub created_at: DateTime<Utc>,
    /// Every deadline change since creation, oldest first.
    #[serde(default)]
    pub history: Vec<LeaseChange>,
    /// The destroy most recently queued by an expired deadline.
    #[serde(default)]
    pub fired: Option<LeaseFire>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseChange {
    pub at: DateTime<Utc>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFire {
    pub run_id: String,
    pub at: DateTime<Utc>,
}

impl StackLease {
    /// The instant destruction is due: the deadline plus any grace window.
    pub fn expires_at(&self) -> DateTime<Utc> {
        let grace = TimeDelta::from_std(self.grace).unwrap_or(TimeDelta::MAX);
        self.deadline
            .checked_add_signed(grace)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }

    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at()
    }

    /// Time left before destruction is due; zero once expired.
    pub fn remaining(&self, now: DateTime<Utc>) -> Duration {
        (self.expires_at() - now).to_std().unwrap_or_default()
    }
}

/// Set or replace a stack lease with an absolute deadline or a duration from now.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LeaseRequest {
    #[serde(default, with = "humantime_serde")]
    pub duration: Option<Duration>,
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default, with = "humantime_serde")]
    pub grace: Duration,
}

/// Push an unexpired lease deadline later by `by`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseExtension {
    #[serde(with = "humantime_serde")]
    pub by: Duration,
}

/// One-run authorization for an exact risky operation from a specific program revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalGrant {
    pub revision: String,
    pub urn: Urn,
    pub risk: String,
    pub fingerprint: String,
}

/// A risky operation awaiting an exact, one-run grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequirement {
    pub urn: Urn,
    pub risk: String,
    pub reason: String,
    /// Absent while inputs are still unknown; such operations are approved only after
    /// their dependencies resolve during apply.
    pub fingerprint: Option<String>,
}

impl ApprovalRequirement {
    pub fn selector(&self) -> String {
        format!("{}/{}", self.urn, self.risk)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub kind: RunKind,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub targets: Vec<Urn>,
    #[serde(default)]
    pub no_refresh: bool,
    /// For a destroy request, compute and return the deletion plan without applying it.
    #[serde(default)]
    pub plan_only: bool,
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default)]
    pub approvals: Vec<ApprovalGrant>,
}

const fn default_parallelism() -> usize {
    8
}

impl RunRequest {
    pub fn new(kind: RunKind) -> Self {
        Self {
            kind,
            revision: None,
            targets: Vec::new(),
            no_refresh: false,
            plan_only: false,
            parallelism: default_parallelism(),
            retry: RetryPolicy::default(),
            approvals: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionRun {
    pub run_id: String,
    pub stack: String,
    pub revision: String,
    pub request: RunRequest,
    pub status: ExecutionStatus,
    pub attempt: u32,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_retry_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalRequirement>,
    /// Resources with interrupted replacements owned by this execution.
    #[serde(default)]
    pub recovery_resources: Vec<Urn>,
    /// Planned trigger operations already completed by an earlier partial execution.
    /// These are checkpointed because trigger side effects are not generally observable.
    #[serde(default)]
    pub completed_triggers: Vec<Urn>,
}

impl ExecutionRun {
    pub fn queued(
        run_id: impl Into<String>,
        stack: impl Into<String>,
        revision: impl Into<String>,
        request: RunRequest,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            stack: stack.into(),
            revision: revision.into(),
            request,
            status: ExecutionStatus::Queued,
            attempt: 0,
            requested_at: Utc::now(),
            started_at: None,
            finished_at: None,
            next_retry_at: None,
            result: Value::Null,
            error: None,
            pending_approvals: Vec::new(),
            recovery_resources: Vec::new(),
            completed_triggers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub run_id: String,
    pub stack: String,
    pub kind: String,
    #[serde(default)]
    pub urn: Option<Urn>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub message: String,
    pub at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceDecl;

    #[test]
    fn revision_is_stable_and_content_addressed() {
        let one = ProgramSubmission::new(
            "demo",
            Program {
                resources: vec![ResourceDecl::new(
                    "memory.value",
                    "x",
                    serde_json::json!({"value": 1}),
                )],
            },
        );
        let same = one.clone();
        let mut changed = one.clone();
        changed.program.resources[0].inputs["value"] = serde_json::json!(2);
        assert_eq!(one.revision().unwrap(), same.revision().unwrap());
        assert_ne!(one.revision().unwrap(), changed.revision().unwrap());
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff_secs: 2,
            max_backoff_secs: 9,
        };
        assert_eq!(policy.backoff_secs(1), 2);
        assert_eq!(policy.backoff_secs(2), 4);
        assert_eq!(policy.backoff_secs(3), 8);
        assert_eq!(policy.backoff_secs(4), 9);
    }

    #[test]
    fn older_run_requests_default_to_no_approvals() {
        let request: RunRequest = serde_json::from_value(serde_json::json!({
            "kind": "apply",
            "parallelism": 8
        }))
        .unwrap();
        assert!(request.approvals.is_empty());
    }
}
