//! Edition 0.2 authoring. Pure functions, nominal records and scoped constructors.
//! Provider lowering and configuration execution reuse the legacy engine.
use crate::{
    language::{
        self, Analysis, Atom, Binding, Completion, Env, Evaluator, Hint, Navigation, Symbol, Val,
    },
    syntax::{self, Diagnostic, Expr, ExprKind, Field, Function, Name, Span, Stmt, StmtKind},
};
use ifx_program::{ResourceDecl, model::unknown, schema::FieldType};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};

type Result<T> = std::result::Result<T, Diagnostic>;
pub const EDITION: &str = "0.2-draft";
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Data(FieldType),
    Record(String),
    Handle(String),
    List(Box<Ty>),
    Map(Box<Ty>),
    Unit,
}
#[derive(Clone)]
enum Value {
    Data(Atom),
    Record(String, BTreeMap<String, Value>),
    Handle(Val),
    List(Ty, Vec<Value>),
    Map(Ty, BTreeMap<String, Value>),
    Type(String),
    Builder(Builder),
    Unit,
}
#[derive(Clone)]
struct Builder {
    owner: String,
    values: BTreeMap<String, Value>,
    key: Option<String>,
    configs: Vec<(String, Name, Vec<Stmt>, Span)>,
}
#[derive(Clone)]
struct Record {
    file: String,
    public: bool,
    name: Name,
    fields: Vec<Field>,
}
#[derive(Clone)]
struct Callable {
    file: String,
    owner: Option<String>,
    function: Function,
}
#[derive(Default)]
struct Definitions {
    records: BTreeMap<String, Record>,
    functions: BTreeMap<String, Callable>,
    names: BTreeMap<String, BTreeMap<String, String>>,
    effects: RefCell<BTreeMap<String, bool>>,
    effect_work: Cell<usize>,
    diagnostic_file: String,
}
#[derive(Clone)]
struct Local {
    value: Value,
    name: Name,
}
type Locals = BTreeMap<String, Local>;
fn error(span: Span, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(span, message)
}
fn builtin(id: &str) -> Option<&'static str> {
    match id {
        "ifx::linode::Instance" => Some("linode.instance"),
        "ifx::memory::Value" => Some("memory.value"),
        _ => None,
    }
}
fn name_expr(name: &str, span: Span) -> Expr {
    Expr {
        span,
        kind: ExprKind::Name(Name {
            text: name.into(),
            span,
        }),
    }
}

/// No filesystem, network or execution capabilities are accepted at this boundary.
pub fn analyze(
    entry: &str,
    sources: &BTreeMap<String, String>,
    imports: &language::Imports,
    inputs: &BTreeMap<String, Json>,
) -> Analysis {
    run(entry, sources, imports, inputs, true)
}
/// Type-check definitions for editors and libraries without requiring root parameter values.
pub fn check(
    entry: &str,
    sources: &BTreeMap<String, String>,
    imports: &language::Imports,
) -> Analysis {
    run(entry, sources, imports, &BTreeMap::new(), false)
}
fn run(
    entry: &str,
    sources: &BTreeMap<String, String>,
    imports: &language::Imports,
    inputs: &BTreeMap<String, Json>,
    expand: bool,
) -> Analysis {
    let mut analysis = Analysis::default();
    if sources.len() > 32
        || sources.values().any(|s| s.len() > syntax::MAX_SOURCE)
        || sources.values().map(String::len).sum::<usize>() > 1024 * 1024
    {
        analysis
            .diagnostics
            .push(error(Span::default(), "workspace exceeds source limits"));
        return analysis;
    }
    fn input_valid(v: &Json, depth: usize) -> bool {
        if depth > 32 {
            return false;
        }
        match v {
            Json::Array(a) => a.len() <= 256 && a.iter().all(|v| input_valid(v, depth + 1)),
            Json::Object(m) => {
                m.len() <= 256
                    && m.iter()
                        .all(|(k, v)| !k.starts_with('$') && input_valid(v, depth + 1))
            }
            Json::Number(n) => n.as_i64().is_some(),
            Json::String(s) => s.len() <= 64 * 1024,
            Json::Bool(_) => true,
            Json::Null => false,
        }
    }
    if inputs.len() > 128
        || !inputs.values().all(|v| input_valid(v, 0))
        || inputs.values().map(language::json_size).sum::<usize>() > 256 * 1024
    {
        analysis.diagnostics.push(error(Span::default(), "inputs require bounded concrete data; reserved references and fabricated handles are forbidden"));
        return analysis;
    }
    let parsed: BTreeMap<_, _> = sources
        .iter()
        .map(|(p, s)| (p.clone(), syntax::parse(s)))
        .collect();
    let mut definitions = Definitions::default();
    let setup = definitions.collect(&parsed, imports);
    for (file, ast) in &parsed {
        for diagnostic in &ast.diagnostics {
            analysis.diagnostics.push(if file == entry {
                diagnostic.clone()
            } else {
                error(Span::default(), format!("{file}: {}", diagnostic.message))
            });
        }
    }
    if let Err(d) = setup {
        let d = if definitions.diagnostic_file != entry {
            error(
                Span::default(),
                format!(
                    "{} at byte {}: {}",
                    definitions.diagnostic_file, d.span.start, d.message
                ),
            )
        } else {
            d
        };
        analysis.diagnostics.push(d);
        return analysis;
    }
    analysis.modules = definitions
        .names
        .get(entry)
        .map(|_| {
            imports
                .get(entry)
                .into_iter()
                .flat_map(|m| m.iter())
                .flat_map(|(prefix, file)| {
                    definitions
                        .names
                        .get(file)
                        .into_iter()
                        .flat_map(|names| names.iter())
                        .filter(|(_, id)| definitions.public(id))
                        .map(move |(name, _)| format!("{prefix}::{name}"))
                })
                .chain(["ifx::linode::Instance".into(), "ifx::memory::Value".into()])
                .collect()
        })
        .unwrap_or_default();
    for (name, id) in definitions.names.get(entry).into_iter().flatten() {
        let (span, detail) = if let Some(r) = definitions.records.get(id) {
            (
                if r.file == entry {
                    r.name.span
                } else {
                    Span::default()
                },
                format!("struct {}", r.name.text),
            )
        } else if let Some(f) = definitions.functions.get(id) {
            (
                if f.file == entry {
                    f.function.name.span
                } else {
                    Span::default()
                },
                format!("fn {} -> {}", f.function.name.text, f.function.result),
            )
        } else {
            (Span::default(), "provider type".into())
        };
        analysis.symbols.push(Symbol {
            name: name.clone(),
            span,
            scope: Span {
                start: 0,
                end: sources.get(entry).map_or(0, String::len),
            },
            detail,
        });
    }
    for statement in parsed
        .get(entry)
        .map(|p| p.statements.as_slice())
        .unwrap_or_default()
    {
        if let StmtKind::Use { name, .. } = &statement.kind {
            if let Some(id) = definitions.names.get(entry).and_then(|n| n.get(&name.text)) {
                let target = definitions
                    .records
                    .get(id)
                    .map(|r| (&r.file, &r.name))
                    .or_else(|| {
                        definitions
                            .functions
                            .get(id)
                            .map(|f| (&f.file, &f.function.name))
                    });
                if let Some((file, target)) = target {
                    let coord = |byte: usize| {
                        let p = &sources[file][..byte];
                        (
                            p.bytes().filter(|b| *b == b'\n').count(),
                            p.rsplit('\n').next().unwrap_or("").encode_utf16().count(),
                        )
                    };
                    analysis.navigation.push(Navigation {
                        span: statement.span,
                        source: file.clone(),
                        start: coord(target.span.start),
                        end: coord(target.span.end),
                    });
                }
            }
        }
    }
    let schemas = language::catalog();
    {
        let backend = Evaluator::new(&schemas, true, &mut analysis);
        let mut front = Front {
            backend,
            definitions: &definitions,
            sources,
            entry,
            file: entry.into(),
            owner: None,
            expected: Ty::Unit,
            graph: true,
            namespace: Vec::new(),
            calls: Vec::new(),
            keys: BTreeSet::new(),
            scope: Span::default(),
        };
        if let Err(d) = front.check_definitions() {
            let d = if front.file != entry {
                error(
                    Span::default(),
                    format!("{} at byte {}: {}", front.file, d.span.start, d.message),
                )
            } else {
                d
            };
            front.backend.analysis.diagnostics.push(d);
        }
    }
    if expand && analysis.diagnostics.is_empty() {
        let backend = Evaluator::new(&schemas, false, &mut analysis);
        let mut front = Front {
            backend,
            definitions: &definitions,
            sources,
            entry,
            file: entry.into(),
            owner: None,
            expected: Ty::Unit,
            graph: true,
            namespace: Vec::new(),
            calls: Vec::new(),
            keys: BTreeSet::new(),
            scope: Span::default(),
        };
        let main = definitions.names.get(entry).and_then(|n| n.get("main"));
        if let Some(main) = main {
            let supplied: BTreeMap<_, _> = inputs
                .iter()
                .map(|(n, v)| (n.clone(), Value::Data(Atom::known(v.clone()))))
                .collect();
            match front.invoke(main, supplied, None, Span::default()) {
                Ok(result) => {
                    let mut compilation = front.backend.output;
                    compilation.language = format!("ifx/{EDITION}");
                    compilation.source_digests = sources
                        .iter()
                        .map(|(p, s)| (p.clone(), format!("{:x}", Sha256::digest(s.as_bytes()))))
                        .collect();
                    let encoded = result.encode();
                    compilation.outputs = match (&result, encoded) {
                        (Value::Record(..) | Value::Map(..), Json::Object(o)) => {
                            o.into_iter().collect()
                        }
                        (Value::Unit, _) => BTreeMap::new(),
                        (_, value) => BTreeMap::from([("result".into(), value)]),
                    };
                    let mut seen = BTreeSet::new();
                    if compilation
                        .program
                        .resources
                        .iter()
                        .any(|r| !seen.insert(r.urn.clone()))
                    {
                        front
                            .backend
                            .analysis
                            .diagnostics
                            .push(error(Span::default(), "duplicate resource identity"));
                    } else {
                        front.backend.analysis.compilation = Some(compilation);
                    }
                }
                Err(d) => front.backend.analysis.diagnostics.push(d),
            }
        } else if !sources.contains_key(entry) {
            front
                .backend
                .analysis
                .diagnostics
                .push(error(Span::default(), "entry source is not supplied"));
        }
    }
    analysis.diagnostics.truncate(100);
    analysis
}
impl Definitions {
    fn public(&self, id: &str) -> bool {
        builtin(id).is_some()
            || self.records.get(id).is_some_and(|r| r.public)
            || self
                .functions
                .get(id)
                .is_some_and(|f| f.function.public && f.function.name.text != "main")
    }
    fn collect(
        &mut self,
        parsed: &BTreeMap<String, syntax::Parsed>,
        imports: &language::Imports,
    ) -> Result<()> {
        for (file, ast) in parsed {
            self.diagnostic_file = file.clone();
            let names = self.names.entry(file.clone()).or_default();
            for statement in &ast.statements {
                let (name, id) = match &statement.kind {
                    StmtKind::Struct {
                        public,
                        name,
                        fields,
                    } => {
                        let id = format!("{file}#{}", name.text);
                        self.records.insert(
                            id.clone(),
                            Record {
                                file: file.clone(),
                                public: *public,
                                name: name.clone(),
                                fields: fields.clone(),
                            },
                        );
                        (name, id)
                    }
                    StmtKind::Function(function) => {
                        let id = format!("{file}#{}", function.name.text);
                        self.functions.insert(
                            id.clone(),
                            Callable {
                                file: file.clone(),
                                owner: None,
                                function: function.clone(),
                            },
                        );
                        (&function.name, id)
                    }
                    StmtKind::Use { .. } | StmtKind::Impl { .. } => continue,
                    _ => {
                        return Err(error(
                            statement.span,
                            "edition 0.2 files contain definitions; executable statements belong in main or a constructor",
                        ));
                    }
                };
                if names.insert(name.text.clone(), id).is_some() {
                    return Err(error(name.span, "duplicate definition"));
                }
            }
        }
        let exports = self.names.clone();
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (file, ast) in parsed {
            self.diagnostic_file = file.clone();
            for statement in &ast.statements {
                if let StmtKind::Use { name, path } = &statement.kind {
                    let target = if builtin(path).is_some() {
                        path.clone()
                    } else {
                        let (prefix, item) = path
                            .rsplit_once("::")
                            .ok_or_else(|| error(statement.span, "use requires a public item"))?;
                        let module =
                            imports
                                .get(file)
                                .and_then(|m| m.get(prefix))
                                .ok_or_else(|| {
                                    error(
                                        statement.span,
                                        format!("module `{prefix}` is not declared in Ifx.toml"),
                                    )
                                })?;
                        edges
                            .entry(file.clone())
                            .or_default()
                            .insert(module.clone());
                        let id = exports
                            .get(module)
                            .and_then(|n| n.get(item))
                            .ok_or_else(|| error(statement.span, "unknown imported item"))?;
                        if !self.public(id) {
                            return Err(error(
                                statement.span,
                                "imported item is private or entry-only",
                            ));
                        }
                        id.clone()
                    };
                    if self
                        .names
                        .get_mut(file)
                        .expect("invariant: collected file")
                        .insert(name.text.clone(), target)
                        .is_some()
                    {
                        return Err(error(name.span, "import shadows a definition"));
                    }
                }
            }
        }
        fn visit(
            file: &str,
            edges: &BTreeMap<String, BTreeSet<String>>,
            active: &mut BTreeSet<String>,
            done: &mut BTreeSet<String>,
        ) -> Result<()> {
            if done.contains(file) {
                return Ok(());
            }
            if !active.insert(file.into()) {
                return Err(error(Span::default(), "module import cycle"));
            }
            for target in edges.get(file).into_iter().flatten() {
                visit(target, edges, active, done)?;
            }
            active.remove(file);
            done.insert(file.into());
            Ok(())
        }
        let mut done = BTreeSet::new();
        for file in parsed.keys() {
            visit(file, &edges, &mut BTreeSet::new(), &mut done)?;
        }
        for (file, ast) in parsed {
            self.diagnostic_file = file.clone();
            for statement in &ast.statements {
                if let StmtKind::Impl { name, functions } = &statement.kind {
                    let owner = self.resolve(file, None, &name.text, name.span)?;
                    if !self.records.get(&owner).is_some_and(|r| r.file == *file) {
                        return Err(error(
                            name.span,
                            "impl must accompany its own user-defined struct; provider/imported types are sealed",
                        ));
                    }
                    for function in functions {
                        let id = format!("{owner}#{}", function.name.text);
                        if self
                            .functions
                            .insert(
                                id,
                                Callable {
                                    file: file.clone(),
                                    owner: Some(owner.clone()),
                                    function: function.clone(),
                                },
                            )
                            .is_some()
                        {
                            return Err(error(function.name.span, "duplicate method"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn resolve(&self, file: &str, owner: Option<&str>, name: &str, span: Span) -> Result<String> {
        if name == "Self" {
            return owner
                .map(str::to_owned)
                .ok_or_else(|| error(span, "Self requires an impl"));
        }
        if let Some((ty, method)) = name.rsplit_once("::") {
            if ty.contains("::") {
                return Err(error(
                    span,
                    "associated calls require Type::method; import qualified items with use",
                ));
            }
            let owner = self.resolve(file, owner, ty, span)?;
            let id = format!("{owner}#{method}");
            let f = self
                .functions
                .get(&id)
                .ok_or_else(|| error(span, "unknown associated function"))?;
            if f.file != file && !f.function.public {
                return Err(error(span, "associated function is private"));
            }
            return Ok(id);
        }
        self.names
            .get(file)
            .and_then(|n| n.get(name))
            .cloned()
            .ok_or_else(|| error(span, format!("unknown name `{name}`")))
    }
    fn ty(&self, file: &str, owner: Option<&str>, text: &str, span: Span) -> Result<Ty> {
        match text {
            "String" => Ok(Ty::Data(FieldType::String)),
            "Int" => Ok(Ty::Data(FieldType::Int)),
            "Bool" => Ok(Ty::Data(FieldType::Bool)),
            "Unit" => Ok(Ty::Unit),
            _ => {
                for (prefix, list) in [("List[", true), ("Map[", false)] {
                    if let Some(inner) = text.strip_prefix(prefix).and_then(|s| s.strip_suffix(']'))
                    {
                        let ty = Box::new(self.ty(file, owner, inner, span)?);
                        return Ok(if list { Ty::List(ty) } else { Ty::Map(ty) });
                    }
                }
                let id = self.resolve(file, owner, text, span)?;
                if let Some(kind) = builtin(&id) {
                    Ok(Ty::Handle(kind.into()))
                } else if self.records.contains_key(&id) {
                    Ok(Ty::Record(id))
                } else {
                    Err(error(span, "expected a type"))
                }
            }
        }
    }
    fn effect(&self, id: &str, stack: &mut Vec<String>) -> Result<bool> {
        if builtin(id).is_some() {
            return Ok(true);
        }
        if let Some(effect) = self.effects.borrow().get(id) {
            return Ok(*effect);
        }
        self.effect_work.set(self.effect_work.get() + 1);
        if self.effect_work.get() > 10_000 {
            return Err(error(Span::default(), "effect analysis work limit"));
        }
        if stack.len() >= 32 || stack.iter().any(|s| s == id) {
            return Err(error(
                Span::default(),
                "recursive call cycle or depth limit",
            ));
        }
        let Some(call) = self.functions.get(id) else {
            return Err(error(
                Span::default(),
                "constructor or function is not defined",
            ));
        };
        stack.push(id.into());
        let mut targets = Vec::new();
        walk_calls(&call.function.body, &mut targets);
        for p in &call.function.parameters {
            if let Some(e) = &p.default {
                expr_calls(e, &mut targets);
            }
        }
        targets.sort();
        targets.dedup();
        let mut graph = false;
        for name in targets {
            let target = if let Some(ty) = name.strip_suffix("::new") {
                let owner =
                    self.resolve(&call.file, call.owner.as_deref(), ty, call.function.span)?;
                if builtin(&owner).is_some() {
                    graph = true;
                    continue;
                }
                format!("{owner}#new")
            } else {
                self.resolve(&call.file, call.owner.as_deref(), &name, call.function.span)?
            };
            graph |= self.effect(&target, stack)?;
        }
        stack.pop();
        self.effects.borrow_mut().insert(id.into(), graph);
        Ok(graph)
    }
}
struct Front<'a> {
    backend: Evaluator<'a>,
    definitions: &'a Definitions,
    sources: &'a BTreeMap<String, String>,
    entry: &'a str,
    file: String,
    owner: Option<String>,
    expected: Ty,
    graph: bool,
    namespace: Vec<String>,
    calls: Vec<String>,
    keys: BTreeSet<Vec<String>>,
    scope: Span,
}
impl Front<'_> {
    fn spend(&mut self, span: Span) -> Result<()> {
        self.backend.work += 1;
        if self.backend.work > 10_000 {
            Err(error(span, "evaluation work limit exceeded"))
        } else {
            Ok(())
        }
    }
    fn ty(&self, text: &str, span: Span) -> Result<Ty> {
        self.definitions
            .ty(&self.file, self.owner.as_deref(), text, span)
    }
    fn nav(&mut self, span: Span, file: &str, name: &Name) {
        if self.backend.checking && self.file == self.entry {
            let source = &self.sources[file];
            let coord = |byte: usize| {
                let p = &source[..byte.min(source.len())];
                (
                    p.bytes().filter(|b| *b == b'\n').count(),
                    p.rsplit('\n').next().unwrap_or("").encode_utf16().count(),
                )
            };
            self.backend.analysis.navigation.push(Navigation {
                span,
                source: file.into(),
                start: coord(name.span.start),
                end: coord(name.span.end),
            });
        }
    }
    fn bind(&mut self, locals: &mut Locals, name: &Name, value: Value) -> Result<()> {
        value.complete(name.span)?;
        if locals.len() >= 128
            || locals.values().map(|l| l.value.size()).sum::<usize>() + value.size() > 256 * 1024
        {
            return Err(error(name.span, "binding storage limit exceeded"));
        }
        if locals.contains_key(&name.text)
            || self
                .definitions
                .names
                .get(&self.file)
                .is_some_and(|n| n.contains_key(&name.text))
        {
            return Err(error(name.span, "binding shadows an existing name"));
        }
        if self.backend.checking && self.file == self.entry {
            self.backend.analysis.symbols.push(Symbol {
                name: name.text.clone(),
                span: name.span,
                scope: self.scope,
                detail: format!("{:?}", value.ty()),
            });
        }
        locals.insert(
            name.text.clone(),
            Local {
                value,
                name: name.clone(),
            },
        );
        Ok(())
    }
    fn check(&self, expected: &Ty, value: &Value, span: Span) -> Result<()> {
        value.complete(span)?;
        match (expected, value) {
            (Ty::Data(FieldType::List { item }), Value::List(_, items)) => {
                for v in items {
                    self.check(&Ty::Data(*item.clone()), v, span)?;
                }
                Ok(())
            }
            (Ty::Data(FieldType::Map { value: ty }), Value::Map(_, items)) => {
                for v in items.values() {
                    self.check(&Ty::Data(*ty.clone()), v, span)?;
                }
                Ok(())
            }
            (Ty::Data(ty), Value::List(..) | Value::Map(..)) => language::check_atom(
                ty,
                &Atom {
                    json: value.encode(),
                    ty: value.ty().schema().unwrap_or(FieldType::Any),
                },
                span,
            ),
            (Ty::List(_) | Ty::Map(_), Value::Data(a)) => {
                let ty = expected
                    .schema()
                    .ok_or_else(|| error(span, "nominal collections require typed values"))?;
                language::check_atom(&ty, a, span)
            }
            (Ty::Data(ty), Value::Data(a)) => language::check_atom(ty, a, span),
            (Ty::Record(a), Value::Record(b, _)) if a == b => Ok(()),
            (Ty::Handle(kind), Value::Handle(Val::Resource { kind: got, .. })) if kind == got => {
                Ok(())
            }
            (Ty::List(ty), Value::List(_, items)) => {
                for value in items {
                    self.check(ty, value, span)?;
                }
                Ok(())
            }
            (Ty::Map(ty), Value::Map(_, items)) => {
                for value in items.values() {
                    self.check(ty, value, span)?;
                }
                Ok(())
            }
            (Ty::Unit, Value::Unit) => Ok(()),
            _ => Err(error(
                span,
                format!("expected {expected:?}, got {:?}", value.ty()),
            )),
        }
    }
    fn placeholder(&mut self, ty: &Ty, depth: usize) -> Result<Value> {
        self.spend(Span::default())?;
        if depth > 32 {
            return Err(error(
                Span::default(),
                "recursive struct or type depth limit",
            ));
        }
        Ok(match ty {
            Ty::Data(ty) => Value::Data(Atom {
                json: unknown(),
                ty: ty.clone(),
            }),
            Ty::Record(id) => {
                let r = self.definitions.records[id].clone();
                let mut fields = BTreeMap::new();
                let mut size = 0;
                for f in &r.fields {
                    let ty = self.definitions.ty(&r.file, Some(id), &f.ty, f.name.span)?;
                    let v = self.placeholder(&ty, depth + 1)?;
                    size += v.size() + f.name.text.len() + 16;
                    if size > 64 * 1024 {
                        return Err(error(f.name.span, "expanded type exceeds 64 KiB"));
                    }
                    fields.insert(f.name.text.clone(), v);
                }
                Value::Record(id.clone(), fields)
            }
            Ty::Handle(kind) => Value::Handle(Val::Resource {
                urn: ResourceDecl::new(kind, "placeholder", json!({})).urn,
                kind: kind.clone(),
            }),
            Ty::List(ty) => {
                self.placeholder(ty, depth + 1)?;
                Value::List(*ty.clone(), Vec::new())
            }
            Ty::Map(ty) => {
                self.placeholder(ty, depth + 1)?;
                Value::Map(*ty.clone(), BTreeMap::new())
            }
            Ty::Unit => Value::Unit,
        })
    }
    fn check_definitions(&mut self) -> Result<()> {
        for (id, record) in &self.definitions.records.clone() {
            self.file = record.file.clone();
            self.owner = Some(id.clone());
            self.placeholder(&Ty::Record(id.clone()), 0)?;
            let mut names = BTreeSet::new();
            for f in &record.fields {
                if !names.insert(&f.name.text) {
                    return Err(error(f.name.span, "duplicate struct field"));
                }
                let ty = self.ty(&f.ty, f.name.span)?;
                if record.public && f.public {
                    self.public_type(&ty, f.name.span)?;
                }
                if let Some(default) = &f.default {
                    let v = self.expr(default, &Locals::new())?;
                    self.check(&ty, &v, default.span)?;
                }
            }
        }
        for (id, call) in &self.definitions.functions.clone() {
            self.file = call.file.clone();
            self.owner = call.owner.clone();
            let graph = self.definitions.effect(id, &mut Vec::new())?;
            if graph && !call.graph_context() {
                return Err(error(
                    call.function.name.span,
                    "ordinary functions are pure; put infrastructure in a constructor",
                ));
            }
            if call.function.name.text == "new"
                && (call.owner.is_none() || call.function.result != "Self")
            {
                return Err(error(
                    call.function.name.span,
                    "new must be an associated constructor returning Self",
                ));
            }
            if call.function.public {
                self.public_type(
                    &self.ty(&call.function.result, call.function.name.span)?,
                    call.function.name.span,
                )?;
            }
            let mut supplied = BTreeMap::new();
            for (index, p) in call.function.parameters.iter().enumerate() {
                if p.name.text == "self"
                    && (index != 0 || call.owner.is_none() || call.function.name.text == "new")
                {
                    return Err(error(p.name.span, "invalid self parameter"));
                }
                if call.function.name.text == "new" && p.name.text == "key" {
                    return Err(error(p.name.span, "key is reserved constructor metadata"));
                }
                let ty = self.ty(&p.ty, p.name.span)?;
                if call.function.public {
                    self.public_type(&ty, p.name.span)?;
                }
                supplied.insert(p.name.text.clone(), self.placeholder(&ty, 0)?);
            }
            self.backend.output.program.resources.clear();
            self.backend.output.configurations.clear();
            self.invoke(id, supplied, None, call.function.span)?;
        }
        self.file = self.entry.into();
        Ok(())
    }
    fn public_type(&self, ty: &Ty, span: Span) -> Result<()> {
        match ty {
            Ty::Record(id) if !self.definitions.records[id].public => {
                Err(error(span, "public interface exposes a private type"))
            }
            Ty::List(t) | Ty::Map(t) => self.public_type(t, span),
            _ => Ok(()),
        }
    }
    fn invoke(
        &mut self,
        id: &str,
        mut supplied: BTreeMap<String, Value>,
        receiver: Option<Value>,
        span: Span,
    ) -> Result<Value> {
        self.spend(span)?;
        if self.calls.len() >= 32 || self.calls.iter().any(|f| f == id) {
            return Err(error(span, "recursive call cycle or depth limit"));
        }
        let call = self
            .definitions
            .functions
            .get(id)
            .ok_or_else(|| error(span, "unknown function"))?
            .clone();
        if let Some(receiver) = receiver {
            supplied.insert("self".into(), receiver);
        }
        let old = (
            self.file.clone(),
            self.owner.clone(),
            self.expected.clone(),
            self.graph,
            self.scope,
        );
        self.file = call.file.clone();
        self.owner = call.owner.clone();
        self.expected = self.ty(&call.function.result, call.function.name.span)?;
        self.graph = call.graph_context();
        self.scope = call.function.span;
        self.calls.push(id.into());
        let result = (|| {
            let mut locals = Locals::new();
            for p in &call.function.parameters {
                let ty = self.ty(&p.ty, p.name.span)?;
                let default = if let Some(e) = &p.default {
                    let v = self.expr(e, &Locals::new())?;
                    self.check(&ty, &v, e.span)?;
                    Some(v)
                } else {
                    None
                };
                let mut value = supplied.remove(&p.name.text).or(default).ok_or_else(|| {
                    error(
                        span,
                        format!("missing required parameter `{}`", p.name.text),
                    )
                })?;
                self.check(&ty, &value, span)?;
                value.refine(&ty);
                self.bind(&mut locals, &p.name, value)?;
            }
            if !supplied.is_empty() {
                return Err(error(span, "unknown or duplicate parameter"));
            }
            let value = self
                .block(&call.function.body, &mut locals)?
                .unwrap_or(Value::Unit);
            self.check(&self.expected, &value, span)?;
            Ok(value)
        })();
        self.calls.pop();
        self.file = old.0;
        self.owner = old.1;
        self.expected = old.2;
        self.graph = old.3;
        self.scope = old.4;
        result.map_err(|d| {
            if call.file != self.file {
                error(
                    span,
                    format!("{} at byte {}: {}", call.file, d.span.start, d.message),
                )
            } else {
                d
            }
        })
    }
    fn block(&mut self, body: &[Stmt], locals: &mut Locals) -> Result<Option<Value>> {
        let old = self.scope;
        if let (Some(first), Some(last)) = (body.first(), body.last()) {
            self.scope = Span {
                start: first.span.start,
                end: last.span.end,
            };
        }
        let result = self.block_inner(body, locals);
        self.scope = old;
        result
    }
    fn block_inner(&mut self, body: &[Stmt], locals: &mut Locals) -> Result<Option<Value>> {
        for statement in body {
            self.spend(statement.span)?;
            match &statement.kind {
                StmtKind::Bind {
                    category,
                    name,
                    ty,
                    value,
                } if category == "let" => {
                    let v = self.expr(value, locals)?;
                    let mut v = self.finalize(v, locals, value.span)?;
                    if let Some(ty) = ty {
                        let ty = self.ty(ty, name.span)?;
                        self.check(&ty, &v, value.span)?;
                        v.refine(&ty);
                    }
                    self.bind(locals, name, v)?;
                }
                StmtKind::Return(expr) => {
                    let value = if let Some(e) = expr {
                        self.expr(e, locals)?
                    } else {
                        Value::Unit
                    };
                    self.check(&self.expected, &value, statement.span)?;
                    return Ok(Some(value));
                }
                StmtKind::Expr(expr) => {
                    let v = self.expr(expr, locals)?;
                    let builder = matches!(v, Value::Builder(_));
                    self.finalize(v, locals, expr.span)?;
                    if !builder {
                        return Err(error(expr.span, "unused value"));
                    }
                }
                StmtKind::If { condition, yes, no } => {
                    let value = self.expr(condition, locals)?;
                    self.check(&Ty::Data(FieldType::Bool), &value, condition.span)?;
                    if self.backend.checking {
                        let a = self.block(yes, &mut locals.clone())?;
                        let b = self.block(no, &mut locals.clone())?;
                        if a.is_some() && b.is_some() {
                            return Ok(a);
                        }
                    } else {
                        let Value::Data(v) = value else {
                            unreachable!()
                        };
                        let test = v.json.as_bool().ok_or_else(|| {
                            error(condition.span, "graph condition must be known")
                        })?;
                        if let Some(v) =
                            self.block(if test { yes } else { no }, &mut locals.clone())?
                        {
                            return Ok(Some(v));
                        }
                    }
                }
                StmtKind::For {
                    key,
                    value,
                    collection,
                    body,
                } => {
                    let values = self.expr(collection, locals)?;
                    let entries = match values {
                        Value::Map(ty, items) if value.is_some() => {
                            if items.is_empty() && self.backend.checking {
                                vec![(
                                    Value::Data(Atom::known(json!("entry"))),
                                    Some(self.placeholder(&ty, 0)?),
                                )]
                            } else {
                                items
                                    .into_iter()
                                    .map(|(k, v)| (Value::Data(Atom::known(json!(k))), Some(v)))
                                    .collect()
                            }
                        }
                        Value::List(ty, items) if value.is_none() => {
                            if items.is_empty() && self.backend.checking {
                                vec![(self.placeholder(&ty, 0)?, None)]
                            } else {
                                items.into_iter().map(|v| (v, None)).collect()
                            }
                        }
                        _ => {
                            return Err(error(
                                collection.span,
                                "iterate lists with one binding or known maps with key, value",
                            ));
                        }
                    };
                    if entries.len() > 256 {
                        return Err(error(collection.span, "collection exceeds 256 entries"));
                    }
                    for (k, v) in entries {
                        let old_scope = self.scope;
                        self.scope = statement.span;
                        let mut inner = locals.clone();
                        self.bind(&mut inner, key, k)?;
                        if let (Some(name), Some(v)) = (value, v) {
                            self.bind(&mut inner, name, v)?;
                        }
                        let result = self.block(body, &mut inner)?;
                        self.scope = old_scope;
                        if let Some(result) = result {
                            if !self.backend.checking {
                                return Ok(Some(result));
                            }
                        }
                    }
                }
                _ => {
                    return Err(error(
                        statement.span,
                        "only let, return, if, for and expression statements are allowed in function bodies",
                    ));
                }
            }
        }
        Ok(None)
    }
    fn expr(&mut self, e: &Expr, locals: &Locals) -> Result<Value> {
        self.spend(e.span)?;
        let result = match &e.kind {
            ExprKind::Literal(v) => Value::Data(Atom::known(v.clone())),
            ExprKind::Name(name) => {
                if self.backend.checking && self.file == self.entry {
                    if let Some((ty, _)) = name.text.rsplit_once("::") {
                        if let Ok(id) = self.definitions.resolve(
                            &self.file,
                            self.owner.as_deref(),
                            ty,
                            name.span,
                        ) {
                            self.backend.analysis.hints.push(Hint {
                                span: Span {
                                    start: name.span.start,
                                    end: name.span.start + ty.len(),
                                },
                                items: self.completions(&Value::Type(id)),
                            });
                        }
                    }
                }
                if let Some(local) = locals.get(&name.text) {
                    self.nav(name.span, &self.file.clone(), &local.name);
                    local.value.clone()
                } else {
                    let id = self.definitions.resolve(
                        &self.file,
                        self.owner.as_deref(),
                        name.text.trim_end_matches("::"),
                        name.span,
                    )?;
                    if let Some(r) = self.definitions.records.get(&id) {
                        self.nav(name.span, &r.file, &r.name);
                    }
                    Value::Type(id)
                }
            }
            ExprKind::Record(name, fields) => {
                let id = self.definitions.resolve(
                    &self.file,
                    self.owner.as_deref(),
                    &name.text,
                    name.span,
                )?;
                let record = self
                    .definitions
                    .records
                    .get(&id)
                    .ok_or_else(|| {
                        error(
                            name.span,
                            "provider handles are sealed; expected a user struct",
                        )
                    })?
                    .clone();
                self.nav(name.span, &record.file, &record.name);
                if record.file != self.file && record.fields.iter().any(|f| !f.public) {
                    return Err(error(
                        name.span,
                        "cannot construct a struct with private fields",
                    ));
                }
                let mut values = BTreeMap::new();
                for (name, expr) in fields {
                    let field = record
                        .fields
                        .iter()
                        .find(|f| f.name.text == name.text)
                        .ok_or_else(|| error(name.span, "unknown struct field"))?;
                    self.nav(name.span, &record.file, &field.name);
                    let mut v = self.expr(expr, locals)?;
                    let ty =
                        self.definitions
                            .ty(&record.file, Some(&id), &field.ty, field.name.span)?;
                    self.check(&ty, &v, expr.span)?;
                    v.refine(&ty);
                    if values.values().map(Value::size).sum::<usize>() + v.size() > 64 * 1024 {
                        return Err(error(e.span, "expanded value exceeds 64 KiB"));
                    }
                    if values.insert(name.text.clone(), v).is_some() {
                        return Err(error(name.span, "duplicate struct field"));
                    }
                }
                for field in &record.fields {
                    if !values.contains_key(&field.name.text) {
                        let default = field.default.as_ref().ok_or_else(|| {
                            error(
                                e.span,
                                format!("missing required field `{}`", field.name.text),
                            )
                        })?;
                        let old = (self.file.clone(), self.owner.clone());
                        self.file = record.file.clone();
                        self.owner = Some(id.clone());
                        let v = self.expr(default, &Locals::new());
                        self.file = old.0;
                        self.owner = old.1;
                        let v = v?;
                        if values.values().map(Value::size).sum::<usize>() + v.size() > 64 * 1024 {
                            return Err(error(e.span, "expanded value exceeds 64 KiB"));
                        }
                        values.insert(field.name.text.clone(), v);
                    }
                }
                Value::Record(id, values)
            }
            ExprKind::List(items) => {
                let mut values = Vec::new();
                let mut ty = Ty::Unit;
                let mut size = 0;
                for item in items {
                    let v = self.expr(item, locals)?;
                    v.complete(item.span)?;
                    if !values.is_empty() {
                        self.check(&ty, &v, item.span)?;
                    } else {
                        ty = v.ty();
                    }
                    size += v.size();
                    if values.len() >= 256 || size > 64 * 1024 {
                        return Err(error(e.span, "collection exceeds limits"));
                    }
                    values.push(v);
                }
                Value::List(ty, values)
            }
            ExprKind::Map(items) => {
                let mut values = BTreeMap::new();
                let mut ty = Ty::Unit;
                let mut size = 0;
                for (name, item) in items {
                    let v = self.expr(item, locals)?;
                    v.complete(item.span)?;
                    if !values.is_empty() {
                        self.check(&ty, &v, item.span)?;
                    } else {
                        ty = v.ty();
                    }
                    size += v.size() + name.text.len();
                    if values.len() >= 256 || size > 64 * 1024 {
                        return Err(error(e.span, "collection exceeds limits"));
                    }
                    if values.insert(name.text.clone(), v).is_some() {
                        return Err(error(name.span, "duplicate map key"));
                    }
                }
                Value::Map(ty, values)
            }
            ExprKind::Member(base, name) => {
                let value = self.expr(base, locals)?;
                match value {
                    Value::Record(id, fields) => {
                        let record = &self.definitions.records[&id];
                        let field = record
                            .fields
                            .iter()
                            .find(|f| f.name.text == name.text)
                            .ok_or_else(|| error(name.span, "unknown struct field"))?;
                        if !field.public && record.file != self.file {
                            return Err(error(name.span, "field is private"));
                        }
                        self.nav(name.span, &record.file, &field.name);
                        fields[&name.text].clone()
                    }
                    Value::Map(ty, _) if self.backend.checking => self.placeholder(&ty, 0)?,
                    Value::Map(_, fields) => fields
                        .get(&name.text)
                        .cloned()
                        .ok_or_else(|| error(name.span, "unknown map key"))?,
                    v => self.bridge(
                        Expr {
                            span: e.span,
                            kind: ExprKind::Member(
                                Box::new(name_expr("receiver", base.span)),
                                name.clone(),
                            ),
                        },
                        BTreeMap::from([("receiver".into(), v)]),
                    )?,
                }
            }
            ExprKind::Index(base, index) => {
                let value = self.expr(base, locals)?;
                let index = self.expr(index, locals)?;
                match (value, index) {
                    (Value::Map(ty, _), Value::Data(a)) if self.backend.checking => {
                        language::check_atom(&FieldType::String, &a, e.span)?;
                        self.placeholder(&ty, 0)?
                    }
                    (Value::List(ty, _), Value::Data(a)) if self.backend.checking => {
                        language::check_atom(&FieldType::Int, &a, e.span)?;
                        self.placeholder(&ty, 0)?
                    }
                    (Value::Map(_, items), Value::Data(a)) => items
                        .get(
                            a.json
                                .as_str()
                                .ok_or_else(|| error(e.span, "map index must be a known String"))?,
                        )
                        .cloned()
                        .ok_or_else(|| error(e.span, "map key is absent"))?,
                    (Value::List(_, items), Value::Data(a)) => items
                        .get(
                            a.json
                                .as_u64()
                                .and_then(|i| usize::try_from(i).ok())
                                .ok_or_else(|| {
                                    error(e.span, "list index must be a known nonnegative Int")
                                })?,
                        )
                        .cloned()
                        .ok_or_else(|| error(e.span, "list index is absent"))?,
                    (a, b) => self.bridge(
                        Expr {
                            span: e.span,
                            kind: ExprKind::Index(
                                Box::new(name_expr("left", base.span)),
                                Box::new(name_expr("right", e.span)),
                            ),
                        },
                        BTreeMap::from([("left".into(), a), ("right".into(), b)]),
                    )?,
                }
            }
            ExprKind::Binary(left, op, right) => {
                let a = self.expr(left, locals)?;
                let b = self.expr(right, locals)?;
                self.bridge(
                    Expr {
                        span: e.span,
                        kind: ExprKind::Binary(
                            Box::new(name_expr("left", left.span)),
                            op.clone(),
                            Box::new(name_expr("right", right.span)),
                        ),
                    },
                    BTreeMap::from([("left".into(), a), ("right".into(), b)]),
                )?
            }
            ExprKind::Not(inner) => {
                let a = self.expr(inner, locals)?;
                self.bridge(
                    Expr {
                        span: e.span,
                        kind: ExprKind::Not(Box::new(name_expr("value", inner.span))),
                    },
                    BTreeMap::from([("value".into(), a)]),
                )?
            }
            ExprKind::Call(callee, args) => self.call(callee, args, locals, e.span)?,
            ExprKind::Lambda(..) => {
                return Err(error(
                    e.span,
                    "lambdas belong to configuration/policy methods",
                ));
            }
        };
        if result.size() > 64 * 1024 {
            return Err(error(e.span, "expanded value exceeds 64 KiB"));
        }
        if self.backend.checking && self.file == self.entry {
            let items = self.completions(&result);
            if !items.is_empty() {
                self.backend.analysis.hints.push(Hint {
                    span: e.span,
                    items,
                });
            }
        }
        Ok(result)
    }
    fn bridge(&mut self, expr: Expr, values: BTreeMap<String, Value>) -> Result<Value> {
        let mut env = Env::new();
        for (name, value) in values {
            env.insert(
                name.clone(),
                Binding {
                    name: Name {
                        text: name,
                        span: expr.span,
                    },
                    value: self.legacy(&value, expr.span)?,
                },
            );
        }
        // Synthetic operand names are not source symbols; keep only semantic backend hints.
        let hints = self.backend.analysis.hints.len();
        let occurrences = self.backend.analysis.occurrences.len();
        let result = self.backend.expr(&expr, &env);
        self.backend.analysis.occurrences.truncate(occurrences);
        if self.file != self.entry {
            self.backend.analysis.hints.truncate(hints);
        }
        match result? {
            Val::Data(a) => Ok(Value::Data(a)),
            Val::Resource { urn, kind } => Ok(Value::Handle(Val::Resource { urn, kind })),
            _ => Err(error(expr.span, "expected a completed value")),
        }
    }
    fn legacy(&self, value: &Value, span: Span) -> Result<Val> {
        Ok(match value {
            Value::Data(a) => Val::Data(a.clone()),
            Value::Handle(v) => v.clone(),
            Value::Unit => Val::Void,
            Value::Record(id, fields) => {
                let record = &self.definitions.records[id];
                let mut visible = BTreeMap::new();
                for field in &record.fields {
                    if field.public || record.file == self.file {
                        visible.insert(
                            field.name.text.clone(),
                            self.legacy(&fields[&field.name.text], span)?,
                        );
                    }
                }
                Val::Record { fields: visible }
            }
            Value::List(_, _) | Value::Map(_, _) => {
                // Until host collections carry nominal metadata, refuse nominal captures.
                if value.ty().schema().is_none() {
                    return Err(error(
                        span,
                        "configuration cannot capture collections of structs or handles in this MVP",
                    ));
                }
                Val::Data(Atom {
                    json: value.encode(),
                    ty: value
                        .ty()
                        .schema()
                        .expect("invariant: primitive capture checked"),
                })
            }
            _ => {
                return Err(error(
                    span,
                    "pending builders/types cannot be stored, passed or captured",
                ));
            }
        })
    }
    fn call(&mut self, callee: &Expr, args: &[Expr], locals: &Locals, span: Span) -> Result<Value> {
        if let ExprKind::Name(name) = &callee.kind {
            if let Some(ty) = name.text.strip_suffix("::new") {
                if !args.is_empty() {
                    return Err(error(
                        span,
                        "new() uses named setters; positional constructor arguments are not allowed",
                    ));
                }
                let id =
                    self.definitions
                        .resolve(&self.file, self.owner.as_deref(), ty, name.span)?;
                if let Some(record) = self.definitions.records.get(&id) {
                    let method = self
                        .definitions
                        .functions
                        .get(&format!("{id}#new"))
                        .ok_or_else(|| error(span, "type has no new constructor"))?;
                    if record.file != self.file && !method.function.public {
                        return Err(error(name.span, "constructor is private"));
                    }
                    self.nav(name.span, &method.file, &method.function.name);
                } else if builtin(&id).is_none() {
                    return Err(error(span, "new requires a type"));
                }
                return Ok(Value::Builder(Builder {
                    owner: id,
                    values: BTreeMap::new(),
                    key: None,
                    configs: Vec::new(),
                }));
            }
            let id = self.definitions.resolve(
                &self.file,
                self.owner.as_deref(),
                &name.text,
                name.span,
            )?;
            let function = self
                .definitions
                .functions
                .get(&id)
                .ok_or_else(|| error(span, "expected an ordinary function"))?
                .clone();
            if function.function.name.text == "main" {
                return Err(error(span, "main is entry-only"));
            }
            if function
                .function
                .parameters
                .first()
                .is_some_and(|p| p.name.text == "self")
            {
                return Err(error(name.span, "instance method requires a receiver"));
            }
            self.nav(name.span, &function.file, &function.function.name);
            let supplied = self.positional(&function, args, locals, span, false)?;
            return self.invoke(&id, supplied, None, span);
        }
        let ExprKind::Member(base, method) = &callee.kind else {
            return Err(error(span, "expected a function, constructor or method"));
        };
        let receiver = self.expr(base, locals)?;
        match receiver {
            Value::Builder(mut builder) => {
                if method.text == "configure" && builtin(&builder.owner).is_some() {
                    if args.len() != 2 {
                        return Err(error(span, "configure requires key and lambda"));
                    }
                    let key_value = self.expr(&args[0], locals)?;
                    let key = self.key(&key_value, args[0].span)?;
                    let ExprKind::Lambda(parameters, body) = &args[1].kind else {
                        return Err(error(args[1].span, "expected configuration lambda"));
                    };
                    if parameters.len() != 1 {
                        return Err(error(args[1].span, "configure requires one host parameter"));
                    }
                    builder
                        .configs
                        .push((key, parameters[0].clone(), body.clone(), args[1].span));
                } else {
                    if args.len() != 1 {
                        return Err(error(span, "setter requires exactly one argument"));
                    }
                    let value = self.expr(&args[0], locals)?;
                    if method.text == "key" {
                        if builder.key.is_some() {
                            return Err(error(span, "duplicate key setter"));
                        }
                        builder.key = Some(self.key(&value, args[0].span)?);
                    } else {
                        if let Some(kind) = builtin(&builder.owner) {
                            let schema = self
                                .backend
                                .schemas
                                .iter()
                                .find(|s| s.type_name == kind)
                                .expect("invariant: generated builtin exists");
                            let field = schema
                                .input_field(&method.text)
                                .ok_or_else(|| error(method.span, "unknown resource input"))?;
                            self.check(&Ty::Data(field.ty.clone()), &value, args[0].span)?;
                        } else {
                            let function =
                                &self.definitions.functions[&format!("{}#new", builder.owner)];
                            let parameter = function
                                .function
                                .parameters
                                .iter()
                                .find(|p| p.name.text == method.text)
                                .ok_or_else(|| {
                                    error(method.span, "unknown constructor parameter")
                                })?;
                            let ty = self.definitions.ty(
                                &function.file,
                                function.owner.as_deref(),
                                &parameter.ty,
                                parameter.name.span,
                            )?;
                            self.check(&ty, &value, args[0].span)?;
                            self.nav(method.span, &function.file, &parameter.name);
                        }
                        if builder.values.insert(method.text.clone(), value).is_some() {
                            return Err(error(span, "parameter set more than once"));
                        }
                    }
                }
                Ok(Value::Builder(builder))
            }
            Value::Record(id, fields) => {
                let method_id = format!("{id}#{}", method.text);
                let function = self
                    .definitions
                    .functions
                    .get(&method_id)
                    .ok_or_else(|| error(method.span, "unknown method"))?
                    .clone();
                if function.file != self.file && !function.function.public {
                    return Err(error(method.span, "method is private"));
                }
                if !function
                    .function
                    .parameters
                    .first()
                    .is_some_and(|p| p.name.text == "self")
                {
                    return Err(error(method.span, "method requires an instance receiver"));
                }
                self.nav(method.span, &function.file, &function.function.name);
                let supplied = self.positional(&function, args, locals, span, true)?;
                self.invoke(&method_id, supplied, Some(Value::Record(id, fields)), span)
            }
            _ => Err(error(
                method.span,
                "completed values do not have builder setters",
            )),
        }
    }
    fn positional(
        &mut self,
        function: &Callable,
        args: &[Expr],
        locals: &Locals,
        span: Span,
        receiver: bool,
    ) -> Result<BTreeMap<String, Value>> {
        let params: Vec<_> = function
            .function
            .parameters
            .iter()
            .skip(usize::from(receiver))
            .collect();
        if args.len() > params.len() {
            return Err(error(span, "too many function arguments"));
        }
        let mut supplied = BTreeMap::new();
        for (p, e) in params.into_iter().zip(args) {
            supplied.insert(p.name.text.clone(), self.expr(e, locals)?);
        }
        Ok(supplied)
    }
    fn key(&self, value: &Value, span: Span) -> Result<String> {
        self.check(&Ty::Data(FieldType::String), value, span)?;
        let Value::Data(a) = value else {
            unreachable!()
        };
        if self.backend.checking && ifx_program::model::contains_unknown(&a.json) {
            let mut refs = Vec::new();
            ifx_program::model::collect_refs(&a.json, &mut refs);
            if refs.is_empty() {
                return Ok("<parameter>".into());
            }
        }
        let key = a.json.as_str().ok_or_else(|| {
            error(
                span,
                "identity must be a known String, not a provider output",
            )
        })?;
        if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            return Err(error(
                span,
                "identity requires 1–128 bytes without control characters",
            ));
        }
        Ok(key.into())
    }
    fn finalize(&mut self, value: Value, locals: &Locals, span: Span) -> Result<Value> {
        let Value::Builder(builder) = value else {
            return Ok(value);
        };
        let graph = if builtin(&builder.owner).is_some() {
            true
        } else {
            self.definitions
                .effect(&format!("{}#new", builder.owner), &mut Vec::new())?
        };
        if graph && !self.graph {
            return Err(error(
                span,
                "ordinary functions are pure; infrastructure belongs in a constructor",
            ));
        }
        if graph && builder.key.is_none() {
            return Err(error(
                span,
                "infrastructure construction requires .key(...)",
            ));
        }
        if !graph && builder.key.is_some() {
            return Err(error(
                span,
                "pure data constructors do not accept .key(...)",
            ));
        }
        if let Some(kind) = builtin(&builder.owner) {
            let mut path = self.namespace.clone();
            path.push(builder.key.expect("invariant: graph key checked"));
            // Uniform encoding keeps a literal JSON-looking leaf distinct from a namespace path.
            let key = json!(path).to_string();
            let mut inputs = serde_json::Map::new();
            for (name, v) in builder.values {
                inputs.insert(name, v.encode());
            }
            let decl = ResourceDecl::new(kind, &key, Json::Object(inputs));
            let mut env = Env::new();
            let mut capture_names = BTreeSet::new();
            for (_, _, body, _) in &builder.configs {
                referenced_names(body, &mut capture_names);
            }
            for (name, local) in locals
                .iter()
                .filter(|(name, _)| capture_names.contains(name.as_str()))
            {
                env.insert(
                    name.clone(),
                    Binding {
                        name: local.name.clone(),
                        value: self.legacy(&local.value, span)?,
                    },
                );
            }
            let before = (
                self.backend.analysis.hints.len(),
                self.backend.analysis.symbols.len(),
                self.backend.analysis.occurrences.len(),
            );
            let result = self
                .backend
                .finalize_resource(decl, builder.configs, span, &env);
            if self.file != self.entry {
                self.backend.analysis.hints.truncate(before.0);
                self.backend.analysis.symbols.truncate(before.1);
                self.backend.analysis.occurrences.truncate(before.2);
            }
            return result.map(Value::Handle);
        }
        let pushed = if let Some(key) = builder.key {
            if self.namespace.len() >= 8 {
                return Err(error(span, "graph namespace depth limit"));
            }
            self.namespace.push(key);
            if !self.backend.checking && !self.keys.insert(self.namespace.clone()) {
                self.namespace.pop();
                return Err(error(span, "duplicate constructor namespace key"));
            }
            true
        } else {
            false
        };
        let result = self.invoke(
            &format!("{}#new", builder.owner),
            builder.values,
            None,
            span,
        );
        if pushed {
            self.namespace.pop();
        }
        result
    }
    fn completions(&self, value: &Value) -> Vec<Completion> {
        let item = |label: String, detail: String| Completion { label, detail };
        match value {
            Value::Type(id) => self
                .definitions
                .functions
                .iter()
                .filter(|(_, f)| {
                    f.owner.as_ref() == Some(id)
                        && f.function
                            .parameters
                            .first()
                            .is_none_or(|p| p.name.text != "self")
                        && (f.file == self.file || f.function.public)
                })
                .map(|(_, f)| {
                    item(
                        f.function.name.text.clone(),
                        format!("fn {} -> {}", f.function.name.text, f.function.result),
                    )
                })
                .chain(builtin(id).map(|_| item("new".into(), "provider constructor".into())))
                .collect(),
            Value::Record(id, _) => {
                let r = &self.definitions.records[id];
                r.fields
                    .iter()
                    .filter(|f| f.public || r.file == self.file)
                    .map(|f| item(f.name.text.clone(), f.ty.clone()))
                    .chain(
                        self.definitions
                            .functions
                            .values()
                            .filter(|f| {
                                f.owner.as_ref() == Some(id)
                                    && f.function
                                        .parameters
                                        .first()
                                        .is_some_and(|p| p.name.text == "self")
                                    && (f.file == self.file || f.function.public)
                            })
                            .map(|f| {
                                item(
                                    f.function.name.text.clone(),
                                    format!("method -> {}", f.function.result),
                                )
                            }),
                    )
                    .collect()
            }
            Value::Builder(b) => {
                let mut items = if let Some(kind) = builtin(&b.owner) {
                    self.backend
                        .schemas
                        .iter()
                        .find(|s| s.type_name == kind)
                        .into_iter()
                        .flat_map(|s| s.inputs.iter())
                        .map(|f| item(f.name.clone(), f.ty.display()))
                        .chain(std::iter::once(item(
                            "configure".into(),
                            "deferred configuration".into(),
                        )))
                        .collect::<Vec<_>>()
                } else {
                    self.definitions.functions[&format!("{}#new", b.owner)]
                        .function
                        .parameters
                        .iter()
                        .map(|p| item(p.name.text.clone(), p.ty.clone()))
                        .collect()
                };
                items.retain(|i| !b.values.contains_key(&i.label));
                let graph = builtin(&b.owner).is_some()
                    || self
                        .definitions
                        .effect(&format!("{}#new", b.owner), &mut Vec::new())
                        .unwrap_or(false);
                if graph && b.key.is_none() {
                    items.push(item("key".into(), "stable infrastructure identity".into()));
                }
                items
            }
            Value::Handle(v) => self.backend.completions(v),
            _ => Vec::new(),
        }
    }
}
impl Callable {
    fn graph_context(&self) -> bool {
        (self.owner.is_some() && self.function.name.text == "new")
            || (self.owner.is_none() && self.function.name.text == "main")
    }
}
impl Ty {
    fn schema(&self) -> Option<FieldType> {
        Some(match self {
            Self::Data(t) => t.clone(),
            Self::List(t) => FieldType::List {
                item: Box::new(t.schema()?),
            },
            Self::Map(t) => FieldType::Map {
                value: Box::new(t.schema()?),
            },
            _ => return None,
        })
    }
}
impl Value {
    fn complete(&self, span: Span) -> Result<()> {
        match self {
            Self::Builder(_) | Self::Type(_) => Err(error(
                span,
                "pending builders/types cannot be stored, passed or returned",
            )),
            Self::Record(_, fields) | Self::Map(_, fields) => {
                for v in fields.values() {
                    v.complete(span)?;
                }
                Ok(())
            }
            Self::List(_, items) => {
                for v in items {
                    v.complete(span)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn refine(&mut self, ty: &Ty) {
        if let Self::Data(a) = self {
            match (ty, &a.json) {
                (Ty::List(t), Json::Array(items)) => {
                    *self = Self::List(
                        *t.clone(),
                        items
                            .iter()
                            .map(|v| Self::Data(Atom::known(v.clone())))
                            .collect(),
                    );
                }
                (Ty::Map(t), Json::Object(items)) if a.concrete() => {
                    *self = Self::Map(
                        *t.clone(),
                        items
                            .iter()
                            .map(|(k, v)| (k.clone(), Self::Data(Atom::known(v.clone()))))
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        if let Self::Data(a) = self {
            if let Some(schema) = ty.schema() {
                a.ty = schema;
            }
        }
        match (self, ty) {
            (Self::List(t, items), Ty::List(expected)) => {
                *t = *expected.clone();
                for v in items {
                    v.refine(expected);
                }
            }
            (Self::Map(t, items), Ty::Map(expected)) => {
                *t = *expected.clone();
                for v in items.values_mut() {
                    v.refine(expected);
                }
            }
            _ => {}
        }
    }

    fn ty(&self) -> Ty {
        match self {
            Self::Data(a) => Ty::Data(a.ty.clone()),
            Self::Record(id, _) => Ty::Record(id.clone()),
            Self::Handle(Val::Resource { kind, .. }) => Ty::Handle(kind.clone()),
            Self::List(ty, _) => Ty::List(Box::new(ty.clone())),
            Self::Map(ty, _) => Ty::Map(Box::new(ty.clone())),
            _ => Ty::Unit,
        }
    }
    fn size(&self) -> usize {
        match self {
            Self::Data(a) => language::json_size(&a.json),
            Self::Record(id, fields) => {
                id.len()
                    + fields
                        .iter()
                        .map(|(k, v)| k.len() + v.size() + 16)
                        .sum::<usize>()
            }
            Self::List(_, items) => items.iter().map(|v| v.size() + 16).sum(),
            Self::Map(_, items) => items.iter().map(|(k, v)| k.len() + v.size() + 16).sum(),
            Self::Handle(v) => language::value_size(v),
            Self::Builder(b) => {
                b.key.as_ref().map_or(0, String::len)
                    + b.owner.len()
                    + b.values
                        .iter()
                        .map(|(k, v)| k.len() + v.size() + 16)
                        .sum::<usize>()
                    + b.configs
                        .iter()
                        .map(|(_, _, _, s)| s.end.saturating_sub(s.start))
                        .sum::<usize>()
            }
            Self::Type(s) => s.len(),
            Self::Unit => 0,
        }
    }
    fn encode(&self) -> Json {
        match self {
            Self::Data(a) => a.json.clone(),
            Self::Record(_, fields) | Self::Map(_, fields) => Json::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.encode()))
                    .collect(),
            ),
            Self::List(_, items) => Json::Array(items.iter().map(Self::encode).collect()),
            Self::Handle(Val::Resource { urn, kind }) => json!({"$resource":urn,"kind":kind}),
            _ => Json::Null,
        }
    }
}
fn walk_calls(body: &[Stmt], calls: &mut Vec<String>) {
    for s in body {
        match &s.kind {
            StmtKind::Bind { value, .. } | StmtKind::Expr(value) => expr_calls(value, calls),
            StmtKind::Return(Some(e)) => expr_calls(e, calls),
            StmtKind::If { condition, yes, no } => {
                expr_calls(condition, calls);
                walk_calls(yes, calls);
                walk_calls(no, calls);
            }
            StmtKind::For {
                collection, body, ..
            } => {
                expr_calls(collection, calls);
                walk_calls(body, calls);
            }
            _ => {}
        }
    }
}
fn expr_calls(e: &Expr, calls: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Call(callee, args) => {
            if let ExprKind::Name(n) = &callee.kind {
                calls.push(n.text.clone());
            }
            expr_calls(callee, calls);
            for arg in args {
                expr_calls(arg, calls);
            }
        }
        ExprKind::Record(_, fields) | ExprKind::Map(fields) => {
            for (_, v) in fields {
                expr_calls(v, calls);
            }
        }
        ExprKind::List(items) => {
            for v in items {
                expr_calls(v, calls);
            }
        }
        ExprKind::Member(base, _) | ExprKind::Not(base) => expr_calls(base, calls),
        ExprKind::Index(a, b) | ExprKind::Binary(a, _, b) => {
            expr_calls(a, calls);
            expr_calls(b, calls);
        }
        ExprKind::Lambda(_, body) => walk_calls(body, calls),
        _ => {}
    }
}

fn referenced_names(body: &[Stmt], names: &mut BTreeSet<String>) {
    fn expr(e: &Expr, names: &mut BTreeSet<String>) {
        match &e.kind {
            ExprKind::Name(n) => {
                names.insert(n.text.clone());
            }
            ExprKind::Record(_, items) | ExprKind::Map(items) => {
                for (_, e) in items {
                    expr(e, names);
                }
            }
            ExprKind::List(items) => {
                for e in items {
                    expr(e, names);
                }
            }
            ExprKind::Member(e, _) | ExprKind::Not(e) => expr(e, names),
            ExprKind::Index(a, b) | ExprKind::Binary(a, _, b) => {
                expr(a, names);
                expr(b, names);
            }
            ExprKind::Call(c, args) => {
                expr(c, names);
                for a in args {
                    expr(a, names);
                }
            }
            ExprKind::Lambda(_, body) => referenced_names(body, names),
            _ => {}
        }
    }
    for s in body {
        match &s.kind {
            StmtKind::Bind { value, .. } | StmtKind::Expr(value) => expr(value, names),
            StmtKind::Return(Some(e)) => expr(e, names),
            StmtKind::If { condition, yes, no } => {
                expr(condition, names);
                referenced_names(yes, names);
                referenced_names(no, names);
            }
            StmtKind::For {
                collection, body, ..
            } => {
                expr(collection, names);
                referenced_names(body, names);
            }
            _ => {}
        }
    }
}
