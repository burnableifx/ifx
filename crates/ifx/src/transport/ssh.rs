//! SSH transport that shells out to the system `ssh` binary, so `~/.ssh/config`,
//! agents, certificates and jump hosts all work exactly as they do interactively.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;

use super::{Cmd, Output, Transport};
use crate::model::{Connection, SshActivation};

const DEFAULT_CONNECT_TIMEOUT: u64 = 180;

pub struct Ssh {
    conn: Connection,
    control_dir: Option<PathBuf>,
    activated: OnceCell<()>,
    ready: OnceCell<()>,
}

impl Ssh {
    pub fn new(conn: Connection, control_dir: Option<PathBuf>) -> Self {
        assert!(matches!(conn, Connection::Ssh { .. }));
        Self {
            conn,
            control_dir,
            activated: OnceCell::new(),
            ready: OnceCell::new(),
        }
    }

    fn base_args(&self) -> anyhow::Result<Vec<String>> {
        ssh_args(&self.conn, self.control_dir.as_deref())
    }

    async fn activate(&self) -> anyhow::Result<()> {
        let Some(activation) = self.conn.activation() else {
            return Ok(());
        };
        open_activation(activation).await
    }

    async fn run(&self, line: &str, stdin: Option<&[u8]>) -> anyhow::Result<Output> {
        let mut child = tokio::process::Command::new("ssh")
            .args(self.base_args()?)
            .arg("--")
            .arg(line)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(data) = stdin {
            let mut pipe = child.stdin.take().expect("piped");
            let data = data.to_vec();
            tokio::spawn(async move {
                let _ = pipe.write_all(&data).await;
                let _ = pipe.shutdown().await;
            });
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Retry a trivial command until the host answers or the deadline passes.
    async fn wait_ready(&self) -> anyhow::Result<()> {
        let timeout = match &self.conn {
            Connection::Ssh {
                connect_timeout_secs,
                ..
            } => connect_timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            _ => unreachable!(),
        };
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let label = self.conn.label();
        let mut last;
        loop {
            let out = self.run("true", None).await?;
            if out.status == 0 {
                return Ok(());
            }
            last = out.stderr_str().trim().to_string();
            // 255 = ssh itself failed (refused, timeout, auth). Anything else means the
            // host ran our command, so it is reachable.
            if out.status != 255 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            tracing::info!(target: "ifx::transport", conn = %label, "waiting for ssh: {last}");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        anyhow::bail!("{label}: not reachable after {timeout}s: {last}")
    }
}

fn ssh_args(
    conn: &Connection,
    control_dir: Option<&std::path::Path>,
) -> anyhow::Result<Vec<String>> {
    let Connection::Ssh {
        host,
        user,
        port,
        identity,
        extra_args,
        via,
        ..
    } = conn
    else {
        anyhow::bail!("SSH routing requires an SSH connection")
    };
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
    ];
    if let Some(dir) = control_dir {
        args.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}/%C", dir.display()),
            "-o".into(),
            "ControlPersist=120".into(),
        ]);
    }
    if let Some(p) = port {
        args.extend(["-p".into(), p.to_string()]);
    }
    if let Some(i) = identity {
        args.extend(["-i".into(), i.clone()]);
    }
    if let Some(via) = via {
        anyhow::ensure!(
            via.activation().is_none(),
            "an SSH bastion cannot itself require execution-scoped direct access"
        );
        let mut proxy = ssh_args(via, None)?;
        let target = proxy
            .pop()
            .ok_or_else(|| anyhow::anyhow!("SSH bastion has no target"))?;
        proxy.extend(["-W".into(), "%h:%p".into(), target]);
        let command = std::iter::once("ssh".to_string())
            .chain(proxy)
            .map(|argument| shell_escape::unix::escape(argument.into()).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        args.extend(["-o".into(), format!("ProxyCommand={command}")]);
    }
    args.extend(extra_args.iter().cloned());
    let target = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.clone(),
    };
    args.push(target);
    Ok(args)
}

#[async_trait]
impl Transport for Ssh {
    fn connection(&self) -> &Connection {
        &self.conn
    }

    fn execution_scoped(&self) -> bool {
        self.conn.activation().is_some()
    }

    async fn finish(&self) -> anyhow::Result<()> {
        if self.activated.get().is_some()
            && let Some(activation) = self.conn.activation()
        {
            close_activation(activation).await?;
        }
        Ok(())
    }

    async fn exec(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.activated.get_or_try_init(|| self.activate()).await?;
        self.ready.get_or_try_init(|| self.wait_ready()).await?;
        let line = cmd.wrapped(self.conn.sudo());
        tracing::debug!(target: "ifx::transport", conn = %self.conn.label(), %line, "exec");
        let out = self.run(&line, cmd.stdin.as_deref()).await?;
        if out.status == 255 {
            anyhow::bail!(
                "{}: ssh failed: {}",
                self.conn.label(),
                out.stderr_str().trim()
            );
        }
        Ok(out)
    }
}

async fn open_activation(activation: &SshActivation) -> anyhow::Result<()> {
    if tokio::net::TcpStream::connect(("127.0.0.1", activation.host_port))
        .await
        .is_ok()
    {
        mark_activation_pending(activation)?;
        return Ok(());
    }
    anyhow::ensure!(
        activation.reopen,
        "immutable management is sealed; replace the instance to change guest configuration"
    );
    mark_activation_pending(activation)?;
    if activation.persistent_netdev {
        let command = format!(
            "hostfwd_add {} tcp:127.0.0.1:{}-:{}",
            activation.netdev_id, activation.host_port, activation.guest_port
        );
        let response = qmp_execute(
            &activation.monitor,
            "human-monitor-command",
            serde_json::json!({"command-line": command}),
        )
        .await?;
        ensure_hmp_success(&response)?;
    } else {
        qmp_execute(
                &activation.monitor,
                "netdev_add",
                serde_json::json!({
                    "type": "user",
                    "id": activation.netdev_id,
                    "hostfwd": [format!("tcp:127.0.0.1:{}-:{}", activation.host_port, activation.guest_port)],
                }),
            )
            .await?;
        if let Err(error) = qmp_execute(
            &activation.monitor,
            "device_add",
            serde_json::json!({
                "driver": "virtio-net-pci",
                "id": activation.device_id,
                "netdev": activation.netdev_id,
                "mac": activation.mac,
            }),
        )
        .await
        {
            let _ = qmp_execute(
                &activation.monitor,
                "netdev_del",
                serde_json::json!({"id": activation.netdev_id}),
            )
            .await;
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) async fn close_activation(activation: &SshActivation) -> anyhow::Result<()> {
    if activation.persistent_netdev {
        let command = format!(
            "hostfwd_remove {} tcp:127.0.0.1:{}",
            activation.netdev_id, activation.host_port
        );
        let response = qmp_execute(
            &activation.monitor,
            "human-monitor-command",
            serde_json::json!({"command-line": command}),
        )
        .await?;
        ensure_hmp_removal(&response)?;
        clear_activation_pending(activation)?;
        return Ok(());
    }
    let _ = qmp_execute(
        &activation.monitor,
        "device_del",
        serde_json::json!({"id": activation.device_id}),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match qmp_execute(
            &activation.monitor,
            "netdev_del",
            serde_json::json!({"id": activation.netdev_id}),
        )
        .await
        {
            Ok(_) => {
                clear_activation_pending(activation)?;
                return Ok(());
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(netdev = %activation.netdev_id, "waiting to remove QEMU management NIC: {error:#}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) if qmp_object_is_absent(&error) => {
                clear_activation_pending(activation)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn activation_pending(activation: &SshActivation) -> bool {
    !activation.lease.is_empty() && std::path::Path::new(&activation.lease).is_file()
}

pub(crate) fn mark_activation_pending(activation: &SshActivation) -> anyhow::Result<()> {
    if activation.lease.is_empty() {
        return Ok(());
    }
    std::fs::write(&activation.lease, b"cleanup required\n")?;
    Ok(())
}

pub(crate) fn clear_activation_pending(activation: &SshActivation) -> anyhow::Result<()> {
    if activation.lease.is_empty() {
        return Ok(());
    }
    match std::fs::remove_file(&activation.lease) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn qmp_object_is_absent(error: &anyhow::Error) -> bool {
    let error = format!("{error:#}").to_ascii_lowercase();
    error.contains("not found") || error.contains("not exist") || error.contains("does not exist")
}

fn ensure_hmp_success(response: &serde_json::Value) -> anyhow::Result<()> {
    let result = response
        .get("return")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    anyhow::ensure!(
        !result.to_ascii_lowercase().starts_with("error")
            && !result.to_ascii_lowercase().contains("could not"),
        "QEMU monitor command failed: {result}"
    );
    Ok(())
}

fn ensure_hmp_removal(response: &serde_json::Value) -> anyhow::Result<()> {
    let result = response
        .get("return")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if result.to_ascii_lowercase().contains("could not remove") {
        return Ok(());
    }
    ensure_hmp_success(response)
}

#[cfg(unix)]
async fn qmp_execute(
    monitor: &str,
    command: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let stream = tokio::net::UnixStream::connect(monitor).await?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let greeting = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("QMP monitor closed before its greeting"))?;
    let greeting: serde_json::Value = serde_json::from_str(&greeting)?;
    anyhow::ensure!(greeting.get("QMP").is_some(), "invalid QMP greeting");
    writer
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .await?;
    read_qmp_reply(&mut lines).await?;
    let request = serde_json::json!({"execute": command, "arguments": arguments});
    writer
        .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
        .await?;
    read_qmp_reply(&mut lines).await
}

#[cfg(unix)]
async fn read_qmp_reply<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> anyhow::Result<serde_json::Value> {
    while let Some(line) = lines.next_line().await? {
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if value.get("return").is_some() {
            return Ok(value);
        }
        if let Some(error) = value.get("error") {
            anyhow::bail!("QMP command failed: {error}");
        }
    }
    anyhow::bail!("QMP monitor closed without a response")
}

#[cfg(not(unix))]
async fn qmp_execute(
    _monitor: &str,
    _command: &str,
    _arguments: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!("execution-scoped QEMU management requires a QMP Unix socket")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_connection(host: &str, identity: &str, via: Option<Connection>) -> Connection {
        Connection::Ssh {
            host: host.into(),
            user: Some("debian".into()),
            port: Some(22),
            identity: Some(identity.into()),
            connect_timeout_secs: Some(30),
            sudo: true,
            extra_args: Vec::new(),
            via: via.map(Box::new),
            activation: None,
        }
    }

    #[test]
    fn nested_proxy_keeps_each_hops_identity() {
        let router = ssh_connection("10.0.0.10", "/keys/router", None);
        let middle = ssh_connection("10.0.0.20", "/keys/middle", Some(router));
        let leaf = ssh_connection("10.0.1.30", "/keys/leaf", Some(middle));

        let args = ssh_args(&leaf, None).unwrap();
        let proxy = args
            .iter()
            .find_map(|argument| argument.strip_prefix("ProxyCommand="))
            .unwrap();
        assert!(args.windows(2).any(|pair| pair == ["-i", "/keys/leaf"]));
        assert!(proxy.contains("/keys/middle"));
        assert!(proxy.contains("/keys/router"));
        assert!(proxy.contains("10.0.0.20"));
        assert!(proxy.contains("10.0.0.10"));
        assert_eq!(args.last().unwrap(), "debian@10.0.1.30");
    }

    #[test]
    fn hmp_errors_are_not_mistaken_for_qmp_success() {
        assert!(ensure_hmp_success(&serde_json::json!({"return": ""})).is_ok());
        assert!(
            ensure_hmp_success(&serde_json::json!({
                "return": "Could not remove host forwarding rule"
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn activation_records_cleanup_intent_before_accepting_existing_access() {
        let directory = tempfile::tempdir().unwrap();
        let lease = directory.path().join("management-pending");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let activation = SshActivation {
            monitor: "/does/not/exist".into(),
            lease: lease.to_string_lossy().into_owned(),
            host_port: listener.local_addr().unwrap().port(),
            guest_port: 22,
            netdev_id: "mgmt".into(),
            device_id: "ifx-mgmt".into(),
            mac: "52:54:00:12:34:56".into(),
            persistent_netdev: false,
            reopen: false,
        };

        open_activation(&activation).await.unwrap();
        assert!(activation_pending(&activation));
        clear_activation_pending(&activation).unwrap();
        assert!(!activation_pending(&activation));
    }

    #[tokio::test]
    async fn immutable_activation_refuses_to_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let lease = directory.path().join("management-pending");
        let error = open_activation(&SshActivation {
            monitor: "/does/not/exist".into(),
            lease: lease.to_string_lossy().into_owned(),
            host_port: 0,
            guest_port: 22,
            netdev_id: "mgmt".into(),
            device_id: "ifx-mgmt".into(),
            mac: "52:54:00:12:34:56".into(),
            persistent_netdev: false,
            reopen: false,
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("immutable management is sealed"));
        assert!(!lease.exists());
    }
}
