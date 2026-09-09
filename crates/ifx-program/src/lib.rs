//! Lightweight Rust authoring surface for IFX programs.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::ExitCode;

pub mod generated;
pub mod model;
pub mod schema;
pub mod stack;

pub use generated::check;
#[cfg(feature = "host")]
pub use generated::host;
#[cfg(feature = "linode")]
pub use generated::linode;
pub use generated::memory;
#[cfg(feature = "qemu")]
pub use generated::qemu;

pub mod providers {
    pub use crate::check;
    #[cfg(feature = "host")]
    pub use crate::host;
    #[cfg(feature = "linode")]
    pub use crate::linode;
    pub use crate::memory;
    #[cfg(feature = "qemu")]
    pub use crate::qemu;
}

pub use model::{Connection, Program, ResourceDecl, Urn};
pub use stack::{ConcatPart, Declare, Handle, Input, IntoConcatPart, ResourceType, Stack, secret};

#[cfg(feature = "linode")]
impl linode::Rule {
    /// Accept `protocol` traffic on `ports` from anywhere (IPv4 and IPv6).
    pub fn allow(
        label: impl Into<Input<String>>,
        protocol: linode::RuleProtocol,
        ports: impl Into<Input<String>>,
    ) -> Self {
        linode::Rule::builder()
            .label(label)
            .action(linode::RuleAction::Accept)
            .protocol(protocol)
            .ports(ports)
            .addresses(
                linode::Addresses::builder()
                    .ipv4(["0.0.0.0/0"])
                    .ipv6(["::/0"])
                    .build(),
            )
            .build()
    }
}

/// Runtime values supplied by `ifxd` when it executes a compiled stack emitter.
#[derive(Clone, Debug)]
pub struct Context {
    stack: String,
    config: BTreeMap<String, serde_json::Value>,
}

impl Context {
    pub fn from_environment() -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let stack = std::env::var("IFX_STACK").unwrap_or_else(|_| "default".to_string());
        let config = match std::env::var("IFX_CONFIG") {
            Ok(config) => serde_json::from_str(&config).context("parsing IFX_CONFIG")?,
            Err(std::env::VarError::NotPresent) => BTreeMap::new(),
            Err(error) => return Err(anyhow::Error::from(error).context("reading IFX_CONFIG")),
        };
        Ok(Self { stack, config })
    }

    pub fn stack(&self) -> &str {
        &self.stack
    }

    /// Decode one required stack configuration value.
    pub fn config<T>(&self, key: &str) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        use anyhow::Context as _;

        let value = self
            .config
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing required config `{key}`"))?;
        serde_json::from_value(value).with_context(|| format!("decoding config `{key}`"))
    }

    /// Decode one optional stack configuration value.
    pub fn config_optional<T>(&self, key: &str) -> anyhow::Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        use anyhow::Context as _;

        self.config
            .get(key)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .with_context(|| format!("decoding config `{key}`"))
    }
}

/// Build one Program and write its compact JSON representation to stdout.
pub fn emit_program<F>(build: F) -> ExitCode
where
    F: FnOnce(&mut Stack, &Context) -> anyhow::Result<()>,
{
    match build_and_write(build) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn build_and_write<F>(build: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Stack, &Context) -> anyhow::Result<()>,
{
    let context = Context::from_environment()?;
    let mut stack = Stack::new();
    build(&mut stack, &context)?;

    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &stack.into_program())?;
    writeln!(output)?;
    Ok(())
}
