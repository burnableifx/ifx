//! `linode.instance`: a Linode virtual machine, with an SSH [`Connection`] output so
//! host-scoped resources can configure it.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::api::{Linode, js};
use super::catalog;
use crate::model::Connection;
use crate::provider::{Actual, Applied, Ctx, Diff, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "linode.instance";

pub struct InstanceHandler {
    api: Arc<Linode>,
}

impl InstanceHandler {
    pub fn new(api: Arc<Linode>) -> Self {
        Self { api }
    }

    async fn find_by_label(&self, label: &str) -> Result<Option<Value>> {
        let items = self
            .api
            .list("/linode/instances", Some(&json!({"label": label})))
            .await?;
        Ok(items
            .into_iter()
            .find(|v| js::str(v, "label") == Some(label)))
    }

    /// Id of the firewall attached to the instance, if any (`null` otherwise).
    async fn firewall_id(&self, id: &str) -> Result<Value> {
        let fws = self
            .api
            .list(&format!("/linode/instances/{id}/firewalls"), None)
            .await?;
        Ok(fws.first().map(|f| js::get(f, "id")).unwrap_or(Value::Null))
    }

    async fn observe(&self, v: Value, inputs: &Value) -> Result<Actual> {
        let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("instance has no id: {v}"))?;
        let id = id.to_string();
        let firewall_id = self.firewall_id(&id).await?;
        let ipv4s = ipv4s(&v);
        let props = json!({
            "label": js::get(&v, "label"),
            "region": js::get(&v, "region"),
            "type": js::get(&v, "type"),
            "image": js::get(&v, "image"),
            // The API never returns these create-only values. Carry the last applied
            // inputs so changing one still produces a replacement diff.
            "backup_id": js::get(inputs, "backup_id"),
            "root_pass": js::get(inputs, "root_pass"),
            "authorized_keys": js::get(inputs, "authorized_keys"),
            "authorized_users": js::get(inputs, "authorized_users"),
            "disk_encryption": js::get(&v, "disk_encryption"),
            "boot_size": js::get(inputs, "boot_size"),
            "swap_size": js::get(inputs, "swap_size"),
            "interface_generation": js::get(&v, "interface_generation"),
            "interfaces": js::get(inputs, "interfaces"),
            "ipv4": js::get(inputs, "ipv4"),
            "kernel": js::get(inputs, "kernel"),
            "network_helper": js::get(inputs, "network_helper"),
            "placement_group_id": v.pointer("/placement_group/id").cloned().unwrap_or(Value::Null),
            "stackscript_id": js::get(inputs, "stackscript_id"),
            "stackscript_data": js::get(inputs, "stackscript_data"),
            "maintenance_policy": js::get(&v, "maintenance_policy"),
            "watchdog_enabled": js::get(&v, "watchdog_enabled"),
            "alerts": managed_object(&v, "/alerts", inputs, "alerts"),
            "tags": js::get(&v, "tags"),
            "private_ip": ipv4s.iter().any(|a| is_private(a)),
            "backups_enabled": v.pointer("/backups/enabled").and_then(Value::as_bool).unwrap_or(false),
            "backup_schedule": managed_object(&v, "/backups/schedule", inputs, "backup_schedule"),
            "booted": booted(js::str(&v, "status").unwrap_or("")),
            "firewall_id": firewall_id,
            "user_data": js::get(inputs, "user_data"),
            // Not API state: carried from the last applied inputs so a change is a diff.
            "ssh_user": js::get(inputs, "ssh_user"),
            "connect_timeout_secs": js::get(inputs, "connect_timeout_secs"),
        });
        Ok(Actual {
            id: Some(id),
            props,
            outputs: outputs(&v, inputs),
        })
    }

    async fn wait_settled(&self, path: &str, what: &str, want_running: bool) -> Result<Value> {
        let target = if want_running { "running" } else { "offline" };
        self.api
            .wait_until(
                path,
                what,
                |v| js::str(v, "status") == Some(target),
                |v| format!("status={}", js::str(v, "status").unwrap_or("?")),
            )
            .await
    }
}

fn managed_object(observed: &Value, pointer: &str, inputs: &Value, field: &str) -> Value {
    let Some(wanted) = inputs.get(field).and_then(Value::as_object) else {
        return observed.pointer(pointer).cloned().unwrap_or(Value::Null);
    };
    let actual = observed.pointer(pointer).and_then(Value::as_object);
    Value::Object(
        wanted
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    actual
                        .and_then(|object| object.get(key))
                        .cloned()
                        .unwrap_or(Value::Null),
                )
            })
            .collect(),
    )
}

fn ipv4s(v: &Value) -> Vec<String> {
    v.get("ipv4")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_private(addr: &str) -> bool {
    addr.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_private())
}

fn booted(status: &str) -> bool {
    !matches!(status, "offline" | "stopped" | "shutting_down")
}

fn outputs(v: &Value, inputs: &Value) -> Value {
    let all = ipv4s(v);
    let public = all.iter().find(|a| !is_private(a)).cloned();
    let ipv6 = js::str(v, "ipv6").map(|s| s.split('/').next().unwrap_or(s).to_string());
    let user = js::str(inputs, "ssh_user").unwrap_or("root").to_string();
    let connection = Connection::Ssh {
        host: public.clone().or_else(|| ipv6.clone()).unwrap_or_default(),
        user: Some(user),
        port: None,
        identity: None,
        connect_timeout_secs: inputs.get("connect_timeout_secs").and_then(Value::as_u64),
        sudo: false,
        extra_args: Vec::new(),
        via: None,
        activation: None,
    };
    json!({
        "id": js::get(v, "id"),
        "label": js::get(v, "label"),
        "ipv4": public,
        "ipv4s": all,
        "ipv6": ipv6,
        "status": js::get(v, "status"),
        "region": js::get(v, "region"),
        "type": js::get(v, "type"),
        "disk_encryption": js::get(v, "disk_encryption"),
        "interface_generation": js::get(v, "interface_generation"),
        "maintenance_policy": js::get(v, "maintenance_policy"),
        "watchdog_enabled": js::get(v, "watchdog_enabled"),
        "connection": connection,
    })
}

fn normalize(v: &Value) -> Value {
    let mut v = v.clone();
    js::map_key(&mut v, "tags", js::sorted);
    v
}

fn validate_backup_settings(inputs: &Value, currently_enabled: bool) -> Result<()> {
    let enabled = js::bool(inputs, "backups_enabled").unwrap_or(currently_enabled);
    anyhow::ensure!(
        enabled
            || !inputs
                .get("backup_schedule")
                .is_some_and(|value| !value.is_null()),
        "`backup_schedule` requires `backups_enabled` to be true"
    );
    Ok(())
}

fn validate_image_authentication(inputs: &Value) -> Result<()> {
    if inputs.get("image").is_none_or(Value::is_null) {
        return Ok(());
    }
    let password = js::str(inputs, "root_pass").is_some_and(|value| !value.is_empty());
    let has_list = |field: &str| {
        inputs
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty())
    };
    anyhow::ensure!(
        password || has_list("authorized_keys") || has_list("authorized_users"),
        "image provisioning requires `root_pass`, `authorized_keys`, or `authorized_users`"
    );
    Ok(())
}

fn validate_source(inputs: &Value) -> Result<()> {
    let has_image = inputs.get("image").is_some_and(|value| !value.is_null());
    let has_backup = inputs
        .get("backup_id")
        .is_some_and(|value| !value.is_null());
    anyhow::ensure!(
        inputs
            .get("stackscript_id")
            .is_none_or(|value| value.is_null() || has_image),
        "`stackscript_id` requires an `image` source"
    );
    anyhow::ensure!(
        has_image || has_backup || js::bool(inputs, "booted") == Some(false),
        "empty Linode provisioning requires `booted` to be false"
    );
    Ok(())
}

#[async_trait]
impl Handler for InstanceHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A Linode virtual machine. Outputs an SSH connection so `host.*` resources can \
             configure it once it is running.",
        )
        .input(field("label", FieldType::String).required().doc(
            "Unique name shown in the Linode Cloud Manager; an existing instance with this label \
             is adopted instead of created.",
        ))
        .input(
            field("region", FieldType::open_enum(catalog::regions()))
                .required()
                .replace()
                .doc(
                    "Data-center slug, e.g. `us-east` or `eu-central` (`linode-cli regions list`).",
                ),
        )
        .input(
            field("type", FieldType::open_enum(catalog::types()))
                .required()
                .doc(
                    "Plan slug that sets CPU/RAM/disk, e.g. `g6-nanode-1`; changing it resizes the \
             instance in place (with a reboot).",
                ),
        )
        .input(
            field("image", FieldType::open_enum(catalog::images()))
                .group("source")
                .replace()
                .doc("Operating-system image slug the disk is built from, e.g. `linode/debian12`."),
        )
        .input(
            field("backup_id", FieldType::Int)
                .group("source")
                .replace()
                .doc("Backup id to restore into the new instance; mutually exclusive with `image`."),
        )
        .input(
            field("root_pass", FieldType::String)
                .sensitive()
                .replace()
                .doc("Root password set at first boot; never read back from the API."),
        )
        .input(
            field("authorized_keys", FieldType::list(FieldType::String))
                .replace()
                .doc("Public SSH keys written to root's `authorized_keys` at first boot."),
        )
        .input(
            field("authorized_users", FieldType::list(FieldType::String))
                .replace()
                .doc(
                    "Linode account usernames whose profile SSH keys are installed at first boot.",
                ),
        )
        .input(
            field("disk_encryption", FieldType::enumeration(["enabled", "disabled"]))
                .replace()
                .doc("Local disk encryption policy selected when the instance is created."),
        )
        .input(field("boot_size", FieldType::Int).replace().doc(
            "Primary boot disk size in MiB; remaining plan storage stays unallocated.",
        ))
        .input(field("swap_size", FieldType::Int).replace().doc(
            "Swap disk size in MiB (Linode defaults to 512).",
        ))
        .input(
            field(
                "interface_generation",
                FieldType::enumeration(["legacy_config", "linode"]),
            )
            .replace()
            .doc("Networking model selected at creation; this cannot be changed in place."),
        )
        .input(
            field("interfaces", FieldType::list(FieldType::Any))
                .replace()
                .doc("Linode or legacy interface definitions passed to the create API."),
        )
        .input(
            field("ipv4", FieldType::list(FieldType::String))
                .replace()
                .doc("An unassigned reserved public IPv4 address to assign at creation."),
        )
        .input(field("kernel", FieldType::String).replace().doc(
            "Kernel id selected for the initial configuration profile.",
        ))
        .input(field("network_helper", FieldType::Bool).replace().doc(
            "Enable Network Helper for Linode-interface networking.",
        ))
        .input(field("placement_group_id", FieldType::Int).replace().doc(
            "Placement group to join at creation; it must be in the selected region.",
        ))
        .input(field("stackscript_id", FieldType::Int).replace().doc(
            "StackScript id to run during image deployment.",
        ))
        .input(
            field("stackscript_data", FieldType::Any)
                .sensitive()
                .replace()
                .doc("StackScript UDF values; treated as sensitive because they commonly contain credentials."),
        )
        .input(
            field("tags", FieldType::list(FieldType::String))
                .doc("Free-form labels for grouping and filtering in the Cloud Manager."),
        )
        .input(field("private_ip", FieldType::Bool).replace().doc(
            "Also assign a private (192.168.x.x) address reachable from other Linodes in the \
             same region.",
        ))
        .input(
            field("backups_enabled", FieldType::Bool)
                .doc("Enrol in the paid Linode Backup Service (daily snapshots)."),
        )
        .input(
            field(
                "backup_schedule",
                FieldType::object_named("BackupSchedule", vec![
                    field(
                        "day",
                        FieldType::open_enum([
                            "Scheduling",
                            "Sunday",
                            "Monday",
                            "Tuesday",
                            "Wednesday",
                            "Thursday",
                            "Friday",
                            "Saturday",
                        ]),
                    )
                    .doc("Preferred backup day, or `Scheduling` for automatic assignment."),
                    field("window", FieldType::String)
                        .doc("Preferred two-hour UTC window (`W0` through `W22`)."),
                ]),
            )
            .doc("Paid Backup Service schedule."),
        )
        .input(
            field(
                "maintenance_policy",
                FieldType::enumeration(["linode/migrate", "linode/power_off_on"]),
            )
            .doc("Prefer live migration or power-off/on during host maintenance."),
        )
        .input(field("watchdog_enabled", FieldType::Bool).doc(
            "Enable Lassie to restart an instance that powers off unexpectedly.",
        ))
        .input(
            field(
                "alerts",
                FieldType::object_named("InstanceAlerts", vec![
                    field("cpu", FieldType::Int).doc("CPU usage threshold."),
                    field("io", FieldType::Int).doc("Disk I/O threshold."),
                    field("network_in", FieldType::Int).doc("Inbound network threshold."),
                    field("network_out", FieldType::Int).doc("Outbound network threshold."),
                    field("transfer_quota", FieldType::Int).doc("Transfer quota percentage threshold."),
                ]),
            )
            .doc("Cloud Manager alert thresholds."),
        )
        .input(
            field("booted", FieldType::Bool)
                .default(true)
                .doc("Keep the instance powered on (`true`) or shut down (`false`)."),
        )
        .input(field("firewall_id", FieldType::Int).replace().doc(
            "Id of a Cloud Firewall to attach at creation; prefer `linode.firewall(linodes=...)` \
             for attachments that can change later.",
        ))
        .input(
            field("user_data", FieldType::String)
                .sensitive()
                .replace()
                .doc(
                    "cloud-init user data (e.g. a `#cloud-config` document) run on first boot; \
                     base64-encoded for you.",
                ),
        )
        .input(
            field("ssh_user", FieldType::String)
                .default("root")
                .doc("Login user for the `connection` output; not sent to Linode."),
        )
        .input(field("connect_timeout_secs", FieldType::Int).doc(
            "Seconds `host.*` resources keep retrying the first SSH connection while the \
             instance finishes booting.",
        ))
        .output(field("id", FieldType::Int).doc("Numeric Linode id."))
        .output(field("label", FieldType::String).doc("Instance label."))
        .output(field("ipv4", FieldType::String).doc("First public IPv4 address."))
        .output(
            field("ipv4s", FieldType::list(FieldType::String))
                .doc("Every IPv4 address, public and private."),
        )
        .output(
            field("ipv6", FieldType::String).doc("Public IPv6 address (without prefix length)."),
        )
        .output(
            field("status", FieldType::String)
                .doc("Lifecycle state reported by Linode, e.g. `running` or `offline`."),
        )
        .output(field("region", FieldType::String).doc("Region slug."))
        .output(field("type", FieldType::String).doc("Plan slug."))
        .output(field("disk_encryption", FieldType::String).doc("Observed disk encryption policy."))
        .output(field("interface_generation", FieldType::String).doc("Observed networking model."))
        .output(field("maintenance_policy", FieldType::String).doc("Observed host maintenance policy."))
        .output(field("watchdog_enabled", FieldType::Bool).doc("Whether Lassie is enabled."))
        .output(field("connection", FieldType::Connection).doc(
            "SSH connection to the public IPv4 as `ssh_user`; pass as `on=` to `host.*` resources.",
        ))
    }

    async fn read(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let mut found = match id {
            Some(id) => self.api.get_opt(&format!("/linode/instances/{id}")).await?,
            None => None,
        };
        if found.is_none()
            && let Some(label) = js::str(inputs, "label")
        {
            found = self.find_by_label(label).await?;
        }
        match found {
            Some(v) => Ok(Some(self.observe(v, inputs).await?)),
            None => Ok(None),
        }
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let schema = self.schema();
        let replace: Vec<&str> = schema.replace_fields().collect();
        Ok(Diff::generic(
            &normalize(desired),
            &normalize(&actual.props),
            &replace,
        ))
    }

    async fn create(&self, _cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        validate_backup_settings(inputs, false)?;
        validate_image_authentication(inputs)?;
        validate_source(inputs)?;
        let mut body = Map::new();
        js::copy(
            &mut body,
            inputs,
            &[
                "label",
                "region",
                "type",
                "image",
                "backup_id",
                "root_pass",
                "authorized_keys",
                "authorized_users",
                "disk_encryption",
                "boot_size",
                "swap_size",
                "interface_generation",
                "interfaces",
                "ipv4",
                "kernel",
                "network_helper",
                "stackscript_id",
                "stackscript_data",
                "tags",
                "private_ip",
                "backups_enabled",
                "booted",
                "firewall_id",
            ],
        );
        if let Some(id) = inputs
            .get("placement_group_id")
            .filter(|value| !value.is_null())
        {
            body.insert("placement_group".into(), json!({"id": id}));
        }
        if let Some(ud) = js::str(inputs, "user_data") {
            let encoded = base64::engine::general_purpose::STANDARD.encode(ud);
            body.insert("metadata".into(), json!({"user_data": encoded}));
        }
        let created = self
            .api
            .post("/linode/instances", &Value::Object(body))
            .await?;
        let id = js::i64(&created, "id").ok_or_else(|| anyhow::anyhow!("create returned no id"))?;
        let path = format!("/linode/instances/{id}");
        let want_running = js::bool(inputs, "booted").unwrap_or(true);
        let mut v = self
            .wait_settled(&path, "instance provisioning", want_running)
            .await?;
        let mut settings = Map::new();
        js::copy(
            &mut settings,
            inputs,
            &["alerts", "maintenance_policy", "watchdog_enabled"],
        );
        if let Some(schedule) = inputs
            .get("backup_schedule")
            .filter(|value| !value.is_null())
        {
            settings.insert("backups".into(), json!({"schedule": schedule}));
        }
        if !settings.is_empty() {
            v = self.api.put(&path, &Value::Object(settings)).await?;
        }
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v, inputs),
        })
    }

    async fn update(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
        actual: &Actual,
    ) -> Result<Applied> {
        validate_backup_settings(
            inputs,
            js::bool(&actual.props, "backups_enabled") == Some(true),
        )?;
        let id = id.ok_or_else(|| anyhow::anyhow!("update without id"))?;
        let path = format!("/linode/instances/{id}");
        let props = &actual.props;
        let want_running = js::bool(inputs, "booted").unwrap_or(true);

        if let Some(ty) = js::str(inputs, "type")
            && js::str(props, "type") != Some(ty)
        {
            let body = json!({"type": ty, "allow_auto_disk_resize": true});
            self.api.post(&format!("{path}/resize"), &body).await?;
            let settled = ["running", "offline"];
            self.api
                .wait_until(
                    &path,
                    "instance resize",
                    |v| {
                        js::str(v, "type") == Some(ty)
                            && settled.contains(&js::str(v, "status").unwrap_or(""))
                    },
                    |v| {
                        format!(
                            "type={} status={}",
                            js::str(v, "type").unwrap_or("?"),
                            js::str(v, "status").unwrap_or("?")
                        )
                    },
                )
                .await?;
        }

        if let Some(want) = js::bool(inputs, "backups_enabled")
            && js::bool(props, "backups_enabled") != Some(want)
        {
            let action = if want { "enable" } else { "cancel" };
            self.api
                .post(&format!("{path}/backups/{action}"), &json!({}))
                .await?;
        }

        let mut patch = Map::new();
        for k in [
            "label",
            "tags",
            "alerts",
            "maintenance_policy",
            "watchdog_enabled",
        ] {
            if let Some(dv) = inputs.get(k).filter(|v| !v.is_null())
                && js::sorted(dv) != js::sorted(&js::get(props, k))
            {
                patch.insert(k.into(), dv.clone());
            }
        }
        if let Some(schedule) = inputs
            .get("backup_schedule")
            .filter(|value| !value.is_null())
            && props.get("backup_schedule") != Some(schedule)
        {
            patch.insert("backups".into(), json!({"schedule": schedule}));
        }
        if !patch.is_empty() {
            self.api.put(&path, &Value::Object(patch)).await?;
        }

        if js::bool(props, "booted") != Some(want_running) {
            let action = if want_running { "boot" } else { "shutdown" };
            self.api
                .post(&format!("{path}/{action}"), &json!({}))
                .await?;
            self.wait_settled(&path, &format!("instance {action}"), want_running)
                .await?;
        }

        let v = self.api.get(&path).await?;
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v, inputs),
        })
    }

    async fn delete(&self, _cx: &Ctx<'_>, id: Option<&str>, _inputs: &Value) -> Result<()> {
        let Some(id) = id else { return Ok(()) };
        let path = format!("/linode/instances/{id}");
        self.api.delete(&path).await?;
        self.api.wait_gone(&path, "instance deletion").await
    }
}
