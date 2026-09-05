//! Health checks as resources: `check.http`, `check.tcp`, `check.exec`. Declared in the
//! stack next to what they watch, so `url = "http://" + web.ipv4` is a normal reference.
//! Applying a check runs it once; `ifx check` and `ifxd` run it again on demand or on an
//! interval and record the results.

mod exec;
mod http;
mod tcp;

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

pub use crate::generated::check::*;
use crate::provider::{Actual, Applied, CheckOutcome, Checker, Ctx, Registry, Result};
use crate::schema::{FieldSchema, FieldType, field};
use crate::store::Health;
pub use exec::ExecCheck;
pub use http::HttpCheck;
pub use tcp::TcpCheck;

pub fn register(r: &mut Registry) {
    r.register_check(HttpCheck);
    r.register_check(TcpCheck);
    r.register_check(ExecCheck);
}

/// Inputs every check type shares.
pub(crate) fn common_inputs() -> Vec<FieldSchema> {
    vec![
        field("required", FieldType::Bool)
            .default(false)
            .doc("Fail `apply` if the check is unhealthy right after the stack is applied."),
        field("interval_secs", FieldType::Int)
            .doc("How often `ifxd` runs this check; the daemon default applies if unset."),
        field("timeout_secs", FieldType::Int)
            .default(10)
            .doc("Give up and report unhealthy after this many seconds."),
    ]
}

pub(crate) fn common_outputs() -> Vec<FieldSchema> {
    vec![
        field(
            "status",
            FieldType::enumeration(["healthy", "degraded", "unhealthy", "unknown"]),
        )
        .doc("Result of the most recent run during apply."),
        field("message", FieldType::String).doc("Human-readable detail from the last run."),
        field("latency_ms", FieldType::Int).doc("Time the last run took."),
        field("checked_at", FieldType::String).doc("RFC 3339 timestamp of the last run."),
    ]
}

/// Shared `Handler` behaviour: a check "exists" once applied; its identity is its
/// inputs; create/update run the check once and expose the result as outputs.
#[async_trait]
pub(crate) trait CheckResource: Checker + Send + Sync {
    fn check_schema(&self) -> crate::schema::ResourceSchema;
}

pub(crate) fn outcome_outputs(o: &CheckOutcome) -> Value {
    json!({
        "status": o.status,
        "message": o.message,
        "latency_ms": o.latency_ms,
        "checked_at": Utc::now().to_rfc3339(),
    })
}

pub(crate) async fn run_once<C: Checker + ?Sized>(
    c: &C,
    cx: &Ctx<'_>,
    inputs: &Value,
) -> Result<Applied> {
    let outcome = c.check(cx, inputs).await?;
    if c.required(inputs) && outcome.status != Health::Healthy {
        anyhow::bail!("check {}: {}", outcome.status_str(), outcome.message);
    }
    Ok(Applied {
        id: Some("check".into()),
        outputs: outcome_outputs(&outcome),
    })
}

impl CheckOutcome {
    pub fn healthy(message: impl Into<String>, started: Instant) -> Self {
        Self {
            status: Health::Healthy,
            message: message.into(),
            latency_ms: Some(ms(started)),
        }
    }

    pub fn unhealthy(message: impl Into<String>, started: Instant) -> Self {
        Self {
            status: Health::Unhealthy,
            message: message.into(),
            latency_ms: Some(ms(started)),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            status: Health::Unknown,
            message: message.into(),
            latency_ms: None,
        }
    }

    pub fn status_str(&self) -> &'static str {
        match self.status {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Unhealthy => "unhealthy",
            Health::Drifted => "drifted",
            Health::Unknown => "unknown",
        }
    }
}

fn ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// Implements `Handler` for a check type in terms of `Checker` + `check_schema`.
macro_rules! check_handler {
    ($t:ty) => {
        #[async_trait::async_trait]
        impl crate::provider::Handler for $t {
            fn schema(&self) -> crate::schema::ResourceSchema {
                crate::providers::check::CheckResource::check_schema(self)
            }

            async fn read(
                &self,
                _cx: &crate::provider::Ctx<'_>,
                id: Option<&str>,
                inputs: &serde_json::Value,
            ) -> crate::provider::Result<Option<crate::provider::Actual>> {
                Ok(id.map(|id| crate::provider::Actual {
                    id: Some(id.to_string()),
                    props: inputs.clone(),
                    outputs: serde_json::Value::Object(Default::default()),
                }))
            }

            async fn create(
                &self,
                cx: &crate::provider::Ctx<'_>,
                inputs: &serde_json::Value,
            ) -> crate::provider::Result<crate::provider::Applied> {
                crate::providers::check::run_once(self, cx, inputs).await
            }

            async fn update(
                &self,
                cx: &crate::provider::Ctx<'_>,
                _id: Option<&str>,
                inputs: &serde_json::Value,
                _actual: &crate::provider::Actual,
            ) -> crate::provider::Result<crate::provider::Applied> {
                crate::providers::check::run_once(self, cx, inputs).await
            }

            async fn delete(
                &self,
                _cx: &crate::provider::Ctx<'_>,
                _id: Option<&str>,
                _inputs: &serde_json::Value,
            ) -> crate::provider::Result<()> {
                Ok(())
            }
        }
    };
}
pub(crate) use check_handler;

#[allow(dead_code)]
fn _assert_actual_used(_: Actual) {}
