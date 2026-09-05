//! `host.user`: a local account, its supplementary groups, shell, home and SSH
//! authorized keys.

use anyhow::bail;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{bool_field, esc, on_field, req_str, sorted_set, str_field, str_list, transport};
use crate::provider::{Actual, Applied, Ctx, Diff, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{Cmd, Transport, WriteOpts};

pub struct UserHandler;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: String,
    pub shell: String,
}

/// One `getent passwd` line: `name:x:uid:gid:gecos:home:shell`.
pub fn parse_passwd(line: &str) -> Option<Passwd> {
    let f: Vec<&str> = line.trim().split(':').collect();
    if f.len() < 7 {
        return None;
    }
    Some(Passwd {
        name: f[0].to_string(),
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        gecos: f[4].to_string(),
        home: f[5].to_string(),
        shell: f[6].to_string(),
    })
}

/// Output of `id -gn NAME; id -Gn NAME`: primary group, then supplementary groups
/// (sorted, primary excluded).
pub fn parse_groups(stdout: &str) -> (String, Vec<String>) {
    let mut lines = stdout.lines();
    let primary = lines.next().unwrap_or("").trim().to_string();
    let all: Vec<String> = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .filter(|g| *g != primary)
        .map(String::from)
        .collect();
    (primary, sorted_set(&all))
}

/// Key lines of an `authorized_keys` file, sorted; blank and comment lines dropped.
pub fn parse_authorized_keys(text: &str) -> Vec<String> {
    let keys: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    sorted_set(&keys)
}

async fn getent(t: &dyn Transport, name: &str) -> Result<Option<Passwd>> {
    let out = t.exec(&Cmd::new("getent").args(["passwd", name])).await?;
    match out.status {
        0 => Ok(parse_passwd(out.stdout_str().lines().next().unwrap_or(""))),
        2 => Ok(None),
        _ => bail!(
            "getent passwd {name} failed (exit {}): {}",
            out.status,
            out.stderr_str().trim()
        ),
    }
}

async fn groups(t: &dyn Transport, name: &str) -> Result<(String, Vec<String>)> {
    let n = esc(name);
    let out = t
        .exec(&Cmd::sh(format!("id -gn {n} && id -Gn {n}")))
        .await?
        .ok("id")?;
    Ok(parse_groups(&out.stdout_str()))
}

fn keys_path(home: &str) -> String {
    format!("{}/.ssh/authorized_keys", home.trim_end_matches('/'))
}

fn outputs(pw: Option<&Passwd>) -> Value {
    match pw {
        Some(pw) => {
            json!({ "uid": pw.uid, "gid": pw.gid, "home": pw.home, "shell": pw.shell, "exists": true })
        }
        None => json!({ "uid": null, "gid": null, "home": null, "shell": null, "exists": false }),
    }
}

async fn write_keys(t: &dyn Transport, pw: &Passwd, primary: &str, keys: &[String]) -> Result<()> {
    let script = format!(
        "h={h}; mkdir -p \"$h/.ssh\" || exit 1; chmod 700 \"$h/.ssh\" || exit 1; chown {og} \"$h/.ssh\"",
        h = esc(&pw.home),
        og = esc(&format!("{}:{primary}", pw.name)),
    );
    t.exec(&Cmd::sh(script).privileged())
        .await?
        .ok("prepare ~/.ssh")?;
    let mut data = keys.join("\n");
    if !data.is_empty() {
        data.push('\n');
    }
    let opts = WriteOpts {
        mode: Some("0600".into()),
        owner: Some(pw.name.clone()),
        group: Some(primary.to_string()),
        privileged: true,
    };
    t.write_file(&keys_path(&pw.home), data.as_bytes(), &opts)
        .await
}

async fn ensure(t: &dyn Transport, inputs: &Value) -> Result<Applied> {
    let name = req_str(inputs, "name")?;
    let existing = getent(t, name).await?;

    if str_field(inputs, "state") == Some("absent") {
        if existing.is_some() {
            t.exec(&Cmd::new("userdel").arg(name).privileged())
                .await?
                .ok(&format!("userdel {name}"))?;
        }
        return Ok(Applied {
            id: None,
            outputs: outputs(None),
        });
    }

    let shell = str_field(inputs, "shell");
    let home = str_field(inputs, "home");
    let system = bool_field(inputs, "system").unwrap_or(false);
    let want_groups = inputs
        .get("groups")
        .map(|_| sorted_set(&str_list(inputs, "groups")));

    match &existing {
        None => {
            let mut cmd = Cmd::new("useradd");
            if system {
                cmd = cmd.arg("-r");
            } else {
                cmd = cmd.arg("-m");
            }
            if let Some(h) = home {
                cmd = cmd.arg("-d").arg(h);
            }
            if let Some(s) = shell {
                cmd = cmd.arg("-s").arg(s);
            }
            if let Some(g) = &want_groups
                && !g.is_empty()
            {
                cmd = cmd.arg("-G").arg(g.join(","));
            }
            t.exec(&cmd.arg(name).privileged())
                .await?
                .ok(&format!("useradd {name}"))?;
        }
        Some(pw) => {
            let mut cmd = Cmd::new("usermod");
            let mut changed = false;
            if let Some(s) = shell
                && s != pw.shell
            {
                cmd = cmd.arg("-s").arg(s);
                changed = true;
            }
            if let Some(h) = home
                && h != pw.home
            {
                cmd = cmd.arg("-m").arg("-d").arg(h);
                changed = true;
            }
            if let Some(g) = &want_groups {
                let (_, current) = groups(t, name).await?;
                if *g != current {
                    cmd = cmd.arg("-G").arg(g.join(","));
                    changed = true;
                }
            }
            if changed {
                t.exec(&cmd.arg(name).privileged())
                    .await?
                    .ok(&format!("usermod {name}"))?;
            }
        }
    }

    let Some(pw) = getent(t, name).await? else {
        bail!("user {name} missing after useradd")
    };
    if inputs.get("authorized_keys").is_some_and(|v| !v.is_null()) {
        let keys = sorted_set(&str_list(inputs, "authorized_keys"));
        let (primary, _) = groups(t, name).await?;
        write_keys(t, &pw, &primary, &keys).await?;
    }
    Ok(Applied {
        id: None,
        outputs: outputs(Some(&pw)),
    })
}

#[async_trait]
impl Handler for UserHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "host.user",
            "A local user account: supplementary groups, shell, home directory and SSH \
             authorized keys. Needs root on the host for any change.",
        )
        .input(on_field())
        .input(field("name", FieldType::String).required().replace().doc(
            "Login name. Changing it replaces the resource (old account deleted, new one created).",
        ))
        .input(field("groups", FieldType::list(FieldType::String)).doc(
            "Exact set of supplementary groups (the primary group is not included). Left alone when unset.",
        ))
        .input(field("shell", FieldType::String).doc(
            "Login shell, e.g. \"/bin/bash\". Left alone when unset.",
        ))
        .input(field("home", FieldType::String).doc(
            "Home directory path; an existing home is moved when this changes. Left alone when unset.",
        ))
        .input(field("system", FieldType::Bool).default(false).doc(
            "Create as a system account (`useradd -r`: low UID, no home directory). Only affects creation.",
        ))
        .input(field("authorized_keys", FieldType::list(FieldType::String)).doc(
            "Public key lines written to `~/.ssh/authorized_keys` (mode 0600, owned by the \
             user), replacing its previous contents. Left alone when unset.",
        ))
        .input(field("state", FieldType::enumeration(["present", "absent"])).default("present").doc(
            "\"present\" ensures the account exists as described; \"absent\" deletes it \
             (the home directory is kept).",
        ))
        .output(field("uid", FieldType::Int).doc("Numeric user id."))
        .output(field("gid", FieldType::Int).doc("Numeric primary group id."))
        .output(field("home", FieldType::String).doc("Home directory as recorded in the passwd database."))
        .output(field("shell", FieldType::String).doc("Login shell as recorded in the passwd database."))
        .output(field("exists", FieldType::Bool).doc("Whether the account existed after the last apply."))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let t = transport(cx, inputs).await?;
        let name = req_str(inputs, "name")?;
        let Some(pw) = getent(&*t, name).await? else {
            if str_field(inputs, "state") == Some("absent") {
                return Ok(Some(Actual {
                    id: None,
                    props: json!({ "on": inputs["on"], "name": name, "state": "absent" }),
                    outputs: outputs(None),
                }));
            }
            return Ok(None);
        };
        let (_, supplementary) = groups(&*t, name).await?;
        let mut props = json!({
            "on": inputs["on"],
            "name": name,
            "state": "present",
            "groups": supplementary,
            "shell": pw.shell,
            "home": pw.home,
        });
        match t.read_file(&keys_path(&pw.home), true).await {
            Ok(data) => {
                let text = data
                    .map(|d| String::from_utf8_lossy(&d).into_owned())
                    .unwrap_or_default();
                props["authorized_keys"] = json!(parse_authorized_keys(&text));
            }
            Err(e) => {
                tracing::warn!(target: "ifx::host", user = name, "cannot read authorized_keys: {e:#}");
            }
        }
        Ok(Some(Actual {
            id: None,
            props,
            outputs: outputs(Some(&pw)),
        }))
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let mut desired = desired.clone();
        for key in ["groups", "authorized_keys"] {
            if desired.get(key).is_some_and(|v| v.is_array()) {
                desired[key] = json!(sorted_set(&str_list(&desired, key)));
            }
        }
        Ok(Diff::generic(&desired, &actual.props, &["on", "name"]))
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        ensure(&*t, inputs).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        ensure(&*t, inputs).await
    }

    /// Delete the account (home directory kept); nothing to do for `absent` declarations.
    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        if str_field(inputs, "state") == Some("absent") {
            return Ok(());
        }
        let t = transport(cx, inputs).await?;
        let name = req_str(inputs, "name")?;
        if getent(&*t, name).await?.is_some() {
            t.exec(&Cmd::new("userdel").arg(name).privileged())
                .await?
                .ok(&format!("userdel {name}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwd_line() {
        let pw = parse_passwd("deploy:x:1001:1001:Deploy User:/home/deploy:/bin/bash\n").unwrap();
        assert_eq!(pw.name, "deploy");
        assert_eq!(pw.uid, 1001);
        assert_eq!(pw.gid, 1001);
        assert_eq!(pw.gecos, "Deploy User");
        assert_eq!(pw.home, "/home/deploy");
        assert_eq!(pw.shell, "/bin/bash");
        assert!(parse_passwd("short:x:1").is_none());
        assert!(parse_passwd("a:x:nope:1:::").is_none());
    }

    #[test]
    fn group_lists() {
        let (primary, supp) = parse_groups("deploy\ndeploy wheel docker adm\n");
        assert_eq!(primary, "deploy");
        assert_eq!(supp, ["adm", "docker", "wheel"]);
        assert_eq!(parse_groups("").1, Vec::<String>::new());
    }

    #[test]
    fn authorized_keys_lines() {
        let text = "# comment\n\nssh-ed25519 BBB b@x\nssh-ed25519 AAA a@x  \n";
        assert_eq!(
            parse_authorized_keys(text),
            ["ssh-ed25519 AAA a@x", "ssh-ed25519 BBB b@x"]
        );
        assert_eq!(keys_path("/home/x/"), "/home/x/.ssh/authorized_keys");
    }

    #[test]
    fn diff_ignores_group_order() {
        let h = UserHandler;
        let on = json!({"kind": "local", "sudo": false});
        let actual = Actual {
            id: None,
            props: json!({"on": on, "name": "u", "state": "present", "groups": ["a", "b"], "shell": "/bin/sh", "home": "/home/u", "authorized_keys": []}),
            outputs: Value::Null,
        };
        let d = h
            .diff(
                &json!({"on": on, "name": "u", "state": "present", "groups": ["b", "a"]}),
                &actual,
            )
            .unwrap();
        assert!(d.is_empty(), "{d:?}");
        let d = h
            .diff(
                &json!({"on": on, "name": "u", "state": "present", "groups": ["c"]}),
                &actual,
            )
            .unwrap();
        assert_eq!(d.changes[0].field, "groups");
        let d = h
            .diff(&json!({"on": on, "name": "v", "state": "present"}), &actual)
            .unwrap();
        assert!(d.requires_replace());
    }
}
