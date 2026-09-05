//! HTTP client for the daemon execution boundary.

use std::time::Duration;

use anyhow::Context;
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::control::{
    ApprovalGrant, ExecutionEvent, ExecutionRun, LeaseExtension, LeaseRequest,
    ProgramResolveRequest, ProgramRevision, RunRequest, StackBuildStatus, StackLease,
};
use crate::explorer::TopologySnapshot;
use crate::monitor::StackStatus;
use crate::state::State;

/// Client for the versioned `ifxd` control API.
#[derive(Clone)]
pub struct DaemonClient {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
}

impl DaemonClient {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let base = Url::parse(base).with_context(|| format!("invalid ifxd URL `{base}`"))?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https"),
            "ifxd URL must use http:// or https://"
        );
        Ok(Self {
            base,
            token: std::env::var("IFXD_TOKEN").ok(),
            http: reqwest::Client::new(),
        })
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    pub async fn health(&self) -> anyhow::Result<()> {
        let url = self.endpoint(&["healthz"])?;
        let response = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}: is ifxd running?"))?;
        anyhow::ensure!(
            response.status().is_success(),
            "GET {url}: {}",
            response.status()
        );
        Ok(())
    }

    pub async fn resolve(
        &self,
        stack: &str,
        request: &ProgramResolveRequest,
    ) -> anyhow::Result<ProgramRevision> {
        self.send(
            Method::POST,
            &["api", "v1", "stacks", stack, "program"],
            Some(request),
        )
        .await
    }

    pub async fn build_status(&self, stack: &str) -> anyhow::Result<StackBuildStatus> {
        self.send::<(), _>(Method::GET, &["api", "v1", "stacks", stack, "build"], None)
            .await
    }

    pub async fn retry_build(&self, stack: &str) -> anyhow::Result<StackBuildStatus> {
        self.send::<(), _>(Method::POST, &["api", "v1", "stacks", stack, "build"], None)
            .await
    }

    pub async fn active_revision(&self, stack: &str) -> anyhow::Result<ProgramRevision> {
        self.send::<(), _>(
            Method::GET,
            &["api", "v1", "stacks", stack, "revisions", "active"],
            None,
        )
        .await
    }

    pub async fn start_run(
        &self,
        stack: &str,
        request: &RunRequest,
    ) -> anyhow::Result<ExecutionRun> {
        self.send(
            Method::POST,
            &["api", "v1", "stacks", stack, "runs"],
            Some(request),
        )
        .await
    }

    pub async fn runs(&self, stack: &str) -> anyhow::Result<Vec<ExecutionRun>> {
        self.send::<(), _>(Method::GET, &["api", "v1", "stacks", stack, "runs"], None)
            .await
    }

    pub async fn run(&self, run_id: &str) -> anyhow::Result<ExecutionRun> {
        self.send::<(), _>(Method::GET, &["api", "v1", "runs", run_id], None)
            .await
    }

    pub async fn events(&self, run_id: &str) -> anyhow::Result<Vec<ExecutionEvent>> {
        self.send::<(), _>(
            Method::GET,
            &["api", "v1", "runs", run_id, "events.json"],
            None,
        )
        .await
    }

    pub async fn cancel(&self, run_id: &str) -> anyhow::Result<ExecutionRun> {
        self.send::<(), _>(Method::POST, &["api", "v1", "runs", run_id, "cancel"], None)
            .await
    }

    pub async fn retry_now(&self, run_id: &str) -> anyhow::Result<ExecutionRun> {
        self.send::<(), _>(Method::POST, &["api", "v1", "runs", run_id, "retry"], None)
            .await
    }

    pub async fn approve(
        &self,
        run_id: &str,
        grant: &ApprovalGrant,
    ) -> anyhow::Result<ExecutionRun> {
        self.send(
            Method::POST,
            &["api", "v1", "runs", run_id, "approve"],
            Some(grant),
        )
        .await
    }

    /// Poll a durable run and deliver each persisted event exactly once.
    pub async fn wait_run(
        &self,
        run_id: &str,
        mut on_event: impl FnMut(&ExecutionEvent),
    ) -> anyhow::Result<ExecutionRun> {
        let mut seen = 0;
        loop {
            let events = self.events(run_id).await?;
            for event in events.iter().skip(seen) {
                on_event(event);
            }
            seen = events.len();
            let run = self.run(run_id).await?;
            if run.status.terminal() {
                // The terminal status and final event are separate durable writes.
                let events = self.events(run_id).await?;
                for event in events.iter().skip(seen) {
                    on_event(event);
                }
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn state(&self, stack: &str) -> anyhow::Result<State> {
        self.send::<(), _>(Method::GET, &["api", "v1", "stacks", stack, "state"], None)
            .await
    }

    pub async fn replace_state(&self, stack: &str, state: &State) -> anyhow::Result<State> {
        self.send(
            Method::PUT,
            &["api", "v1", "stacks", stack, "state"],
            Some(state),
        )
        .await
    }

    pub async fn forget_state(&self, stack: &str, urn: &str) -> anyhow::Result<State> {
        self.send::<(), _>(
            Method::DELETE,
            &["api", "v1", "stacks", stack, "state", urn],
            None,
        )
        .await
    }

    pub async fn lease(&self, stack: &str) -> anyhow::Result<StackLease> {
        self.send::<(), _>(Method::GET, &["api", "v1", "stacks", stack, "lease"], None)
            .await
    }

    pub async fn set_lease(
        &self,
        stack: &str,
        request: &LeaseRequest,
    ) -> anyhow::Result<StackLease> {
        self.send(
            Method::PUT,
            &["api", "v1", "stacks", stack, "lease"],
            Some(request),
        )
        .await
    }

    pub async fn extend_lease(
        &self,
        stack: &str,
        extension: &LeaseExtension,
    ) -> anyhow::Result<StackLease> {
        self.send(
            Method::POST,
            &["api", "v1", "stacks", stack, "lease", "extend"],
            Some(extension),
        )
        .await
    }

    pub async fn clear_lease(&self, stack: &str) -> anyhow::Result<()> {
        self.send::<(), ()>(
            Method::DELETE,
            &["api", "v1", "stacks", stack, "lease"],
            None,
        )
        .await
    }

    pub async fn status(&self, stack: &str) -> anyhow::Result<StackStatus> {
        self.send::<(), _>(Method::GET, &["api", "stacks", stack], None)
            .await
    }

    pub async fn topology(
        &self,
        stack: &str,
        no_refresh: bool,
    ) -> anyhow::Result<TopologySnapshot> {
        let mut url = self.endpoint(&["api", "stacks", stack, "topology"])?;
        if no_refresh {
            url.query_pairs_mut().append_pair("no_refresh", "true");
        }
        self.send_url::<(), _>(Method::GET, url, None).await
    }

    fn endpoint(&self, segments: &[&str]) -> anyhow::Result<Url> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("ifxd URL cannot be a base URL"))?;
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        Ok(url)
    }

    async fn send<B, R>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&B>,
    ) -> anyhow::Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let url = self.endpoint(segments)?;
        self.send_url(method, url, body).await
    }

    async fn send_url<B, R>(&self, method: Method, url: Url, body: Option<&B>) -> anyhow::Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let mut request = self.http.request(method.clone(), url.clone());
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("{method} {url}: is ifxd running?"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| value["error"].as_str().map(str::to_owned))
                .unwrap_or(text);
            anyhow::bail!("{method} {url}: {status}: {message}")
        }
        if status == StatusCode::NO_CONTENT {
            return serde_json::from_str("null").map_err(Into::into);
        }
        response
            .json()
            .await
            .with_context(|| format!("decoding {method} {url} response"))
    }
}
