//! `host.file`: a regular file or directory whose content, mode and ownership are
//! reconciled in place.

use anyhow::{Context as _, anyhow, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{bool_field, esc, on_field, req_str, sha256_hex, str_field, transport};
use crate::provider::{Actual, Applied, Ctx, Diff, FieldChange, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{Cmd, Transport, WriteOpts};

/// Files larger than this are compared by hash only; their content is never fetched.
const CONTENT_CAP: u64 = 1 << 20;

pub struct FileHandler;

/// What `stat` reports about an existing path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub kind: String,
    pub mode: String,
    pub owner: String,
    pub group: String,
    pub size: u64,
    /// Only for regular files.
    pub sha256: Option<String>,
}

impl Stat {
    pub fn is_dir(&self) -> bool {
        self.kind == "directory"
    }
}

/// `644` -> `0644`; symbolic modes are passed through untouched.
pub(crate) fn normalize_mode(m: &str) -> String {
    let t = m.trim();
    if !t.is_empty() && t.len() <= 4 && t.chars().all(|c| c.is_ascii_digit()) {
        format!("{t:0>4}")
    } else {
        t.to_string()
    }
}

/// Parse the two-line output of the stat script: `kind|mode|owner|group|size`, then an
/// optional sha256 line for regular files.
pub(crate) fn parse_stat(stdout: &str) -> Result<Stat> {
    let mut lines = stdout.lines();
    let first = lines.next().ok_or_else(|| anyhow!("empty stat output"))?;
    let parts: Vec<&str> = first.splitn(5, '|').collect();
    ensure!(parts.len() == 5, "unexpected stat output: {first:?}");
    let sha256 = lines
        .next()
        .map(str::trim)
        .filter(|s| s.len() == 64)
        .map(String::from);
    Ok(Stat {
        kind: parts[0].to_string(),
        mode: normalize_mode(parts[1]),
        owner: parts[2].to_string(),
        group: parts[3].to_string(),
        size: parts[4].trim().parse().unwrap_or(0),
        sha256,
    })
}

async fn stat(t: &dyn Transport, path: &str, privileged: bool) -> Result<Option<Stat>> {
    let script = format!(
        "p={p}; if [ ! -e \"$p\" ]; then exit 44; fi; \
         stat -c '%F|%a|%U|%G|%s' -- \"$p\" || exit 1; \
         if [ -f \"$p\" ]; then \
           if command -v sha256sum >/dev/null 2>&1; then sha256sum -- \"$p\"; \
           else shasum -a 256 -- \"$p\"; fi | cut -d' ' -f1; \
         fi",
        p = esc(path)
    );
    let mut cmd = Cmd::sh(script);
    cmd.privileged = privileged;
    let out = t.exec(&cmd).await?;
    match out.status {
        44 => Ok(None),
        0 => parse_stat(&out.stdout_str()).map(Some),
        _ => bail!("stat {path}: {}", out.stderr_str().trim()),
    }
}

/// `chown`/`chgrp` clause for the shell variable `$p`, or empty when neither is set.
fn chown_clause(owner: Option<&str>, group: Option<&str>) -> String {
    match (owner, group) {
        (Some(o), Some(g)) => format!(" chown {} \"$p\" || exit 1;", esc(&format!("{o}:{g}"))),
        (Some(o), None) => format!(" chown {} \"$p\" || exit 1;", esc(o)),
        (None, Some(g)) => format!(" chgrp {} \"$p\" || exit 1;", esc(g)),
        (None, None) => String::new(),
    }
}

fn outputs(path: &str, exists: bool, sha256: Option<&str>) -> Value {
    json!({ "path": path, "exists": exists, "sha256": sha256 })
}

async fn remove(t: &dyn Transport, path: &str, directory: bool, privileged: bool) -> Result<()> {
    let mut cmd = Cmd::new("rm")
        .arg(if directory { "-rf" } else { "-f" })
        .arg("--")
        .arg(path);
    cmd.privileged = privileged;
    t.exec(&cmd).await?.ok(&format!("remove {path}"))?;
    Ok(())
}

async fn ensure(t: &dyn Transport, inputs: &Value) -> Result<Applied> {
    let path = req_str(inputs, "path")?;
    let privileged = bool_field(inputs, "privileged").unwrap_or(false);
    let directory = bool_field(inputs, "directory").unwrap_or(false);
    let mode = str_field(inputs, "mode").map(normalize_mode);
    let owner = str_field(inputs, "owner");
    let group = str_field(inputs, "group");

    if str_field(inputs, "state") == Some("absent") {
        remove(t, path, directory, privileged).await?;
        return Ok(Applied {
            id: None,
            outputs: outputs(path, false, None),
        });
    }

    let observed = stat(t, path, privileged).await?;

    if directory {
        if let Some(st) = &observed {
            ensure!(
                st.is_dir(),
                "{path} exists and is not a directory ({})",
                st.kind
            );
        }
        let mut script = format!("p={}; mkdir -p \"$p\" || exit 1;", esc(path));
        if let Some(m) = &mode {
            script.push_str(&format!(" chmod {} \"$p\" || exit 1;", esc(m)));
        }
        script.push_str(&chown_clause(owner, group));
        let mut cmd = Cmd::sh(script);
        cmd.privileged = privileged;
        t.exec(&cmd)
            .await?
            .ok(&format!("create directory {path}"))?;
        return Ok(Applied {
            id: None,
            outputs: outputs(path, true, None),
        });
    }

    if let Some(st) = &observed {
        ensure!(
            !st.is_dir(),
            "{path} is a directory; set `directory: true` or remove it first"
        );
    }
    let content = str_field(inputs, "content");
    let source = str_field(inputs, "source");
    ensure!(
        content.is_none() || source.is_none(),
        "`content` and `source` are mutually exclusive"
    );
    let data: Option<Vec<u8>> = match (content, source) {
        (Some(c), _) => Some(c.as_bytes().to_vec()),
        (_, Some(s)) => Some(std::fs::read(s).with_context(|| format!("reading source `{s}`"))?),
        (None, None) if observed.is_none() => Some(Vec::new()),
        (None, None) => None,
    };
    let sha = match &data {
        Some(d) => Some(sha256_hex(d)),
        None => observed.as_ref().and_then(|s| s.sha256.clone()),
    };
    let unchanged =
        data.is_some() && sha.is_some() && sha == observed.as_ref().and_then(|s| s.sha256.clone());

    if let (Some(d), false) = (&data, unchanged) {
        if observed.is_none() {
            let mut cmd = Cmd::sh(format!("mkdir -p \"$(dirname {})\"", esc(path)));
            cmd.privileged = privileged;
            t.exec(&cmd)
                .await?
                .ok(&format!("create parent of {path}"))?;
        }
        // Preserve what is not being managed: the mode always, ownership when root
        // (a non-root writer cannot give the file away anyway).
        let keep = observed.as_ref();
        let opts = WriteOpts {
            mode: Some(
                mode.clone()
                    .or_else(|| keep.map(|s| s.mode.clone()))
                    .unwrap_or_else(|| "0644".into()),
            ),
            owner: owner
                .map(String::from)
                .or_else(|| keep.filter(|_| privileged).map(|s| s.owner.clone())),
            group: group.map(String::from).or_else(|| {
                keep.filter(|_| privileged || owner.is_some())
                    .map(|s| s.group.clone())
            }),
            privileged,
        };
        t.write_file(path, d, &opts).await?;
    } else if mode.is_some() || owner.is_some() || group.is_some() {
        let mut script = format!("p={};", esc(path));
        if let Some(m) = &mode {
            script.push_str(&format!(" chmod {} \"$p\" || exit 1;", esc(m)));
        }
        script.push_str(&chown_clause(owner, group));
        let mut cmd = Cmd::sh(script);
        cmd.privileged = privileged;
        t.exec(&cmd)
            .await?
            .ok(&format!("set attributes of {path}"))?;
    }
    Ok(Applied {
        id: None,
        outputs: outputs(path, true, sha.as_deref()),
    })
}

#[async_trait]
impl Handler for FileHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "host.file",
            "A file or directory on a host. Content, permissions and ownership are reconciled in \
             place; `state: \"absent\"` removes it.",
        )
        .input(on_field())
        .input(field("path", FieldType::String).required().replace().doc(
            "Absolute path on the host. Changing it replaces the resource: the old path is \
             removed and the new one created.",
        ))
        .input(field("content", FieldType::String).group("body").doc(
            "Exact file contents as a UTF-8 string. Mutually exclusive with `source`; ignored \
             for directories.",
        ))
        .input(field("source", FieldType::String).group("body").doc(
            "Local file (relative to the working directory) whose bytes are copied to the host, \
             compared by SHA-256. Mutually exclusive with `content`.",
        ))
        .input(field("mode", FieldType::String).doc(
            "Octal permission bits such as \"0644\" or \"0755\". When unset, existing \
             permissions are kept and new files get 0644.",
        ))
        .input(field("owner", FieldType::String).doc(
            "Owning user name. Changing it needs root: `privileged: true` on a sudo-enabled \
             connection.",
        ))
        .input(field("group", FieldType::String).doc(
            "Owning group name. Changing it needs root or membership in the target group.",
        ))
        .input(field("state", FieldType::enumeration(["present", "absent"])).default("present").doc(
            "\"present\" ensures the path exists as described; \"absent\" removes it \
             (directories recursively).",
        ))
        .input(field("directory", FieldType::Bool).default(false).replace().doc(
            "Manage a directory instead of a regular file (`content`/`source` are ignored). \
             Switching between the two replaces the resource.",
        ))
        .input(field("privileged", FieldType::Bool).default(false).doc(
            "Run every command through `sudo -n` when the connection allows sudo; needed for \
             paths the connecting user cannot write.",
        ))
        .output(field("path", FieldType::String).doc("The managed path, as given."))
        .output(field("exists", FieldType::Bool).doc("Whether the path existed after the last apply."))
        .output(field("sha256", FieldType::String).doc(
            "Hex SHA-256 of the file contents; null for directories and absent files.",
        ))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let t = transport(cx, inputs).await?;
        let path = req_str(inputs, "path")?;
        let privileged = bool_field(inputs, "privileged").unwrap_or(false);
        let Some(st) = stat(&*t, path, privileged).await? else {
            if str_field(inputs, "state") == Some("absent") {
                return Ok(Some(Actual {
                    id: None,
                    props: json!({ "on": inputs["on"], "path": path, "state": "absent" }),
                    outputs: outputs(path, false, None),
                }));
            }
            return Ok(None);
        };
        let mut props = json!({
            "on": inputs["on"],
            "path": path,
            "state": "present",
            "directory": st.is_dir(),
            "mode": st.mode,
            "owner": st.owner,
            "group": st.group,
        });
        if !st.is_dir() {
            if let Some(sha) = &st.sha256 {
                props["content_sha256"] = json!(sha);
            }
            if st.size <= CONTENT_CAP
                && let Some(data) = t.read_file(path, privileged).await?
            {
                props["content"] = String::from_utf8(data)
                    .map(Value::String)
                    .unwrap_or(Value::Null);
            }
        }
        Ok(Some(Actual {
            id: None,
            props,
            outputs: outputs(path, true, st.sha256.as_deref()),
        }))
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let mut desired = desired.clone();
        if let Some(m) = str_field(&desired, "mode") {
            desired["mode"] = json!(normalize_mode(m));
        }
        let mut diff = Diff::generic(&desired, &actual.props, &["on", "path", "directory"]);

        // Content compared by hash: always for `source`, and for `content` when the
        // observation carries only a hash (large file).
        let want_file = str_field(&desired, "state") != Some("absent")
            && !bool_field(&desired, "directory").unwrap_or(false)
            && str_field(&actual.props, "state") == Some("present");
        if want_file {
            let observed = str_field(&actual.props, "content_sha256");
            let want = if let Some(src) = str_field(&desired, "source") {
                let data = std::fs::read(src).with_context(|| format!("reading source `{src}`"))?;
                Some(("source", sha256_hex(&data)))
            } else if let (Some(c), None) =
                (str_field(&desired, "content"), actual.props.get("content"))
            {
                Some(("content", sha256_hex(c.as_bytes())))
            } else {
                None
            };
            if let Some((name, want)) = want
                && observed != Some(want.as_str())
            {
                diff.changes.push(FieldChange {
                    field: name.into(),
                    from: observed.map(|s| json!(format!("sha256:{s}"))),
                    to: Some(json!(format!("sha256:{want}"))),
                    forces_replace: false,
                });
            }
        }
        Ok(diff)
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

    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        let t = transport(cx, inputs).await?;
        let path = req_str(inputs, "path")?;
        remove(
            &*t,
            path,
            bool_field(inputs, "directory").unwrap_or(false),
            bool_field(inputs, "privileged").unwrap_or(false),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_normalization() {
        assert_eq!(normalize_mode("644"), "0644");
        assert_eq!(normalize_mode("0644"), "0644");
        assert_eq!(normalize_mode("2755"), "2755");
        assert_eq!(normalize_mode("u+x"), "u+x");
    }

    #[test]
    fn stat_parsing() {
        let st = parse_stat(&format!(
            "regular file|644|root|root|12\n{}\n",
            "a".repeat(64)
        ))
        .unwrap();
        assert_eq!(st.mode, "0644");
        assert_eq!(st.size, 12);
        assert!(!st.is_dir());
        assert_eq!(st.sha256.as_deref(), Some("a".repeat(64).as_str()));
        let st = parse_stat("directory|755|me|wheel|4096\n").unwrap();
        assert!(st.is_dir());
        assert_eq!(st.sha256, None);
        assert!(parse_stat("garbage").is_err());
    }

    #[test]
    fn chown_clauses() {
        assert_eq!(chown_clause(None, None), "");
        assert!(chown_clause(Some("a"), Some("b")).contains("chown 'a:b'"));
        assert!(chown_clause(Some("a"), None).contains("chown a "));
        assert!(chown_clause(None, Some("b")).contains("chgrp b"));
    }
}
