//! `ifxd` — owns execution and observation for one or more stacks, persists immutable
//! revisions and durable runs, and exposes the control plane over HTTP.

mod api;
mod broker;
mod config;
mod watch;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use crate::config::Config;

#[derive(Parser, Debug)]
#[command(
    name = "ifxd",
    version,
    about = "ifx execution and observation daemon."
)]
struct Args {
    /// Config file (TOML). Flags override it.
    #[arg(short, long, env = "IFXD_CONFIG")]
    config: Option<PathBuf>,
    /// Store endpoint owned by ifxd (ws://host:port for a SurrealDB server, or
    /// surrealkv://path for an embedded store).
    #[arg(long, env = "IFX_DB")]
    db: Option<String>,
    /// HTTP listen address.
    #[arg(long, env = "IFXD_LISTEN")]
    listen: Option<String>,
    /// Bearer token required by the API. Prefer the IFXD_TOKEN environment variable.
    #[arg(long, env = "IFXD_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Stack directory to watch, as `dir` or `dir=stackname`. Repeatable.
    #[arg(long = "stack", value_name = "DIR[=NAME]")]
    stacks: Vec<String>,
    /// Default interval between health-check rounds (e.g. 30s, 5m).
    #[arg(long, value_name = "DURATION")]
    check_interval: Option<String>,
    /// Default interval between drift-detection rounds.
    #[arg(long, value_name = "DURATION")]
    drift_interval: Option<String>,
    /// Refuse to apply any stack that has no lease.
    #[arg(long)]
    require_lease: bool,
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    ifx::cli::init_tracing(args.verbose);
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let mut cfg = match &args.config {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    cfg.apply_args(&args)?;
    anyhow::ensure!(
        !cfg.stacks.is_empty() || !cfg.brokers.is_empty(),
        "configure local stacks or broker targets"
    );
    if !cfg.brokers.is_empty() {
        anyhow::ensure!(
            cfg.token.is_none(),
            "broker mode uses scoped grants, not IFXD_TOKEN or --token"
        );
        anyhow::ensure!(
            cfg.stacks.is_empty(),
            "broker mode cannot compile or execute local stacks"
        );
        let address: std::net::SocketAddr = cfg.listen.parse()?;
        anyhow::ensure!(
            address.ip().is_loopback(),
            "broker mode must listen on loopback behind a trusted TLS endpoint"
        );
        let app = broker::router(broker::Broker::load(&cfg.brokers)?);
        let listener = tokio::net::TcpListener::bind(address).await?;
        tracing::info!(listen = %address, "broker listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
        return Ok(());
    }

    let registry = ifx::Registry::builtin();
    // One store handle per project (database), resolved exactly as `ifx -C dir` does:
    // `ifx.toml` `name`, else the directory basename.
    let mut stores: BTreeMap<String, Arc<dyn ifx::Store>> = BTreeMap::new();
    let mut watchers = Vec::new();
    for s in &cfg.stacks {
        let mut ctx = ifx::LoadCtx::from_dir(&s.dir, s.file.clone(), &s.name, &[])?;
        let project = cfg.project.clone().unwrap_or_else(|| ctx.project.clone());
        ctx.project.clone_from(&project);
        let store = match stores.get(&project) {
            Some(st) => st.clone(),
            None => {
                let st = ifx::store::open(&cfg.db, "ifx", &project).await?;
                tracing::info!(db = %st.describe(), project, "store opened");
                stores.insert(project.clone(), st.clone());
                st
            }
        };
        let w = Arc::new(watch::Watcher::start(ctx, s.clone(), registry.clone(), store).await?);
        w.spawn(&cfg).await?;
        tracing::info!(stack = %s.name, project, dir = %s.dir.display(), "watching");
        watchers.push(w);
    }
    let app = api::router(api::AppState {
        registry,
        watchers,
        token: cfg.token.map(Arc::<str>::from),
    });
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!(listen = %cfg.listen, "api listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
