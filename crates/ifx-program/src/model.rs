//! Core data model: URNs, resource declarations, and the JSON conventions used to
//! carry references between resources through otherwise plain inputs.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Key used inside an input object to mark a reference to another resource's output.
pub const REF_KEY: &str = "$ref";
/// Optional companion to [`REF_KEY`]: dotted path into the referenced outputs.
pub const REF_PATH_KEY: &str = "$path";
/// Marker for a value that is not yet known (upstream resource not applied).
pub const UNKNOWN_KEY: &str = "$unknown";
/// Key of a string built from parts that may include references: resolved once every
/// part is known, e.g. `{"$concat": ["http://", {"$ref": "linode.instance:web",
/// "$path": "ipv4"}, "/"]}`.
pub const CONCAT_KEY: &str = "$concat";
/// Wraps a value the front-end marked sensitive (`secret(..)`): `{"$secret": v}`. The
/// engine strips the wrapper before handlers see the value and redacts the field in
/// plan output.
pub const SECRET_KEY: &str = "$secret";

/// Unique resource name: `<type>:<name>`, e.g. `linode.instance:web`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Urn(String);

impl Urn {
    pub fn new(type_name: &str, name: &str) -> Self {
        Self(format!("{type_name}:{name}"))
    }

    pub fn parse(s: &str) -> Result<Self, ModelError> {
        match s.split_once(':') {
            Some((t, n)) if !t.is_empty() && !n.is_empty() => Ok(Self(s.to_string())),
            _ => Err(ModelError::BadUrn(s.to_string())),
        }
    }

    pub fn type_name(&self) -> &str {
        self.0.split_once(':').map(|(t, _)| t).unwrap_or(&self.0)
    }

    pub fn name(&self) -> &str {
        self.0.split_once(':').map(|(_, n)| n).unwrap_or("")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Urn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Urn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Urn({})", self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid urn `{0}` (expected `<type>:<name>`)")]
    BadUrn(String),
    #[error("resource `{0}` references unknown resource `{1}`")]
    DanglingRef(Urn, Urn),
    #[error("dependency cycle involving `{0}`")]
    Cycle(Urn),
    #[error("duplicate resource `{0}`")]
    Duplicate(Urn),
}

/// A reference to another resource's output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRef {
    pub urn: Urn,
    /// Dotted path into the outputs object; empty means the whole outputs object.
    #[serde(default)]
    pub path: String,
}

impl OutputRef {
    pub fn new(urn: Urn, path: impl Into<String>) -> Self {
        Self {
            urn,
            path: path.into(),
        }
    }

    /// JSON marker form used inside inputs.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert(REF_KEY.into(), Value::String(self.urn.to_string()));
        if !self.path.is_empty() {
            m.insert(REF_PATH_KEY.into(), Value::String(self.path.clone()));
        }
        Value::Object(m)
    }

    /// Parse a marker object back into a ref, if it is one.
    pub fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let urn = obj.get(REF_KEY)?.as_str()?;
        if obj.keys().any(|k| k != REF_KEY && k != REF_PATH_KEY) {
            return None;
        }
        let path = obj
            .get(REF_PATH_KEY)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(Self {
            urn: Urn::parse(urn).ok()?,
            path,
        })
    }
}

/// Marker for a not-yet-known value.
pub fn unknown() -> Value {
    let mut m = Map::new();
    m.insert(UNKNOWN_KEY.into(), Value::Bool(true));
    Value::Object(m)
}

pub fn is_unknown(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key(UNKNOWN_KEY))
}

/// True if any unknown marker appears anywhere in the value.
pub fn contains_unknown(v: &Value) -> bool {
    match v {
        Value::Object(o) => is_unknown(v) || o.values().any(contains_unknown),
        Value::Array(a) => a.iter().any(contains_unknown),
        _ => false,
    }
}

/// Wrap a value as sensitive.
pub fn secret(v: Value) -> Value {
    let mut m = Map::new();
    m.insert(SECRET_KEY.into(), v);
    Value::Object(m)
}

/// The wrapped value, if `v` is a secret marker.
pub fn secret_inner(v: &Value) -> Option<&Value> {
    let o = v.as_object()?;
    if o.len() != 1 {
        return None;
    }
    o.get(SECRET_KEY)
}

/// Remove every secret marker, returning the plain value and the names of the
/// top-level fields that contained one.
pub fn strip_secrets(v: Value) -> (Value, Vec<String>) {
    fn strip(v: Value, hit: &mut bool) -> Value {
        if let Some(inner) = secret_inner(&v) {
            *hit = true;
            return strip(inner.clone(), hit);
        }
        match v {
            Value::Object(o) => {
                Value::Object(o.into_iter().map(|(k, x)| (k, strip(x, hit))).collect())
            }
            Value::Array(a) => Value::Array(a.into_iter().map(|x| strip(x, hit)).collect()),
            other => other,
        }
    }
    match v {
        Value::Object(o) => {
            let mut fields = Vec::new();
            let out = o
                .into_iter()
                .map(|(k, x)| {
                    let mut hit = false;
                    let x = strip(x, &mut hit);
                    if hit {
                        fields.push(k.clone());
                    }
                    (k, x)
                })
                .collect();
            (Value::Object(out), fields)
        }
        other => {
            let mut hit = false;
            (strip(other, &mut hit), Vec::new())
        }
    }
}

/// Collect every output reference appearing in a value.
pub fn collect_refs(v: &Value, out: &mut Vec<OutputRef>) {
    if let Some(r) = OutputRef::from_value(v) {
        out.push(r);
        return;
    }
    match v {
        Value::Object(o) => o.values().for_each(|x| collect_refs(x, out)),
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

/// Build a concat marker from parts (strings, refs, other concats).
pub fn concat(parts: Vec<Value>) -> Value {
    let mut m = Map::new();
    m.insert(CONCAT_KEY.into(), Value::Array(parts));
    Value::Object(m)
}

/// The parts of a concat marker, if `v` is one.
pub fn concat_parts(v: &Value) -> Option<&Vec<Value>> {
    let o = v.as_object()?;
    if o.len() != 1 {
        return None;
    }
    o.get(CONCAT_KEY)?.as_array()
}

/// Render a resolved scalar for concatenation.
fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// Look up a dotted path inside a JSON value. Array indices are numeric segments.
pub fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    path.split('.').try_fold(v, |cur, seg| match cur {
        Value::Object(o) => o.get(seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Replace every reference marker with the referenced output, via `lookup`.
/// A lookup returning `None` yields an unknown marker.
pub fn resolve_refs(v: &Value, lookup: &dyn Fn(&OutputRef) -> Option<Value>) -> Value {
    if let Some(r) = OutputRef::from_value(v) {
        return lookup(&r).unwrap_or_else(unknown);
    }
    if let Some(parts) = concat_parts(v) {
        let mut out = String::new();
        for p in parts {
            let r = resolve_refs(p, lookup);
            if contains_unknown(&r) {
                return unknown();
            }
            match scalar_str(&r) {
                Some(s) => out.push_str(&s),
                None => return unknown(),
            }
        }
        return Value::String(out);
    }
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), resolve_refs(x, lookup)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve_refs(x, lookup)).collect()),
        other => other.clone(),
    }
}

/// How a host-scoped resource reaches its target.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Connection {
    Unavailable {
        reason: String,
    },
    Local {
        /// Prefix privileged commands with `sudo -n`.
        #[serde(default)]
        sudo: bool,
    },
    Ssh {
        host: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        /// Identity file passed as `-i`.
        #[serde(default)]
        identity: Option<String>,
        /// Seconds to keep retrying the first connection (fresh VMs take a while).
        #[serde(default)]
        connect_timeout_secs: Option<u64>,
        #[serde(default)]
        sudo: bool,
        /// Extra raw `ssh` arguments.
        #[serde(default)]
        extra_args: Vec<String>,
        /// SSH hop used to reach this target. Nested hops retain their own credentials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<Box<Connection>>,
        /// Execution-scoped access that must be opened before SSH and closed afterwards.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<Box<SshActivation>>,
    },
}

/// A temporary SSH endpoint controlled by the local QEMU monitor.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshActivation {
    pub monitor: String,
    /// Durable cleanup intent written before direct access is attached.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lease: String,
    pub host_port: u16,
    pub guest_port: u16,
    pub netdev_id: String,
    pub device_id: String,
    pub mac: String,
    /// The user-mode netdev remains for egress; only its host forward is temporary.
    pub persistent_netdev: bool,
    /// Permit later runs to reopen access after the initial deployment seals it.
    pub reopen: bool,
}

impl Connection {
    pub fn local() -> Self {
        Self::Local { sudo: false }
    }

    pub fn ssh(host: impl Into<String>, user: Option<String>) -> Self {
        Self::Ssh {
            host: host.into(),
            user,
            port: None,
            identity: None,
            connect_timeout_secs: None,
            sudo: false,
            extra_args: Vec::new(),
            via: None,
            activation: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn activation(&self) -> Option<&SshActivation> {
        match self {
            Self::Ssh { activation, .. } => activation.as_deref(),
            Self::Unavailable { .. } | Self::Local { .. } => None,
        }
    }

    pub fn has_execution_scoped_access(&self) -> bool {
        self.activation().is_some()
    }

    pub fn sudo(&self) -> bool {
        match self {
            Self::Unavailable { .. } => false,
            Self::Local { sudo } | Self::Ssh { sudo, .. } => *sudo,
        }
    }

    /// Human label for logs.
    pub fn label(&self) -> String {
        match self {
            Self::Unavailable { .. } => "unavailable".into(),
            Self::Local { .. } => "local".into(),
            Self::Ssh {
                host, user, port, ..
            } => {
                let u = user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
                let p = port.map(|p| format!(":{p}")).unwrap_or_default();
                format!("ssh://{u}{host}{p}")
            }
        }
    }
}

/// A single declared resource: the unit the engine plans and applies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceDecl {
    pub urn: Urn,
    /// Inputs as declared; may contain [`OutputRef`] markers.
    pub inputs: Value,
    /// Explicit ordering edges in addition to those implied by references.
    #[serde(default)]
    pub depends_on: Vec<Urn>,
    /// Resources whose change forces this resource to be re-applied even if its own
    /// inputs are unchanged (used by `host.exec`, service restarts, ...).
    #[serde(default)]
    pub triggers: Vec<Urn>,
    /// Refuse to delete or replace.
    #[serde(default)]
    pub protect: bool,
}

impl ResourceDecl {
    pub fn new(type_name: &str, name: &str, inputs: Value) -> Self {
        Self {
            urn: Urn::new(type_name, name),
            inputs,
            depends_on: Vec::new(),
            triggers: Vec::new(),
            protect: false,
        }
    }

    pub fn type_name(&self) -> &str {
        self.urn.type_name()
    }

    /// Every URN this resource must be ordered after.
    pub fn dependencies(&self) -> BTreeSet<Urn> {
        let mut refs = Vec::new();
        collect_refs(&self.inputs, &mut refs);
        refs.into_iter()
            .map(|r| r.urn)
            .chain(self.depends_on.iter().cloned())
            .chain(self.triggers.iter().cloned())
            .collect()
    }
}

/// A complete desired-state description produced by a Rust stack and consumed by the
/// engine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Program {
    pub resources: Vec<ResourceDecl>,
}

impl Program {
    pub fn get(&self, urn: &Urn) -> Option<&ResourceDecl> {
        self.resources.iter().find(|r| &r.urn == urn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urn_roundtrip() {
        let u = Urn::new("linode.instance", "web");
        assert_eq!(u.type_name(), "linode.instance");
        assert_eq!(u.name(), "web");
        assert_eq!(Urn::parse("linode.instance:web").unwrap(), u);
        assert!(Urn::parse("nocolon").is_err());
    }

    #[test]
    fn ref_markers() {
        let r = OutputRef::new(Urn::new("a", "b"), "ipv4");
        let v = r.to_value();
        assert_eq!(OutputRef::from_value(&v), Some(r));
        assert!(OutputRef::from_value(&json!({"$ref": "a:b", "other": 1})).is_none());
    }

    #[test]
    fn resolve_and_collect() {
        let inputs = json!({
            "on": {"$ref": "linode.instance:web", "$path": "connection"},
            "list": [1, {"$ref": "x:y"}],
        });
        let mut refs = Vec::new();
        collect_refs(&inputs, &mut refs);
        assert_eq!(refs.len(), 2);
        let resolved = resolve_refs(&inputs, &|r| {
            (r.urn.as_str() == "linode.instance:web").then(|| json!({"kind": "local"}))
        });
        assert_eq!(resolved["on"], json!({"kind": "local"}));
        assert!(is_unknown(&resolved["list"][1]));
        assert!(contains_unknown(&resolved));
    }

    #[test]
    fn concat_resolves_when_known() {
        let r = OutputRef::new(Urn::new("a", "b"), "ip").to_value();
        let v = json!({"url": concat(vec![json!("http://"), r.clone(), json!(":"), json!(80)])});
        let mut refs = Vec::new();
        collect_refs(&v, &mut refs);
        assert_eq!(refs.len(), 1);
        let unknown_yet = resolve_refs(&v, &|_| None);
        assert!(is_unknown(&unknown_yet["url"]));
        let known = resolve_refs(&v, &|_| Some(json!("10.0.0.1")));
        assert_eq!(known["url"], "http://10.0.0.1:80");
    }

    #[test]
    fn path_lookup() {
        let v = json!({"a": {"b": [10, 20]}});
        assert_eq!(get_path(&v, "a.b.1"), Some(&json!(20)));
        assert_eq!(get_path(&v, ""), Some(&v));
        assert_eq!(get_path(&v, "a.z"), None);
    }

    #[test]
    fn nested_ssh_and_activation_roundtrip() {
        let bastion = Connection::ssh("10.0.0.10", Some("debian".into()));
        let connection = Connection::Ssh {
            host: "10.0.1.20".into(),
            user: Some("debian".into()),
            port: Some(22),
            identity: Some("/keys/leaf".into()),
            connect_timeout_secs: Some(60),
            sudo: true,
            extra_args: Vec::new(),
            via: Some(Box::new(bastion)),
            activation: Some(Box::new(SshActivation {
                monitor: "/run/ifx/leaf.qmp".into(),
                lease: "/run/ifx/leaf.management-pending".into(),
                host_port: 22022,
                guest_port: 22,
                netdev_id: "mgmt".into(),
                device_id: "ifx-mgmt".into(),
                mac: "52:54:00:12:34:56".into(),
                persistent_netdev: false,
                reopen: true,
            })),
        };
        let encoded = serde_json::to_value(&connection).unwrap();
        assert_eq!(encoded["kind"], "ssh");
        assert_eq!(encoded["via"]["host"], "10.0.0.10");
        assert_eq!(encoded["activation"]["host_port"], 22022);
        assert_eq!(
            serde_json::from_value::<Connection>(encoded).unwrap(),
            connection
        );
        assert!(connection.has_execution_scoped_access());
    }
}
