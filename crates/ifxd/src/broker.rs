//! A deliberately narrow credential boundary: fixed upstreams, fixed stacks,
//! expiring caller grants, no caller-provided request bodies or executor options.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Status,
    Plan,
    Ignite,
    Extinguish,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Ifxd,
    BurnableSandbox,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub name: String,
    pub backend: Backend,
    pub url: String,
    pub token_file: PathBuf,
    /// Required only for the Burnable sandbox adapter.
    pub account_id: Option<String>,
    pub stacks: Vec<StackBinding>,
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackBinding {
    pub name: String,
    pub remote: String,
    /// Operator-pinned Burnable manifest, never supplied by a caller.
    pub manifest: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub name: String,
    pub token_sha256: String,
    pub stacks: Vec<String>,
    pub operations: Vec<Operation>,
    pub expires_at: DateTime<Utc>,
}

struct Target {
    config: TargetConfig,
    url: Url,
    credential: HeaderValue,
}

pub struct Broker {
    targets: Vec<Target>,
    http: Client,
    slots: tokio::sync::Semaphore,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn uuid_v4(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            14 => byte == b'4',
            19 => b"89ab".contains(&byte),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

impl Broker {
    pub fn load(configs: &[TargetConfig]) -> anyhow::Result<Arc<Self>> {
        ensure!(
            !configs.is_empty() && configs.len() <= 64,
            "configure 1–64 broker targets"
        );
        let mut names = BTreeSet::new();
        let mut targets = Vec::new();
        for config in configs {
            ensure!(
                identifier(&config.name) && names.insert(config.name.clone()),
                "broker target names must be unique identifiers"
            );
            let url = Url::parse(&config.url).context("invalid broker upstream origin")?;
            let loopback = url
                .host_str()
                .and_then(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|ip| ip.is_loopback());
            ensure!(
                (url.scheme() == "https" || (url.scheme() == "http" && loopback))
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "broker upstream must be an HTTPS origin (HTTP is permitted only for literal loopback)"
            );
            ensure!(
                !config.stacks.is_empty() && config.stacks.len() <= 128,
                "configure 1–128 stack bindings per target"
            );
            let mut stacks = BTreeSet::new();
            for stack in &config.stacks {
                ensure!(
                    identifier(&stack.name)
                        && identifier(&stack.remote)
                        && stacks.insert(stack.name.clone()),
                    "stack bindings must use unique local names and fixed remote identifiers"
                );
                match config.backend {
                    Backend::Ifxd => ensure!(
                        stack.manifest.is_none() && config.account_id.is_none(),
                        "IFXD bindings cannot contain manifests or account overrides"
                    ),
                    Backend::BurnableSandbox => {
                        ensure!(
                            config.account_id.as_deref().is_some_and(uuid_v4)
                                && uuid_v4(&stack.remote),
                            "Burnable sandbox requires fixed UUIDv4 account and deployment IDs"
                        );
                        let Some(manifest) = &stack.manifest else {
                            anyhow::bail!("Burnable sandbox requires an operator-pinned manifest")
                        };
                        ensure!(
                            manifest.is_object() && serde_json::to_vec(manifest)?.len() <= 4096,
                            "Burnable manifest must be a JSON object of at most 4096 bytes"
                        );
                    }
                }
            }
            ensure!(
                !config.grants.is_empty() && config.grants.len() <= 128,
                "configure 1–128 grants per target"
            );
            let mut grant_names = BTreeSet::new();
            for grant in &config.grants {
                ensure!(
                    identifier(&grant.name) && grant_names.insert(grant.name.clone()),
                    "grant names must be unique identifiers"
                );
                ensure!(
                    grant.token_sha256.len() == 64
                        && grant
                            .token_sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    "grant tokens must be lowercase SHA-256 digests"
                );
                ensure!(
                    !grant.operations.is_empty()
                        && !grant.stacks.is_empty()
                        && grant.stacks.iter().all(|stack| stacks.contains(stack)),
                    "grant requires operations and registered stacks"
                );
            }
            let file = std::fs::File::open(&config.token_file)
                .context("cannot open upstream credential file")?;
            ensure!(
                file.metadata()?.is_file(),
                "upstream credential must be a regular file"
            );
            let mut token = String::new();
            file.take(4097)
                .read_to_string(&mut token)
                .context("cannot read upstream credential")?;
            ensure!(
                token.len() <= 4096
                    && !token.trim().is_empty()
                    && token.trim().bytes().all(|byte| byte.is_ascii_graphic()),
                "invalid upstream credential"
            );
            let mut credential = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
                .context("invalid credential header")?;
            credential.set_sensitive(true);
            let upstream_hash = hex::encode(Sha256::digest(token.trim().as_bytes()));
            ensure!(
                configs
                    .iter()
                    .flat_map(|config| &config.grants)
                    .all(|grant| grant.token_sha256 != upstream_hash),
                "caller and upstream credentials must differ"
            );
            targets.push(Target {
                config: config.clone(),
                url,
                credential,
            });
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Arc::new(Self {
            targets,
            http,
            slots: tokio::sync::Semaphore::new(16),
        }))
    }

    async fn upstream(
        &self,
        target: &Target,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<(StatusCode, Value), StatusCode> {
        let mut url = target.url.clone();
        url.path_segments_mut()
            .map_err(|_| StatusCode::BAD_GATEWAY)?
            .clear()
            .extend(segments);
        let mut request = self
            .http
            .request(method, url)
            .header("Authorization", target.credential.clone());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let status = response.status();
        if !status.is_success() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?
        {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(StatusCode::BAD_GATEWAY);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_GATEWAY)?;
        Ok((status, value))
    }

    async fn call(
        &self,
        target: &Target,
        stack: &StackBinding,
        operation: Operation,
    ) -> Result<(StatusCode, Summary), StatusCode> {
        let mut summary = Summary {
            target: target.config.name.clone(),
            stack: stack.name.clone(),
            operation,
            state: "unknown".into(),
            resources: None,
            run_id: None,
            reserved_mills: None,
            minutes: None,
        };
        match target.config.backend {
            Backend::Ifxd => {
                let (status, value) = if operation == Operation::Status {
                    self.upstream(target, Method::GET, &["api", "stacks", &stack.remote], None)
                        .await?
                } else {
                    let kind = match operation {
                        Operation::Plan => "plan",
                        Operation::Ignite => "apply",
                        Operation::Extinguish => "destroy",
                        Operation::Status => unreachable!(),
                    };
                    self.upstream(
                        target,
                        Method::POST,
                        &["api", "v1", "stacks", &stack.remote, "runs"],
                        Some(json!({"kind": kind})),
                    )
                    .await?
                };
                if value.get("stack").and_then(Value::as_str) != Some(&stack.remote) {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                if operation == Operation::Status {
                    summary.resources = Some(
                        value
                            .get("resources")
                            .and_then(Value::as_u64)
                            .ok_or(StatusCode::BAD_GATEWAY)?,
                    );
                    summary.state = selected_state(
                        value.get("overall"),
                        &["healthy", "degraded", "unhealthy", "drifted", "unknown"],
                    )
                    .unwrap_or("unknown")
                    .into();
                } else {
                    summary.state = selected_state(
                        value.get("status"),
                        &[
                            "queued",
                            "running",
                            "retry_wait",
                            "approval_wait",
                            "recovery_wait",
                            "succeeded",
                            "failed",
                            "cancelled",
                        ],
                    )
                    .ok_or(StatusCode::BAD_GATEWAY)?
                    .into();
                    summary.run_id = Some(
                        value
                            .get("run_id")
                            .and_then(Value::as_str)
                            .filter(|value| identifier(value))
                            .ok_or(StatusCode::BAD_GATEWAY)?
                            .into(),
                    );
                }
                Ok((status, summary))
            }
            Backend::BurnableSandbox => {
                let (_, account) = self
                    .upstream(target, Method::GET, &["api", "account"], None)
                    .await?;
                if account.get("id").and_then(Value::as_str) != target.config.account_id.as_deref()
                    || account.get("sandbox") != Some(&Value::Bool(true))
                {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                let manifest = stack
                    .manifest
                    .as_ref()
                    .ok_or(StatusCode::BAD_GATEWAY)?
                    .to_string();
                let (status, value) = match operation {
                    Operation::Status => {
                        self.upstream(target, Method::GET, &["api", "stacks", &stack.remote], None)
                            .await?
                    }
                    Operation::Plan => {
                        self.upstream(
                            target,
                            Method::POST,
                            &["api", "stacks", "plan"],
                            Some(json!({"manifest": manifest})),
                        )
                        .await?
                    }
                    Operation::Ignite => {
                        self.upstream(
                            target,
                            Method::POST,
                            &["api", "stacks"],
                            Some(json!({"manifest": manifest, "request": stack.remote})),
                        )
                        .await?
                    }
                    Operation::Extinguish => {
                        self.upstream(
                            target,
                            Method::DELETE,
                            &["api", "stacks", &stack.remote],
                            None,
                        )
                        .await?
                    }
                };
                if value.get("sandbox") != Some(&Value::Bool(true)) {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                if operation == Operation::Plan {
                    summary.state = "planned".into();
                    summary.reserved_mills = Some(
                        value
                            .get("reserved")
                            .and_then(Value::as_u64)
                            .ok_or(StatusCode::BAD_GATEWAY)?,
                    );
                    summary.minutes = Some(
                        value
                            .get("minutes")
                            .and_then(Value::as_u64)
                            .ok_or(StatusCode::BAD_GATEWAY)?,
                    );
                } else {
                    let result = value.get("stack").ok_or(StatusCode::BAD_GATEWAY)?;
                    if result.get("id").and_then(Value::as_str) != Some(&stack.remote) {
                        return Err(StatusCode::BAD_GATEWAY);
                    }
                    summary.state = selected_state(
                        result.get("status"),
                        &["running", "partial", "extinguished"],
                    )
                    .ok_or(StatusCode::BAD_GATEWAY)?
                    .into();
                    summary.resources = Some(
                        result
                            .get("resources")
                            .and_then(Value::as_array)
                            .ok_or(StatusCode::BAD_GATEWAY)?
                            .len() as u64,
                    );
                }
                Ok((status, summary))
            }
        }
    }
}

fn selected_state<'a>(value: Option<&'a Value>, allowed: &[&str]) -> Option<&'a str> {
    value
        .and_then(Value::as_str)
        .filter(|state| allowed.contains(state))
}

#[derive(Serialize)]
struct Summary {
    target: String,
    stack: String,
    operation: Operation,
    state: String,
    resources: Option<u64>,
    run_id: Option<String>,
    reserved_mills: Option<u64>,
    minutes: Option<u64>,
}

pub fn router(broker: Arc<Broker>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/api/v1/brokers/{target}/stacks/{stack}/{operation}",
            post(call),
        )
        .with_state(broker)
}

async fn call(
    State(broker): State<Arc<Broker>>,
    Path((target_name, stack_name, operation)): Path<(String, String, Operation)>,
    request: Request,
) -> Response {
    let Some(token) = request
        .headers()
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() >= 32 && token.len() <= 4096)
    else {
        return failure(StatusCode::UNAUTHORIZED);
    };
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    let Some(target) = broker
        .targets
        .iter()
        .find(|target| target.config.name == target_name)
    else {
        return failure(StatusCode::UNAUTHORIZED);
    };
    let grant = target.config.grants.iter().find(|grant| {
        grant.token_sha256 == digest
            && grant.expires_at > Utc::now()
            && grant.stacks.contains(&stack_name)
            && grant.operations.contains(&operation)
    });
    let Some(grant) = grant else {
        return failure(StatusCode::FORBIDDEN);
    };
    let Some(stack) = target
        .config
        .stacks
        .iter()
        .find(|stack| stack.name == stack_name)
    else {
        return failure(StatusCode::FORBIDDEN);
    };
    if request.uri().query().is_some() {
        return failure(StatusCode::BAD_REQUEST);
    }
    let Ok(_slot) = broker.slots.try_acquire() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE);
    };
    // Bound even an authenticated caller that stalls while sending its body.
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), to_bytes(request.into_body(), 0)).await,
        Ok(Ok(_))
    ) {
        return failure(StatusCode::BAD_REQUEST);
    }
    let result = broker.call(target, stack, operation).await;
    tracing::info!(target = %target_name, stack = %stack_name, caller = %grant.name, ?operation, success = result.is_ok(), "broker operation");
    match result {
        Ok((status, summary)) => {
            // Only a reviewed projection leaves this boundary. Even the opaque run
            // ID must not reflect an upstream credential supplied by a bad server.
            let upstream_secret = target
                .credential
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "));
            if summary
                .run_id
                .as_deref()
                .is_some_and(|run| upstream_secret.is_some_and(|secret| run.contains(secret)))
            {
                return failure(StatusCode::BAD_GATEWAY);
            }
            (status, [("Cache-Control", "no-store")], axum::Json(summary)).into_response()
        }
        Err(status) => failure(status),
    }
}

fn failure(status: StatusCode) -> Response {
    let message = if status == StatusCode::BAD_GATEWAY {
        "upstream operation failed; mutation outcome may be unknown; do not blindly retry"
    } else {
        "broker request not authorized or invalid"
    };
    (
        status,
        [("Cache-Control", "no-store")],
        axum::Json(json!({"error": message})),
    )
        .into_response()
}
