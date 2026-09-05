//! The provider side of the engine: one [`Handler`] per resource type, collected in a
//! [`Registry`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::Urn;
use crate::schema::ResourceSchema;
use crate::transport::TransportPool;

pub type Result<T> = anyhow::Result<T>;

/// What a provider observed about a resource that exists.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Actual {
    /// Provider-side identifier, when the resource has one (cloud ids). Host-scoped
    /// resources are identified by their inputs and leave this `None`.
    pub id: Option<String>,
    /// Observed values of input fields, in the same shape as inputs. Fields the
    /// provider cannot observe are omitted and never produce a diff; fields that are
    /// observably unset must be reported as `null` so that setting them is a change.
    pub props: Value,
    /// Observed outputs.
    pub outputs: Value,
}

/// Result of a create or update.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub id: Option<String>,
    pub outputs: Value,
}

/// A single field-level difference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    pub from: Option<Value>,
    pub to: Option<Value>,
    pub forces_replace: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub changes: Vec<FieldChange>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn requires_replace(&self) -> bool {
        self.changes.iter().any(|c| c.forces_replace)
    }

    /// Field-wise comparison of desired against observed, restricted to fields the
    /// observation reports. `replace_fields` marks which differences force replacement.
    pub fn generic(desired: &Value, actual_props: &Value, replace_fields: &[&str]) -> Self {
        let mut changes = Vec::new();
        let (Some(d), Some(a)) = (desired.as_object(), actual_props.as_object()) else {
            return Self { changes };
        };
        for (k, dv) in d {
            if dv.is_null() {
                continue;
            }
            let Some(av) = a.get(k) else { continue };
            if av != dv {
                changes.push(FieldChange {
                    field: k.clone(),
                    from: Some(av.clone()),
                    to: Some(dv.clone()),
                    forces_replace: replace_fields.contains(&k.as_str()),
                });
            }
        }
        Self { changes }
    }
}

/// A mutating operation a handler may classify as requiring explicit approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    Update,
    Replace,
    Delete,
    Trigger,
}

/// A named risk attached to a specific resource operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRisk {
    /// Stable, machine-facing name used by approval clients, such as `restart`.
    pub name: String,
    /// Human-facing explanation of what the operation may disrupt or destroy.
    pub reason: String,
}

/// Per-operation context handed to handlers.
pub struct Ctx<'a> {
    pub urn: &'a Urn,
    pub transports: &'a TransportPool,
    /// Set when one of the resource's declared `triggers` changed in this run.
    pub triggered: bool,
}

/// Implements one resource type. Inputs arrive fully resolved (no references, no
/// unknowns) and already validated against [`Handler::schema`].
#[async_trait]
pub trait Handler: Send + Sync {
    fn schema(&self) -> ResourceSchema;

    /// Observe the resource. `None` means it does not exist.
    async fn read(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<Option<Actual>>;

    /// Seal provider access left open by an interrupted executor. Implementations
    /// must be idempotent and return whether they recovered durable cleanup intent.
    async fn recover_execution_access(
        &self,
        _cx: &Ctx<'_>,
        _id: Option<&str>,
        _inputs: &Value,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Observe one side of an interrupted replacement without releasing provider-side
    /// rollback material. Providers whose ordinary reads clean post-commit residue
    /// must override this method. The default is identical to [`Handler::read`].
    async fn read_for_recovery(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        self.read(cx, id, inputs).await
    }

    /// Compare desired inputs with what was observed. The default is a field-wise
    /// comparison using the schema's `replace` flags.
    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let schema = self.schema();
        let replace: Vec<&str> = schema.replace_fields().collect();
        Ok(Diff::generic(desired, &actual.props, &replace))
    }

    /// Risks that require a one-run approval before this operation may execute.
    /// The default keeps existing providers approval-free.
    fn risks(
        &self,
        _operation: OperationKind,
        _desired: &Value,
        _actual: Option<&Actual>,
    ) -> Result<Vec<OperationRisk>> {
        Ok(Vec::new())
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied>;

    /// Replace an existing resource. Providers may override this to stage and commit a
    /// replacement transactionally. The default preserves the traditional
    /// delete-before-create lifecycle.
    async fn replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        self.delete(cx, id, old_inputs).await?;
        self.create(cx, inputs).await
    }

    /// Finish or adopt a replacement whose durable intent survived an interrupted
    /// engine process. Providers with their own transaction manifest may override
    /// this to recover an incomplete physical commit before observation.
    async fn recover_replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        inputs: &Value,
        old_actual: Option<&Actual>,
        desired_actual: Option<&Actual>,
    ) -> Result<Applied> {
        if let Some(actual) = old_actual {
            if self.diff(inputs, actual)?.is_empty() {
                return Ok(Applied {
                    id: actual.id.clone(),
                    outputs: actual.outputs.clone(),
                });
            }
            return self.replace(cx, id, old_inputs, inputs, actual).await;
        }
        if let Some(actual) = desired_actual {
            anyhow::ensure!(
                self.diff(inputs, actual)?.is_empty(),
                "replacement recovery found the desired identity with mismatched properties"
            );
            return Ok(Applied {
                id: actual.id.clone(),
                outputs: actual.outputs.clone(),
            });
        }
        self.create(cx, inputs).await
    }

    /// Roll back an interrupted replacement. The default can only confirm that the
    /// old resource still exists and matches its prior applied inputs; providers with
    /// staged transactions may restore their own backup before returning.
    async fn abort_replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        _inputs: &Value,
    ) -> Result<()> {
        let actual = self
            .read(cx, id, old_inputs)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the previous resource no longer exists"))?;
        anyhow::ensure!(
            self.diff(old_inputs, &actual)?.is_empty(),
            "the previous resource no longer matches its applied state"
        );
        Ok(())
    }

    /// Release provider-side rollback material after successor state is durably
    /// committed. A failure cannot restore the old state journal; implementations
    /// must leave cleanup retryable from ordinary observation.
    async fn finalize_replace(
        &self,
        _cx: &Ctx<'_>,
        _old_id: Option<&str>,
        _old_inputs: &Value,
        _id: Option<&str>,
        _inputs: &Value,
    ) -> Result<()> {
        Ok(())
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
        actual: &Actual,
    ) -> Result<Applied>;

    async fn delete(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<()>;
}

/// Outcome of running a health check once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub status: crate::store::Health,
    pub message: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
}

/// A resource type that is also a health check. Its handler records the check's
/// definition like any resource; `check` runs it (from `ifx check`, from apply, and on
/// an interval from `ifxd`). Inputs arrive resolved, exactly as stored in state.
#[async_trait]
pub trait Checker: Send + Sync {
    async fn check(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<CheckOutcome>;

    /// Seconds between runs when monitored by `ifxd`; `None` uses the daemon default.
    fn interval_secs(&self, inputs: &Value) -> Option<u64> {
        inputs.get("interval_secs").and_then(Value::as_u64)
    }

    /// Whether an unhealthy result should fail `apply`.
    fn required(&self, inputs: &Value) -> bool {
        inputs
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// All known resource types.
#[derive(Default, Clone)]
pub struct Registry {
    handlers: BTreeMap<String, Arc<dyn Handler>>,
    checkers: BTreeMap<String, Arc<dyn Checker>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with every provider enabled by cargo features.
    pub fn builtin() -> Self {
        let mut r = Self::new();
        crate::providers::register_all(&mut r);
        r
    }

    pub fn register(&mut self, handler: impl Handler + 'static) -> &mut Self {
        let name = handler.schema().type_name;
        self.handlers.insert(name, Arc::new(handler));
        self
    }

    pub fn register_arc(&mut self, handler: Arc<dyn Handler>) -> &mut Self {
        self.handlers.insert(handler.schema().type_name, handler);
        self
    }

    /// Register a type that is both a resource and a health check.
    pub fn register_check<T: Handler + Checker + 'static>(&mut self, t: T) -> &mut Self {
        let name = t.schema().type_name;
        let arc = Arc::new(t);
        self.handlers.insert(name.clone(), arc.clone());
        self.checkers.insert(name, arc);
        self
    }

    pub fn checker(&self, type_name: &str) -> Option<Arc<dyn Checker>> {
        self.checkers.get(type_name).cloned()
    }

    pub fn is_check(&self, type_name: &str) -> bool {
        self.checkers.contains_key(type_name)
    }

    pub fn get(&self, type_name: &str) -> Option<Arc<dyn Handler>> {
        self.handlers.get(type_name).cloned()
    }

    pub fn type_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }

    pub fn schemas(&self) -> Vec<ResourceSchema> {
        self.handlers.values().map(|h| h.schema()).collect()
    }
}
