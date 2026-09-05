//! `host.service`: a systemd unit's enablement and running state, with restart or
//! reload when a trigger fires.

use std::collections::BTreeMap;

use anyhow::bail;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{bool_field, on_field, req_str, str_field, transport};
use crate::provider::{Actual, Applied, Ctx, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{Cmd, Transport};

pub struct ServiceHandler;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitState {
    pub load_state: String,
    pub active_state: String,
    pub unit_file_state: String,
}

/// Parse `systemctl show` output (`Key=Value` per line).
pub fn parse_show(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub fn parse_unit_state(stdout: &str) -> UnitState {
    let mut m = parse_show(stdout);
    UnitState {
        load_state: m.remove("LoadState").unwrap_or_default(),
        active_state: m.remove("ActiveState").unwrap_or_default(),
        unit_file_state: m.remove("UnitFileState").unwrap_or_default(),
    }
}

/// Same truth table as `systemctl is-enabled` exit status.
pub fn is_enabled(unit_file_state: &str) -> bool {
    matches!(
        unit_file_state,
        "enabled" | "enabled-runtime" | "static" | "indirect" | "generated" | "transient" | "alias"
    )
}

pub fn is_active(active_state: &str) -> bool {
    matches!(
        active_state,
        "active" | "reloading" | "activating" | "refreshing"
    )
}

fn run_state(active_state: &str) -> &'static str {
    if is_active(active_state) {
        "running"
    } else {
        "stopped"
    }
}

async fn show(t: &dyn Transport, name: &str) -> Result<Option<UnitState>> {
    let cmd = Cmd::new("systemctl").args([
        "show",
        "-p",
        "LoadState",
        "-p",
        "ActiveState",
        "-p",
        "UnitFileState",
        "--",
        name,
    ]);
    let out = t.exec(&cmd).await?;
    let state = parse_unit_state(&out.stdout_str());
    if state.load_state == "not-found" {
        return Ok(None);
    }
    if !out.success() {
        let err = out.stderr_str();
        if err.contains("not-found") || err.contains("could not be found") {
            return Ok(None);
        }
        bail!(
            "systemctl show {name} failed (exit {}): {}",
            out.status,
            err.trim()
        );
    }
    Ok(Some(state))
}

async fn systemctl(t: &dyn Transport, verb: &str, name: Option<&str>) -> Result<()> {
    let mut cmd = Cmd::new("systemctl").arg(verb).privileged();
    if let Some(n) = name {
        cmd = cmd.arg("--").arg(n);
    }
    t.exec(&cmd)
        .await?
        .ok(&format!("systemctl {verb} {}", name.unwrap_or_default()))?;
    Ok(())
}

fn outputs(u: &UnitState) -> Value {
    json!({ "active_state": u.active_state, "unit_file_state": u.unit_file_state })
}

async fn ensure(t: &dyn Transport, inputs: &Value, triggered: bool) -> Result<Applied> {
    let name = req_str(inputs, "name")?;
    if bool_field(inputs, "daemon_reload").unwrap_or(false) {
        systemctl(t, "daemon-reload", None).await?;
    }
    let Some(u) = show(t, name).await? else {
        bail!("unit `{name}` not found")
    };

    if let Some(want) = bool_field(inputs, "enabled")
        && want != is_enabled(&u.unit_file_state)
    {
        systemctl(t, if want { "enable" } else { "disable" }, Some(name)).await?;
    }

    let active = is_active(&u.active_state);
    let bounce = triggered && bool_field(inputs, "restart_on_trigger").unwrap_or(true);
    let bounce_verb = if bool_field(inputs, "reload").unwrap_or(false) {
        "reload-or-restart"
    } else {
        "restart"
    };
    match str_field(inputs, "state") {
        Some("running") if !active => systemctl(t, "start", Some(name)).await?,
        Some("running") if bounce => systemctl(t, bounce_verb, Some(name)).await?,
        Some("stopped") if active => systemctl(t, "stop", Some(name)).await?,
        None if bounce && active => systemctl(t, bounce_verb, Some(name)).await?,
        _ => {}
    }

    let u = show(t, name).await?.unwrap_or_default();
    Ok(Applied {
        id: None,
        outputs: outputs(&u),
    })
}

#[async_trait]
impl Handler for ServiceHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "host.service",
            "A systemd unit: whether it is enabled at boot and running now. Restarts (or \
             reloads) when a resource listed in `triggers` changes.",
        )
        .input(on_field())
        .input(field("name", FieldType::String).required().replace().doc(
            "Unit name such as \"nginx\" or \"nginx.service\". Changing it replaces the \
             resource (the old unit is stopped/disabled as `delete` would).",
        ))
        .input(field("enabled", FieldType::Bool).doc(
            "Start the unit at boot (`systemctl enable`/`disable`). Left alone when unset.",
        ))
        .input(field("state", FieldType::enumeration(["running", "stopped"])).doc(
            "Whether the unit should be active right now. Left alone when unset.",
        ))
        .input(field("daemon_reload", FieldType::Bool).default(false).doc(
            "Run `systemctl daemon-reload` before acting, for freshly written unit files.",
        ))
        .input(field("restart_on_trigger", FieldType::Bool).default(true).doc(
            "Restart (or reload) the unit when one of the resource's `triggers` changes.",
        ))
        .input(field("reload", FieldType::Bool).default(false).doc(
            "On trigger, use `systemctl reload-or-restart` instead of a full restart.",
        ))
        .output(field("active_state", FieldType::String).doc(
            "systemd ActiveState after the last apply, e.g. \"active\" or \"inactive\".",
        ))
        .output(field("unit_file_state", FieldType::String).doc(
            "systemd UnitFileState after the last apply, e.g. \"enabled\", \"disabled\", \"static\".",
        ))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let t = transport(cx, inputs).await?;
        let name = req_str(inputs, "name")?;
        let Some(u) = show(&*t, name).await? else {
            return Ok(None);
        };
        Ok(Some(Actual {
            id: None,
            props: json!({
                "on": inputs["on"],
                "name": name,
                "enabled": is_enabled(&u.unit_file_state),
                "state": run_state(&u.active_state),
            }),
            outputs: outputs(&u),
        }))
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        ensure(&*t, inputs, cx.triggered).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        ensure(&*t, inputs, cx.triggered).await
    }

    /// Undo what the declaration asked for: stop if it wanted `running`, disable if it
    /// wanted `enabled`. A unit that is no longer present is left alone.
    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        let t = transport(cx, inputs).await?;
        let name = req_str(inputs, "name")?;
        let Some(u) = show(&*t, name).await? else {
            return Ok(());
        };
        if str_field(inputs, "state") == Some("running") && is_active(&u.active_state) {
            systemctl(&*t, "stop", Some(name)).await?;
        }
        if bool_field(inputs, "enabled") == Some(true) && is_enabled(&u.unit_file_state) {
            systemctl(&*t, "disable", Some(name)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_output() {
        let u = parse_unit_state("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n");
        assert_eq!(
            u,
            UnitState {
                load_state: "loaded".into(),
                active_state: "active".into(),
                unit_file_state: "enabled".into()
            }
        );
        let u = parse_unit_state("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n");
        assert_eq!(u.load_state, "not-found");
        assert_eq!(u.unit_file_state, "");
        assert_eq!(parse_show("junk\nA=b=c\n")["A"], "b=c");
    }

    #[test]
    fn enabled_and_active_tables() {
        for s in [
            "enabled",
            "enabled-runtime",
            "static",
            "indirect",
            "generated",
            "alias",
        ] {
            assert!(is_enabled(s), "{s}");
        }
        for s in ["disabled", "masked", "masked-runtime", "linked", "bad", ""] {
            assert!(!is_enabled(s), "{s}");
        }
        assert!(is_active("active"));
        assert!(is_active("activating"));
        assert!(!is_active("inactive"));
        assert!(!is_active("failed"));
        assert_eq!(run_state("deactivating"), "stopped");
    }
}
