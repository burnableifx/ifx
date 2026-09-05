use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{CheckResource, check_handler, common_inputs, common_outputs};
use crate::provider::{CheckOutcome, Checker, Ctx, Result};
use crate::schema::{FieldType, ResourceSchema, field};

/// `check.http`: request a URL and compare status / body.
#[derive(Default)]
pub struct HttpCheck;

impl CheckResource for HttpCheck {
    fn check_schema(&self) -> ResourceSchema {
        let mut s = ResourceSchema::new(
            "check.http",
            "HTTP health check: fetch a URL and expect a status code and, optionally, a body substring.",
        )
        .input(field("url", FieldType::String).required().doc("URL to request, e.g. `\"http://\" + web.ipv4 + \"/healthz\"`."))
        .input(field("method", FieldType::String).default("GET").doc("HTTP method."))
        .input(field("expect_status", FieldType::Int).default(200).doc("Expected response status code."))
        .input(field("expect_body", FieldType::String).doc("Substring the response body must contain."))
        .input(field("headers", FieldType::map(FieldType::String)).doc("Extra request headers."))
        .input(field("insecure", FieldType::Bool).default(false).doc("Skip TLS certificate verification."));
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
impl Checker for HttpCheck {
    async fn check(&self, _cx: &Ctx<'_>, inputs: &Value) -> Result<CheckOutcome> {
        let url = inputs["url"].as_str().unwrap_or_default().to_string();
        let method = inputs["method"].as_str().unwrap_or("GET").to_uppercase();
        let expect_status = inputs["expect_status"].as_u64().unwrap_or(200) as u16;
        let expect_body = inputs["expect_body"].as_str().map(str::to_string);
        let timeout = Duration::from_secs(inputs["timeout_secs"].as_u64().unwrap_or(10));
        let insecure = inputs["insecure"].as_bool().unwrap_or(false);
        let started = Instant::now();
        let client = match reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(insecure)
            .build()
        {
            Ok(c) => c,
            Err(e) => return Ok(CheckOutcome::unknown(format!("client: {e}"))),
        };
        let m = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(_) => return Ok(CheckOutcome::unknown(format!("bad method {method}"))),
        };
        let mut req = client.request(m, &url);
        if let Some(h) = inputs["headers"].as_object() {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return Ok(CheckOutcome::unhealthy(format!("{url}: {e}"), started)),
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status != expect_status {
            return Ok(CheckOutcome::unhealthy(
                format!("{url}: status {status}, expected {expect_status}"),
                started,
            ));
        }
        if let Some(needle) = expect_body
            && !body.contains(&needle)
        {
            return Ok(CheckOutcome::unhealthy(
                format!("{url}: body does not contain {needle:?}"),
                started,
            ));
        }
        Ok(CheckOutcome::healthy(format!("{url}: {status}"), started))
    }
}

check_handler!(HttpCheck);
