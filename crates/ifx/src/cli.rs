//! Thin command-line client for the `ifxd` execution and compilation boundary.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::color::Paint;
use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::Value;

use crate::client::DaemonClient;
use crate::control::{
    ApprovalGrant, BuildPhase, ExecutionEvent, ExecutionRun, ExecutionStatus, LeaseExtension,
    LeaseRequest, ProgramResolveRequest, RetryPolicy, RunRequest, StackLease,
};
use crate::engine::{Plan, Report};
use crate::model::Urn;
use crate::provider::Registry;
use crate::state::State;
use crate::store::{HealthRecord, RunKind};
use crate::{monitor, render, stubs};

/// Everything a front-end needs to build a [`Program`].
#[derive(Clone)]
pub struct LoadCtx {
    pub dir: PathBuf,
    pub file: Option<PathBuf>,
    pub stack: String,
    pub config: BTreeMap<String, Value>,
    /// Project name: `ifx.toml` `name`, else the directory basename.
    pub project: String,
}

impl LoadCtx {
    /// Build from a stack directory, applying `ifx.toml` and overrides. Shared by the
    /// CLI and `ifxd`.
    pub fn from_dir(
        dir: &Path,
        file: Option<PathBuf>,
        stack: &str,
        overrides: &[(String, Value)],
    ) -> anyhow::Result<Self> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let mut config = BTreeMap::new();
        let mut file = file.map(|file| {
            if file.is_absolute() {
                file
            } else {
                dir.join(file)
            }
        });
        let mut project = None;
        let toml_path = dir.join("ifx.toml");
        if toml_path.exists() {
            let text = std::fs::read_to_string(&toml_path)?;
            let sf: StackFile = toml::from_str(&text)
                .with_context(|| format!("parsing {}", toml_path.display()))?;
            for (k, v) in sf.config {
                config.insert(k, serde_json::to_value(v)?);
            }
            if file.is_none() {
                file = sf.file.map(|f| dir.join(f));
            }
            project = sf.name;
        }
        for (k, v) in overrides {
            config.insert(k.clone(), v.clone());
        }
        let project = project.unwrap_or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("default")
                .to_string()
        });
        Ok(Self {
            dir,
            file,
            stack: stack.to_string(),
            config,
            project,
        })
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "ifx",
    version,
    about = "Infrastructure as one reconciled graph."
)]
pub struct Cli {
    /// Stack directory (holds ifx.toml, the stack file, and .ifx/).
    #[arg(short = 'C', long, global = true, default_value = ".")]
    pub dir: PathBuf,
    /// Rust stack manifest (default: Cargo.toml in the stack directory).
    #[arg(short, long, global = true)]
    pub file: Option<PathBuf>,
    /// Stack name (one program, several independent deployments).
    #[arg(
        short,
        long,
        global = true,
        env = "IFX_STACK",
        default_value = "default"
    )]
    pub stack: String,
    /// URL of the ifxd execution daemon.
    #[arg(
        long,
        global = true,
        env = "IFXD_URL",
        default_value = "http://127.0.0.1:7433"
    )]
    pub daemon: String,
    /// Config value (`key=value`), overrides ifx.toml `[config]`.
    #[arg(short = 'c', long = "config", global = true, value_name = "KEY=VALUE")]
    pub config: Vec<String>,
    /// Only act on these resources (and their dependencies).
    #[arg(short, long, global = true, value_name = "URN")]
    pub target: Vec<String>,
    /// Max concurrent operations.
    #[arg(short = 'j', long, global = true, default_value_t = 8)]
    pub parallel: usize,
    /// Maximum execution attempts, including the first attempt.
    #[arg(long, global = true, default_value_t = 3)]
    pub attempts: u32,
    /// Initial retry delay; subsequent delays grow exponentially.
    #[arg(long, global = true, default_value_t = 2)]
    pub retry_backoff_secs: u64,
    /// Cap for exponential retry delays.
    #[arg(long, global = true, default_value_t = 60)]
    pub retry_max_backoff_secs: u64,
    /// Show unchanged resources and debug logs.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Force an immediate stack build instead of waiting for the file watcher.
    Build,
    /// Show what apply would do.
    Plan {
        /// Trust state instead of observing resources.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Create, update, and delete resources to match the program.
    Apply {
        #[arg(short, long)]
        yes: bool,
        #[arg(long)]
        no_refresh: bool,
        /// Approve this plan's exact URN/risk operation (repeatable).
        #[arg(long, value_name = "URN/RISK")]
        approve: Vec<String>,
        /// Set the stack lease first: destroy everything this long after now (e.g. 30m, 2h).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        lease: Option<Duration>,
    },
    /// Delete every resource in state.
    Destroy {
        #[arg(short, long)]
        yes: bool,
        /// Approve this plan's exact URN/risk operation (repeatable).
        #[arg(long, value_name = "URN/RISK")]
        approve: Vec<String>,
    },
    /// Re-observe resources and update state.
    Refresh,
    /// Run every health check in the stack once.
    Check {
        #[arg(long)]
        json: bool,
    },
    /// Health, drift, and recent runs from ifxd.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Inspect and control durable daemon runs.
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Set, extend, show, or clear the stack's destruction deadline.
    Lease {
        #[command(subcommand)]
        cmd: LeaseCmd,
    },
    /// Print outputs of applied resources.
    Outputs,
    /// Inspect, export, import, or edit state.
    State {
        #[command(subcommand)]
        cmd: StateCmd,
    },
    /// Export the dependency graph as Graphviz, JSON, or an interactive HTML document.
    Graph {
        /// Output format. HTML and JSON include current plan, health, and run history.
        #[arg(long, value_enum, default_value = "dot")]
        format: GraphFormat,
        /// Write here instead of stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// For HTML and JSON, trust state instead of observing resources.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Describe available resource types.
    Schema {
        /// Only this type.
        type_name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Generate typed stubs for a front-end language.
    Stubs {
        #[arg(value_enum)]
        lang: stubs::Lang,
        /// Write here instead of stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Print the daemon-emitted Program as JSON.
    #[command(hide = true)]
    EmitProgram,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    Dot,
    Json,
    Html,
}

#[derive(Subcommand, Debug)]
pub enum StateCmd {
    List,
    Show {
        urn: String,
    },
    /// Forget a resource without deleting it.
    Rm {
        urn: String,
    },
    /// Write the stack's state as JSON.
    Export {
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Replace the stack's state from a JSON export.
    Import {
        file: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum RunCmd {
    /// List recent runs for this stack.
    List,
    /// Show one run, including its result or error.
    Show { run_id: String },
    /// Show the durable event stream for one run.
    Events { run_id: String },
    /// Cancel a queued, running, or waiting run.
    Cancel { run_id: String },
    /// Wake a run immediately when it is waiting in exponential backoff.
    Retry { run_id: String },
    /// Grant one exact risk currently blocking a run.
    Approve { run_id: String, selector: String },
}

#[derive(Subcommand, Debug)]
pub enum LeaseCmd {
    /// Set or replace the lease: destroy everything at a deadline or this long after now.
    Set {
        /// Duration from now, e.g. 30m, 2h, 1h30m.
        #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
        duration: Option<Duration>,
        /// Absolute RFC 3339 deadline, e.g. 2026-09-04T18:00:00Z.
        #[arg(long, value_name = "RFC3339", conflicts_with = "duration")]
        until: Option<DateTime<Utc>>,
    },
    /// Show the lease and time remaining.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Push an unexpired deadline later.
    Extend {
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        by: Duration,
    },
    /// Remove the lease. Refused when the daemon requires one.
    Clear,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime_serde::re::humantime::parse_duration(s).map_err(|e| e.to_string())
}

#[derive(Debug, Default, Deserialize)]
struct StackFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    config: BTreeMap<String, toml::Value>,
}

fn parse_config_arg(s: &str) -> anyhow::Result<(String, Value)> {
    let (k, v) = s
        .split_once('=')
        .with_context(|| format!("config `{s}`: expected key=value"))?;
    let v = serde_json::from_str::<Value>(v).unwrap_or(Value::String(v.to_string()));
    Ok((k.to_string(), v))
}

impl Cli {
    pub fn load_ctx(&self, registry: Registry) -> anyhow::Result<LoadCtx> {
        let overrides = self
            .config
            .iter()
            .map(|c| parse_config_arg(c))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let _ = registry;
        LoadCtx::from_dir(&self.dir, self.file.clone(), &self.stack, &overrides)
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.attempts,
            initial_backoff_secs: self.retry_backoff_secs,
            max_backoff_secs: self.retry_max_backoff_secs,
        }
    }

    fn run_request(&self, kind: RunKind, no_refresh: bool) -> anyhow::Result<RunRequest> {
        let targets = self
            .target
            .iter()
            .map(|t| Urn::parse(t).map_err(Into::into))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(RunRequest {
            kind,
            revision: None,
            targets,
            no_refresh,
            plan_only: false,
            parallelism: self.parallel,
            retry: self.retry_policy(),
            approvals: Vec::new(),
        })
    }
}

fn approval_grants(
    selectors: &[String],
    plan: &Plan,
    revision: &str,
) -> anyhow::Result<Vec<ApprovalGrant>> {
    let required: Vec<_> = plan.approvals().collect();
    let mut grants = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let approval = required
            .iter()
            .find(|approval| approval.selector() == *selector)
            .with_context(|| {
                format!("approval selector `{selector}` is not required by this plan")
            })?;
        let fingerprint = approval.fingerprint.as_ref().with_context(|| {
            format!("approval `{selector}` is deferred until its dependencies resolve during apply")
        })?;
        grants.push(ApprovalGrant {
            revision: revision.to_string(),
            urn: approval.urn.clone(),
            risk: approval.risk.clone(),
            fingerprint: fingerprint.clone(),
        });
    }
    Ok(grants)
}

fn complete_plan_approvals(
    request: &mut RunRequest,
    plan: &Plan,
    revision: &str,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    for approval in plan.approvals() {
        let Some(fingerprint) = &approval.fingerprint else {
            continue;
        };
        let already_granted = request.approvals.iter().any(|grant| {
            grant.revision == revision
                && grant.urn == approval.urn
                && grant.risk == approval.risk
                && grant.fingerprint == *fingerprint
        });
        if already_granted {
            continue;
        }
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "approval required for `{}`: {}; rerun with `--approve {}`",
            approval.selector(),
            approval.reason,
            approval.selector()
        );
        if !confirm_approval(out, approval)? {
            return Ok(false);
        }
        request.approvals.push(ApprovalGrant {
            revision: revision.to_string(),
            urn: approval.urn.clone(),
            risk: approval.risk.clone(),
            fingerprint: fingerprint.clone(),
        });
    }
    Ok(true)
}

/// Run one CLI command against the daemon-owned stack.
pub async fn run(cli: Cli, registry: Registry) -> anyhow::Result<ExitCode> {
    init_tracing(cli.verbose);
    crate::color::init(crate::color::auto());
    let ctx = cli.load_ctx(registry.clone())?;
    let mut out = std::io::stdout(); // not locked: the event sink writes to stdout too

    // Front-end-only commands never contact the execution daemon.
    match &cli.cmd {
        Cmd::Schema { type_name, json } => {
            let schemas: Vec<_> = registry
                .schemas()
                .into_iter()
                .filter(|s| type_name.as_ref().is_none_or(|t| &s.type_name == t))
                .collect();
            anyhow::ensure!(!schemas.is_empty(), "no such resource type");
            if *json {
                writeln!(out, "{}", serde_json::to_string_pretty(&schemas)?)?;
            } else {
                write!(out, "{}", stubs::describe(&schemas))?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Stubs { lang, out: path } => {
            let text = stubs::generate(*lang, &registry.schemas());
            match path {
                Some(p) => {
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(p, text)?;
                }
                None => write!(out, "{text}")?,
            }
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let client = DaemonClient::new(&cli.daemon)?;

    if matches!(cli.cmd, Cmd::Build) {
        let build = client.retry_build(&cli.stack).await?;
        if build.phase == BuildPhase::Ready {
            writeln!(
                out,
                "ready generation {} in {} ms",
                build.generation,
                build.duration_ms.unwrap_or_default()
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        writeln!(
            out,
            "build {:?}: {}",
            build.phase,
            build.error.as_deref().unwrap_or("build did not complete")
        )?;
        return Ok(ExitCode::from(1));
    }

    if matches!(cli.cmd, Cmd::EmitProgram) {
        let revision = resolve_current(&client, &ctx, &cli.stack).await?;
        writeln!(out, "{}", serde_json::to_string(&revision.program)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    if let Cmd::Graph {
        format: GraphFormat::Dot,
        out: path,
        ..
    } = &cli.cmd
    {
        let revision = resolve_current(&client, &ctx, &cli.stack).await?;
        let graph = crate::graph::Graph::build(&revision.program)?;
        write_text(&mut out, path.as_deref(), &graph.to_dot())?;
        return Ok(ExitCode::SUCCESS);
    }

    if let Cmd::State { cmd } = &cli.cmd {
        let state = client.state(&cli.stack).await?;
        match cmd {
            StateCmd::List => {
                for (urn, e) in &state.resources {
                    writeln!(out, "{urn}\t{}", e.id.as_deref().unwrap_or("-"))?;
                }
            }
            StateCmd::Show { urn } => {
                let urn = Urn::parse(urn)?;
                let e = state
                    .get(&urn)
                    .with_context(|| format!("{urn} not in state"))?;
                writeln!(out, "{}", serde_json::to_string_pretty(e)?)?;
            }
            StateCmd::Rm { urn } => {
                let urn = Urn::parse(urn)?;
                client.forget_state(&cli.stack, &urn.to_string()).await?;
            }
            StateCmd::Export { out: path } => {
                let text = serde_json::to_string_pretty(&state)?;
                match path {
                    Some(p) => std::fs::write(p, text)?,
                    None => writeln!(out, "{text}")?,
                }
            }
            StateCmd::Import { file } => {
                let text = std::fs::read_to_string(file)?;
                let mut imported: State = serde_json::from_str(&text)?;
                imported.stack = cli.stack.clone();
                client.replace_state(&cli.stack, &imported).await?;
                writeln!(out, "imported {} resource(s)", imported.resources.len())?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Cmd::Run { cmd } = &cli.cmd {
        match cmd {
            RunCmd::List => {
                for run in client.runs(&cli.stack).await? {
                    writeln!(
                        out,
                        "{}\t{:?}\t{:?}\tattempt {}",
                        run.run_id, run.request.kind, run.status, run.attempt
                    )?;
                }
            }
            RunCmd::Show { run_id } => {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&client.run(run_id).await?)?
                )?;
            }
            RunCmd::Events { run_id } => {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&client.events(run_id).await?)?
                )?;
            }
            RunCmd::Cancel { run_id } => {
                let run = client.cancel(run_id).await?;
                if run.status == ExecutionStatus::Cancelled {
                    writeln!(out, "cancelled {}", run.run_id)?;
                } else {
                    writeln!(
                        out,
                        "{} remains in recovery_wait: {}",
                        run.run_id,
                        run.error.as_deref().unwrap_or("rollback is incomplete")
                    )?;
                    return Ok(ExitCode::from(1));
                }
            }
            RunCmd::Retry { run_id } => {
                let run = client.retry_now(run_id).await?;
                writeln!(out, "triggered retry for {}", run.run_id)?;
            }
            RunCmd::Approve { run_id, selector } => {
                let run = client.run(run_id).await?;
                let approval = run
                    .pending_approvals
                    .iter()
                    .find(|approval| approval.selector() == *selector)
                    .with_context(|| {
                        format!("approval `{selector}` is not pending on run `{run_id}`")
                    })?;
                let fingerprint = approval
                    .fingerprint
                    .as_ref()
                    .context("pending approval has no resolved fingerprint")?;
                client
                    .approve(
                        run_id,
                        &ApprovalGrant {
                            revision: run.revision.clone(),
                            urn: approval.urn.clone(),
                            risk: approval.risk.clone(),
                            fingerprint: fingerprint.clone(),
                        },
                    )
                    .await?;
                writeln!(out, "approved {selector} for run {run_id}")?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Cmd::Lease { cmd } = &cli.cmd {
        match cmd {
            LeaseCmd::Set { duration, until } => {
                let lease = client
                    .set_lease(
                        &cli.stack,
                        &LeaseRequest {
                            duration: *duration,
                            deadline: *until,
                            ..LeaseRequest::default()
                        },
                    )
                    .await?;
                render_lease(&mut out, &lease, false)?;
            }
            LeaseCmd::Show { json } => {
                let lease = client.lease(&cli.stack).await?;
                render_lease(&mut out, &lease, *json)?;
            }
            LeaseCmd::Extend { by } => {
                let lease = client
                    .extend_lease(&cli.stack, &LeaseExtension { by: *by })
                    .await?;
                render_lease(&mut out, &lease, false)?;
            }
            LeaseCmd::Clear => {
                client.clear_lease(&cli.stack).await?;
                writeln!(out, "lease cleared")?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    match &cli.cmd {
        Cmd::Plan { no_refresh } => {
            let revision = resolve_current(&client, &ctx, &cli.stack).await?;
            let mut request = cli.run_request(RunKind::Plan, *no_refresh)?;
            request.revision = Some(revision.revision);
            request.retry.max_attempts = 1;
            let run = execute(&client, &cli.stack, &request, cli.verbose, &mut out).await?;
            if run.status != ExecutionStatus::Succeeded {
                return failed_execution(&mut out, &run);
            }
            let plan: Plan = result_field(&run, "plan")?;
            render::plan(&mut out, &plan, &registry, cli.verbose > 0)?;
            return Ok(if plan.has_changes() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            });
        }
        Cmd::Apply { .. } | Cmd::Destroy { .. } => {
            let (yes, no_refresh, approval_selectors) = match &cli.cmd {
                Cmd::Apply {
                    yes,
                    no_refresh,
                    approve,
                    ..
                } => (*yes, *no_refresh, approve.as_slice()),
                Cmd::Destroy { yes, approve } => (*yes, false, approve.as_slice()),
                _ => unreachable!(),
            };
            if let Cmd::Apply {
                lease: Some(duration),
                ..
            } = &cli.cmd
            {
                let lease = client
                    .set_lease(
                        &cli.stack,
                        &LeaseRequest {
                            duration: Some(*duration),
                            ..LeaseRequest::default()
                        },
                    )
                    .await?;
                render_lease(&mut out, &lease, false)?;
            }
            let (revision, mut request) = match &cli.cmd {
                Cmd::Apply { .. } => {
                    let revision = resolve_current(&client, &ctx, &cli.stack).await?;
                    let mut request = cli.run_request(RunKind::Plan, no_refresh)?;
                    request.revision = Some(revision.revision.clone());
                    (Some(revision.revision), request)
                }
                Cmd::Destroy { .. } => {
                    let mut request = cli.run_request(RunKind::Destroy, false)?;
                    request.plan_only = true;
                    (None, request)
                }
                _ => unreachable!(),
            };
            request.retry.max_attempts = 1;
            let preview = execute(&client, &cli.stack, &request, cli.verbose, &mut out).await?;
            if preview.status != ExecutionStatus::Succeeded {
                return failed_execution(&mut out, &preview);
            }
            let plan: Plan = result_field(&preview, "plan")?;
            render::plan(&mut out, &plan, &registry, cli.verbose > 0)?;
            request.approvals = approval_grants(approval_selectors, &plan, &preview.revision)?;
            if !plan.has_changes() {
                return Ok(ExitCode::SUCCESS);
            }
            if !yes && !confirm(&mut out)? {
                writeln!(out, "aborted")?;
                return Ok(ExitCode::from(1));
            }
            if !complete_plan_approvals(&mut request, &plan, &preview.revision, &mut out)? {
                writeln!(out, "aborted; previewed run was not changed")?;
                return Ok(ExitCode::from(1));
            }
            writeln!(out)?;
            request.kind = match cli.cmd {
                Cmd::Apply { .. } => RunKind::Apply,
                Cmd::Destroy { .. } => RunKind::Destroy,
                _ => unreachable!(),
            };
            request.revision = revision;
            request.plan_only = false;
            request.retry = cli.retry_policy();
            let run = execute_with_approvals(&client, &cli.stack, &request, cli.verbose, &mut out)
                .await?;
            let report: Report = result_field(&run, "report")?;
            render::report(&mut out, &report)?;
            render::outputs(&mut out, &report, &registry)?;
            if run.status == ExecutionStatus::ApprovalWait {
                writeln!(
                    out,
                    "\nRun {} remains durable. Approve it with:",
                    run.run_id
                )?;
                for approval in &run.pending_approvals {
                    writeln!(
                        out,
                        "  ifx run approve {} {}",
                        run.run_id,
                        approval.selector()
                    )?;
                }
                return Ok(ExitCode::from(3));
            }
            if run.status == ExecutionStatus::Succeeded {
                let checks: Vec<HealthRecord> = result_field(&run, "checks").unwrap_or_default();
                if !checks.is_empty() {
                    writeln!(out)?;
                    writeln!(out, "{}", "Checks:".bold())?;
                    render_health(&mut out, &checks)?;
                }
            }
            if run.status != ExecutionStatus::Succeeded
                && let Some(error) = &run.error
            {
                writeln!(out, "\n{} {error}", "error:".red().bold())?;
            }
            return Ok(if run.status == ExecutionStatus::Succeeded {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
        Cmd::Refresh => {
            let revision = resolve_current(&client, &ctx, &cli.stack).await?;
            let mut request = cli.run_request(RunKind::Refresh, false)?;
            request.revision = Some(revision.revision);
            let run = execute(&client, &cli.stack, &request, cli.verbose, &mut out).await?;
            if run.status != ExecutionStatus::Succeeded {
                return failed_execution(&mut out, &run);
            }
            let changed: Vec<Urn> = result_field(&run, "changed")?;
            writeln!(out, "refreshed; {} resource(s) changed", changed.len())?;
            for u in changed {
                writeln!(out, "  {u}")?;
            }
        }
        Cmd::Check { json } => {
            let request = cli.run_request(RunKind::Check, false)?;
            let run = execute(&client, &cli.stack, &request, cli.verbose, &mut out).await?;
            if run.status != ExecutionStatus::Succeeded {
                return failed_execution(&mut out, &run);
            }
            let checks: Vec<HealthRecord> = result_field(&run, "checks")?;
            if *json {
                writeln!(out, "{}", serde_json::to_string_pretty(&checks)?)?;
            } else if checks.is_empty() {
                writeln!(out, "no checks in stack `{}`", cli.stack)?;
            } else {
                render_health(&mut out, &checks)?;
            }
            let worst = checks.iter().map(|c| c.status).max();
            return Ok(exit_for_health(worst));
        }
        Cmd::Status { json } => {
            let status = client.status(&cli.stack).await?;
            render_status(&mut out, &status, *json)?;
            return Ok(exit_for_health(status.overall));
        }
        Cmd::Outputs => {
            let state = client.state(&cli.stack).await?;
            let mut report = crate::engine::Report::default();
            for (urn, e) in &state.resources {
                report.outputs.insert(urn.clone(), e.outputs.clone());
            }
            render::outputs(&mut out, &report, &registry)?;
        }
        Cmd::Graph {
            format,
            out: path,
            no_refresh,
        } => {
            let _revision = resolve_current(&client, &ctx, &cli.stack).await?;
            let snapshot = client.topology(&cli.stack, *no_refresh).await?;
            let text = match format {
                GraphFormat::Json => serde_json::to_string_pretty(&snapshot)?,
                GraphFormat::Html => crate::explorer::offline_html(&snapshot)?,
                GraphFormat::Dot => unreachable!("DOT graph exits before opening the store"),
            };
            write_text(&mut out, path.as_deref(), &text)?;
        }
        Cmd::Build
        | Cmd::Schema { .. }
        | Cmd::Stubs { .. }
        | Cmd::State { .. }
        | Cmd::Run { .. }
        | Cmd::Lease { .. }
        | Cmd::EmitProgram => {
            unreachable!()
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn resolve_current(
    client: &DaemonClient,
    ctx: &LoadCtx,
    stack: &str,
) -> anyhow::Result<crate::ProgramRevision> {
    client
        .resolve(
            stack,
            &ProgramResolveRequest {
                config: Some(ctx.config.clone()),
            },
        )
        .await
}

async fn execute(
    client: &DaemonClient,
    stack: &str,
    request: &RunRequest,
    verbose: u8,
    out: &mut impl Write,
) -> anyhow::Result<ExecutionRun> {
    execute_inner(client, stack, request, verbose, out, false).await
}

async fn execute_with_approvals(
    client: &DaemonClient,
    stack: &str,
    request: &RunRequest,
    verbose: u8,
    out: &mut impl Write,
) -> anyhow::Result<ExecutionRun> {
    execute_inner(client, stack, request, verbose, out, true).await
}

async fn execute_inner(
    client: &DaemonClient,
    stack: &str,
    request: &RunRequest,
    verbose: u8,
    out: &mut impl Write,
    prompt_for_approvals: bool,
) -> anyhow::Result<ExecutionRun> {
    let started = client.start_run(stack, request).await?;
    let mut seen = 0;
    loop {
        let events = client.events(&started.run_id).await?;
        for event in events.iter().skip(seen) {
            render_execution_event(out, event, verbose)?;
        }
        seen = events.len();
        let run = client.run(&started.run_id).await?;
        if run.status.terminal() {
            let events = client.events(&started.run_id).await?;
            for event in events.iter().skip(seen) {
                render_execution_event(out, event, verbose)?;
            }
            return Ok(run);
        }
        if run.status == ExecutionStatus::RecoveryWait {
            return Ok(run);
        }
        if run.status == ExecutionStatus::ApprovalWait {
            if !prompt_for_approvals || !std::io::stdin().is_terminal() {
                return Ok(run);
            }
            for approval in &run.pending_approvals {
                let Some(fingerprint) = &approval.fingerprint else {
                    anyhow::bail!(
                        "{}: daemon requested approval before resolving its fingerprint",
                        approval.selector()
                    );
                };
                let already_granted = run.request.approvals.iter().any(|grant| {
                    grant.revision == run.revision
                        && grant.urn == approval.urn
                        && grant.risk == approval.risk
                        && grant.fingerprint == *fingerprint
                });
                if already_granted {
                    continue;
                }
                if !confirm_approval(out, approval)? {
                    return Ok(run);
                }
                client
                    .approve(
                        &run.run_id,
                        &ApprovalGrant {
                            revision: run.revision.clone(),
                            urn: approval.urn.clone(),
                            risk: approval.risk.clone(),
                            fingerprint: fingerprint.clone(),
                        },
                    )
                    .await?;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn render_execution_event(
    out: &mut impl Write,
    event: &ExecutionEvent,
    verbose: u8,
) -> std::io::Result<()> {
    match event.kind.as_str() {
        "resource_started" => writeln!(
            out,
            "  {} {}...",
            event.action.as_deref().unwrap_or("run").cyan(),
            event
                .urn
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        ),
        "resource_finished" => writeln!(
            out,
            "  {} {}",
            "done".green(),
            event
                .urn
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        ),
        "resource_failed" | "failed" => writeln!(
            out,
            "  {} {} {}",
            "failed".red(),
            event
                .urn
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            event.message
        ),
        "retry_scheduled" | "retry_triggered" | "recovery_retry" => {
            writeln!(out, "  {} {}", "retry:".yellow(), event.message)
        }
        "recovery_wait" | "recovery_rollback_failed" => {
            writeln!(out, "  {} {}", "recovery:".yellow(), event.message)
        }
        "recovery_aborted" => writeln!(out, "  {} {}", "rollback:".yellow(), event.message),
        "approval_required" => writeln!(
            out,
            "  {} {} {}",
            "approval required:".yellow().bold(),
            event
                .urn
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            event.action.as_deref().unwrap_or_default()
        ),
        "approval_granted" | "approval_accepted" | "approval_resumed" => {
            writeln!(out, "  {} {}", "approval:".green(), event.message)
        }
        _ if verbose > 0 => writeln!(out, "  {} {}", event.kind.dimmed(), event.message),
        _ => Ok(()),
    }
}

fn result_field<T: serde::de::DeserializeOwned>(
    run: &ExecutionRun,
    field: &str,
) -> anyhow::Result<T> {
    let value = run
        .result
        .get(field)
        .cloned()
        .with_context(|| format!("run {} returned no `{field}` result", run.run_id))?;
    serde_json::from_value(value)
        .with_context(|| format!("decoding `{field}` result from run {}", run.run_id))
}

fn failed_execution(out: &mut impl Write, run: &ExecutionRun) -> anyhow::Result<ExitCode> {
    writeln!(
        out,
        "{} run {} {:?}: {}",
        "error:".red().bold(),
        run.run_id,
        run.status,
        run.error.as_deref().unwrap_or("execution did not succeed")
    )?;
    Ok(ExitCode::from(1))
}

fn write_text(out: &mut impl Write, path: Option<&Path>, text: &str) -> anyhow::Result<()> {
    match path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)?;
        }
        None => write!(out, "{text}")?,
    }
    Ok(())
}

fn exit_for_health(h: Option<crate::store::Health>) -> ExitCode {
    use crate::store::Health;
    match h {
        None | Some(Health::Healthy) => ExitCode::SUCCESS,
        Some(Health::Degraded) | Some(Health::Drifted) | Some(Health::Unknown) => ExitCode::from(2),
        Some(Health::Unhealthy) => ExitCode::from(1),
    }
}

pub fn render_health(
    out: &mut impl Write,
    records: &[crate::store::HealthRecord],
) -> std::io::Result<()> {
    use crate::store::Health;
    for r in records {
        let line = format!(
            "  {} {}  {}{}",
            r.status.symbol(),
            r.urn,
            r.message,
            r.latency_ms
                .map(|l| format!(" ({l} ms)"))
                .unwrap_or_default()
        );
        match r.status {
            Health::Healthy => writeln!(out, "{}", line.green())?,
            Health::Degraded | Health::Drifted => writeln!(out, "{}", line.yellow())?,
            Health::Unhealthy => writeln!(out, "{}", line.red())?,
            Health::Unknown => writeln!(out, "{}", line.dimmed())?,
        }
    }
    Ok(())
}

fn render_lease(out: &mut impl Write, lease: &StackLease, json: bool) -> anyhow::Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(lease)?)?;
        return Ok(());
    }
    writeln!(out, "{}", lease_line(lease))?;
    for change in &lease.history {
        writeln!(
            out,
            "  {}  {} -> {}",
            change.at.format("%Y-%m-%d %H:%M:%S"),
            change.from.format("%H:%M:%S"),
            change.to.format("%H:%M:%S")
        )?;
    }
    if let Some(fired) = &lease.fired {
        writeln!(
            out,
            "  destroy {} queued at {}",
            fired.run_id,
            fired.at.format("%H:%M:%S")
        )?;
    }
    Ok(())
}

fn lease_line(lease: &StackLease) -> String {
    let now = Utc::now();
    let deadline = lease.deadline.format("%Y-%m-%d %H:%M:%S UTC");
    if lease.expired(now) {
        return format!("{} {deadline}  {}", "lease".bold(), "expired".red());
    }
    let remaining = Duration::from_secs(lease.remaining(now).as_secs());
    format!(
        "{} {deadline}  {} remaining",
        "lease".bold(),
        humantime_serde::re::humantime::format_duration(remaining)
    )
}

fn render_status(out: &mut impl Write, s: &monitor::StackStatus, json: bool) -> anyhow::Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(s)?)?;
        return Ok(());
    }
    let overall = s
        .overall
        .map(|h| format!("{} {:?}", h.symbol(), h).to_lowercase())
        .unwrap_or_else(|| "no observations yet".into());
    writeln!(
        out,
        "{} {}  {} resource(s), {} check(s)  overall: {}",
        "stack".bold(),
        s.stack.bold(),
        s.resources,
        s.checks,
        overall
    )?;
    if let Some(build) = &s.build {
        writeln!(
            out,
            "build {:?}  generation {}  {} ms{}",
            build.phase,
            build.generation,
            build.duration_ms.unwrap_or_default(),
            build
                .error
                .as_deref()
                .map(|error| format!("  {error}"))
                .unwrap_or_default()
        )?;
    }
    if let Some(lease) = &s.lease {
        writeln!(out, "{}", lease_line(lease))?;
    }
    if !s.health.is_empty() {
        writeln!(out)?;
        render_health(out, &s.health)?;
    }
    if !s.runs.is_empty() {
        writeln!(out)?;
        writeln!(out, "{}", "recent runs:".bold())?;
        for r in &s.runs {
            let status = match r.ok {
                Some(true) => "ok".green().to_string(),
                Some(false) => "failed".red().to_string(),
                None => "running".yellow().to_string(),
            };
            writeln!(
                out,
                "  {}  {:<8} {}  {}",
                r.started_at.format("%Y-%m-%d %H:%M:%S"),
                format!("{:?}", r.kind).to_lowercase(),
                status,
                r.error.as_deref().unwrap_or(&r.summary.to_string()),
            )?;
        }
    }
    Ok(())
}

fn confirm(out: &mut impl Write) -> anyhow::Result<bool> {
    write!(out, "\nApply these changes? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn confirm_approval(
    out: &mut impl Write,
    approval: &crate::ApprovalRequirement,
) -> anyhow::Result<bool> {
    writeln!(out, "\nRisk approval required: {}", approval.selector())?;
    writeln!(out, "  {}", approval.reason)?;
    writeln!(
        out,
        "  fingerprint: {}",
        approval.fingerprint.as_deref().unwrap_or("deferred")
    )?;
    write!(out, "Approve this exact operation? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub fn control_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    // SSH control paths must be short; prefer $XDG_RUNTIME_DIR.
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(".ifx"));
    let p = base.join("ifx-ssh");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "ifx=info,ifxd=info",
        1 => "ifx=debug,ifxd=debug",
        _ => "ifx=trace,ifxd=trace",
    };
    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_defaults_to_dot_without_refreshing_state() {
        let cli = Cli::try_parse_from(["ifx", "graph"]).unwrap();
        let Cmd::Graph {
            format,
            out,
            no_refresh,
        } = cli.cmd
        else {
            panic!("expected graph command");
        };
        assert_eq!(format, GraphFormat::Dot);
        assert!(out.is_none());
        assert!(!no_refresh);
    }

    #[test]
    fn graph_accepts_an_offline_html_destination() {
        let cli = Cli::try_parse_from([
            "ifx",
            "graph",
            "--format",
            "html",
            "--out",
            "graph.html",
            "--no-refresh",
        ])
        .unwrap();
        let Cmd::Graph {
            format,
            out,
            no_refresh,
        } = cli.cmd
        else {
            panic!("expected graph command");
        };
        assert_eq!(format, GraphFormat::Html);
        assert_eq!(out.as_deref(), Some(Path::new("graph.html")));
        assert!(no_refresh);
    }

    #[test]
    fn apply_accepts_repeatable_exact_approval_selectors() {
        let cli = Cli::try_parse_from([
            "ifx",
            "apply",
            "--approve",
            "qemu.instance:node/restart",
            "--approve",
            "qemu.volume:data/shrink",
        ])
        .unwrap();
        let Cmd::Apply { approve, .. } = cli.cmd else {
            panic!("expected apply command");
        };
        assert_eq!(
            approve,
            ["qemu.instance:node/restart", "qemu.volume:data/shrink"]
        );
    }

    #[test]
    fn run_approve_accepts_a_pending_selector() {
        let cli = Cli::try_parse_from([
            "ifx",
            "run",
            "approve",
            "run-123",
            "qemu.instance:node/restart",
        ])
        .unwrap();
        let Cmd::Run {
            cmd: RunCmd::Approve { run_id, selector },
        } = cli.cmd
        else {
            panic!("expected run approve command");
        };
        assert_eq!(run_id, "run-123");
        assert_eq!(selector, "qemu.instance:node/restart");
    }

    #[test]
    fn manifest_overrides_are_relative_to_the_stack_root() {
        let dir = tempfile::tempdir().unwrap();
        let context =
            LoadCtx::from_dir(dir.path(), Some(PathBuf::from("stack.toml")), "test", &[]).unwrap();
        assert_eq!(context.file, Some(dir.path().join("stack.toml")));
    }
}
