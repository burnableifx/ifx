//! Machine-readable description of every resource type. One source of truth that
//! drives validation, plan rendering (sensitive fields), the LSP, generated docs,
//! and the generated Rust front end.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Connection, OutputRef, contains_unknown, is_unknown};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldType {
    String,
    Int,
    Float,
    Bool,
    /// A [`Connection`] object (`{"kind": "ssh", ...}` / `{"kind": "local"}`).
    Connection,
    List {
        item: Box<FieldType>,
    },
    Map {
        value: Box<FieldType>,
    },
    /// A fixed set of named fields. `name` is the type name generated front-ends use.
    Object {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        fields: Vec<FieldSchema>,
    },
    /// One of `variants`. An `open` enum also accepts any other string (the variants are
    /// a catalog of known values, e.g. cloud regions, not an exhaustive set).
    Enum {
        variants: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        open: bool,
    },
    /// Any JSON.
    Any,
}

impl FieldType {
    pub fn list(item: FieldType) -> Self {
        Self::List {
            item: Box::new(item),
        }
    }

    pub fn map(value: FieldType) -> Self {
        Self::Map {
            value: Box::new(value),
        }
    }

    pub fn object(fields: Vec<FieldSchema>) -> Self {
        Self::Object { name: None, fields }
    }

    /// An object type with a name for generated code (`Rule`, `Addresses`).
    pub fn object_named(name: impl Into<String>, fields: Vec<FieldSchema>) -> Self {
        Self::Object {
            name: Some(name.into()),
            fields,
        }
    }

    pub fn enumeration<S: Into<String>>(variants: impl IntoIterator<Item = S>) -> Self {
        Self::Enum {
            variants: variants.into_iter().map(Into::into).collect(),
            open: false,
        }
    }

    /// An enum whose variants are known values but not the only valid ones.
    pub fn open_enum<S: Into<String>>(variants: impl IntoIterator<Item = S>) -> Self {
        Self::Enum {
            variants: variants.into_iter().map(Into::into).collect(),
            open: true,
        }
    }

    /// Short human/LSP-facing rendering, e.g. `list[string]`.
    pub fn display(&self) -> String {
        match self {
            Self::String => "string".into(),
            Self::Int => "int".into(),
            Self::Float => "float".into(),
            Self::Bool => "bool".into(),
            Self::Connection => "connection".into(),
            Self::List { item } => format!("list[{}]", item.display()),
            Self::Map { value } => format!("dict[string, {}]", value.display()),
            Self::Object { fields, .. } => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.ty.display()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            Self::Enum { variants, open } => {
                let mut s = variants
                    .iter()
                    .map(|v| format!("\"{v}\""))
                    .collect::<Vec<_>>()
                    .join(" | ");
                if *open {
                    s.push_str(" | string");
                }
                s
            }
            Self::Any => "any".into(),
        }
    }

    fn check(&self, v: &Value, path: &str, errors: &mut Vec<String>) {
        if let Some(inner) = crate::model::secret_inner(v) {
            return self.check(inner, path, errors);
        }
        if is_unknown(v)
            || OutputRef::from_value(v).is_some()
            || crate::model::concat_parts(v).is_some()
        {
            return;
        }
        let bad = |errors: &mut Vec<String>, expected: &str| {
            errors.push(format!("`{path}`: expected {expected}, got {}", kind_of(v)));
        };
        match self {
            Self::String => {
                if !v.is_string() {
                    bad(errors, "string");
                }
            }
            Self::Int => {
                if !v.is_i64() && !v.is_u64() {
                    bad(errors, "int");
                }
            }
            Self::Float => {
                if !v.is_number() {
                    bad(errors, "float");
                }
            }
            Self::Bool => {
                if !v.is_boolean() {
                    bad(errors, "bool");
                }
            }
            Self::Connection => {
                let mut refs = Vec::new();
                crate::model::collect_refs(v, &mut refs);
                if contains_unknown(v) || !refs.is_empty() {
                    return;
                }
                if let Err(e) = serde_json::from_value::<Connection>(v.clone()) {
                    errors.push(format!("`{path}`: invalid connection: {e}"));
                }
            }
            Self::List { item } => match v.as_array() {
                Some(a) => {
                    for (i, x) in a.iter().enumerate() {
                        item.check(x, &format!("{path}[{i}]"), errors);
                    }
                }
                None => bad(errors, "list"),
            },
            Self::Map { value } => match v.as_object() {
                Some(o) => {
                    for (k, x) in o {
                        value.check(x, &format!("{path}.{k}"), errors);
                    }
                }
                None => bad(errors, "dict"),
            },
            Self::Object { fields, .. } => match v.as_object() {
                Some(_) => check_fields(fields, v, path, errors),
                None => bad(errors, "object"),
            },
            Self::Enum { variants, open } => match v.as_str() {
                Some(_) if *open => {}
                Some(s) if variants.iter().any(|x| x == s) => {}
                _ => bad(errors, &format!("one of {}", self.display())),
            },
            Self::Any => {}
        }
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSchema {
    pub name: String,
    pub ty: FieldType,
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Never shown in plan output or logs.
    #[serde(default)]
    pub sensitive: bool,
    /// Changing this field replaces the resource instead of updating it in place.
    #[serde(default)]
    pub replace: bool,
    /// Fields sharing a group name are mutually exclusive: at most one may be set.
    /// A required group (any member has `required`) must have exactly one set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Required when another input equals a value: `("type", "master")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_when: Option<(String, Value)>,
}

impl FieldSchema {
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: String::new(),
            required: false,
            default: None,
            sensitive: false,
            replace: false,
            group: None,
            required_when: None,
        }
    }

    /// Require this input whenever `field` equals `value` (after defaults).
    pub fn required_when(mut self, field: impl Into<String>, value: impl Into<Value>) -> Self {
        self.required_when = Some((field.into(), value.into()));
        self
    }

    pub fn group(mut self, name: impl Into<String>) -> Self {
        self.group = Some(name.into());
        self
    }

    pub fn doc(mut self, doc: impl Into<String>) -> Self {
        self.doc = doc.into();
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn default(mut self, v: impl Into<Value>) -> Self {
        self.default = Some(v.into());
        self
    }

    pub fn sensitive(mut self) -> Self {
        self.sensitive = true;
        self
    }

    pub fn replace(mut self) -> Self {
        self.replace = true;
        self
    }
}

/// Shorthand constructor.
pub fn field(name: impl Into<String>, ty: FieldType) -> FieldSchema {
    FieldSchema::new(name, ty)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceSchema {
    pub type_name: String,
    #[serde(default)]
    pub doc: String,
    pub inputs: Vec<FieldSchema>,
    pub outputs: Vec<FieldSchema>,
}

impl ResourceSchema {
    pub fn new(type_name: impl Into<String>, doc: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
            doc: doc.into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    pub fn input(mut self, f: FieldSchema) -> Self {
        self.inputs.push(f);
        self
    }

    pub fn output(mut self, f: FieldSchema) -> Self {
        self.outputs.push(f);
        self
    }

    pub fn input_field(&self, name: &str) -> Option<&FieldSchema> {
        self.inputs.iter().find(|f| f.name == name)
    }

    /// Exclusive input groups in declaration order: `(group, members)`.
    pub fn groups(&self) -> Vec<(&str, Vec<&FieldSchema>)> {
        let mut out: Vec<(&str, Vec<&FieldSchema>)> = Vec::new();
        for f in &self.inputs {
            if let Some(g) = &f.group {
                match out.iter_mut().find(|(name, _)| *name == g.as_str()) {
                    Some((_, members)) => members.push(f),
                    None => out.push((g.as_str(), vec![f])),
                }
            }
        }
        out
    }

    pub fn replace_fields(&self) -> impl Iterator<Item = &str> {
        self.inputs
            .iter()
            .filter(|f| f.replace)
            .map(|f| f.name.as_str())
    }

    pub fn sensitive_fields(&self) -> impl Iterator<Item = &str> {
        self.inputs
            .iter()
            .chain(self.outputs.iter())
            .filter(|f| f.sensitive)
            .map(|f| f.name.as_str())
    }

    /// Validate resolved inputs. Unknown values are accepted anywhere.
    pub fn validate(&self, inputs: &Value) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        match inputs.as_object() {
            Some(_) => check_fields(&self.inputs, inputs, "", &mut errors),
            None => errors.push("inputs must be an object".into()),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Fill in schema defaults for absent fields (top level only).
    pub fn apply_defaults(&self, inputs: &mut Value) {
        if let Some(obj) = inputs.as_object_mut() {
            for f in &self.inputs {
                if let Some(d) = &f.default {
                    obj.entry(f.name.clone()).or_insert_with(|| d.clone());
                }
            }
        }
    }
}

fn check_fields(fields: &[FieldSchema], v: &Value, path: &str, errors: &mut Vec<String>) {
    let obj = v.as_object().expect("caller checked object");
    let join = |name: &str| {
        if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        }
    };
    let present = |name: &str| !matches!(obj.get(name), Some(Value::Null) | None);
    for f in fields {
        match obj.get(&f.name) {
            Some(Value::Null) | None if f.required && f.group.is_none() => {
                errors.push(format!("`{}`: required", join(&f.name)));
            }
            Some(Value::Null) | None => {}
            Some(x) => f.ty.check(x, &join(&f.name), errors),
        }
    }
    for f in fields {
        if let Some((other, want)) = &f.required_when
            && obj.get(other) == Some(want)
            && !present(&f.name)
        {
            errors.push(format!(
                "`{}`: required when `{}` is {want}",
                join(&f.name),
                join(other)
            ));
        }
    }
    let mut seen_groups: Vec<&str> = Vec::new();
    for f in fields {
        let Some(g) = &f.group else { continue };
        if seen_groups.contains(&g.as_str()) {
            continue;
        }
        seen_groups.push(g);
        let members: Vec<&FieldSchema> = fields
            .iter()
            .filter(|m| m.group.as_deref() == Some(g))
            .collect();
        let names: Vec<String> = members
            .iter()
            .map(|m| format!("`{}`", join(&m.name)))
            .collect();
        let set = members.iter().filter(|m| present(&m.name)).count();
        if set > 1 {
            errors.push(format!("only one of {} may be set", names.join(", ")));
        } else if set == 0 && members.iter().any(|m| m.required) {
            errors.push(format!("one of {} is required", names.join(", ")));
        }
    }
    for k in obj.keys() {
        if !fields.iter().any(|f| &f.name == k) {
            let mut msg = format!("`{}`: unknown field", join(k));
            if let Some(s) = suggest(k, fields.iter().map(|f| f.name.as_str())) {
                msg.push_str(&format!(" (did you mean `{s}`?)"));
            }
            errors.push(msg);
        }
    }
}

/// Cheap typo suggestion: the candidate with the smallest edit distance if it is close.
fn suggest<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, c)| *d <= 2.max(c.len() / 3))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> ResourceSchema {
        ResourceSchema::new("t.thing", "test")
            .input(field("on", FieldType::Connection).required())
            .input(field("path", FieldType::String).required().replace())
            .input(field("mode", FieldType::String).default("0644"))
            .input(field("tags", FieldType::list(FieldType::String)))
            .input(field(
                "state",
                FieldType::enumeration(["present", "absent"]),
            ))
    }

    #[test]
    fn validates_types_and_required() {
        let s = schema();
        assert!(
            s.validate(&json!({"on": {"kind": "local"}, "path": "/x"}))
                .is_ok()
        );
        let errs = s
            .validate(&json!({"on": {"kind": "nope"}, "tags": [1], "state": "meh", "pth": "/x"}))
            .unwrap_err();
        let joined = errs.join("\n");
        assert!(joined.contains("`path`: required"), "{joined}");
        assert!(joined.contains("invalid connection"), "{joined}");
        assert!(joined.contains("`tags[0]`: expected string"), "{joined}");
        assert!(joined.contains("one of"), "{joined}");
        assert!(joined.contains("did you mean `path`"), "{joined}");
    }

    #[test]
    fn unknown_values_pass() {
        let s = schema();
        let v = json!({"on": crate::model::unknown(), "path": crate::model::unknown()});
        assert!(s.validate(&v).is_ok());
    }

    #[test]
    fn refs_pass() {
        let s = schema();
        let r = OutputRef::new(crate::model::Urn::new("a", "b"), "x").to_value();
        let v = json!({"on": {"kind": "ssh", "host": r.clone()}, "path": r, "tags": [r]});
        assert!(s.validate(&v).is_ok());
    }

    #[test]
    fn groups_and_open_enums() {
        let s = ResourceSchema::new("t.g", "")
            .input(field("content", FieldType::String).required().group("body"))
            .input(field("source", FieldType::String).group("body"))
            .input(field("region", FieldType::open_enum(["us-east"])));
        assert!(
            s.validate(&json!({"content": "x", "region": "somewhere-new"}))
                .is_ok()
        );
        let e = s
            .validate(&json!({"content": "x", "source": "y"}))
            .unwrap_err()
            .join("\n");
        assert!(e.contains("only one of `content`, `source`"), "{e}");
        let e = s.validate(&json!({})).unwrap_err().join("\n");
        assert!(e.contains("one of `content`, `source` is required"), "{e}");
        assert_eq!(s.groups()[0].0, "body");
        let s = ResourceSchema::new("t.r", "")
            .input(field("type", FieldType::enumeration(["master", "slave"])).default("master"))
            .input(field("soa_email", FieldType::String).required_when("type", "master"));
        let mut v = json!({});
        s.apply_defaults(&mut v);
        let e = s.validate(&v).unwrap_err().join("\n");
        assert!(
            e.contains("`soa_email`: required when `type` is \"master\""),
            "{e}"
        );
        assert!(s.validate(&json!({"type": "slave"})).is_ok());
        assert!(
            s.validate(&json!({"soa_email": crate::model::secret(json!("x"))}))
                .is_ok()
        );
        assert!(
            s.validate(&json!({"soa_email": crate::model::secret(json!(1))}))
                .is_err()
        );
        assert_eq!(FieldType::open_enum(["a"]).display(), "\"a\" | string");
    }

    #[test]
    fn defaults() {
        let s = schema();
        let mut v = json!({"on": {"kind": "local"}, "path": "/x"});
        s.apply_defaults(&mut v);
        assert_eq!(v["mode"], "0644");
    }
}
