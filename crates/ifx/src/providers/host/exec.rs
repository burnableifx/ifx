//! `host.exec`: run a shell command, guarded by `creates`/`unless`/`only_if`, re-run on
//! command change or trigger.

use anyhow::{anyhow, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{bool_field, esc, now_rfc3339, on_field, req_str, str_field, transport};
use crate::provider::{Actual, Applied, Ctx, Diff, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{Cmd, Transport};

/// Props marker set when a guard decided the command need not run.
const GUARD_KEY: &str = "guard_satisfied";

pub struct ExecHandler;

fn valid_env_name(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Wrap `body` with the resource's `cwd` and `env` so guards and the command see the
/// same environment. `env` is exported inside the script so it survives `sudo`.
pub(crate) fn script(inputs: &Value, body: &str) -> Result<String> {
    let mut s = String::new();
    if let Some(cwd) = str_field(inputs, "cwd") {
        s.push_str(&format!("cd {} || exit 1\n", esc(cwd)));
    }
    if let Some(env) = inputs.get("env").and_then(Value::as_object) {
        let mut vars: Vec<(&String, &Value)> = env.iter().collect();
        vars.sort_by_key(|(k, _)| *k);
        for (k, v) in vars {
            ensure!(valid_env_name(k), "invalid environment variable name `{k}`");
            let v = v
                .as_str()
                .ok_or_else(|| anyhow!("env `{k}` must be a string"))?;
            s.push_str(&format!("export {k}={}\n", esc(v)));
        }
    }
    s.push_str(body);
    Ok(s)
}

fn cmd(inputs: &Value, body: &str) -> Result<Cmd> {
    let mut c = Cmd::sh(script(inputs, body)?);
    c.privileged = bool_field(inputs, "privileged").unwrap_or(false);
    Ok(c)
}

/// `None` when no guard is configured; otherwise whether any guard says "skip".
async fn guard_satisfied(t: &dyn Transport, inputs: &Value) -> Result<Option<bool>> {
    let creates = str_field(inputs, "creates");
    let unless = str_field(inputs, "unless");
    let only_if = str_field(inputs, "only_if");
    if creates.is_none() && unless.is_none() && only_if.is_none() {
        return Ok(None);
    }
    if let Some(p) = creates
        && t.exec(&cmd(inputs, &format!("test -e {}", esc(p)))?)
            .await?
            .success()
    {
        return Ok(Some(true));
    }
    if let Some(u) = unless
        && t.exec(&cmd(inputs, u)?).await?.success()
    {
        return Ok(Some(true));
    }
    if let Some(o) = only_if
        && !t.exec(&cmd(inputs, o)?).await?.success()
    {
        return Ok(Some(true));
    }
    Ok(Some(false))
}

/// Outputs reported when the command did not run in this observation.
fn idle_outputs(id: Option<&str>) -> Value {
    json!({ "stdout": null, "stderr": null, "status": null, "ran_at": id })
}

async fn run(t: &dyn Transport, inputs: &Value) -> Result<Applied> {
    let command = req_str(inputs, "command")?;
    let out = t.exec(&cmd(inputs, command)?).await?;
    if !out.success() {
        let stderr = out.stderr_str();
        let stdout = out.stdout_str();
        let mut msg = format!("command exited {}", out.status);
        if !stderr.trim().is_empty() {
            msg.push_str(&format!(": {}", stderr.trim()));
        }
        if !stdout.trim().is_empty() {
            msg.push_str(&format!("\nstdout: {}", stdout.trim()));
        }
        bail!(msg);
    }
    let ran_at = now_rfc3339();
    Ok(Applied {
        id: Some(ran_at.clone()),
        outputs: json!({
            "stdout": out.stdout_str(),
            "stderr": out.stderr_str(),
            "status": out.status,
            "ran_at": ran_at,
        }),
    })
}

#[async_trait]
impl Handler for ExecHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "host.exec",
            "Run a shell command on a host. Without guards it runs once and again whenever its \
             inputs change or a trigger fires; with guards it runs whenever they say the work \
             is still needed.",
        )
        .input(on_field())
        .input(
            field("command", FieldType::String)
                .required()
                .doc("Shell script run with `sh -c`; a non-zero exit fails the apply."),
        )
        .input(
            field("creates", FieldType::String).doc(
                "Path on the host; the command is skipped while it exists (relative to `cwd`).",
            ),
        )
        .input(field("unless", FieldType::String).doc(
            "Shell command evaluated on every plan; the main command is skipped when it exits 0.",
        ))
        .input(field("only_if", FieldType::String).doc(
            "Shell command evaluated on every plan; the main command runs only when it exits 0.",
        ))
        .input(
            field("cwd", FieldType::String)
                .doc("Working directory for the command and its guards."),
        )
        .input(
            field("env", FieldType::map(FieldType::String))
                .doc("Environment variables exported to the command and its guards."),
        )
        .input(field("privileged", FieldType::Bool).default(false).doc(
            "Run the command and its guards through `sudo -n` when the connection allows sudo.",
        ))
        .output(field("stdout", FieldType::String).doc(
            "Standard output of the last run in this apply; null when the command did not run.",
        ))
        .output(field("stderr", FieldType::String).doc(
            "Standard error of the last run in this apply; null when the command did not run.",
        ))
        .output(
            field("status", FieldType::Int)
                .doc("Exit status of the last run (always 0, since failures abort the apply)."),
        )
        .output(
            field("ran_at", FieldType::String)
                .doc("RFC 3339 UTC timestamp of the most recent run; null if it has never run."),
        )
    }

    async fn read(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<Option<Actual>> {
        let t = transport(cx, inputs).await?;
        let id_s = id.map(String::from);
        match guard_satisfied(&*t, inputs).await? {
            Some(true) => {
                let mut props = inputs.clone();
                props[GUARD_KEY] = json!(true);
                Ok(Some(Actual {
                    id: id_s,
                    props,
                    outputs: idle_outputs(id),
                }))
            }
            Some(false) => Ok(None),
            None if id.is_some() => Ok(Some(Actual {
                id: id_s,
                props: inputs.clone(),
                outputs: idle_outputs(id),
            })),
            None => Ok(None),
        }
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        if actual.props.get(GUARD_KEY).and_then(Value::as_bool) == Some(true) {
            // A satisfied guard wins over input changes, except moving to another host.
            let only_on = json!({ "on": actual.props.get("on").cloned().unwrap_or(Value::Null) });
            return Ok(Diff::generic(desired, &only_on, &["on"]));
        }
        Ok(Diff::generic(desired, &actual.props, &["on"]))
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        run(&*t, inputs).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        run(&*t, inputs).await
    }

    async fn delete(&self, _cx: &Ctx<'_>, _id: Option<&str>, _inputs: &Value) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_wraps_cwd_and_env() {
        let inputs = json!({"cwd": "/tmp/x y", "env": {"FOO": "a'b", "BAR": "1"}});
        let s = script(&inputs, "echo hi").unwrap();
        assert_eq!(
            s,
            "cd '/tmp/x y' || exit 1\nexport BAR=1\nexport FOO='a'\\''b'\necho hi"
        );
        assert!(script(&json!({"env": {"bad-name": "x"}}), "true").is_err());
        assert!(script(&json!({"env": {"N": 1}}), "true").is_err());
        assert_eq!(script(&json!({}), "true").unwrap(), "true");
    }

    #[test]
    fn env_names() {
        assert!(valid_env_name("_A1"));
        assert!(!valid_env_name("1A"));
        assert!(!valid_env_name(""));
        assert!(!valid_env_name("A-B"));
    }

    #[test]
    fn guard_wins_over_command_change() {
        let h = ExecHandler;
        let on = json!({"kind": "local", "sudo": false});
        let actual = Actual {
            id: None,
            props: json!({"on": on, "command": "old", GUARD_KEY: true}),
            outputs: Value::Null,
        };
        assert!(
            h.diff(&json!({"on": on, "command": "new"}), &actual)
                .unwrap()
                .is_empty()
        );
        let moved = json!({"on": {"kind": "local", "sudo": true}, "command": "old"});
        assert!(h.diff(&moved, &actual).unwrap().requires_replace());
    }
}
