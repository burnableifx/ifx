use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{CheckResource, check_handler, common_inputs, common_outputs};
use crate::provider::{CheckOutcome, Checker, Ctx, Result};
use crate::schema::{FieldType, ResourceSchema, field};

/// `check.tcp`: open a TCP connection.
#[derive(Default)]
pub struct TcpCheck;

impl CheckResource for TcpCheck {
    fn check_schema(&self) -> ResourceSchema {
        let mut s = ResourceSchema::new(
            "check.tcp",
            "TCP health check: a connection to host:port must succeed.",
        )
        .input(
            field("host", FieldType::String)
                .required()
                .doc("Hostname or IP, typically `web.ipv4`."),
        )
        .input(field("port", FieldType::Int).required().doc("TCP port."));
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
impl Checker for TcpCheck {
    async fn check(&self, _cx: &Ctx<'_>, inputs: &Value) -> Result<CheckOutcome> {
        let host = inputs["host"].as_str().unwrap_or_default();
        let port = inputs["port"].as_u64().unwrap_or(0);
        let timeout = Duration::from_secs(inputs["timeout_secs"].as_u64().unwrap_or(10));
        let addr = format!("{host}:{port}");
        let started = Instant::now();
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await {
            Ok(Ok(_)) => Ok(CheckOutcome::healthy(format!("{addr}: connected"), started)),
            Ok(Err(e)) => Ok(CheckOutcome::unhealthy(format!("{addr}: {e}"), started)),
            Err(_) => Ok(CheckOutcome::unhealthy(
                format!("{addr}: timed out"),
                started,
            )),
        }
    }
}

check_handler!(TcpCheck);
