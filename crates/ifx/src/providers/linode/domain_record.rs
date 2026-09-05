//! `linode.domain_record`: one DNS record inside a `linode.domain` zone.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::api::{Linode, js};
use crate::provider::{Actual, Applied, Ctx, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "linode.domain_record";
const RECORD_TYPES: [&str; 9] = ["A", "AAAA", "CNAME", "TXT", "MX", "SRV", "NS", "CAA", "PTR"];

pub struct DomainRecordHandler {
    api: Arc<Linode>,
}

impl DomainRecordHandler {
    pub fn new(api: Arc<Linode>) -> Self {
        Self { api }
    }

    /// Adopt by the strongest available natural key. DNS permits several records with
    /// the same type/name, so ambiguous matches are rejected instead of taking over an
    /// arbitrary record.
    async fn find(&self, domain_id: i64, inputs: &Value) -> Result<Option<Value>> {
        let ty = js::str(inputs, "type").unwrap_or("");
        let name = js::str(inputs, "name").unwrap_or("");
        let items = self
            .api
            .list(&format!("/domains/{domain_id}/records"), None)
            .await?;
        let candidates: Vec<Value> = items
            .into_iter()
            .filter(|v| js::str(v, "type") == Some(ty) && js::str(v, "name").unwrap_or("") == name)
            .collect();
        if candidates.len() <= 1 {
            return Ok(candidates.into_iter().next());
        }
        let identity = ["target", "service", "tag", "port", "priority", "weight"];
        let exact: Vec<Value> = candidates
            .into_iter()
            .filter(|candidate| {
                identity.iter().all(|key| {
                    inputs
                        .get(*key)
                        .is_none_or(|wanted| candidate.get(*key) == Some(wanted))
                })
            })
            .collect();
        anyhow::ensure!(
            exact.len() <= 1,
            "multiple {ty} records named `{name}` exist in domain {domain_id}; import state with the record id or make target/service/tag unique"
        );
        Ok(exact.into_iter().next())
    }
}

fn domain_id(inputs: &Value) -> Result<i64> {
    js::i64(inputs, "domain_id").ok_or_else(|| anyhow::anyhow!("`domain_id` is required"))
}

fn observe(v: Value, domain_id: i64) -> Result<Actual> {
    let id = js::i64(&v, "id").ok_or_else(|| anyhow::anyhow!("record has no id: {v}"))?;
    let props = json!({
        "domain_id": domain_id,
        "type": js::get(&v, "type"),
        "name": js::get(&v, "name"),
        "target": js::get(&v, "target"),
        "ttl_sec": js::get(&v, "ttl_sec"),
        "priority": js::get(&v, "priority"),
        "weight": js::get(&v, "weight"),
        "port": js::get(&v, "port"),
        "service": js::get(&v, "service"),
        "tag": js::get(&v, "tag"),
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

fn body(inputs: &Value) -> Value {
    let mut m = Map::new();
    js::copy(
        &mut m,
        inputs,
        &[
            "type", "name", "target", "ttl_sec", "priority", "weight", "port", "service", "tag",
        ],
    );
    Value::Object(m)
}

#[async_trait]
impl Handler for DomainRecordHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A single DNS record (A, CNAME, MX, ...) in a `linode.domain` zone.",
        )
        .input(
            field("domain_id", FieldType::Int)
                .required()
                .replace()
                .doc("Id of the zone the record lives in, usually a reference like `zone.id`."),
        )
        .input(
            field("type", FieldType::enumeration(RECORD_TYPES))
                .required()
                .replace()
                .doc(
                    "Record type: `A`/`AAAA` map names to addresses, `CNAME` aliases a name, `MX` \
                 names mail servers, `TXT` holds text, `SRV`/`NS`/`CAA`/`PTR` as per DNS.",
                ),
        )
        .input(field("name", FieldType::String).doc(
            "Hostname relative to the zone (`www` for `www.example.com`); empty or omitted \
                 for the zone apex.",
        ))
        .input(field("target", FieldType::String).required().doc(
            "Record value: an IP for `A`/`AAAA`, a hostname for `CNAME`/`MX`/`NS`, text for `TXT`.",
        ))
        .input(
            field("ttl_sec", FieldType::Int)
                .doc("Seconds resolvers may cache this record (0 = zone default)."),
        )
        .input(
            field("priority", FieldType::Int)
                .required_when("type", "SRV")
                .doc("Preference for `MX`/`SRV` records; lower is tried first."),
        )
        .input(
            field("weight", FieldType::Int)
                .required_when("type", "SRV")
                .doc("Relative weight among SRV records with the same priority."),
        )
        .input(
            field("port", FieldType::Int)
                .required_when("type", "SRV")
                .doc("Target port; required for SRV records."),
        )
        .input(
            field("service", FieldType::String)
                .required_when("type", "SRV")
                .doc("Service name without surrounding underscores; required for SRV records."),
        )
        .input(
            field(
                "tag",
                FieldType::enumeration(["issue", "issuewild", "iodef"]),
            )
            .required_when("type", "CAA")
            .doc("CAA property tag; required for CAA records."),
        )
        .output(field("id", FieldType::Int).doc("Numeric record id."))
    }

    async fn read(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        let domain_id = domain_id(inputs)?;
        let mut found = match id {
            Some(id) => {
                self.api
                    .get_opt(&format!("/domains/{domain_id}/records/{id}"))
                    .await?
            }
            None => None,
        };
        if found.is_none() {
            found = self.find(domain_id, inputs).await?;
        }
        found.map(|v| observe(v, domain_id)).transpose()
    }

    async fn create(&self, _cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let domain_id = domain_id(inputs)?;
        let v = self
            .api
            .post(&format!("/domains/{domain_id}/records"), &body(inputs))
            .await?;
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
        let domain_id = domain_id(inputs)?;
        let v = self
            .api
            .put(&format!("/domains/{domain_id}/records/{id}"), &body(inputs))
            .await?;
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(&v),
        })
    }

    async fn delete(&self, _cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<()> {
        let Some(id) = id else { return Ok(()) };
        let domain_id = domain_id(inputs)?;
        self.api
            .delete(&format!("/domains/{domain_id}/records/{id}"))
            .await
    }
}
