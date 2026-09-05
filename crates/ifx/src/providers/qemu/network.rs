//! `qemu.network`: a persisted rootless network endpoint consumed by QEMU instances.

use std::path::PathBuf;

use anyhow::Context as _;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{required_string, resource_path, write_atomic};
use crate::provider::{Actual, Applied, Ctx, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "qemu.network";

#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkHandler;

#[derive(Debug, Deserialize, Serialize)]
struct NetworkRecord {
    id: String,
    name: String,
    mode: String,
    dir: String,
    endpoint: String,
    #[serde(default)]
    cidr: String,
}

fn record_path(cx: &Ctx<'_>, inputs: &Value) -> Result<PathBuf> {
    Ok(resource_path(
        required_string(inputs, "dir")?,
        "networks",
        cx.urn.name(),
        "json",
    ))
}

fn validate_cidr(cidr: &str) -> Result<()> {
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid QEMU network CIDR `{cidr}`"))?;
    let address: std::net::Ipv4Addr = address
        .parse()
        .with_context(|| format!("invalid QEMU network address `{address}`"))?;
    anyhow::ensure!(
        prefix == "24" && address.octets()[3] == 0,
        "QEMU networks currently require a /24 network address, got `{cidr}`"
    );
    anyhow::ensure!(
        address.is_private(),
        "QEMU network `{cidr}` must use RFC 1918 private address space"
    );
    Ok(())
}

fn allocate(
    name: &str,
    mode: &str,
    dir: &str,
    requested_cidr: Option<&str>,
) -> Result<NetworkRecord> {
    anyhow::ensure!(
        matches!(mode, "socket" | "user"),
        "unsupported QEMU network mode `{mode}`"
    );
    let digest = Sha256::digest(format!("{dir}\0{name}").as_bytes());
    let id = format!("{name}-{}", hex::encode(&digest[..6]));
    let default_cidr = format!("10.{}.{}.0/24", 1 + digest[5] % 253, digest[6]);
    let cidr = requested_cidr.unwrap_or(&default_cidr);
    validate_cidr(cidr)?;
    let endpoint = if mode == "socket" {
        let multicast = format!("239.{}.{}.{}", 192 + (digest[0] & 3), digest[1], digest[2]);
        let port = 20_000 + u16::from_be_bytes([digest[3], digest[4]]) % 30_000;
        format!("socket+mcast://{multicast}:{port}?cidr={cidr}")
    } else {
        format!("user://{}?cidr={cidr}", hex::encode(&digest[..8]))
    };
    Ok(NetworkRecord {
        id,
        name: name.to_string(),
        mode: mode.to_string(),
        dir: dir.to_string(),
        endpoint,
        cidr: cidr.to_string(),
    })
}

fn outputs(record: &NetworkRecord) -> Value {
    json!({
        "id": record.id,
        "endpoint": record.endpoint,
        "cidr": record.cidr,
    })
}

fn actual(record: NetworkRecord) -> Actual {
    Actual {
        id: Some(record.id.clone()),
        props: json!({
            "name": record.name,
            "mode": record.mode,
            "dir": record.dir,
            "cidr": record.cidr,
        }),
        outputs: outputs(&record),
    }
}

fn load(path: &std::path::Path) -> Result<Option<NetworkRecord>> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut record: NetworkRecord = serde_json::from_slice(&data)
        .with_context(|| format!("parsing QEMU network record {}", path.display()))?;
    if record.cidr.is_empty() {
        record.cidr = record
            .endpoint
            .split_once("?cidr=")
            .map(|(_, cidr)| cidr.to_string())
            .unwrap_or_default();
    }
    Ok(Some(record))
}

#[async_trait]
impl Handler for NetworkHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A rootless local QEMU network. Socket mode is a multicast Ethernet bus shared by attached VMs.",
        )
        .input(
            field("name", FieldType::String)
                .required()
                .replace()
                .doc("Stable network name used to allocate its endpoint and guest subnet."),
        )
        .input(
            field("mode", FieldType::enumeration(["socket", "user"]))
                .default("socket")
                .replace()
                .doc("`socket` interconnects VMs; `user` adds an isolated outbound-only NIC."),
        )
        .input(
            field("dir", FieldType::String)
                .required()
                .replace()
                .doc("Persistent QEMU working directory containing the network record."),
        )
        .input(
            field("cidr", FieldType::String)
                .replace()
                .doc("Optional RFC 1918 /24 guest subnet; omitted values are allocated deterministically."),
        )
        .output(field("id", FieldType::String).doc("Stable local network identifier."))
        .output(field("cidr", FieldType::String).doc("Allocated guest subnet."))
        .output(field("endpoint", FieldType::String).doc(
            "Provider endpoint consumed by `qemu.instance.networks`; referencing it creates a graph edge.",
        ))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        Ok(load(&record_path(cx, inputs)?)?.map(actual))
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let record = allocate(
            required_string(inputs, "name")?,
            inputs
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("socket"),
            required_string(inputs, "dir")?,
            inputs.get("cidr").and_then(Value::as_str),
        )?;
        let path = record_path(cx, inputs)?;
        write_atomic(&path, &(serde_json::to_vec_pretty(&record)?))?;
        Ok(Applied {
            id: Some(record.id.clone()),
            outputs: outputs(&record),
        })
    }

    async fn update(
        &self,
        _cx: &Ctx<'_>,
        _id: Option<&str>,
        _inputs: &Value,
        actual: &Actual,
    ) -> Result<Applied> {
        // All inputs replace. A trigger may still call update, which is intentionally a
        // no-op that preserves the observed endpoint.
        Ok(Applied {
            id: actual.id.clone(),
            outputs: actual.outputs.clone(),
        })
    }

    async fn delete(&self, cx: &Ctx<'_>, _id: Option<&str>, inputs: &Value) -> Result<()> {
        let path = record_path(cx, inputs)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_allocation_is_stable_and_encodes_a_bus_and_subnet() {
        let first = allocate("lab", "socket", "/tmp/ifx", None).unwrap();
        let second = allocate("lab", "socket", "/tmp/ifx", None).unwrap();
        assert_eq!(first.endpoint, second.endpoint);
        assert!(
            first.endpoint.starts_with("socket+mcast://239."),
            "{}",
            first.endpoint
        );
        assert!(first.endpoint.contains("?cidr=10."), "{}", first.endpoint);
        assert!(first.endpoint.ends_with(".0/24"), "{}", first.endpoint);
    }

    #[test]
    fn user_allocation_is_explicitly_not_a_shared_bus() {
        let record = allocate("outbound", "user", ".ifx/qemu", Some("172.20.4.0/24")).unwrap();
        assert!(record.endpoint.starts_with("user://"));
        assert!(record.endpoint.ends_with("?cidr=172.20.4.0/24"));
    }

    #[test]
    fn rejects_public_or_non_network_cidrs() {
        assert!(allocate("bad", "socket", "/tmp/ifx", Some("192.0.2.0/24")).is_err());
        assert!(allocate("bad", "socket", "/tmp/ifx", Some("10.2.3.4/24")).is_err());
        assert!(allocate("bad", "socket", "/tmp/ifx", Some("10.2.0.0/16")).is_err());
    }
}
