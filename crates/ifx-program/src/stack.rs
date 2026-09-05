//! The Rust program API: [`Stack`] collects declarations, [`Handle`] gives typed
//! references to a declared resource's outputs, and [`Input`] is a value that is either a
//! literal or such a reference. The per-type builders live in [`crate::generated`].

use std::marker::PhantomData;

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::model::{OutputRef, Program, ResourceDecl, Urn};

/// A resource type marker (`host::File`, `linode::Instance`, ...).
pub trait ResourceType {
    const TYPE: &'static str;
}

/// Something that turns into one [`ResourceDecl`]: the generated `*Inputs` structs.
pub trait Declare {
    type Type: ResourceType;
    fn into_decl(self) -> anyhow::Result<ResourceDecl>;
}

/// An input that is either a literal or a reference to another resource's output.
#[derive(Clone, Debug)]
pub enum Input<T> {
    Value(T),
    Ref(OutputRef),
    /// A model expression such as string concatenation.
    Expression(Value),
    /// A value (or reference) the plan must never print; see [`secret`].
    Secret(Box<Input<T>>),
}

impl<T: Serialize> Serialize for Input<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Input::Value(v) => v.serialize(s),
            Input::Ref(r) => r.to_value().serialize(s),
            Input::Expression(value) => value.serialize(s),
            Input::Secret(inner) => {
                let v = serde_json::to_value(inner).map_err(serde::ser::Error::custom)?;
                crate::model::secret(v).serialize(s)
            }
        }
    }
}

/// Mark an input as sensitive: it is passed to the provider but never shown in plan
/// output (`root_pass: <sensitive>`), whatever the schema says about the field.
pub fn secret<T>(v: impl Into<Input<T>>) -> Input<T> {
    match v.into() {
        s @ Input::Secret(_) => s,
        other => Input::Secret(Box::new(other)),
    }
}

impl<T> From<T> for Input<T> {
    fn from(v: T) -> Self {
        Input::Value(v)
    }
}

impl From<&str> for Input<String> {
    fn from(v: &str) -> Self {
        Input::Value(v.to_string())
    }
}

impl From<&String> for Input<String> {
    fn from(v: &String) -> Self {
        Input::Value(v.clone())
    }
}

impl From<&str> for Input<Value> {
    fn from(v: &str) -> Self {
        Input::Value(Value::String(v.to_string()))
    }
}

impl From<i32> for Input<i64> {
    fn from(v: i32) -> Self {
        Input::Value(i64::from(v))
    }
}

impl From<u32> for Input<i64> {
    fn from(v: u32) -> Self {
        Input::Value(i64::from(v))
    }
}

impl<T> Input<T> {
    pub fn reference(urn: Urn, path: impl Into<String>) -> Self {
        Input::Ref(OutputRef::new(urn, path))
    }

    /// Convert the literal, keeping references and the secret wrapper as they are.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Input<U> {
        match self {
            Input::Value(v) => Input::Value(f(v)),
            Input::Ref(r) => Input::Ref(r),
            Input::Expression(value) => Input::Expression(value),
            Input::Secret(inner) => Input::Secret(Box::new(inner.map(f))),
        }
    }

    /// Like [`Input::map`] for fallible conversions.
    pub fn try_map<U, E>(self, f: impl FnOnce(T) -> Result<U, E>) -> Result<Input<U>, E> {
        Ok(match self {
            Input::Value(v) => Input::Value(f(v)?),
            Input::Ref(r) => Input::Ref(r),
            Input::Expression(value) => Input::Expression(value),
            Input::Secret(inner) => Input::Secret(Box::new(inner.try_map(f)?)),
        })
    }

    /// Reinterpret as another type (references carry no type at runtime).
    pub fn cast<U>(self) -> Input<U>
    where
        T: Into<U>,
    {
        self.map(Into::into)
    }

    /// Select a nested output field or array index while preserving its reference.
    pub fn select<U>(self, path: &str) -> anyhow::Result<Input<U>>
    where
        T: Serialize,
        U: serde::de::DeserializeOwned,
    {
        match self {
            Input::Ref(mut reference) => {
                reference.path = [reference.path.as_str(), path]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(".");
                Ok(Input::Ref(reference))
            }
            Input::Value(value) => {
                let value = serde_json::to_value(value)?;
                let selected = crate::model::get_path(&value, path)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("input has no path `{path}`"))?;
                Ok(Input::Value(serde_json::from_value(selected)?))
            }
            Input::Secret(inner) => Ok(Input::Secret(Box::new(inner.select(path)?))),
            Input::Expression(_) => {
                anyhow::bail!("cannot select `{path}` from a computed expression")
            }
        }
    }
}

impl<T> Input<Vec<T>>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    pub fn at(self, index: usize) -> anyhow::Result<Input<T>> {
        self.select(&index.to_string())
    }
}

/// One scalar or output reference used to construct a deferred string.
pub struct ConcatPart(Value);

pub trait IntoConcatPart {
    fn into_concat_part(self) -> anyhow::Result<ConcatPart>;
}

impl IntoConcatPart for &str {
    fn into_concat_part(self) -> anyhow::Result<ConcatPart> {
        Ok(ConcatPart(Value::String(self.to_string())))
    }
}

impl IntoConcatPart for String {
    fn into_concat_part(self) -> anyhow::Result<ConcatPart> {
        Ok(ConcatPart(Value::String(self)))
    }
}

impl<T: Serialize> IntoConcatPart for Input<T> {
    fn into_concat_part(self) -> anyhow::Result<ConcatPart> {
        Ok(ConcatPart(serde_json::to_value(self)?))
    }
}

pub fn concat(parts: impl IntoIterator<Item = ConcatPart>) -> Input<String> {
    Input::Expression(crate::model::concat(
        parts.into_iter().map(|part| part.0).collect(),
    ))
}

/// Build a deferred string from literals and typed output references.
#[macro_export]
macro_rules! concat {
    ($($part:expr),+ $(,)?) => {{
        use $crate::stack::IntoConcatPart as _;
        let parts = vec![$($part.into_concat_part()?),+];
        $crate::stack::concat(parts)
    }};
}

/// Handle to a declared resource; the generated `impl Handle<Marker>` blocks add one
/// typed accessor per output.
pub struct Handle<R> {
    urn: Urn,
    _r: PhantomData<fn() -> R>,
}

impl<R> Clone for Handle<R> {
    fn clone(&self) -> Self {
        Self {
            urn: self.urn.clone(),
            _r: PhantomData,
        }
    }
}

impl<R> std::fmt::Debug for Handle<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Handle({})", self.urn)
    }
}

impl<R> Handle<R> {
    pub fn new(urn: Urn) -> Self {
        Self {
            urn,
            _r: PhantomData,
        }
    }

    pub fn urn(&self) -> &Urn {
        &self.urn
    }

    /// Reference to an output field (dotted path). The generated accessors are the
    /// checked way to do this; this is for untyped resources.
    pub(crate) fn output<T>(&self, path: &str) -> Input<T> {
        Input::Ref(OutputRef::new(self.urn.clone(), path))
    }
}

impl Handle<Value> {
    /// Reference to an output of an untyped resource declared with [`Stack::resource`].
    pub fn out<T>(&self, path: &str) -> Input<T> {
        self.output(path)
    }
}

/// Collects resource declarations into a [`Program`].
#[derive(Default, Debug)]
pub struct Stack {
    program: Program,
}

impl Stack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a built declaration: `stack.add(File::builder("x").on(..).path(..).build())`.
    pub fn add<D: Declare>(&mut self, d: D) -> anyhow::Result<Handle<D::Type>> {
        let decl = d.into_decl()?;
        let urn = decl.urn.clone();
        self.push(decl)?;
        Ok(Handle::new(urn))
    }

    /// Untyped declaration for types without generated builders.
    pub fn resource(
        &mut self,
        type_name: &str,
        name: &str,
        inputs: Value,
    ) -> anyhow::Result<Handle<Value>> {
        let decl = ResourceDecl::new(type_name, name, strip_nulls(inputs));
        let urn = decl.urn.clone();
        self.push(decl)?;
        Ok(Handle::new(urn))
    }

    fn push(&mut self, decl: ResourceDecl) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.program.get(&decl.urn).is_none(),
            "duplicate resource {}",
            decl.urn
        );
        self.program.resources.push(decl);
        Ok(())
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn into_program(self) -> Program {
        self.program
    }
}

/// `Option::None` fields serialize as null; treat them as absent so schema defaults apply.
pub(crate) fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_wraps_values_and_refs() {
        let v: Input<String> = secret("hunter2");
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            json!({"$secret": "hunter2"})
        );
        let r: Input<String> = secret(Input::<String>::reference(Urn::new("a", "b"), "x"));
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"$secret": {"$ref": "a:b", "$path": "x"}})
        );
        let (plain, fields) = crate::model::strip_secrets(json!({"p": {"$secret": "s"}, "q": 1}));
        assert_eq!(plain, json!({"p": "s", "q": 1}));
        assert_eq!(fields, ["p"]);
    }

    #[test]
    fn untyped_and_duplicates() {
        let mut s = Stack::new();
        let a = s
            .resource("memory.value", "a", json!({"value": "x", "opt": null}))
            .unwrap();
        let r: Input<String> = a.out("value");
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"$ref": "memory.value:a", "$path": "value"})
        );
        assert_eq!(s.program().resources[0].inputs, json!({"value": "x"}));
        assert!(s.resource("memory.value", "a", json!({})).is_err());
    }
}
