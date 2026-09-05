//! `qemu.instance`: a local cloud-image VM with a management SSH connection and
//! optional rootless socket networks shared with other QEMU instances.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::{absolute, required_string, resource_dir, write_atomic};
use crate::model::{Connection, SshActivation};
use crate::provider::{
    Actual, Applied, Ctx, Diff, FieldChange, Handler, OperationKind, OperationRisk, Result,
};
use crate::schema::{FieldType, ResourceSchema, field};
use crate::transport::{
    Cmd, Ssh, Transport as _, clear_ssh_activation_pending, close_ssh_activation,
    mark_ssh_activation_pending,
};

pub const TYPE: &str = "qemu.instance";

#[derive(Clone, Copy, Debug, Default)]
pub struct InstanceHandler;

#[derive(Clone, Debug)]
struct Paths {
    dir: PathBuf,
    config: PathBuf,
    disk: PathBuf,
    seed: PathBuf,
    user_data: PathBuf,
    vendor_data: PathBuf,
    meta_data: PathBuf,
    network_config: PathBuf,
    private_key: PathBuf,
    public_key: PathBuf,
    known_hosts: PathBuf,
    pid: PathBuf,
    monitor: PathBuf,
    console: PathBuf,
    management_cleanup: PathBuf,
}

impl Paths {
    fn new(cx: &Ctx<'_>, inputs: &Value) -> Result<Self> {
        let root = absolute(resource_dir(
            required_string(inputs, "dir")?,
            "instances",
            cx.urn.name(),
        ))?;
        Ok(Self {
            config: root.join("config.json"),
            disk: root.join("disk.qcow2"),
            seed: root.join("seed.iso"),
            user_data: root.join("user-data"),
            vendor_data: root.join("vendor-data"),
            meta_data: root.join("meta-data"),
            network_config: root.join("network-config"),
            private_key: root.join("id_ed25519"),
            public_key: root.join("id_ed25519.pub"),
            known_hosts: root.join("known_hosts"),
            pid: root.join("qemu.pid"),
            monitor: root.join("qmp.sock"),
            console: root.join("console.log"),
            management_cleanup: root.join("management-cleanup-pending"),
            dir: root,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstanceRecord {
    version: u32,
    dir_input: String,
    image: String,
    memory_mb: u64,
    cpus: u64,
    #[serde(default = "default_machine")]
    machine: String,
    #[serde(default)]
    cpu: Option<String>,
    #[serde(default = "default_acceleration")]
    acceleration: String,
    disk_gb: u64,
    authorized_keys: Vec<String>,
    #[serde(default)]
    ssh_port: Option<u16>,
    #[serde(default)]
    port_forwards: BTreeMap<u16, u16>,
    networks: Vec<String>,
    ssh_user: String,
    #[serde(default)]
    hostname: String,
    connect_timeout_secs: u64,
    attachments: Vec<Attachment>,
    #[serde(default)]
    volumes: Vec<VolumeAttachment>,
    #[serde(default)]
    restartable: bool,
    management_mac: String,
    #[serde(default = "default_management")]
    management: String,
    #[serde(default)]
    management_via: Option<Connection>,
    #[serde(default = "default_egress")]
    egress: String,
    #[serde(default)]
    bootstrap_complete: bool,
    #[serde(default = "default_bootstrap_revision")]
    bootstrap_revision: String,
}

fn default_machine() -> String {
    "q35".to_string()
}

fn default_acceleration() -> String {
    "auto".to_string()
}

fn default_management() -> String {
    "direct".to_string()
}

fn default_egress() -> String {
    "user_nat".to_string()
}

fn default_bootstrap_revision() -> String {
    "1".to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Management {
    Direct,
    Via,
    DirectThenVia,
    DirectThenRemove,
    Immutable,
    None,
}

impl Management {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "direct" => Ok(Self::Direct),
            "via" => Ok(Self::Via),
            "direct_then_via" => Ok(Self::DirectThenVia),
            "direct_then_remove" => Ok(Self::DirectThenRemove),
            "immutable" => Ok(Self::Immutable),
            "none" => Ok(Self::None),
            other => anyhow::bail!("unsupported QEMU management lifecycle `{other}`"),
        }
    }

    fn needs_bootstrap_direct(self) -> bool {
        matches!(
            self,
            Self::Direct | Self::DirectThenVia | Self::DirectThenRemove | Self::Immutable
        )
    }

    fn steady_direct(self) -> bool {
        self == Self::Direct
    }

    fn uses_via(self) -> bool {
        matches!(self, Self::Via | Self::DirectThenVia)
    }

    fn execution_scoped(self) -> bool {
        matches!(self, Self::DirectThenRemove | Self::Immutable)
    }

    fn seals_bootstrap(self) -> bool {
        matches!(
            self,
            Self::DirectThenVia | Self::DirectThenRemove | Self::Immutable
        )
    }
}

impl InstanceRecord {
    fn management(&self) -> Result<Management> {
        Management::parse(&self.management)
    }

    fn start_with_direct_management(&self) -> Result<bool> {
        let management = self.management()?;
        Ok(management.steady_direct()
            || (!self.bootstrap_complete && management.needs_bootstrap_direct()))
    }
}

fn validate_management_via(
    management: Management,
    management_via: Option<&Connection>,
) -> Result<()> {
    anyhow::ensure!(
        management.uses_via() == management_via.is_some(),
        "`management_via` is required exactly when management is `via` or `direct_then_via`"
    );
    if let Some(via) = management_via {
        anyhow::ensure!(
            matches!(via, Connection::Ssh { .. }),
            "`management_via` must be an SSH connection"
        );
        anyhow::ensure!(
            via.activation().is_none(),
            "`management_via` cannot require execution-scoped direct access"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Attachment {
    endpoint: String,
    backend: String,
    mac: String,
    ip: String,
    prefix: u8,
    dhcp: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct VolumeAttachment {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedVolume {
    path: PathBuf,
    format: String,
    read_only: bool,
    serial: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accelerator {
    Kvm,
    Tcg,
}

impl Accelerator {
    fn args(self, cpu: Option<&str>) -> [String; 4] {
        match self {
            Self::Kvm => ["-accel", "kvm", "-cpu", cpu.unwrap_or("host")],
            Self::Tcg => ["-accel", "tcg,thread=multi", "-cpu", cpu.unwrap_or("max")],
        }
        .map(str::to_string)
    }
}

fn integer(inputs: &Value, field: &str, default: u64) -> Result<u64> {
    let value = inputs.get(field).and_then(Value::as_u64).unwrap_or(default);
    anyhow::ensure!(value > 0, "`{field}` must be a positive integer");
    Ok(value)
}

fn string_list(inputs: &Value, field: &str) -> Result<Vec<String>> {
    inputs
        .get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow::anyhow!("`{field}` entries must be strings"))
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn volume_attachments(inputs: &Value) -> Result<Vec<VolumeAttachment>> {
    let values = inputs
        .get("volumes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut attachments = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let attachment: VolumeAttachment =
            serde_json::from_value(value).with_context(|| format!("parsing `volumes[{index}]`"))?;
        anyhow::ensure!(
            !attachment.path.is_empty(),
            "`volumes[{index}].path` must not be empty"
        );
        if let Some(serial) = &attachment.serial {
            anyhow::ensure!(
                !serial.is_empty()
                    && serial.len() <= 20
                    && serial
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.')),
                "`volumes[{index}].serial` must be 1-20 ASCII letters, digits, dots, dashes, or underscores"
            );
        }
        attachments.push(attachment);
    }
    Ok(attachments)
}

fn derived_volume_serial(urn: &str, path: &str) -> String {
    let digest = stable_bytes(&[urn, "volume", path]);
    format!("ifx-{}", hex::encode(&digest[..8]))
}

fn parse_volume_format(stdout: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(stdout).context("parsing qemu-img volume info")?;
    match value.get("format").and_then(Value::as_str) {
        Some(format @ ("qcow2" | "raw")) => Ok(format.to_string()),
        Some(format) => anyhow::bail!(
            "unsupported attached volume format `{format}`; convert it to qcow2 or raw"
        ),
        None => anyhow::bail!("qemu-img volume info did not include `format`"),
    }
}

fn volume_attachment_type() -> FieldType {
    FieldType::object_named(
        "VolumeAttachment",
        vec![
            field("path", FieldType::String)
                .required()
                .doc("Disk path, normally `qemu.volume.path`; qcow2 and raw are detected safely."),
            field("read_only", FieldType::Bool)
                .doc("Attach without guest write access; omitted values are read-write."),
            field("serial", FieldType::String)
                .doc("Stable 1-20 character virtio serial; omit for a deterministic IFX serial."),
        ],
    )
}

fn restart_permitted(desired: &Value, actual: &Actual) -> bool {
    desired
        .get("restartable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && actual
            .props
            .get("restartable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn live_instance(actual: &Actual) -> bool {
    actual.props.get("state").and_then(Value::as_str) != Some("stopped")
}

fn volume_inputs(value: &Value) -> Value {
    value.get("volumes").cloned().unwrap_or_else(|| json!([]))
}

fn port_forwards(inputs: &Value) -> Result<BTreeMap<u16, u16>> {
    inputs
        .get("port_forwards")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .map(|(host, guest)| {
                    let host = host
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port > 0)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "`port_forwards` host port `{host}` must be between 1 and 65535"
                            )
                        })?;
                    let guest = guest
                        .as_u64()
                        .and_then(|port| u16::try_from(port).ok())
                        .filter(|port| *port > 0)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "`port_forwards.{host}` guest port must be between 1 and 65535"
                            )
                        })?;
                    Ok((host, guest))
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn stable_bytes(parts: &[&str]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest.finalize().into()
}

fn mac(parts: &[&str]) -> String {
    let digest = stable_bytes(parts);
    format!(
        "52:54:{:02x}:{:02x}:{:02x}:{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("network endpoint has invalid CIDR `{cidr}`"))?;
    let address: Ipv4Addr = address
        .parse()
        .with_context(|| format!("network endpoint has invalid IPv4 address `{address}`"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("network endpoint has invalid prefix `{prefix}`"))?;
    anyhow::ensure!(
        prefix == 24,
        "QEMU network endpoint must use a /24 subnet, got `{cidr}`"
    );
    Ok((address, prefix))
}

fn host_address(network: Ipv4Addr, identity: &[u8; 32]) -> Ipv4Addr {
    let octets = network.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], 10 + identity[0] % 240)
}

fn parse_attachment(endpoint: &str, urn: &str, index: usize) -> Result<Attachment> {
    let (kind_and_address, cidr) = endpoint.split_once("?cidr=").ok_or_else(|| {
        anyhow::anyhow!("invalid QEMU network endpoint `{endpoint}`: missing `?cidr=`")
    })?;
    let identity = stable_bytes(&[urn, endpoint]);
    let mac = mac(&[urn, endpoint]);
    let (network, prefix) = parse_cidr(cidr)?;
    if let Some(bus) = kind_and_address.strip_prefix("socket+mcast://") {
        anyhow::ensure!(
            bus.parse::<std::net::SocketAddr>().is_ok(),
            "invalid multicast socket `{bus}`"
        );
        let ip = host_address(network, &identity).to_string();
        Ok(Attachment {
            endpoint: endpoint.to_string(),
            backend: format!("socket,id=lan{index},mcast={bus}"),
            mac,
            ip,
            prefix,
            dhcp: false,
        })
    } else if kind_and_address.strip_prefix("user://").is_some() {
        let octets = network.octets();
        let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], 15).to_string();
        Ok(Attachment {
            endpoint: endpoint.to_string(),
            backend: format!("user,id=lan{index},net={cidr},dhcpstart={ip}"),
            mac,
            ip,
            prefix,
            dhcp: true,
        })
    } else {
        anyhow::bail!("unsupported QEMU network endpoint `{endpoint}`")
    }
}

fn load_record(path: &Path) -> Result<Option<InstanceRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing QEMU instance record {}", path.display()))
        .map(Some)
}

fn parse_pid(text: &str) -> Option<u32> {
    let text = text.trim();
    let pid = text.parse::<u32>().ok()?;
    (pid > 0).then_some(pid)
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| parse_pid(&s))
}

#[cfg(target_os = "linux")]
fn expected_process(pid: u32, disk: &Path) -> bool {
    let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => cmdline,
        Err(_) => return false,
    };
    cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg.ends_with(b"qemu-system-x86_64"))
        && cmdline
            .windows(disk.as_os_str().as_encoded_bytes().len())
            .any(|window| window == disk.as_os_str().as_encoded_bytes())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn expected_process(pid: u32, _disk: &Path) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn expected_process(pid: u32, _disk: &Path) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

fn live_pid(paths: &Paths) -> Option<u32> {
    let pid = read_pid(&paths.pid)?;
    expected_process(pid, &paths.disk).then_some(pid)
}

fn qmp_status_running(value: &Value) -> Option<bool> {
    let status = qmp_status(value)?;
    Some(!matches!(status, "shutdown" | "prelaunch" | "inmigrate"))
}

fn qmp_status(value: &Value) -> Option<&str> {
    value.pointer("/return/status")?.as_str()
}

#[cfg(unix)]
async fn qmp_command(path: &Path, command: &str) -> Result<Value> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to QMP monitor {}", path.display()))?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let greeting = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("QMP monitor closed before its greeting"))?;
    let greeting: Value = serde_json::from_str(&greeting).context("parsing QMP greeting")?;
    anyhow::ensure!(
        greeting.get("QMP").is_some(),
        "invalid QMP greeting: {greeting}"
    );

    writer
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .await?;
    let _ = read_qmp_reply(&mut lines).await?;
    writer
        .write_all(format!("{{\"execute\":{}}}\n", serde_json::to_string(command)?).as_bytes())
        .await?;
    read_qmp_reply(&mut lines).await
}

#[cfg(unix)]
async fn read_qmp_reply<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> Result<Value> {
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line).context("parsing QMP response")?;
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
async fn qmp_command(_path: &Path, _command: &str) -> Result<Value> {
    anyhow::bail!("QMP Unix sockets are unavailable on this platform")
}

async fn is_running(paths: &Paths) -> bool {
    if live_pid(paths).is_none() {
        return false;
    }
    #[cfg(unix)]
    if paths.monitor.exists()
        && let Ok(status) = qmp_command(&paths.monitor, "query-status").await
        && let Some(running) = qmp_status_running(&status)
    {
        return running;
    }
    true
}

async fn observed_state(paths: &Paths) -> &'static str {
    if live_pid(paths).is_none() {
        return "stopped";
    }
    #[cfg(unix)]
    if paths.monitor.exists()
        && let Ok(status) = qmp_command(&paths.monitor, "query-status").await
        && let Some(status) = qmp_status(&status)
    {
        return match status {
            "paused" | "suspended" => "paused",
            "shutdown" | "prelaunch" | "inmigrate" => "stopped",
            _ => "running",
        };
    }
    "running"
}

async fn set_paused(paths: &Paths, paused: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let command = if paused { "stop" } else { "cont" };
        qmp_command(&paths.monitor, command).await?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (paths, paused);
        anyhow::bail!("paused QEMU state requires a QMP Unix socket")
    }
}

fn direct_connection(paths: &Paths, record: &InstanceRecord) -> Result<Connection> {
    let port = record.ssh_port.ok_or_else(|| {
        anyhow::anyhow!("direct QEMU management is missing its allocated SSH port")
    })?;
    Ok(Connection::Ssh {
        host: "127.0.0.1".to_string(),
        user: Some(record.ssh_user.clone()),
        port: Some(port),
        identity: Some(paths.private_key.to_string_lossy().into_owned()),
        connect_timeout_secs: Some(record.connect_timeout_secs),
        sudo: record.ssh_user != "root",
        extra_args: vec![
            "-o".to_string(),
            format!("UserKnownHostsFile={}", paths.known_hosts.display()),
        ],
        via: None,
        activation: None,
    })
}

fn via_connection(paths: &Paths, record: &InstanceRecord) -> Result<Connection> {
    let via = record
        .management_via
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("QEMU via management is missing `management_via`"))?;
    anyhow::ensure!(
        matches!(via, Connection::Ssh { .. }),
        "`management_via` must be an SSH connection"
    );
    anyhow::ensure!(
        via.activation().is_none(),
        "`management_via` cannot require execution-scoped direct access"
    );
    let attachment = record
        .attachments
        .iter()
        .find(|attachment| attachment.endpoint.starts_with("socket+mcast://"))
        .ok_or_else(|| anyhow::anyhow!("via management requires at least one socket network"))?;
    Ok(Connection::Ssh {
        host: attachment.ip.clone(),
        user: Some(record.ssh_user.clone()),
        port: Some(22),
        identity: Some(paths.private_key.to_string_lossy().into_owned()),
        connect_timeout_secs: Some(record.connect_timeout_secs),
        sudo: record.ssh_user != "root",
        extra_args: vec![
            "-o".to_string(),
            format!("UserKnownHostsFile={}", paths.known_hosts.display()),
        ],
        via: Some(Box::new(via.clone())),
        activation: None,
    })
}

fn connection(paths: &Paths, record: &InstanceRecord) -> Result<Connection> {
    let management = record.management()?;
    if management.uses_via() {
        return via_connection(paths, record);
    }
    if management == Management::None {
        return Ok(Connection::unavailable(
            "qemu.instance management is `none`",
        ));
    }
    let mut connection = direct_connection(paths, record)?;
    if management.execution_scoped()
        && let Connection::Ssh { activation, .. } = &mut connection
    {
        *activation = Some(Box::new(self::activation(
            paths,
            record,
            management == Management::DirectThenRemove,
        )?));
    }
    Ok(connection)
}

fn outputs(paths: &Paths, record: &InstanceRecord, pid: Option<u32>, state: &str) -> Result<Value> {
    Ok(json!({
        "pid": pid,
        "ssh_port": record.ssh_port,
        "port_forwards": record.port_forwards.iter().map(|(host, guest)| (host.to_string(), guest)).collect::<BTreeMap<_, _>>(),
        "connection": connection(paths, record)?,
        "management": record.management,
        "management_attached": pid.is_some() && record.ssh_port.is_some_and(|port| !port_available(port)),
        "management_cleanup_pending": paths.management_cleanup.is_file(),
        "egress": record.egress,
        "monitor": if cfg!(unix) { paths.monitor.to_string_lossy().into_owned() } else { "none".to_string() },
        "console": paths.console.to_string_lossy().into_owned(),
        "ips": record.attachments.iter().map(|attachment| attachment.ip.clone()).collect::<Vec<_>>(),
        "volumes": record.volumes,
        "state": state,
    }))
}

fn applied_outputs(
    paths: &Paths,
    record: &InstanceRecord,
    pid: Option<u32>,
    state: &str,
) -> Result<Value> {
    let mut outputs = outputs(paths, record, pid, state)?;
    // Applied state describes the completed execution boundary. The durable lease,
    // not this optimistic field, drives recovery if final cleanup is interrupted.
    outputs["management_cleanup_pending"] = json!(false);
    Ok(outputs)
}

async fn observe(cx: &Ctx<'_>, inputs: &Value) -> Result<Option<Actual>> {
    let paths = Paths::new(cx, inputs)?;
    let Some(record) = load_record(&paths.config)? else {
        return Ok(None);
    };
    if [
        &paths.disk,
        &paths.seed,
        &paths.private_key,
        &paths.public_key,
        &paths.user_data,
        &paths.vendor_data,
        &paths.meta_data,
        &paths.network_config,
    ]
    .iter()
    .any(|path| !path.is_file())
    {
        return Ok(None);
    }
    anyhow::ensure!(
        matches!(record.version, 1 | 2),
        "unsupported QEMU instance record version {}",
        record.version
    );
    for (index, volume) in record.volumes.iter().enumerate() {
        let path = absolute(&volume.path)?;
        anyhow::ensure!(
            path.is_file(),
            "attached QEMU volume {index} does not exist: {}",
            path.display()
        );
    }
    let state = observed_state(&paths).await;
    let pid = if state != "stopped" && is_running(&paths).await {
        live_pid(&paths)
    } else {
        None
    };
    let user_data = std::fs::read_to_string(&paths.user_data).unwrap_or_default();
    Ok(Some(Actual {
        id: Some(paths.dir.to_string_lossy().into_owned()),
        props: json!({
            "dir": record.dir_input,
            "image": record.image,
            "memory_mb": record.memory_mb,
            "cpus": record.cpus,
            "machine": record.machine,
            "cpu": record.cpu,
            "acceleration": record.acceleration,
            "disk_gb": record.disk_gb,
            "authorized_keys": record.authorized_keys,
            "ssh_port": record.ssh_port,
            "port_forwards": record.port_forwards.iter().map(|(host, guest)| (host.to_string(), guest)).collect::<BTreeMap<_, _>>(),
            "networks": record.networks,
            "user_data": user_data,
            "ssh_user": record.ssh_user,
            "hostname": record.hostname,
            "connect_timeout_secs": record.connect_timeout_secs,
            "volumes": record.volumes,
            "restartable": record.restartable,
            "management": record.management,
            "management_via": record.management_via,
            "egress": record.egress,
            "bootstrap_revision": record.bootstrap_revision,
            "state": state,
        }),
        outputs: outputs(&paths, &record, pid, state)?,
    }))
}

fn find_command(names: &[&str]) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn require_command(name: &str) -> Result<String> {
    find_command(&[name]).ok_or_else(|| {
        anyhow::anyhow!(
            "required command `{name}` was not found in PATH; install QEMU and cloud-image tooling"
        )
    })
}

fn seed_builder() -> Result<(String, &'static str)> {
    for (name, kind) in [
        ("cloud-localds", "cloud-localds"),
        ("genisoimage", "genisoimage"),
        ("mkisofs", "genisoimage"),
        ("xorriso", "xorriso"),
    ] {
        if let Some(path) = find_command(&[name]) {
            return Ok((path, kind));
        }
    }
    anyhow::bail!(
        "cannot build cloud-init seed: install `cloud-localds` (cloud-image-utils), `genisoimage`, or `xorriso`"
    )
}

fn preflight(want_running: bool) -> Result<()> {
    let _ = require_command("qemu-img")?;
    let _ = require_command("ssh-keygen")?;
    let _ = seed_builder()?;
    if want_running {
        let _ = require_command("qemu-system-x86_64")?;
    }
    Ok(())
}

async fn run_command(program: &str, args: &[String], what: &str) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running `{program}` for {what}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::bail!(
        "{what} failed with {}{}",
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

async fn prepare_attached_volumes(
    urn: &str,
    attachments: &[VolumeAttachment],
) -> Result<Vec<PreparedVolume>> {
    let qemu_img = require_command("qemu-img")?;
    let mut seen = BTreeSet::new();
    let mut prepared = Vec::with_capacity(attachments.len());
    for (index, attachment) in attachments.iter().enumerate() {
        let path = absolute(&attachment.path)?;
        let canonical = std::fs::canonicalize(&path).with_context(|| {
            format!(
                "resolving attached QEMU volume {} at {}",
                index,
                path.display()
            )
        })?;
        anyhow::ensure!(
            canonical.is_file(),
            "attached QEMU volume {index} is not a file: {}",
            canonical.display()
        );
        anyhow::ensure!(
            seen.insert(canonical.clone()),
            "attached QEMU volume {index} duplicates {}",
            canonical.display()
        );
        let output = tokio::process::Command::new(&qemu_img)
            .args(["info", "--output=json", "--force-share"])
            .arg(&canonical)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("inspecting attached QEMU volume {}", canonical.display()))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!(
                "inspecting attached QEMU volume {} failed with {}{}",
                canonical.display(),
                output.status,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        prepared.push(PreparedVolume {
            path: canonical,
            format: parse_volume_format(&output.stdout)?,
            read_only: attachment.read_only.unwrap_or(false),
            serial: attachment
                .serial
                .clone()
                .unwrap_or_else(|| derived_volume_serial(urn, &attachment.path)),
        });
    }
    Ok(prepared)
}

fn port_available(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

static RESERVED_PORTS: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();

struct PortReservation(u16);

impl Drop for PortReservation {
    fn drop(&mut self) {
        if let Some(ports) = RESERVED_PORTS.get() {
            ports
                .lock()
                .expect("reserved ports mutex poisoned")
                .remove(&self.0);
        }
    }
}

fn reserve_port(requested: Option<u64>, input: &str) -> Result<(u16, PortReservation)> {
    let ports = RESERVED_PORTS.get_or_init(Mutex::default);
    let mut ports = ports.lock().expect("reserved ports mutex poisoned");
    if let Some(requested) = requested {
        let port = u16::try_from(requested)
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| anyhow::anyhow!("`{input}` port must be between 1 and 65535"))?;
        anyhow::ensure!(
            !ports.contains(&port) && port_available(port),
            "`{input}` port {port} is already bound on 127.0.0.1; choose another port"
        );
        ports.insert(port);
        return Ok((port, PortReservation(port)));
    }
    for _ in 0..100 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("asking the OS for an available SSH port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        if ports.insert(port) {
            return Ok((port, PortReservation(port)));
        }
    }
    anyhow::bail!("could not allocate an available loopback SSH port")
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings always serialize")
}

fn validate_hostname(hostname: &str) -> Result<()> {
    anyhow::ensure!(
        !hostname.is_empty() && hostname.len() <= 253,
        "QEMU guest hostname must contain between 1 and 253 ASCII characters"
    );
    anyhow::ensure!(
        hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
        }),
        "QEMU guest hostname `{hostname}` must use dot-separated ASCII letters, digits, or hyphens; each label must begin and end with a letter or digit"
    );
    Ok(())
}

fn hostname(inputs: &Value, resource_name: &str) -> Result<String> {
    if let Some(explicit) = inputs.get("hostname").and_then(Value::as_str) {
        validate_hostname(explicit)?;
        return Ok(explicit.to_string());
    }
    if validate_hostname(resource_name).is_ok() {
        return Ok(resource_name.to_string());
    }
    let mut readable = resource_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    readable = readable.trim_matches('-').to_string();
    if readable.is_empty() {
        readable.push_str("vm");
    }
    readable.truncate(50);
    let digest = stable_bytes(&[resource_name]);
    let hostname = format!("{readable}-{}", hex::encode(&digest[..6]));
    validate_hostname(&hostname)?;
    Ok(hostname)
}

fn vendor_data(user: &str, keys: &[String]) -> String {
    let mut out = format!(
        "#cloud-config\nssh_pwauth: false\ndisable_root: {}\nusers:\n  - name: {}\n    lock_passwd: true\n    shell: /bin/bash\n",
        if user == "root" { "false" } else { "true" },
        yaml_string(user)
    );
    if user != "root" {
        out.push_str("    groups: [adm, sudo]\n    sudo: ALL=(ALL) NOPASSWD:ALL\n");
    }
    out.push_str("    ssh_authorized_keys:\n");
    for key in keys {
        out.push_str(&format!("      - {}\n", yaml_string(key)));
    }
    out
}

fn network_data(record: &InstanceRecord) -> String {
    let mut out = format!(
        "version: 2\nethernets:\n  mgmt:\n    match:\n      macaddress: {}\n    set-name: mgmt0\n    dhcp4: true\n    dhcp6: false\n    optional: true\n",
        yaml_string(&record.management_mac)
    );
    for (index, attachment) in record.attachments.iter().enumerate() {
        out.push_str(&format!(
            "  lan{index}:\n    match:\n      macaddress: {}\n    set-name: lan{index}\n",
            yaml_string(&attachment.mac)
        ));
        if attachment.dhcp {
            out.push_str("    dhcp4: true\n    dhcp6: false\n    optional: true\n");
        } else {
            out.push_str(&format!(
                "    dhcp4: false\n    dhcp6: false\n    optional: true\n    addresses: [{}/{}]\n",
                attachment.ip, attachment.prefix
            ));
        }
    }
    out
}

async fn generate_key(paths: &Paths, ssh_keygen: &str) -> Result<()> {
    if paths.private_key.exists() && paths.public_key.exists() {
        return Ok(());
    }
    run_command(
        ssh_keygen,
        &[
            "-q".into(),
            "-t".into(),
            "ed25519".into(),
            "-N".into(),
            String::new(),
            "-C".into(),
            "ifx-qemu".into(),
            "-f".into(),
            paths.private_key.to_string_lossy().into_owned(),
        ],
        "generating the instance SSH key",
    )
    .await
}

async fn build_seed(paths: &Paths, record: &InstanceRecord, inputs: &Value) -> Result<()> {
    let public_key = std::fs::read_to_string(&paths.public_key)
        .with_context(|| format!("reading {}", paths.public_key.display()))?;
    let mut keys = vec![public_key.trim().to_string()];
    keys.extend(record.authorized_keys.iter().cloned());
    let user_data = inputs
        .get("user_data")
        .and_then(Value::as_str)
        .unwrap_or("#cloud-config\n{}\n");
    std::fs::write(&paths.user_data, user_data)
        .with_context(|| format!("writing {}", paths.user_data.display()))?;
    std::fs::write(&paths.vendor_data, vendor_data(&record.ssh_user, &keys))
        .with_context(|| format!("writing {}", paths.vendor_data.display()))?;
    std::fs::write(
        &paths.meta_data,
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            record.management_mac.replace(':', ""),
            yaml_string(&record.hostname)
        ),
    )
    .with_context(|| format!("writing {}", paths.meta_data.display()))?;
    std::fs::write(&paths.network_config, network_data(record))
        .with_context(|| format!("writing {}", paths.network_config.display()))?;

    let (builder, kind) = seed_builder()?;
    let args = match kind {
        "cloud-localds" => vec![
            format!("--network-config={}", paths.network_config.display()),
            format!("--vendor-data={}", paths.vendor_data.display()),
            paths.seed.to_string_lossy().into_owned(),
            paths.user_data.to_string_lossy().into_owned(),
            paths.meta_data.to_string_lossy().into_owned(),
        ],
        "genisoimage" => vec![
            "-quiet".into(),
            "-output".into(),
            paths.seed.to_string_lossy().into_owned(),
            "-volid".into(),
            "cidata".into(),
            "-joliet".into(),
            "-rock".into(),
            paths.user_data.to_string_lossy().into_owned(),
            paths.vendor_data.to_string_lossy().into_owned(),
            paths.meta_data.to_string_lossy().into_owned(),
            paths.network_config.to_string_lossy().into_owned(),
        ],
        "xorriso" => vec![
            "-as".into(),
            "mkisofs".into(),
            "-quiet".into(),
            "-output".into(),
            paths.seed.to_string_lossy().into_owned(),
            "-volid".into(),
            "cidata".into(),
            "-joliet".into(),
            "-rock".into(),
            paths.user_data.to_string_lossy().into_owned(),
            paths.vendor_data.to_string_lossy().into_owned(),
            paths.meta_data.to_string_lossy().into_owned(),
            paths.network_config.to_string_lossy().into_owned(),
        ],
        _ => unreachable!(),
    };
    let _ = std::fs::remove_file(&paths.seed);
    run_command(&builder, &args, "building the cloud-init seed").await
}

fn kvm_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    false
}

fn accelerator(record: &InstanceRecord) -> Result<Accelerator> {
    match record.acceleration.as_str() {
        "kvm" => {
            anyhow::ensure!(
                kvm_available(),
                "QEMU acceleration is `kvm`, but /dev/kvm is unavailable or not writable; grant KVM access or select `acceleration=\"tcg\"`"
            );
            Ok(Accelerator::Kvm)
        }
        "tcg" => Ok(Accelerator::Tcg),
        "auto" if kvm_available() => Ok(Accelerator::Kvm),
        "auto" => {
            tracing::warn!(target: "ifx::qemu", "/dev/kvm is unavailable; falling back to slower TCG emulation");
            Ok(Accelerator::Tcg)
        }
        other => anyhow::bail!("unsupported QEMU acceleration `{other}`"),
    }
}

fn qemu_args(
    paths: &Paths,
    record: &InstanceRecord,
    accelerator: Accelerator,
    volumes: &[PreparedVolume],
) -> Result<Vec<String>> {
    let direct_management = record.start_with_direct_management()?;
    let mut args = vec![
        "-name".into(),
        format!("ifx-{}", record.management_mac.replace(':', "")),
        "-machine".into(),
        record.machine.clone(),
        "-m".into(),
        record.memory_mb.to_string(),
        "-smp".into(),
        record.cpus.to_string(),
        "-device".into(),
        "virtio-rng-pci".into(),
        "-drive".into(),
        format!(
            "file={},if=none,id=ifx-root,format=qcow2,discard=unmap",
            paths.disk.display()
        ),
        "-device".into(),
        "virtio-blk-pci,drive=ifx-root,id=ifx-root-device,bootindex=1".into(),
        "-drive".into(),
        format!(
            "file={},if=none,id=ifx-seed,format=raw,readonly=on",
            paths.seed.display()
        ),
        "-device".into(),
        "virtio-blk-pci,drive=ifx-seed,id=ifx-seed-device,bootindex=2".into(),
    ];
    if record.egress == "user_nat" || direct_management {
        let (netdev_id, device_id) = if record.egress == "user_nat" {
            ("uplink", "ifx-uplink")
        } else {
            ("mgmt", "ifx-mgmt")
        };
        let mut user = format!("user,id={netdev_id}");
        if direct_management {
            let ssh_port = record
                .ssh_port
                .ok_or_else(|| anyhow::anyhow!("direct QEMU management is missing its SSH port"))?;
            user.push_str(&format!(",hostfwd=tcp:127.0.0.1:{ssh_port}-:22"));
        }
        for (host, guest) in &record.port_forwards {
            user.push_str(&format!(",hostfwd=tcp:127.0.0.1:{host}-:{guest}"));
        }
        args.extend([
            "-netdev".into(),
            user,
            "-device".into(),
            format!(
                "virtio-net-pci,id={device_id},netdev={netdev_id},mac={}",
                record.management_mac
            ),
        ]);
    }
    for (index, attachment) in record.attachments.iter().enumerate() {
        args.extend([
            "-netdev".into(),
            attachment.backend.clone(),
            "-device".into(),
            format!("virtio-net-pci,netdev=lan{index},mac={}", attachment.mac),
        ]);
    }
    for (index, volume) in volumes.iter().enumerate() {
        let file_node = format!("ifx-volume-file-{index}");
        let format_node = format!("ifx-volume-format-{index}");
        args.extend([
            "-blockdev".into(),
            json!({
                "driver": "file",
                "filename": volume.path,
                "node-name": file_node,
                "auto-read-only": volume.read_only,
            })
            .to_string(),
            "-blockdev".into(),
            json!({
                "driver": volume.format,
                "file": file_node,
                "node-name": format_node,
                "read-only": volume.read_only,
            })
            .to_string(),
            "-device".into(),
            format!(
                "virtio-blk-pci,drive={format_node},id=ifx-volume-{index},serial={},bootindex={}",
                volume.serial,
                100 + index
            ),
        ]);
    }
    args.extend([
        "-display".into(),
        "none".into(),
        "-serial".into(),
        format!("file:{}", paths.console.display()),
        "-daemonize".into(),
        "-pidfile".into(),
        paths.pid.to_string_lossy().into_owned(),
        "-no-reboot".into(),
    ]);
    #[cfg(unix)]
    args.extend([
        "-qmp".into(),
        format!("unix:{},server=on,wait=off", paths.monitor.display()),
    ]);
    #[cfg(not(unix))]
    args.extend(["-monitor".into(), "none".into()]);
    args.extend(accelerator.args(record.cpu.as_deref()));
    Ok(args)
}

async fn start(paths: &Paths, record: &InstanceRecord) -> Result<()> {
    let volumes = prepare_attached_volumes(&record.management_mac, &record.volumes).await?;
    if record.start_with_direct_management()?
        && let Some(port) = record.ssh_port
    {
        anyhow::ensure!(
            port_available(port),
            "SSH port {port} is already bound on 127.0.0.1; stop the process using it or replace the VM with another `ssh_port`"
        );
    }
    for host in record.port_forwards.keys() {
        anyhow::ensure!(
            port_available(*host),
            "forwarded host port {host} is already bound on 127.0.0.1; choose another `port_forwards` key"
        );
    }
    #[cfg(unix)]
    anyhow::ensure!(
        paths.monitor.as_os_str().as_encoded_bytes().len() < 100,
        "QMP socket path is too long ({} bytes): choose a shorter QEMU `dir`",
        paths.monitor.as_os_str().as_encoded_bytes().len()
    );
    let qemu = require_command("qemu-system-x86_64")?;
    if record.start_with_direct_management()? && record.management()?.seals_bootstrap() {
        mark_ssh_activation_pending(&activation(paths, record, true)?)?;
    }
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.monitor);
    let args = qemu_args(paths, record, accelerator(record)?, &volumes)?;
    run_command(&qemu, &args, "starting QEMU").await?;
    anyhow::ensure!(
        live_pid(paths).is_some(),
        "QEMU did not remain running; inspect {}",
        paths.console.display()
    );
    Ok(())
}

async fn wait_for_connection(connection: Connection) -> Result<()> {
    let ssh = Ssh::new(connection, None);
    ssh.exec(&Cmd::new("true"))
        .await?
        .ok("waiting for QEMU guest SSH")?;
    Ok(())
}

async fn wait_for_direct_ssh(paths: &Paths, record: &InstanceRecord) -> Result<()> {
    wait_for_connection(direct_connection(paths, record)?).await
}

async fn wait_for_final_ssh(paths: &Paths, record: &InstanceRecord) -> Result<()> {
    wait_for_connection(connection(paths, record)?).await
}

async fn settle_started_instance(
    cx: &Ctx<'_>,
    paths: &Paths,
    record: &mut InstanceRecord,
) -> Result<()> {
    let management = record.management()?;
    if record.bootstrap_complete {
        if management == Management::Direct {
            wait_for_direct_ssh(paths, record).await?;
        } else if management.uses_via() {
            wait_for_final_ssh(paths, record).await?;
        }
        return Ok(());
    }

    if management.needs_bootstrap_direct() {
        wait_for_direct_ssh(paths, record).await?;
    } else if management.uses_via() {
        wait_for_final_ssh(paths, record).await?;
    }

    record.bootstrap_complete = true;
    write_atomic(&paths.config, &serde_json::to_vec_pretty(record)?)?;
    match management {
        Management::DirectThenVia => {
            close_ssh_activation(&activation(paths, record, true)?).await?;
            wait_for_final_ssh(paths, record).await?;
        }
        Management::DirectThenRemove | Management::Immutable => {
            // The direct NIC is still present from bootstrap. Put its execution-scoped
            // connection in the pool so the engine removes it at the apply boundary.
            cx.transports
                .get(&connection(paths, record)?)
                .await
                .exec(&Cmd::new("true"))
                .await?
                .ok("retaining bootstrap management until the apply completes")?;
        }
        Management::Direct | Management::Via | Management::None => {}
    }
    Ok(())
}

async fn finish_instance_management(
    cx: &Ctx<'_>,
    paths: &Paths,
    record: &InstanceRecord,
) -> Result<()> {
    let connection = connection(paths, record)?;
    if connection.has_execution_scoped_access() {
        cx.transports.finish_connection(&connection).await?;
    }
    Ok(())
}

async fn stop_after_error(cx: &Ctx<'_>, paths: &Paths, record: &InstanceRecord) {
    let _ = finish_instance_management(cx, paths, record).await;
    let _ = stop(paths).await;
}

fn activation(paths: &Paths, record: &InstanceRecord, reopen: bool) -> Result<SshActivation> {
    let persistent_netdev = record.egress == "user_nat";
    Ok(SshActivation {
        monitor: paths.monitor.to_string_lossy().into_owned(),
        lease: paths.management_cleanup.to_string_lossy().into_owned(),
        host_port: record
            .ssh_port
            .ok_or_else(|| anyhow::anyhow!("temporary management is missing its SSH port"))?,
        guest_port: 22,
        netdev_id: if persistent_netdev { "uplink" } else { "mgmt" }.to_string(),
        device_id: if persistent_netdev {
            "ifx-uplink"
        } else {
            "ifx-mgmt"
        }
        .to_string(),
        mac: record.management_mac.clone(),
        persistent_netdev,
        reopen,
    })
}

async fn stop(paths: &Paths) -> Result<()> {
    let Some(pid) = live_pid(paths) else {
        let _ = std::fs::remove_file(&paths.pid);
        let _ = std::fs::remove_file(&paths.management_cleanup);
        return Ok(());
    };
    #[cfg(unix)]
    if paths.monitor.exists() {
        let _ = qmp_command(&paths.monitor, "system_powerdown").await;
        let graceful_deadline = Instant::now() + Duration::from_secs(30);
        while expected_process(pid, &paths.disk) && Instant::now() < graceful_deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !expected_process(pid, &paths.disk) {
            let _ = std::fs::remove_file(&paths.pid);
            let _ = std::fs::remove_file(&paths.monitor);
            let _ = std::fs::remove_file(&paths.management_cleanup);
            return Ok(());
        }
        let _ = qmp_command(&paths.monitor, "quit").await;
    }
    #[cfg(unix)]
    if expected_process(pid, &paths.disk) {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    if expected_process(pid, &paths.disk) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status();
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while expected_process(pid, &paths.disk) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::ensure!(
        !expected_process(pid, &paths.disk),
        "QEMU process {pid} did not stop; inspect {}",
        paths.console.display()
    );
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.monitor);
    let _ = std::fs::remove_file(&paths.management_cleanup);
    Ok(())
}

async fn prepare(
    cx: &Ctx<'_>,
    inputs: &Value,
    port: Option<u16>,
) -> Result<(Paths, InstanceRecord)> {
    let hostname = hostname(inputs, cx.urn.name())?;
    let paths = Paths::new(cx, inputs)?;
    let qemu_img = require_command("qemu-img")?;
    let ssh_keygen = require_command("ssh-keygen")?;
    let _ = seed_builder()?;
    let image_input = required_string(inputs, "image")?;
    let image = absolute(image_input)?;
    anyhow::ensure!(
        image.is_file(),
        "QEMU base image does not exist: {}",
        image.display()
    );
    let networks = string_list(inputs, "networks")?;
    let attachments = networks
        .iter()
        .enumerate()
        .map(|(index, endpoint)| parse_attachment(endpoint, cx.urn.as_str(), index))
        .collect::<Result<Vec<_>>>()?;
    let management_name = inputs
        .get("management")
        .and_then(Value::as_str)
        .unwrap_or("direct");
    let management = Management::parse(management_name)?;
    let management_via = inputs
        .get("management_via")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<Connection>(value.clone()))
        .transpose()
        .context("decoding `management_via`")?;
    validate_management_via(management, management_via.as_ref())?;
    if management.uses_via() {
        anyhow::ensure!(
            attachments
                .iter()
                .any(|attachment| attachment.endpoint.starts_with("socket+mcast://")),
            "via management requires at least one socket network"
        );
    }
    let egress = inputs
        .get("egress")
        .and_then(Value::as_str)
        .unwrap_or("user_nat");
    anyhow::ensure!(
        matches!(egress, "user_nat" | "none"),
        "unsupported QEMU egress policy `{egress}`"
    );
    let forwards = port_forwards(inputs)?;
    anyhow::ensure!(
        forwards.is_empty() || egress == "user_nat" || management == Management::Direct,
        "`port_forwards` requires persistent `user_nat` egress unless management is permanently direct"
    );
    let record = InstanceRecord {
        version: 2,
        dir_input: required_string(inputs, "dir")?.to_string(),
        image: image_input.to_string(),
        memory_mb: integer(inputs, "memory_mb", 1024)?,
        cpus: integer(inputs, "cpus", 1)?,
        machine: inputs
            .get("machine")
            .and_then(Value::as_str)
            .unwrap_or("q35")
            .to_string(),
        cpu: inputs
            .get("cpu")
            .and_then(Value::as_str)
            .map(str::to_string),
        acceleration: inputs
            .get("acceleration")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string(),
        disk_gb: integer(inputs, "disk_gb", 10)?,
        authorized_keys: string_list(inputs, "authorized_keys")?,
        ssh_port: port,
        port_forwards: forwards,
        networks,
        ssh_user: inputs
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or("debian")
            .to_string(),
        hostname,
        connect_timeout_secs: integer(inputs, "connect_timeout_secs", 300)?,
        attachments,
        volumes: volume_attachments(inputs)?,
        restartable: inputs
            .get("restartable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        management_mac: mac(&[cx.urn.as_str(), "management"]),
        management: management_name.to_string(),
        management_via,
        egress: egress.to_string(),
        bootstrap_complete: false,
        bootstrap_revision: inputs
            .get("bootstrap_revision")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string(),
    };

    if paths.dir.exists() {
        anyhow::ensure!(
            live_pid(&paths).is_none(),
            "refusing to replace partial instance data while its QEMU process is running"
        );
        std::fs::remove_dir_all(&paths.dir).with_context(|| {
            format!(
                "removing incomplete instance directory {}",
                paths.dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    generate_key(&paths, &ssh_keygen).await?;
    build_seed(&paths, &record, inputs).await?;
    run_command(
        &qemu_img,
        &[
            "create".into(),
            "-q".into(),
            "-f".into(),
            "qcow2".into(),
            "-F".into(),
            "qcow2".into(),
            "-b".into(),
            image.to_string_lossy().into_owned(),
            paths.disk.to_string_lossy().into_owned(),
        ],
        "creating the qcow2 overlay",
    )
    .await?;
    run_command(
        &qemu_img,
        &[
            "resize".into(),
            "-q".into(),
            paths.disk.to_string_lossy().into_owned(),
            format!("{}G", record.disk_gb),
        ],
        "resizing the qcow2 overlay",
    )
    .await?;
    write_atomic(&paths.config, &(serde_json::to_vec_pretty(&record)?))?;
    Ok((paths, record))
}

#[async_trait]
impl Handler for InstanceHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A local QEMU virtual machine with lifecycle-controlled SSH management for `host.*` resources.",
        )
        .input(field("dir", FieldType::String).required().replace().doc(
            "Persistent QEMU working directory for disks, seeds, keys, pidfiles, and monitor sockets.",
        ))
        .input(field("image", FieldType::String).required().replace().doc(
            "Path to a pristine qcow2 cloud image, normally `qemu.image.path`.",
        ))
        .input(field("memory_mb", FieldType::Int).default(1024).replace().doc(
            "Guest memory in MiB; changing it replaces the instance.",
        ))
        .input(field("cpus", FieldType::Int).default(1).replace().doc(
            "Virtual CPU count; changing it replaces the instance.",
        ))
        .input(
            field("machine", FieldType::open_enum(["q35", "pc"]))
                .default("q35")
                .replace()
                .doc("QEMU x86 machine type; other values supported by the installed QEMU are accepted."),
        )
        .input(field("cpu", FieldType::String).replace().doc(
            "Optional QEMU CPU model; defaults to `host` with KVM and `max` with TCG.",
        ))
        .input(
            field(
                "acceleration",
                FieldType::enumeration(["auto", "kvm", "tcg"]),
            )
            .default("auto")
            .replace()
            .doc("Execution accelerator: detect KVM with TCG fallback, require KVM, or force TCG."),
        )
        .input(field("disk_gb", FieldType::Int).default(10).replace().doc(
            "Overlay disk size in GiB; changing it replaces the instance.",
        ))
        .input(
            field("authorized_keys", FieldType::list(FieldType::String))
                .default(json!([]))
                .replace()
                .doc("Additional public SSH keys installed alongside the provider-managed key."),
        )
        .input(field("user_data", FieldType::String).default("").sensitive().replace().doc(
            "Optional cloud-init user data. Provider SSH access is supplied separately as vendor data.",
        ))
        .input(field("ssh_port", FieldType::Int).replace().doc(
            "Loopback port used by direct or execution-scoped SSH; omit to allocate an available port.",
        ))
        .input(
            field(
                "management",
                FieldType::enumeration([
                    "direct",
                    "via",
                    "direct_then_via",
                    "direct_then_remove",
                    "immutable",
                    "none",
                ]),
            )
            .default("direct")
            .replace()
            .doc(
                "Guest-management lifecycle. Transitional modes remove direct host access after bootstrap; `direct_then_remove` reopens it only for changed applies, while `immutable` requires replacement.",
            ),
        )
        .input(
            field("management_via", FieldType::Connection)
                .doc(
                    "Bastion connection for `via` and `direct_then_via`; nested connections retain per-hop SSH identities.",
                ),
        )
        .input(
            field("egress", FieldType::enumeration(["user_nat", "none"]))
                .default("user_nat")
                .replace()
                .doc(
                    "Steady-state outbound NIC, independent of management exposure. `user_nat` has no SSH host forward unless management is direct or temporarily activated.",
                ),
        )
        .input(
            field("bootstrap_revision", FieldType::String)
                .default("1")
                .replace()
                .doc(
                    "Operator-controlled replacement token for immutable guests; increment it to rebuild and bootstrap a new instance.",
                ),
        )
        .input(
            field("networks", FieldType::list(FieldType::String))
                .default(json!([]))
                .replace()
                .doc("`qemu.network.endpoint` references for additional guest NICs."),
        )
        .input(
            field("port_forwards", FieldType::map(FieldType::Int))
                .default(json!({}))
                .replace()
                .doc(
                    "Additional loopback TCP forwards as `{host_port: guest_port}`; useful for local HTTP/TCP checks.",
                ),
        )
        .input(
            field("ssh_user", FieldType::open_enum(["debian", "root"]))
                .default("debian")
                .replace()
                .doc("Cloud guest account used by the SSH connection output."),
        )
        .input(field("hostname", FieldType::String).replace().doc(
            "Guest hostname supplied through NoCloud metadata; omitted values are derived from the resource name.",
        ))
        .input(field("connect_timeout_secs", FieldType::Int).default(300).doc(
            "Seconds to wait for SSH during boot and from downstream `host.*` resources.",
        ))
        .input(
            field("volumes", FieldType::list(volume_attachment_type()))
                .default(json!([]))
                .doc(
                    "Durable data disks, normally built from `qemu.volume.path`; changing attachments updates a stopped VM or performs an approved restart.",
                ),
        )
        .input(
            field("restartable", FieldType::Bool)
                .default(false)
                .doc(
                    "Allow IFX to restart a live VM for attachment changes. Both the applied and desired declarations must opt in; otherwise the restart requires approval.",
                ),
        )
        .input(
            field(
                "state",
                FieldType::enumeration(["running", "paused", "stopped"]),
            )
                .default("running")
                .doc("Desired VM power state; start, pause, resume, and stop are in-place updates."),
        )
        .output(field("pid", FieldType::Int).doc("Live local QEMU process id, or null while stopped."))
        .output(field("ssh_port", FieldType::Int).doc("Allocated loopback SSH port, or null when direct access is never used."))
        .output(
            field("port_forwards", FieldType::map(FieldType::Int))
                .doc("Configured host-to-guest TCP port forwards."),
        )
        .output(field("connection", FieldType::Connection).doc(
            "SSH connection to the guest; pass it to `host.*.on(...)` exactly like a cloud instance connection.",
        ))
        .output(field("management", FieldType::String).doc("Configured management lifecycle."))
        .output(field("management_attached", FieldType::Bool).doc("Whether direct host SSH access is currently attached."))
        .output(field("management_cleanup_pending", FieldType::Bool).doc("Whether interrupted execution left durable management cleanup intent."))
        .output(field("egress", FieldType::String).doc("Configured steady-state egress policy."))
        .output(field("monitor", FieldType::String).doc("QMP monitor socket path, or `none` where unavailable."))
        .output(field("console", FieldType::String).doc("Serial console log path."))
        .output(field("ips", FieldType::list(FieldType::String)).doc(
            "Guest IPv4 addresses in the same order as the attached `networks`.",
        ))
        .output(field("volumes", FieldType::list(volume_attachment_type())).doc(
            "Observed virtio data-disk attachments in guest device order.",
        ))
        .output(field("state", FieldType::enumeration(["running", "paused", "stopped"])).doc("Observed VM power state."))
    }

    fn risks(
        &self,
        operation: OperationKind,
        desired: &Value,
        actual: Option<&Actual>,
    ) -> Result<Vec<OperationRisk>> {
        let Some(actual) = actual else {
            return Ok(Vec::new());
        };
        let implicit_restart = match operation {
            OperationKind::Replace => live_instance(actual),
            OperationKind::Update | OperationKind::Trigger => {
                live_instance(actual) && volume_inputs(desired) != volume_inputs(&actual.props)
            }
            OperationKind::Create | OperationKind::Delete => false,
        };
        if implicit_restart && !restart_permitted(desired, actual) {
            return Ok(vec![OperationRisk {
                name: "instance-restart".into(),
                reason: "the operation must restart a live VM, but both its applied and desired `restartable` declarations have not opted in".into(),
            }]);
        }
        Ok(Vec::new())
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        observe(cx, inputs).await
    }

    async fn recover_execution_access(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<bool> {
        let paths = Paths::new(cx, inputs)?;
        if !paths.management_cleanup.is_file() {
            return Ok(false);
        }
        let Some(record) = load_record(&paths.config)? else {
            std::fs::remove_file(&paths.management_cleanup)
                .with_context(|| format!("removing {}", paths.management_cleanup.display()))?;
            return Ok(true);
        };
        if live_pid(&paths).is_none() {
            clear_ssh_activation_pending(&activation(&paths, &record, true)?)?;
        } else if record.bootstrap_complete {
            close_ssh_activation(&activation(&paths, &record, true)?).await?;
        } else {
            // A partially bootstrapped via/immutable guest cannot safely resume after
            // its direct path is sealed. Stop it so the next apply repeats bootstrap.
            stop(&paths).await?;
        }
        Ok(true)
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let schema = self.schema();
        let replace = schema.replace_fields().collect::<Vec<_>>();
        let mut diff = Diff::generic(desired, &actual.props, &replace);
        let management = desired
            .get("management")
            .and_then(Value::as_str)
            .unwrap_or("direct");
        let should_be_sealed = matches!(
            management,
            "direct_then_via" | "direct_then_remove" | "immutable"
        );
        let cleanup_pending = actual
            .outputs
            .get("management_cleanup_pending")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let unexpectedly_attached = actual
            .outputs
            .get("management_attached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if should_be_sealed && (cleanup_pending || unexpectedly_attached) {
            diff.changes.push(FieldChange {
                field: "management_cleanup".into(),
                from: Some(json!("pending")),
                to: Some(json!("sealed")),
                forces_replace: false,
            });
        }
        Ok(diff)
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let desired_state = inputs
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("running");
        let want_live = desired_state != "stopped";
        preflight(want_live)?;
        let management = Management::parse(
            inputs
                .get("management")
                .and_then(Value::as_str)
                .unwrap_or("direct"),
        )?;
        let requested = inputs.get("ssh_port").and_then(Value::as_u64);
        anyhow::ensure!(
            requested.is_none() || management.needs_bootstrap_direct(),
            "`ssh_port` is only valid for direct or direct-bootstrap management"
        );
        let (port, reservation) = if management.needs_bootstrap_direct() {
            let (port, reservation) = reserve_port(requested, "ssh_port")?;
            (Some(port), Some(reservation))
        } else {
            (None, None)
        };
        let mut reservations = reservation.into_iter().collect::<Vec<_>>();
        for host in port_forwards(inputs)?.keys() {
            reservations.push(reserve_port(Some(u64::from(*host)), "port_forwards")?.1);
        }
        let prepared = prepare(cx, inputs, port).await;
        let (paths, mut record) = match prepared {
            Ok(value) => value,
            Err(error) => return Err(error),
        };
        if want_live {
            if let Err(error) = start(&paths, &record).await {
                let _ = stop(&paths).await;
                return Err(error);
            }
            drop(reservations);
            if let Err(error) = settle_started_instance(cx, &paths, &mut record).await {
                stop_after_error(cx, &paths, &record).await;
                return Err(error);
            }
            if desired_state == "paused" {
                if let Err(error) = set_paused(&paths, true).await {
                    stop_after_error(cx, &paths, &record).await;
                    return Err(error);
                }
            }
        }
        let state = observed_state(&paths).await;
        let pid = live_pid(&paths);
        Ok(Applied {
            id: Some(paths.dir.to_string_lossy().into_owned()),
            outputs: applied_outputs(&paths, &record, pid, state)?,
        })
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let paths = Paths::new(cx, inputs)?;
        let mut record = load_record(&paths.config)?
            .ok_or_else(|| anyhow::anyhow!("cannot update missing QEMU instance record"))?;
        let current_state = observed_state(&paths).await;
        let direct_attached =
            current_state != "stopped" && record.ssh_port.is_some_and(|port| !port_available(port));
        if paths.management_cleanup.is_file()
            || (record.management()?.seals_bootstrap() && direct_attached)
        {
            if current_state == "stopped" {
                clear_ssh_activation_pending(&activation(&paths, &record, true)?)?;
            } else if record.bootstrap_complete {
                close_ssh_activation(&activation(&paths, &record, true)?).await?;
            } else {
                settle_started_instance(cx, &paths, &mut record).await?;
            }
        }
        let desired_volumes = volume_attachments(inputs)?;
        let volumes_changed = desired_volumes != record.volumes;
        if volumes_changed {
            prepare_attached_volumes(&record.management_mac, &desired_volumes).await?;
        }
        let was_live = observed_state(&paths).await != "stopped";
        if volumes_changed && was_live {
            stop(&paths).await?;
        }
        record.connect_timeout_secs = integer(inputs, "connect_timeout_secs", 300)?;
        record.management_via = inputs
            .get("management_via")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value::<Connection>(value.clone()))
            .transpose()
            .context("decoding `management_via`")?;
        validate_management_via(record.management()?, record.management_via.as_ref())?;
        record.volumes = desired_volumes;
        record.restartable = inputs
            .get("restartable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        write_atomic(&paths.config, &(serde_json::to_vec_pretty(&record)?))?;
        let desired_state = inputs
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("running");
        let mut current_state = observed_state(&paths).await;
        if desired_state != "stopped" && current_state == "stopped" {
            let mut reservations = Vec::new();
            if record.start_with_direct_management()? {
                let port = record.ssh_port.ok_or_else(|| {
                    anyhow::anyhow!("direct QEMU management is missing its SSH port")
                })?;
                reservations.push(reserve_port(Some(u64::from(port)), "ssh_port")?.1);
            }
            for host in record.port_forwards.keys() {
                reservations.push(reserve_port(Some(u64::from(*host)), "port_forwards")?.1);
            }
            if let Err(error) = start(&paths, &record).await {
                let _ = stop(&paths).await;
                return Err(error);
            }
            drop(reservations);
            if let Err(error) = settle_started_instance(cx, &paths, &mut record).await {
                stop_after_error(cx, &paths, &record).await;
                return Err(error);
            }
            current_state = "running";
        }
        if desired_state == "running" && current_state == "paused" {
            set_paused(&paths, false).await?;
        } else if desired_state == "paused" && current_state == "running" {
            set_paused(&paths, true).await?;
        } else if desired_state == "stopped" && current_state != "stopped" {
            stop(&paths).await?;
        }
        let state = observed_state(&paths).await;
        let pid = live_pid(&paths);
        Ok(Applied {
            id: id
                .map(str::to_string)
                .or_else(|| Some(paths.dir.to_string_lossy().into_owned())),
            outputs: applied_outputs(&paths, &record, pid, state)?,
        })
    }

    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        let paths = Paths::new(cx, inputs)?;
        if let Some(record) = load_record(&paths.config)? {
            finish_instance_management(cx, &paths, &record).await?;
        }
        stop(&paths).await?;
        match std::fs::remove_dir_all(&paths.dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", paths.dir.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        let root = PathBuf::from("/tmp/ifx-qemu/instances/node-a");
        Paths {
            config: root.join("config.json"),
            disk: root.join("disk.qcow2"),
            seed: root.join("seed.iso"),
            user_data: root.join("user-data"),
            vendor_data: root.join("vendor-data"),
            meta_data: root.join("meta-data"),
            network_config: root.join("network-config"),
            private_key: root.join("id_ed25519"),
            public_key: root.join("id_ed25519.pub"),
            known_hosts: root.join("known_hosts"),
            pid: root.join("qemu.pid"),
            monitor: root.join("qmp.sock"),
            console: root.join("console.log"),
            management_cleanup: root.join("management-cleanup-pending"),
            dir: root,
        }
    }

    fn record() -> InstanceRecord {
        InstanceRecord {
            version: 2,
            dir_input: "/tmp/ifx-qemu".into(),
            image: "/tmp/debian.qcow2".into(),
            memory_mb: 1024,
            cpus: 2,
            machine: "q35".into(),
            cpu: None,
            acceleration: "auto".into(),
            disk_gb: 10,
            authorized_keys: Vec::new(),
            ssh_port: Some(22022),
            port_forwards: BTreeMap::from([(18080, 80)]),
            networks: vec!["socket+mcast://239.192.1.2:22000?cidr=10.42.7.0/24".into()],
            ssh_user: "debian".into(),
            hostname: "node-a".into(),
            connect_timeout_secs: 300,
            attachments: vec![
                parse_attachment(
                    "socket+mcast://239.192.1.2:22000?cidr=10.42.7.0/24",
                    "qemu.instance:node-a",
                    0,
                )
                .unwrap(),
            ],
            volumes: Vec::new(),
            restartable: false,
            management_mac: "52:54:00:12:34:56".into(),
            management: "direct".into(),
            management_via: None,
            egress: "user_nat".into(),
            bootstrap_complete: true,
            bootstrap_revision: "1".into(),
        }
    }

    #[test]
    fn argv_has_management_forward_socket_lan_and_tcg_fallback() {
        let mut record = record();
        record.machine = "pc".into();
        record.cpu = Some("qemu64".into());
        let args = qemu_args(&paths(), &record, Accelerator::Tcg, &[]).unwrap();
        assert!(args.iter().any(|arg| {
            arg.starts_with("user,id=uplink,hostfwd=tcp:127.0.0.1:22022-:22")
                && arg.contains("hostfwd=tcp:127.0.0.1:18080-:80")
        }));
        assert!(args.contains(&"socket,id=lan0,mcast=239.192.1.2:22000".to_string()));
        assert!(args.windows(2).any(|args| args == ["-machine", "pc"]));
        assert!(args.contains(&"tcg,thread=multi".to_string()));
        assert!(args.contains(&"qemu64".to_string()));
    }

    #[test]
    fn argv_uses_json_block_nodes_for_typed_data_volumes() {
        let volume = PreparedVolume {
            path: PathBuf::from("/var/lib/ifx/data,one.qcow2"),
            format: "qcow2".into(),
            read_only: true,
            serial: "ifx-data".into(),
        };
        let args = qemu_args(&paths(), &record(), Accelerator::Kvm, &[volume]).unwrap();
        let block_nodes = args
            .windows(2)
            .filter(|pair| pair[0] == "-blockdev")
            .map(|pair| serde_json::from_str::<Value>(&pair[1]).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(block_nodes.len(), 2);
        assert_eq!(block_nodes[0]["filename"], "/var/lib/ifx/data,one.qcow2");
        assert_eq!(block_nodes[1]["driver"], "qcow2");
        assert_eq!(block_nodes[1]["read-only"], true);
        assert!(args.iter().any(|arg| arg.contains("serial=ifx-data")));
        assert!(
            args.iter()
                .any(|arg| arg.contains("ifx-root-device,bootindex=1"))
        );
        assert!(
            args.iter()
                .any(|arg| arg.contains("serial=ifx-data,bootindex=100"))
        );
    }

    #[test]
    fn direct_then_remove_drops_its_management_nic_after_bootstrap() {
        let mut record = record();
        record.management = "direct_then_remove".into();
        record.egress = "none".into();
        record.bootstrap_complete = false;
        record.port_forwards.clear();

        let bootstrap = qemu_args(&paths(), &record, Accelerator::Tcg, &[]).unwrap();
        assert!(
            bootstrap
                .iter()
                .any(|argument| { argument == "user,id=mgmt,hostfwd=tcp:127.0.0.1:22022-:22" })
        );
        assert!(
            bootstrap
                .iter()
                .any(|argument| argument.contains("id=ifx-mgmt"))
        );

        record.bootstrap_complete = true;
        let steady = qemu_args(&paths(), &record, Accelerator::Tcg, &[]).unwrap();
        assert!(
            !steady
                .iter()
                .any(|argument| argument.starts_with("user,id="))
        );
        assert!(
            !steady
                .iter()
                .any(|argument| argument.contains("id=ifx-mgmt"))
        );

        let connection = connection(&paths(), &record).unwrap();
        let activation = connection.activation().unwrap();
        assert!(!activation.persistent_netdev);
        assert!(activation.reopen);
    }

    #[test]
    fn temporary_management_does_not_remove_steady_state_egress() {
        let mut record = record();
        record.management = "direct_then_remove".into();
        record.bootstrap_complete = true;

        let args = qemu_args(&paths(), &record, Accelerator::Tcg, &[]).unwrap();
        let uplink = args
            .iter()
            .find(|argument| argument.starts_with("user,id=uplink"))
            .unwrap();
        assert!(!uplink.contains("hostfwd=tcp:127.0.0.1:22022-:22"));
        let connection = connection(&paths(), &record).unwrap();
        let activation = connection.activation().unwrap();
        assert!(activation.persistent_netdev);
        assert_eq!(activation.netdev_id, "uplink");
    }

    #[test]
    fn every_management_lifecycle_emits_its_final_connection_policy() {
        let paths = paths();
        let bastion = direct_connection(&paths, &record()).unwrap();

        let mut direct = record();
        direct.management = "direct".into();
        assert!(connection(&paths, &direct).unwrap().activation().is_none());

        for mode in ["via", "direct_then_via"] {
            let mut via = record();
            via.management = mode.into();
            via.management_via = Some(bastion.clone());
            let connection = connection(&paths, &via).unwrap();
            let Connection::Ssh {
                host,
                via: Some(route),
                activation: None,
                ..
            } = connection
            else {
                panic!("{mode} did not emit routed SSH")
            };
            assert_eq!(host, via.attachments[0].ip);
            assert_eq!(*route, bastion);
        }

        let mut temporary = record();
        temporary.management = "direct_then_remove".into();
        assert!(
            connection(&paths, &temporary)
                .unwrap()
                .activation()
                .unwrap()
                .reopen
        );

        let mut immutable = record();
        immutable.management = "immutable".into();
        assert!(
            !connection(&paths, &immutable)
                .unwrap()
                .activation()
                .unwrap()
                .reopen
        );

        let mut none = record();
        none.management = "none".into();
        assert!(matches!(
            connection(&paths, &none).unwrap(),
            Connection::Unavailable { .. }
        ));
    }

    #[test]
    fn interrupted_management_is_planned_as_recoverable_update() {
        let desired = json!({"management": "direct_then_remove"});
        let actual = Actual {
            id: Some("instance".into()),
            props: desired.clone(),
            outputs: json!({
                "management_attached": true,
                "management_cleanup_pending": true,
            }),
        };

        let diff = InstanceHandler.diff(&desired, &actual).unwrap();
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].field, "management_cleanup");
        assert!(!diff.changes[0].forces_replace);
    }

    #[test]
    fn attached_volume_format_and_serial_validation_are_strict() {
        assert_eq!(parse_volume_format(br#"{"format":"raw"}"#).unwrap(), "raw");
        assert!(parse_volume_format(br#"{"format":"vmdk"}"#).is_err());
        assert!(
            volume_attachments(&json!({"volumes": [{"path": "/data", "serial": "bad serial"}]}))
                .is_err()
        );
        assert_eq!(
            derived_volume_serial("qemu.instance:node-a", "/data/one"),
            derived_volume_serial("qemu.instance:node-a", "/data/one")
        );
    }

    #[test]
    fn live_attachment_changes_require_restart_consent_or_approval() {
        let handler = InstanceHandler;
        let actual = Actual {
            id: Some("instance".into()),
            props: json!({
                "state": "running",
                "volumes": [],
                "restartable": false,
            }),
            outputs: Value::Null,
        };
        let desired = json!({
            "volumes": [{"path": "/data/one.qcow2"}],
            "restartable": false,
        });
        let risks = handler
            .risks(OperationKind::Update, &desired, Some(&actual))
            .unwrap();
        assert_eq!(risks[0].name, "instance-restart");

        let opted_in_actual = Actual {
            props: json!({
                "state": "running",
                "volumes": [],
                "restartable": true,
            }),
            ..actual.clone()
        };
        let mut opted_in_desired = desired;
        opted_in_desired["restartable"] = json!(true);
        assert!(
            handler
                .risks(
                    OperationKind::Update,
                    &opted_in_desired,
                    Some(&opted_in_actual)
                )
                .unwrap()
                .is_empty()
        );

        let stopped = Actual {
            props: json!({
                "state": "stopped",
                "volumes": [],
                "restartable": false,
            }),
            ..actual
        };
        assert!(
            handler
                .risks(OperationKind::Update, &opted_in_desired, Some(&stopped))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pidfile_and_qmp_status_parsing_reject_bad_values() {
        assert_eq!(parse_pid(" 1234\n"), Some(1234));
        assert_eq!(parse_pid("0"), None);
        assert_eq!(parse_pid("oops"), None);
        assert_eq!(
            qmp_status_running(&json!({"return": {"status": "running"}})),
            Some(true)
        );
        assert_eq!(
            qmp_status_running(&json!({"return": {"status": "shutdown"}})),
            Some(false)
        );
        assert_eq!(
            qmp_status(&json!({"return": {"status": "paused"}})),
            Some("paused")
        );
        assert_eq!(qmp_status_running(&json!({"event": "STOP"})), None);
    }

    #[test]
    fn socket_ip_and_mac_are_stable_per_instance() {
        let endpoint = "socket+mcast://239.192.1.2:22000?cidr=10.42.7.0/24";
        let a = parse_attachment(endpoint, "qemu.instance:a", 0).unwrap();
        let again = parse_attachment(endpoint, "qemu.instance:a", 0).unwrap();
        let b = parse_attachment(endpoint, "qemu.instance:b", 0).unwrap();
        assert_eq!(a.ip, again.ip);
        assert_eq!(a.mac, again.mac);
        assert_ne!(a.ip, b.ip);
        assert_ne!(a.mac, b.mac);
        assert!(a.ip.starts_with("10.42.7."));
    }

    #[test]
    fn cloud_init_network_data_matches_every_mac() {
        let text = network_data(&record());
        assert!(text.contains("set-name: mgmt0"));
        assert!(text.contains("set-name: lan0"));
        assert!(text.contains("addresses: [10.42.7."));
        assert_eq!(text.matches("optional: true").count(), 2);
    }

    #[test]
    fn hostnames_are_rfc1123_labels_and_yaml_quoted() {
        for hostname in ["node-a", "router.lab", "123"] {
            validate_hostname(hostname).unwrap();
        }
        for hostname in ["", "-node", "node-", "node_name", "node:\nroot: true"] {
            assert!(
                validate_hostname(hostname).is_err(),
                "accepted {hostname:?}"
            );
        }
        assert_eq!(yaml_string("node-a"), "\"node-a\"");
        assert_eq!(hostname(&json!({}), "node-a").unwrap(), "node-a");
        let derived = hostname(&json!({}), "node_a").unwrap();
        assert!(derived.starts_with("node-a-"), "{derived}");
        validate_hostname(&derived).unwrap();
        assert!(hostname(&json!({"hostname": "node_name"}), "node-a").is_err());
    }

    #[tokio::test]
    async fn matching_stopped_instance_is_adoptable_without_a_diff() {
        use crate::model::Urn;
        use crate::transport::TransportPool;

        let temporary = tempfile::tempdir().unwrap();
        let dir = temporary.path().join("qemu");
        let inputs = json!({
            "dir": dir,
            "image": "/tmp/base.qcow2",
            "state": "stopped",
        });
        let urn = Urn::new(TYPE, "adopt-me");
        let transports = TransportPool::default();
        let cx = Ctx {
            urn: &urn,
            transports: &transports,
            triggered: false,
        };
        let paths = Paths::new(&cx, &inputs).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        for path in [
            &paths.disk,
            &paths.seed,
            &paths.private_key,
            &paths.public_key,
            &paths.vendor_data,
            &paths.meta_data,
            &paths.network_config,
        ] {
            std::fs::write(path, "").unwrap();
        }
        let mut record = record();
        record.dir_input = dir.to_string_lossy().into_owned();
        record.image = "/tmp/base.qcow2".into();
        record.memory_mb = 1024;
        record.cpus = 1;
        record.disk_gb = 10;
        record.port_forwards.clear();
        record.networks.clear();
        record.attachments.clear();
        record.ssh_user = "debian".into();
        record.connect_timeout_secs = 300;
        write_atomic(&paths.config, &serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        std::fs::write(&paths.user_data, "").unwrap();

        let mut desired = inputs;
        let handler = InstanceHandler;
        handler.schema().apply_defaults(&mut desired);
        let actual = observe(&cx, &desired).await.unwrap().unwrap();
        assert!(handler.diff(&desired, &actual).unwrap().is_empty());
    }

    #[tokio::test]
    async fn incomplete_instance_is_observed_as_absent() {
        use crate::model::Urn;
        use crate::transport::TransportPool;

        let temporary = tempfile::tempdir().unwrap();
        let inputs = json!({"dir": temporary.path(), "image": "/tmp/base.qcow2"});
        let urn = Urn::new(TYPE, "incomplete");
        let transports = TransportPool::default();
        let cx = Ctx {
            urn: &urn,
            transports: &transports,
            triggered: false,
        };
        let paths = Paths::new(&cx, &inputs).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        write_atomic(
            &paths.config,
            &serde_json::to_vec_pretty(&record()).unwrap(),
        )
        .unwrap();

        assert!(observe(&cx, &inputs).await.unwrap().is_none());
    }
}
