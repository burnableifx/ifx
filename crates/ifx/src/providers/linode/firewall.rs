//! `linode.firewall`: a Cloud Firewall with inbound/outbound rules, attached to
//! instances by id.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::api::{Linode, js};
use crate::provider::{Actual, Applied, Ctx, Diff, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "linode.firewall";

pub struct FirewallHandler {
    api: Arc<Linode>,
}

impl FirewallHandler {
    pub fn new(api: Arc<Linode>) -> Self {
        Self { api }
    }

    async fn find_by_label(&self, label: &str) -> Result<Option<Value>> {
        let items = self
            .api
            .list("/networking/firewalls", Some(&json!({"label": label})))
            .await?;
        Ok(items
            .into_iter()
            .find(|v| js::str(v, "label") == Some(label)))
    }

    /// `(device_id, entity_id, entity_type)` for every managed attachment.
    async fn devices(&self, id: &str) -> Result<Vec<(i64, i64, String)>> {
        let devs = self
            .api
            .list(&format!("/networking/firewalls/{id}/devices"), None)
            .await?;
        Ok(devs
            .iter()
            .filter_map(|d| {
                Some((
                    js::i64(d, "id")?,
                    d.pointer("/entity/id")?.as_i64()?,
                    d.pointer("/entity/type")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    async fn observe(&self, v: Value) -> Result<Actual> {
        let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("firewall has no id: {v}"))?;
        let id = id.to_string();
        let devices = self.devices(&id).await?;
        let linodes = entity_ids(&devices, "linode");
        let interfaces = entity_ids(&devices, "linode_interface");
        let nodebalancers = entity_ids(&devices, "nodebalancer");
        let rules = v.get("rules").cloned().unwrap_or(Value::Null);
        let props = json!({
            "label": js::get(&v, "label"),
            "status": js::get(&v, "status"),
            "inbound_policy": js::get(&rules, "inbound_policy"),
            "outbound_policy": js::get(&rules, "outbound_policy"),
            "inbound": normalize_rules(&js::get(&rules, "inbound")),
            "outbound": normalize_rules(&js::get(&rules, "outbound")),
            "linodes": linodes,
            "interfaces": interfaces,
            "nodebalancers": nodebalancers,
            "tags": js::get(&v, "tags"),
        });
        Ok(Actual {
            id: Some(id),
            props,
            outputs: outputs(&v),
        })
    }
}

fn outputs(v: &Value) -> Value {
    json!({"id": js::get(v, "id"), "status": js::get(v, "status")})
}

/// Keep only the fields we manage, drop nulls and empty address lists, so desired and
/// observed rules compare equal when they mean the same thing.
fn normalize_rule(r: &Value) -> Value {
    let mut m = Map::new();
    js::copy(
        &mut m,
        r,
        &["label", "action", "protocol", "ports", "description"],
    );
    let mut addrs = Map::new();
    if let Some(a) = r.get("addresses") {
        for k in ["ipv4", "ipv6"] {
            if let Some(list) = a.get(k).and_then(Value::as_array).filter(|l| !l.is_empty()) {
                addrs.insert(k.into(), Value::Array(list.clone()));
            }
        }
    }
    m.insert("addresses".into(), Value::Object(addrs));
    Value::Object(m)
}

fn normalize_rules(v: &Value) -> Value {
    match v.as_array() {
        Some(a) => Value::Array(a.iter().map(normalize_rule).collect()),
        None => json!([]),
    }
}

fn normalize(v: &Value) -> Value {
    let mut v = v.clone();
    js::map_key(&mut v, "inbound", normalize_rules);
    js::map_key(&mut v, "outbound", normalize_rules);
    js::map_key(&mut v, "linodes", js::sorted);
    js::map_key(&mut v, "interfaces", js::sorted);
    js::map_key(&mut v, "nodebalancers", js::sorted);
    js::map_key(&mut v, "tags", js::sorted);
    v
}

fn rules_body(inputs: &Value) -> Value {
    json!({
        "inbound": normalize_rules(&js::get(inputs, "inbound")),
        "inbound_policy": js::str(inputs, "inbound_policy").unwrap_or("DROP"),
        "outbound": normalize_rules(&js::get(inputs, "outbound")),
        "outbound_policy": js::str(inputs, "outbound_policy").unwrap_or("ACCEPT"),
    })
}

fn ids(v: &Value, field: &str) -> BTreeSet<i64> {
    v.get(field)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn entity_ids(devices: &[(i64, i64, String)], kind: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = devices
        .iter()
        .filter(|(_, _, actual)| {
            actual == kind || (kind == "linode_interface" && actual == "interface")
        })
        .map(|(_, id, _)| *id)
        .collect();
    ids.sort_unstable();
    ids
}

fn rule_type() -> FieldType {
    FieldType::object_named("Rule", vec![
        field("label", FieldType::String).doc("Optional short name for the rule, e.g. `allow-ssh`."),
        field("action", FieldType::enumeration(["ACCEPT", "DROP"]))
            .required()
            .doc("Allow (`ACCEPT`) or block (`DROP`) traffic matching this rule."),
        field("protocol", FieldType::enumeration(["TCP", "UDP", "ICMP", "IPENCAP"]))
            .required()
            .doc("Transport protocol the rule matches."),
        field("ports", FieldType::String)
            .doc("Port list or ranges, e.g. `22`, `80,443` or `1000-2000`; omit for ICMP."),
        field(
            "addresses",
            FieldType::object_named("Addresses", vec![
                field("ipv4", FieldType::list(FieldType::String))
                    .doc("IPv4 CIDR blocks, e.g. `0.0.0.0/0` for anywhere."),
                field("ipv6", FieldType::list(FieldType::String))
                    .doc("IPv6 CIDR blocks, e.g. `::/0` for anywhere."),
            ]),
        )
        .required()
        .doc("Remote addresses the rule applies to (sources for inbound, destinations for outbound)."),
        field("description", FieldType::String).doc("Free-text note shown in the Cloud Manager."),
    ])
}

#[async_trait]
impl Handler for FirewallHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A Linode Cloud Firewall: network rules enforced outside the instance, attached to \
             one or more instances.",
        )
        .input(field("label", FieldType::String).required().doc(
            "Unique firewall name; an existing firewall with this label is adopted instead of \
             created.",
        ))
        .input(
            field("status", FieldType::enumeration(["enabled", "disabled"]))
                .default("enabled")
                .doc("Whether this firewall currently enforces its rules."),
        )
        .input(
            field("inbound_policy", FieldType::enumeration(["ACCEPT", "DROP"]))
                .default("DROP")
                .doc("What happens to incoming traffic no `inbound` rule matches."),
        )
        .input(
            field(
                "outbound_policy",
                FieldType::enumeration(["ACCEPT", "DROP"]),
            )
            .default("ACCEPT")
            .doc("What happens to outgoing traffic no `outbound` rule matches."),
        )
        .input(
            field("inbound", FieldType::list(rule_type()))
                .doc("Rules for traffic arriving at the instances, evaluated in order."),
        )
        .input(
            field("outbound", FieldType::list(rule_type()))
                .doc("Rules for traffic leaving the instances, evaluated in order."),
        )
        .input(
            field("linodes", FieldType::list(FieldType::Int)).doc(
                "Ids of legacy-interface instances to protect, usually references like `web.id`.",
            ),
        )
        .input(
            field("interfaces", FieldType::list(FieldType::Int))
                .doc("Ids of public or VPC Linode interfaces to protect."),
        )
        .input(
            field("nodebalancers", FieldType::list(FieldType::Int))
                .doc("Ids of NodeBalancers to protect; only inbound TCP rules apply."),
        )
        .input(
            field("tags", FieldType::list(FieldType::String))
                .doc("Free-form labels for grouping and filtering in the Cloud Manager."),
        )
        .output(field("id", FieldType::Int).doc("Numeric firewall id."))
        .output(field("status", FieldType::String).doc("`enabled`, `disabled` or `deleted`."))
    }

    async fn read(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let mut found = match id {
            Some(id) => {
                self.api
                    .get_opt(&format!("/networking/firewalls/{id}"))
                    .await?
            }
            None => None,
        };
        if found.is_none()
            && let Some(label) = js::str(inputs, "label")
        {
            found = self.find_by_label(label).await?;
        }
        match found {
            Some(v) => Ok(Some(self.observe(v).await?)),
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
        let mut body = Map::new();
        js::copy(&mut body, inputs, &["label", "tags"]);
        body.insert("rules".into(), rules_body(inputs));
        let mut devices = Map::new();
        for field in ["linodes", "interfaces", "nodebalancers"] {
            let wanted: Vec<i64> = ids(inputs, field).into_iter().collect();
            if !wanted.is_empty() {
                devices.insert(field.into(), json!(wanted));
            }
        }
        if !devices.is_empty() {
            body.insert("devices".into(), Value::Object(devices));
        }
        let mut v = self
            .api
            .post("/networking/firewalls", &Value::Object(body))
            .await?;
        let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("create returned no id"))?;
        if js::str(inputs, "status") == Some("disabled") {
            v = self
                .api
                .put(
                    &format!("/networking/firewalls/{id}"),
                    &json!({"status": "disabled"}),
                )
                .await?;
        }
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v),
        })
    }

    async fn update(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
        actual: &Actual,
    ) -> Result<Applied> {
        let id = id.ok_or_else(|| anyhow::anyhow!("update without id"))?;
        let path = format!("/networking/firewalls/{id}");
        let desired = normalize(inputs);
        let props = normalize(&actual.props);

        let mut patch = Map::new();
        for k in ["label", "status", "tags"] {
            if let Some(dv) = desired.get(k).filter(|v| !v.is_null())
                && props.get(k) != Some(dv)
            {
                patch.insert(k.into(), dv.clone());
            }
        }
        if !patch.is_empty() {
            self.api.put(&path, &Value::Object(patch)).await?;
        }

        let rules = rules_body(&desired);
        let rules_changed = ["inbound", "inbound_policy", "outbound", "outbound_policy"]
            .iter()
            .any(|k| rules.get(k) != props.get(k));
        if rules_changed {
            self.api.put(&format!("{path}/rules"), &rules).await?;
        }

        let attachment_types = [
            ("linodes", "linode"),
            ("interfaces", "linode_interface"),
            ("nodebalancers", "nodebalancer"),
        ];
        if attachment_types
            .iter()
            .any(|(field, _)| inputs.get(*field).is_some_and(|v| !v.is_null()))
        {
            let have = self.devices(id).await?;
            for (field, kind) in attachment_types {
                if !inputs.get(field).is_some_and(|v| !v.is_null()) {
                    continue;
                }
                let want = ids(&desired, field);
                for (device_id, entity_id, actual_kind) in &have {
                    let same_kind = actual_kind == kind
                        || (kind == "linode_interface" && actual_kind == "interface");
                    if same_kind && !want.contains(entity_id) {
                        self.api
                            .delete(&format!("{path}/devices/{device_id}"))
                            .await?;
                    }
                }
                for entity_id in &want {
                    if !have.iter().any(|(_, id, actual_kind)| {
                        id == entity_id
                            && (actual_kind == kind
                                || (kind == "linode_interface" && actual_kind == "interface"))
                    }) {
                        let body = json!({"id": entity_id, "type": kind});
                        self.api.post(&format!("{path}/devices"), &body).await?;
                    }
                }
            }
        }

        let v = self.api.get(&path).await?;
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v),
        })
    }

    async fn delete(&self, _cx: &Ctx<'_>, id: Option<&str>, _inputs: &Value) -> Result<()> {
        let Some(id) = id else { return Ok(()) };
        self.api
            .delete(&format!("/networking/firewalls/{id}"))
            .await
    }
}
