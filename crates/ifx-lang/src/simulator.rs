//! Deliberately in-memory: no shell, filesystem, network, clocks or credentials.
use crate::{
    language::{Analysis, Binding, Compilation, Evaluator, Val, catalog},
    syntax::{Diagnostic, Span},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Completion {
    Succeeded { watched: Value },
    Uncertain,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Host {
    pub incarnation: u64,
    pub objects: BTreeMap<String, BTreeMap<String, Value>>,
    pub records: BTreeMap<String, u64>,
    pub journal: BTreeMap<String, Completion>,
    pub events: Vec<String>,
    #[serde(skip)]
    pub outputs: BTreeMap<String, Value>,
    #[serde(skip)]
    visited: BTreeSet<String>,
}
impl Host {
    pub fn exists(&self, path: &str) -> bool {
        self.objects.contains_key(&object_key("file", path))
            || self.objects.contains_key(&object_key("directory", path))
    }
    pub(crate) fn ensure(
        &mut self,
        kind: &str,
        key: &str,
        mut fields: BTreeMap<String, Value>,
        _span: Span,
    ) -> Result<(), Diagnostic> {
        match kind {
            "directory" => {
                fields
                    .entry("mode".into())
                    .or_insert(Value::String("0755".into()));
            }
            "file" => {
                fields
                    .entry("mode".into())
                    .or_insert(Value::String("0644".into()));
            }
            "package" => {
                fields
                    .entry("installed".into())
                    .or_insert(Value::Bool(true));
            }
            "service" => {
                fields.entry("enabled".into()).or_insert(Value::Bool(true));
                fields.entry("running".into()).or_insert(Value::Bool(true));
            }
            _ => {}
        }
        let id = object_key(kind, key);
        if self.objects.get(&id) != Some(&fields) {
            self.objects.insert(id, fields);
            self.events.push(format!("converged {kind}"));
        }
        Ok(())
    }
    pub(crate) fn record(&mut self, key: &str) {
        *self.records.entry(key.into()).or_default() += 1;
        self.events.push("recorded action".into());
    }
    fn journal_key(&self, identity: &str) -> String {
        serde_json::json!([self.incarnation, identity]).to_string()
    }
    pub(crate) fn begin(
        &mut self,
        identity: &str,
        policy: &str,
        watched: &Value,
        span: Span,
    ) -> Result<bool, Diagnostic> {
        let key = self.journal_key(identity);
        if !self.visited.insert(key.clone()) {
            return Err(Diagnostic::new(
                span,
                "duplicate action key reached in this apply",
            ));
        }
        match self.journal.get(&key) {
            Some(Completion::Uncertain) => {
                return Err(Diagnostic::new(
                    span,
                    "action outcome is uncertain; reconcile before retrying",
                ));
            }
            Some(Completion::Succeeded { watched: old })
                if policy == "once" || (policy == "on_change" && old == watched) =>
            {
                return Ok(false);
            }
            _ => {}
        }
        self.journal.insert(key, Completion::Uncertain);
        Ok(true)
    }
    pub(crate) fn finish(&mut self, identity: &str, watched: Value, success: bool) {
        if success {
            self.journal.insert(
                self.journal_key(identity),
                Completion::Succeeded { watched },
            );
        }
    }
    /// Explicit test-harness reconciliation: the caller supplies confirmed success.
    pub fn reconcile_success(&mut self, journal_key: &str, watched: Value) -> bool {
        if self.journal.get(journal_key) != Some(&Completion::Uncertain) {
            return false;
        }
        self.journal
            .insert(journal_key.into(), Completion::Succeeded { watched });
        true
    }
    pub fn remove_object(&mut self, kind: &str, key: &str) {
        self.objects.remove(&object_key(kind, key));
    }
    pub fn object(&self, kind: &str, key: &str) -> Option<&BTreeMap<String, Value>> {
        self.objects.get(&object_key(kind, key))
    }
    pub fn replace(&mut self) {
        self.incarnation += 1;
        self.objects.clear();
        self.records.clear();
        self.events.clear();
        self.visited.clear();
    }
}
fn object_key(kind: &str, key: &str) -> String {
    serde_json::json!([kind, key]).to_string()
}
#[derive(Default, Debug, Serialize)]
pub struct Simulator {
    pub hosts: BTreeMap<String, Host>,
}
impl Simulator {
    /// Resource facts are explicit fixtures. No cloud provider is contacted or emulated.
    pub fn apply(
        &mut self,
        compilation: &Compilation,
        outputs: &BTreeMap<String, Value>,
    ) -> Result<(), Diagnostic> {
        let schemas = catalog();
        for host in self.hosts.values_mut() {
            host.visited.clear();
        }
        for config in &compilation.configurations {
            let host = self.hosts.entry(config.target.to_string()).or_default();
            host.outputs = outputs.clone();
            let mut analysis = Analysis::default();
            let mut evaluator = Evaluator::new(&schemas, false, &mut analysis);
            evaluator.identity =
                serde_json::json!([config.target.as_str(), config.key]).to_string();
            evaluator.host = Some(host);
            let mut env = config.captures.clone();
            env.insert(
                config.parameter.text.clone(),
                Binding {
                    value: Val::Host {
                        urn: config.target.clone(),
                    },
                    name: config.parameter.clone(),
                },
            );
            evaluator.block(&config.body, &mut env)?;
        }
        Ok(())
    }
}
