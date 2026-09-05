//! `host.package`: system packages via apt, dnf, yum or apk, detected once per host.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use anyhow::{anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{bool_field, on_field, str_field, str_list, transport};
use crate::model::Connection;
use crate::provider::{Actual, Applied, Ctx, Diff, FieldChange, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{Cmd, Transport};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Manager {
    Apt,
    Dnf,
    Yum,
    Apk,
}

impl Manager {
    /// Pick from the output of `command -v apt-get dnf yum apk`, in that preference.
    pub fn detect(command_v_output: &str) -> Option<Self> {
        let found: Vec<&str> = command_v_output
            .lines()
            .filter_map(|l| l.trim().rsplit('/').next())
            .collect();
        [
            ("apt-get", Self::Apt),
            ("dnf", Self::Dnf),
            ("yum", Self::Yum),
            ("apk", Self::Apk),
        ]
        .into_iter()
        .find(|(bin, _)| found.contains(bin))
        .map(|(_, m)| m)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Yum => "yum",
            Self::Apk => "apk",
        }
    }

    /// Query installation status; exit status is not meaningful (missing names make
    /// dpkg-query/rpm exit non-zero), parse stdout with [`Manager::parse_installed`].
    fn query(self, names: &[String]) -> Cmd {
        match self {
            Self::Apt => Cmd::new("dpkg-query")
                .args(["-W", "-f", "${Package} ${db:Status-Status}\\n"])
                .args(names),
            Self::Dnf | Self::Yum => Cmd::new("rpm")
                .args(["-q", "--qf", "%{NAME}\\n"])
                .args(names),
            Self::Apk => Cmd::new("apk").args(["info", "-e"]).args(names),
        }
    }

    pub fn parse_installed(self, stdout: &str) -> BTreeSet<String> {
        match self {
            Self::Apt => parse_dpkg_query(stdout),
            Self::Dnf | Self::Yum => parse_rpm_q(stdout),
            Self::Apk => parse_apk_info(stdout),
        }
    }

    fn install(self, names: &[String]) -> Cmd {
        match self {
            Self::Apt => Cmd::new("apt-get")
                .args(["install", "-y", "-q", "--no-install-recommends"])
                .args(names)
                .env("DEBIAN_FRONTEND", "noninteractive"),
            Self::Dnf => Cmd::new("dnf").args(["install", "-y", "-q"]).args(names),
            Self::Yum => Cmd::new("yum").args(["install", "-y", "-q"]).args(names),
            Self::Apk => Cmd::new("apk").args(["add", "--no-progress"]).args(names),
        }
        .privileged()
    }

    fn remove(self, names: &[String]) -> Cmd {
        match self {
            Self::Apt => Cmd::new("apt-get")
                .args(["remove", "-y", "-q"])
                .args(names)
                .env("DEBIAN_FRONTEND", "noninteractive"),
            Self::Dnf => Cmd::new("dnf").args(["remove", "-y", "-q"]).args(names),
            Self::Yum => Cmd::new("yum").args(["remove", "-y", "-q"]).args(names),
            Self::Apk => Cmd::new("apk").args(["del", "--no-progress"]).args(names),
        }
        .privileged()
    }

    fn update_cache(self) -> Cmd {
        match self {
            Self::Apt => Cmd::new("apt-get")
                .args(["update", "-q"])
                .env("DEBIAN_FRONTEND", "noninteractive"),
            Self::Dnf => Cmd::new("dnf").args(["makecache", "-q"]),
            Self::Yum => Cmd::new("yum").args(["makecache", "-q"]),
            Self::Apk => Cmd::new("apk").arg("update"),
        }
        .privileged()
    }
}

/// `dpkg-query -W -f '${Package} ${db:Status-Status}\n'`.
pub fn parse_dpkg_query(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, status)| status.trim() == "installed")
        .map(|(name, _)| name.trim().to_string())
        .collect()
}

/// `rpm -q --qf '%{NAME}\n'`: one name per installed package, or
/// `package X is not installed`.
pub fn parse_rpm_q(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !(l.starts_with("package ") && l.ends_with("is not installed"))
        })
        .map(String::from)
        .collect()
}

/// `apk info -e`: prints the names that are installed.
pub fn parse_apk_info(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Default)]
pub struct PackageHandler {
    managers: Mutex<HashMap<Connection, Manager>>,
}

impl PackageHandler {
    async fn manager(&self, t: &dyn Transport) -> Result<Manager> {
        let conn = t.connection();
        let cached = self.managers.lock().unwrap().get(conn).copied();
        if let Some(m) = cached {
            return Ok(m);
        }
        let out = t
            .exec(&Cmd::sh("command -v apt-get dnf yum apk 2>/dev/null; true"))
            .await?;
        let m = Manager::detect(&out.stdout_str()).ok_or_else(|| {
            anyhow!(
                "no supported package manager on {} (looked for apt-get, dnf, yum, apk)",
                conn.label()
            )
        })?;
        self.managers.lock().unwrap().insert(conn.clone(), m);
        Ok(m)
    }

    async fn installed(
        &self,
        t: &dyn Transport,
        m: Manager,
        names: &[String],
    ) -> Result<BTreeSet<String>> {
        if names.is_empty() {
            return Ok(BTreeSet::new());
        }
        let out = t.exec(&m.query(names)).await?;
        Ok(m.parse_installed(&out.stdout_str()))
    }

    async fn ensure(&self, t: &dyn Transport, inputs: &Value) -> Result<Applied> {
        let m = self.manager(t).await?;
        let names = str_list(inputs, "names");
        let present = str_field(inputs, "state") != Some("absent");
        if bool_field(inputs, "update_cache").unwrap_or(false) {
            t.exec(&m.update_cache())
                .await?
                .ok("update package cache")?;
        }
        let installed = self.installed(t, m, &names).await?;
        let todo: Vec<String> = names
            .iter()
            .filter(|n| installed.contains(*n) != present)
            .cloned()
            .collect();
        if !todo.is_empty() {
            let (verb, cmd) = if present {
                ("install", m.install(&todo))
            } else {
                ("remove", m.remove(&todo))
            };
            t.exec(&cmd)
                .await?
                .ok(&format!("{verb} {}", todo.join(" ")))?;
        }
        let installed = self.installed(t, m, &names).await?;
        if present {
            let missing: Vec<&String> = names.iter().filter(|n| !installed.contains(*n)).collect();
            ensure!(
                missing.is_empty(),
                "packages still missing after install: {missing:?}"
            );
        }
        Ok(Applied {
            id: None,
            outputs: outputs(m, &installed),
        })
    }
}

fn outputs(m: Manager, installed: &BTreeSet<String>) -> Value {
    json!({ "installed": installed, "manager": m.name() })
}

#[async_trait]
impl Handler for PackageHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "host.package",
            "System packages installed with the host's package manager (apt, dnf, yum or apk, \
             detected automatically). Commands run non-interactively.",
        )
        .input(on_field())
        .input(field("names", FieldType::list(FieldType::String)).required().doc(
            "Package names as the host's package manager knows them.",
        ))
        .input(field("state", FieldType::enumeration(["present", "absent"])).default("present").doc(
            "\"present\" installs any that are missing; \"absent\" removes any that are installed.",
        ))
        .input(field("update_cache", FieldType::Bool).default(false).doc(
            "Refresh the package index (`apt-get update` and friends) before installing or removing.",
        ))
        .output(field("installed", FieldType::list(FieldType::String)).doc(
            "Which of `names` were installed after the last apply.",
        ))
        .output(field("manager", FieldType::String).doc(
            "Package manager detected on the host: \"apt\", \"dnf\", \"yum\" or \"apk\".",
        ))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let t = transport(cx, inputs).await?;
        let m = self.manager(&*t).await?;
        let names = str_list(inputs, "names");
        let present = str_field(inputs, "state") != Some("absent");
        let installed = self.installed(&*t, m, &names).await?;
        if present && names.iter().all(|n| !installed.contains(n)) {
            return Ok(None);
        }
        let satisfied: Vec<&String> = names
            .iter()
            .filter(|n| installed.contains(*n) == present)
            .collect();
        Ok(Some(Actual {
            id: None,
            props: json!({
                "on": inputs["on"],
                "names": satisfied,
                "state": if present { "present" } else { "absent" },
                "checked": names,
                "installed": installed,
            }),
            outputs: outputs(m, &installed),
        }))
    }

    /// A name is satisfied when it was checked and its installed status matches the
    /// desired state; unchecked names (added since the last apply) count as work to do.
    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let mut diff = Diff::generic(desired, &json!({ "on": actual.props["on"] }), &["on"]);
        let names = str_list(desired, "names");
        let present = str_field(desired, "state") != Some("absent");
        let checked: BTreeSet<String> = str_list(&actual.props, "checked").into_iter().collect();
        let installed: BTreeSet<String> =
            str_list(&actual.props, "installed").into_iter().collect();
        let satisfied: Vec<&String> = names
            .iter()
            .filter(|n| checked.contains(*n) && installed.contains(*n) == present)
            .collect();
        if satisfied.len() != names.len() {
            if str_field(&actual.props, "state").is_some_and(|s| (s == "present") != present) {
                diff.changes.push(FieldChange {
                    field: "state".into(),
                    from: actual.props.get("state").cloned(),
                    to: desired.get("state").cloned(),
                    forces_replace: false,
                });
            }
            diff.changes.push(FieldChange {
                field: "names".into(),
                from: Some(json!(satisfied)),
                to: Some(json!(names)),
                forces_replace: false,
            });
        }
        Ok(diff)
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        self.ensure(&*t, inputs).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let t = transport(cx, inputs).await?;
        self.ensure(&*t, inputs).await
    }

    /// Undo: packages this declaration installed are removed; an `absent` declaration
    /// leaves the host alone.
    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        if str_field(inputs, "state") == Some("absent") {
            return Ok(());
        }
        let t = transport(cx, inputs).await?;
        let m = self.manager(&*t).await?;
        let names = str_list(inputs, "names");
        let installed = self.installed(&*t, m, &names).await?;
        let todo: Vec<String> = names
            .into_iter()
            .filter(|n| installed.contains(n))
            .collect();
        if !todo.is_empty() {
            t.exec(&m.remove(&todo))
                .await?
                .ok(&format!("remove {}", todo.join(" ")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_manager_by_preference() {
        assert_eq!(
            Manager::detect("/usr/bin/apt-get\n/usr/bin/dnf\n"),
            Some(Manager::Apt)
        );
        assert_eq!(
            Manager::detect("/usr/bin/dnf\n/usr/bin/yum\n"),
            Some(Manager::Dnf)
        );
        assert_eq!(Manager::detect("/usr/bin/yum\n"), Some(Manager::Yum));
        assert_eq!(Manager::detect("/sbin/apk\n"), Some(Manager::Apk));
        assert_eq!(Manager::detect(""), None);
    }

    #[test]
    fn dpkg_query_output() {
        let out = "curl installed\nvim not-installed\nnano config-files\nlibc6 installed\n";
        assert_eq!(parse_dpkg_query(out), set(&["curl", "libc6"]));
    }

    #[test]
    fn rpm_q_output() {
        let out = "curl\npackage vim is not installed\nnano\n";
        assert_eq!(parse_rpm_q(out), set(&["curl", "nano"]));
    }

    #[test]
    fn apk_info_output() {
        assert_eq!(parse_apk_info("curl\nnano\n"), set(&["curl", "nano"]));
        assert!(parse_apk_info("").is_empty());
    }

    #[test]
    fn install_commands_are_non_interactive() {
        let names = vec!["curl".to_string()];
        let line = Manager::Apt.install(&names).shell_line();
        assert!(
            line.starts_with("DEBIAN_FRONTEND=noninteractive apt-get install -y"),
            "{line}"
        );
        assert!(Manager::Dnf.install(&names).shell_line().contains("-y"));
        assert!(
            Manager::Apk
                .remove(&names)
                .shell_line()
                .starts_with("apk del")
        );
        assert!(Manager::Apt.install(&names).privileged);
    }

    #[test]
    fn diff_treats_unchecked_names_as_work() {
        let h = PackageHandler::default();
        let on = json!({"kind": "local", "sudo": false});
        let actual = Actual {
            id: None,
            props: json!({"on": on, "names": ["a"], "state": "present", "checked": ["a"], "installed": ["a"]}),
            outputs: Value::Null,
        };
        assert!(
            h.diff(
                &json!({"on": on, "names": ["a"], "state": "present"}),
                &actual
            )
            .unwrap()
            .is_empty()
        );
        let d = h
            .diff(
                &json!({"on": on, "names": ["a", "b"], "state": "present"}),
                &actual,
            )
            .unwrap();
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].from, Some(json!(["a"])));
        let d = h
            .diff(
                &json!({"on": on, "names": ["a"], "state": "absent"}),
                &actual,
            )
            .unwrap();
        assert_eq!(
            d.changes
                .iter()
                .map(|c| c.field.as_str())
                .collect::<Vec<_>>(),
            ["state", "names"]
        );
    }
}
