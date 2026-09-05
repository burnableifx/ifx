use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::Args;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Store endpoint owned by ifxd. Use a `ws://` server for a shared SurrealDB or an
    /// embedded `surrealkv://` store when one daemon process owns the deployment.
    #[serde(default = "default_db")]
    pub db: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Optional bearer token required by every `/api` endpoint. Prefer `IFXD_TOKEN`
    /// over storing this value in the config file.
    #[serde(default)]
    pub token: Option<String>,
    /// Force one database name for every stack; by default each stack uses its own
    /// project (`ifx.toml` `name`, else the directory basename), exactly like `ifx`.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default = "default_check_interval", with = "humantime_serde")]
    pub check_interval: Duration,
    #[serde(default = "default_drift_interval", with = "humantime_serde")]
    pub drift_interval: Duration,
    /// Refuse to apply any stack that has no lease. Applies to every stack; a stack may
    /// also opt in on its own.
    #[serde(default)]
    pub require_lease: bool,
    #[serde(default)]
    pub stacks: Vec<StackConfig>,
    /// Credential-holding broker mode. Mutually exclusive with local stack execution.
    #[serde(default)]
    pub brokers: Vec<crate::broker::TargetConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackConfig {
    pub dir: PathBuf,
    #[serde(default = "default_stack_name")]
    pub name: String,
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default, with = "humantime_serde")]
    pub check_interval: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub drift_interval: Option<Duration>,
    /// Disable drift detection for this stack (checks still run).
    #[serde(default)]
    pub no_drift: bool,
    /// Refuse to apply this stack without a lease.
    #[serde(default)]
    pub require_lease: bool,
}

fn default_db() -> String {
    "ws://127.0.0.1:8000".into()
}
fn default_listen() -> String {
    "127.0.0.1:7433".into()
}
fn default_stack_name() -> String {
    "default".into()
}
fn default_check_interval() -> Duration {
    Duration::from_secs(30)
}
fn default_drift_interval() -> Duration {
    Duration::from_secs(300)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db: default_db(),
            listen: default_listen(),
            token: None,
            project: None,
            check_interval: default_check_interval(),
            drift_interval: default_drift_interval(),
            require_lease: false,
            stacks: Vec::new(),
            brokers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        for s in &mut cfg.stacks {
            if s.dir.is_relative() {
                s.dir = base.join(&s.dir);
            }
        }
        for target in &mut cfg.brokers {
            if target.token_file.is_relative() {
                target.token_file = base.join(&target.token_file);
            }
        }
        Ok(cfg)
    }

    pub fn apply_args(&mut self, args: &Args) -> anyhow::Result<()> {
        if let Some(db) = &args.db {
            self.db = db.clone();
        }
        if let Some(l) = &args.listen {
            self.listen = l.clone();
        }
        if let Some(token) = &args.token {
            self.token = Some(token.clone());
        }
        if let Some(d) = &args.check_interval {
            self.check_interval = parse_duration(d)?;
        }
        if let Some(d) = &args.drift_interval {
            self.drift_interval = parse_duration(d)?;
        }
        if args.require_lease {
            self.require_lease = true;
        }
        for spec in &args.stacks {
            let (dir, name) = match spec.split_once('=') {
                Some((d, n)) => (d, n.to_string()),
                None => (spec.as_str(), default_stack_name()),
            };
            self.stacks.push(StackConfig {
                dir: PathBuf::from(dir),
                name,
                file: None,
                check_interval: None,
                drift_interval: None,
                no_drift: false,
                require_lease: false,
            });
        }
        for stack in &mut self.stacks {
            stack.require_lease |= self.require_lease;
        }
        if let Some(token) = &self.token {
            anyhow::ensure!(!token.is_empty(), "ifxd bearer token cannot be empty");
            anyhow::ensure!(
                !token.chars().any(char::is_whitespace),
                "ifxd bearer token cannot contain whitespace"
            );
        }
        Ok(())
    }
}

fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("bad duration `{s}`"))?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hours" => 3600,
        "d" | "days" => 86400,
        u => anyhow::bail!("bad duration unit `{u}` in `{s}`"),
    };
    Ok(Duration::from_secs(n * mult))
}
