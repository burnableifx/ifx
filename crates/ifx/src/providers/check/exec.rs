use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{CheckResource, check_handler, common_inputs, common_outputs};
use crate::provider::{CheckOutcome, Checker, Ctx, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::Cmd;

/// `check.exec`: run a command on a host; exit 0 is healthy.
#[derive(Default)]
pub struct ExecCheck;

impl CheckResource for ExecCheck {
    fn check_schema(&self) -> ResourceSchema {
        let mut s = ResourceSchema::new(
            "check.exec",
            "Command health check: run a shell command on a host; exit status 0 is healthy, anything else unhealthy.",
        )
        .input(field("on", FieldType::Connection).required().doc("Where to run: `local()` or a host's `connection` output."))
        .input(field("command", FieldType::String).required().doc("Shell command, e.g. `systemctl is-active nginx`."))
        .input(field("privileged", FieldType::Bool).default(false).doc("Run with sudo when the connection allows it."));
        for f in common_inputs() {
            s = s.input(f);
        }
        for f in common_outputs() {
            s = s.output(f);
        }
        s
    }
}

#[async_trait]
impl Checker for ExecCheck {
    async fn check(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<CheckOutcome> {
        let command = inputs["command"].as_str().unwrap_or_default().to_string();
        let timeout = Duration::from_secs(inputs["timeout_secs"].as_u64().unwrap_or(10));
        let transport = match cx.transports.from_value(&inputs["on"]).await {
            Ok(t) => t,
            Err(e) => return Ok(CheckOutcome::unknown(format!("{e:#}"))),
        };
        let mut cmd = Cmd::sh(command.clone());
        cmd.privileged = inputs["privileged"].as_bool().unwrap_or(false);
        let started = Instant::now();
        match tokio::time::timeout(timeout, transport.exec(&cmd)).await {
            Ok(Ok(out)) if out.success() => {
                let msg = out.stdout_str().trim().to_string();
                Ok(CheckOutcome::healthy(
                    if msg.is_empty() { command } else { msg },
                    started,
                ))
            }
            Ok(Ok(out)) => {
                let detail = out.stderr_str().trim().to_string();
                let detail = if detail.is_empty() {
                    out.stdout_str().trim().to_string()
                } else {
                    detail
                };
                Ok(CheckOutcome::unhealthy(
                    format!("exit {}: {detail}", out.status),
                    started,
                ))
            }
            Ok(Err(e)) => Ok(CheckOutcome::unknown(format!("{e:#}"))),
            Err(_) => Ok(CheckOutcome::unhealthy("timed out", started)),
        }
    }
}

check_handler!(ExecCheck);
