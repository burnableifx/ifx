//! `linode.domain`: a DNS zone hosted on Linode's nameservers.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::api::{Linode, js};
use crate::provider::{Actual, Applied, Ctx, Diff, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "linode.domain";

pub struct DomainHandler {
    api: Arc<Linode>,
}

impl DomainHandler {
    pub fn new(api: Arc<Linode>) -> Self {
        Self { api }
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<Value>> {
        let items = self
            .api
            .list("/domains", Some(&json!({"domain": name})))
            .await?;
        Ok(items
            .into_iter()
            .find(|v| js::str(v, "domain") == Some(name)))
    }
}

fn observe(v: Value) -> Result<Actual> {
    let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("domain has no id: {v}"))?;
    let soa = js::str(&v, "soa_email")
        .filter(|s| !s.is_empty())
        .map(Value::from)
        .unwrap_or(Value::Null);
    let props = json!({
        "domain": js::get(&v, "domain"),
        "type": js::get(&v, "type"),
        "soa_email": soa,
        "description": js::get(&v, "description"),
        "status": js::get(&v, "status"),
        "axfr_ips": js::get(&v, "axfr_ips"),
        "master_ips": js::get(&v, "master_ips"),
        "ttl_sec": js::get(&v, "ttl_sec"),
        "refresh_sec": js::get(&v, "refresh_sec"),
        "retry_sec": js::get(&v, "retry_sec"),
        "expire_sec": js::get(&v, "expire_sec"),
        "tags": js::get(&v, "tags"),
    });
    Ok(Actual {
        id: Some(id.to_string()),
        props,
        outputs: outputs(&v),
    })
}

fn outputs(v: &Value) -> Value {
    json!({"id": js::get(v, "id")})
}

fn normalize(v: &Value) -> Value {
    let mut v = v.clone();
    js::map_key(&mut v, "axfr_ips", js::sorted);
    js::map_key(&mut v, "master_ips", js::sorted);
    js::map_key(&mut v, "tags", js::sorted);
    v
}

fn body(inputs: &Value) -> Result<Value> {
    let mut m = Map::new();
    js::copy(
        &mut m,
        inputs,
        &[
            "domain",
            "type",
            "soa_email",
            "description",
            "status",
            "axfr_ips",
            "master_ips",
            "ttl_sec",
            "refresh_sec",
            "retry_sec",
            "expire_sec",
            "tags",
        ],
    );
    let ty = js::str(inputs, "type").unwrap_or("master");
    anyhow::ensure!(
        ty != "master" || m.contains_key("soa_email"),
        "`soa_email` is required for a master domain"
    );
    anyhow::ensure!(
        ty != "slave"
            || m.get("master_ips")
                .and_then(Value::as_array)
                .is_some_and(|ips| !ips.is_empty()),
        "`master_ips` must contain at least one address for a slave domain"
    );
    Ok(Value::Object(m))
}

#[async_trait]
impl Handler for DomainHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A DNS zone served by Linode's nameservers; add records with `linode.domain_record`.",
        )
        .input(field("domain", FieldType::String).required().replace().doc(
            "Zone name, e.g. `example.com`; an existing zone with this name is adopted instead \
             of created.",
        ))
        .input(field("type", FieldType::enumeration(["master", "slave"])).default("master").doc(
            "`master` zones are edited here; `slave` zones copy records from another primary \
             nameserver.",
        ))
        .input(field("soa_email", FieldType::String).required_when("type", "master").doc(
            "Contact address published in the zone's SOA record; required for `master` zones.",
        ))
        .input(field("description", FieldType::String).doc(
            "Human-readable description shown in DNS Manager.",
        ))
        .input(
            field("status", FieldType::enumeration(["active", "disabled"]))
                .default("active")
                .doc("Whether Linode renders and serves the zone."),
        )
        .input(field("axfr_ips", FieldType::list(FieldType::String)).doc(
            "IP addresses allowed to transfer this zone; leave empty unless AXFR is intentional.",
        ))
        .input(
            field("master_ips", FieldType::list(FieldType::String))
                .required_when("type", "slave")
                .doc("Primary DNS server addresses; required for `slave` zones."),
        )
        .input(field("ttl_sec", FieldType::Int).doc(
            "Default seconds resolvers may cache records from this zone (0 = Linode default).",
        ))
        .input(field("refresh_sec", FieldType::Int).doc(
            "Seconds before a slave refreshes its copy (0 = Linode default).",
        ))
        .input(field("retry_sec", FieldType::Int).doc(
            "Seconds before retrying a failed slave refresh (0 = Linode default).",
        ))
        .input(field("expire_sec", FieldType::Int).doc(
            "Seconds before an unrefreshed slave zone stops being authoritative (0 = Linode default).",
        ))
        .input(field("tags", FieldType::list(FieldType::String)).doc(
            "Free-form labels for grouping and filtering in the Cloud Manager.",
        ))
        .output(field("id", FieldType::Int).doc("Numeric domain id; pass as `domain_id` to records."))
    }

    async fn read(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let mut found = match id {
            Some(id) => self.api.get_opt(&format!("/domains/{id}")).await?,
            None => None,
        };
        if found.is_none()
            && let Some(name) = js::str(inputs, "domain")
        {
            found = self.find_by_name(name).await?;
        }
        found.map(observe).transpose()
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
        let v = self.api.post("/domains", &body(inputs)?).await?;
        let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("create returned no id"))?;
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
        _actual: &Actual,
    ) -> Result<Applied> {
        let id = id.ok_or_else(|| anyhow::anyhow!("update without id"))?;
        let v = self
            .api
            .put(&format!("/domains/{id}"), &body(inputs)?)
            .await?;
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v),
        })
    }

    async fn delete(&self, _cx: &Ctx<'_>, id: Option<&str>, _inputs: &Value) -> Result<()> {
        let Some(id) = id else { return Ok(()) };
        self.api.delete(&format!("/domains/{id}")).await
    }
}
