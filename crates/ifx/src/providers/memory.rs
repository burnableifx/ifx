//! In-memory resources with no side effects. Useful for wiring, tests, and as a
//! reference implementation of the handler contract.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value as Json;
use serde_json::json;

use crate::provider::{Actual, Applied, Ctx, Handler, Registry, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub use crate::generated::memory::*;

pub fn register(r: &mut Registry) {
    r.register(Memory::default());
}

/// `memory.value`: stores arbitrary data and echoes it as outputs. Its only truth is the
/// state store, so it behaves like an unobservable cloud resource with an id — handy
/// as a stand-in for "something with outputs you learn after it exists".
#[derive(Default, Clone)]
pub struct Memory {
    store: Arc<Mutex<BTreeMap<String, Json>>>,
    next_id: Arc<Mutex<u64>>,
}

impl Memory {
    pub fn snapshot(&self) -> BTreeMap<String, Json> {
        self.store.lock().unwrap().clone()
    }
}

#[async_trait]
impl Handler for Memory {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            "memory.value",
            "Holds a value in memory; outputs it back as `value`.",
        )
        .input(
            field("value", FieldType::Any)
                .required()
                .doc("Any JSON value."),
        )
        .input(
            field("key", FieldType::String)
                .replace()
                .doc("Changing this replaces the resource."),
        )
        .output(field("id", FieldType::String))
        .output(field("value", FieldType::Any))
        .output(field("key", FieldType::String))
    }

    async fn read(&self, _cx: &Ctx<'_>, id: Option<&str>, inputs: &Json) -> Result<Option<Actual>> {
        // Truth lives in state (the resource is unobservable); the in-process store is
        // only a cache so tests can inspect what was written.
        let Some(id) = id else { return Ok(None) };
        let store = self.store.lock().unwrap();
        let v = store.get(id).cloned().unwrap_or_else(|| inputs.clone());
        Ok(Some(Actual {
            id: Some(id.to_string()),
            props: json!({
                "value": v.get("value").cloned().unwrap_or(Json::Null),
                "key": v.get("key").cloned().unwrap_or(Json::Null),
            }),
            outputs: outputs(id, &v),
        }))
    }

    async fn create(&self, _cx: &Ctx<'_>, inputs: &Json) -> Result<Applied> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            format!("mem-{}-{n}", unique_prefix())
        };
        self.store
            .lock()
            .unwrap()
            .insert(id.clone(), inputs.clone());
        Ok(Applied {
            id: Some(id.clone()),
            outputs: outputs(&id, inputs),
        })
    }

    async fn update(
        &self,
        _cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Json,
        _actual: &Actual,
    ) -> Result<Applied> {
        let id = id.ok_or_else(|| anyhow::anyhow!("update without id"))?;
        self.store
            .lock()
            .unwrap()
            .insert(id.to_string(), inputs.clone());
        Ok(Applied {
            id: Some(id.to_string()),
            outputs: outputs(id, inputs),
        })
    }

    async fn delete(&self, _cx: &Ctx<'_>, id: Option<&str>, _inputs: &Json) -> Result<()> {
        if let Some(id) = id {
            self.store.lock().unwrap().remove(id);
        }
        Ok(())
    }
}

fn outputs(id: &str, inputs: &Json) -> Json {
    json!({
        "id": id,
        "value": inputs.get("value").cloned().unwrap_or(Json::Null),
        "key": inputs.get("key").cloned().unwrap_or(Json::Null),
    })
}

/// Short per-process prefix so ids stay distinct across invocations.
fn unique_prefix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let h = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{:04x}", h & 0xffff)
}
