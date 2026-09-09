use crate::syntax::{self, Diagnostic, Expr, ExprKind, Name, Span, Stmt, StmtKind};
use ifx_program::{
    Program, ResourceDecl, Urn,
    model::{OutputRef, concat, contains_unknown, unknown},
    schema::{FieldType, ResourceSchema},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const VERSION: &str = "ifx/0.1-experimental";
const MAX_WORK: usize = 10_000;
const MAX_VALUE: usize = 64 * 1024;
const MAX_ENV: usize = 256 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Compilation {
    pub language: String,
    pub catalog_digest: String,
    pub source_digests: BTreeMap<String, String>,
    pub program: Program,
    pub configurations: Vec<Configuration>,
    pub outputs: BTreeMap<String, Value>,
}
impl Default for Compilation {
    fn default() -> Self {
        Self {
            language: VERSION.into(),
            catalog_digest: format!("{:x}", Sha256::digest(include_bytes!("catalog.json"))),
            source_digests: BTreeMap::new(),
            program: Program::default(),
            configurations: Vec::new(),
            outputs: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Configuration {
    pub target: Urn,
    pub key: String,
    pub span: Span,
    pub(crate) parameter: Name,
    pub(crate) body: Vec<Stmt>,
    pub(crate) captures: Env,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Atom {
    pub json: Value,
    pub ty: FieldType,
}
impl Atom {
    fn known(json: Value) -> Self {
        let ty = match &json {
            Value::String(_) => FieldType::String,
            Value::Bool(_) => FieldType::Bool,
            Value::Number(_) => FieldType::Int,
            Value::Array(a) => FieldType::list(
                a.first()
                    .map_or(FieldType::Any, |v| Self::known(v.clone()).ty),
            ),
            Value::Object(_) => FieldType::Any,
            Value::Null => FieldType::Any,
        };
        Self { json, ty }
    }
    fn concrete(&self) -> bool {
        let mut refs = Vec::new();
        ifx_program::model::collect_refs(&self.json, &mut refs);
        refs.is_empty() && !contains_unknown(&self.json)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Val {
    ModuleDefinition(String),
    ModuleBuilder {
        path: String,
        key: String,
        inputs: BTreeMap<String, Atom>,
    },
    ModuleOutputs(BTreeMap<String, Atom>),
    Data(Atom),
    Namespace(String),
    Resource {
        urn: Urn,
        kind: String,
    },
    Builder {
        decl: ResourceDecl,
        configs: Vec<(String, Name, Vec<Stmt>, Span)>,
    },
    Host {
        urn: Urn,
    },
    Operation {
        kind: String,
        key: String,
        fields: BTreeMap<String, Value>,
    },
    Void,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub value: Val,
    pub name: Name,
}
pub(crate) type Env = BTreeMap<String, Binding>;
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub span: Span,
    pub scope: Span,
    pub detail: String,
}
#[derive(Clone, Debug)]
pub struct Occurrence {
    pub span: Span,
    pub definition: Option<Span>,
    pub detail: String,
}
#[derive(Clone, Debug)]
pub struct Completion {
    pub label: String,
    pub detail: String,
}
#[derive(Clone, Debug)]
pub struct Hint {
    pub span: Span,
    pub items: Vec<Completion>,
}
#[derive(Default)]
pub struct Analysis {
    pub modules: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub compilation: Option<Compilation>,
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
    pub hints: Vec<Hint>,
    pub argument_hints: Vec<Hint>,
}
pub fn catalog() -> Vec<ResourceSchema> {
    serde_json::from_str(include_str!("catalog.json"))
        .expect("invariant: ifx-gen emits valid catalog JSON")
}

pub fn analyze(source: &str) -> Analysis {
    analyze_workspace(
        "main.ifx",
        &BTreeMap::from([("main.ifx".into(), source.into())]),
    )
}
/// All module contents are explicit inputs. This function never opens files.
pub fn analyze_workspace(entry: &str, sources: &BTreeMap<String, String>) -> Analysis {
    analyze_project(entry, sources, &BTreeMap::new())
}
/// Imports are resolved by the caller; analysis remains independent of disk and Git.
pub type Imports = BTreeMap<String, BTreeMap<String, String>>;
pub fn analyze_project(
    entry: &str,
    sources: &BTreeMap<String, String>,
    imports: &Imports,
) -> Analysis {
    if sources.len() > 32 || sources.values().map(String::len).sum::<usize>() > 1024 * 1024 {
        return Analysis {
            diagnostics: vec![Diagnostic::new(
                Span::default(),
                "workspace exceeds 32 modules or 1 MiB",
            )],
            ..Analysis::default()
        };
    }
    let Some(source) = sources.get(entry) else {
        return Analysis {
            diagnostics: vec![Diagnostic::new(
                Span::default(),
                "entry module is not supplied",
            )],
            ..Analysis::default()
        };
    };
    let parsed_modules: BTreeMap<_, _> = sources
        .iter()
        .map(|(path, source)| (path.clone(), syntax::parse(source)))
        .collect();
    let parsed = &parsed_modules[entry];
    let mut analysis = Analysis {
        modules: imports
            .get(entry)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        diagnostics: parsed.diagnostics.clone(),
        ..Analysis::default()
    };
    let schemas = catalog();
    let mut checker = Evaluator::new(&schemas, true, &mut analysis);
    checker.sources = Some(sources);
    checker.imports = Some(imports);
    checker.parsed_modules = Some(&parsed_modules);
    checker.path = entry.into();
    checker.scope = Span {
        start: 0,
        end: source.len(),
    };
    let mut env = Env::new();
    if let Err(e) = checker.block(&parsed.statements, &mut env) {
        checker.analysis.diagnostics.push(e);
    }
    if analysis.diagnostics.is_empty() {
        let mut compiler = Evaluator::new(&schemas, false, &mut analysis);
        compiler.sources = Some(sources);
        compiler.imports = Some(imports);
        compiler.parsed_modules = Some(&parsed_modules);
        compiler.path = entry.into();
        compiler.output.source_digests = sources
            .iter()
            .map(|(path, source)| {
                (
                    path.clone(),
                    format!("{:x}", Sha256::digest(source.as_bytes())),
                )
            })
            .collect();
        match compiler.block(&parsed.statements, &mut Env::new()) {
            Ok(()) => {
                let mut seen = BTreeSet::new();
                for r in &compiler.output.program.resources {
                    if !seen.insert(r.urn.clone()) {
                        compiler.analysis.diagnostics.push(Diagnostic::new(
                            Span::default(),
                            format!("duplicate resource `{}`", r.urn),
                        ));
                    }
                }
                if compiler.analysis.diagnostics.is_empty() {
                    compiler.analysis.compilation = Some(compiler.output);
                }
            }
            Err(e) => compiler.analysis.diagnostics.push(e),
        }
    }
    analysis.diagnostics.truncate(100);
    analysis
}
pub(crate) type Result<T> = std::result::Result<T, Diagnostic>;
pub(crate) struct Evaluator<'a> {
    pub(crate) schemas: &'a [ResourceSchema],
    pub(crate) checking: bool,
    pub(crate) analysis: &'a mut Analysis,
    pub(crate) output: Compilation,
    pub(crate) host: Option<&'a mut crate::simulator::Host>,
    pub(crate) identity: String,
    pub(crate) policy_depth: usize,
    work: usize,
    scope: Span,
    sources: Option<&'a BTreeMap<String, String>>,
    imports: Option<&'a Imports>,
    path: String,
    namespace: Vec<String>,
    module_depth: usize,
    module_inputs: BTreeMap<String, Atom>,
    module_outputs: BTreeMap<String, Atom>,
    parsed_modules: Option<&'a BTreeMap<String, syntax::Parsed>>,
    block_depth: usize,
}
impl<'a> Evaluator<'a> {
    pub(crate) fn new(
        schemas: &'a [ResourceSchema],
        checking: bool,
        analysis: &'a mut Analysis,
    ) -> Self {
        Self {
            schemas,
            checking,
            analysis,
            output: Compilation::default(),
            host: None,
            identity: String::new(),
            policy_depth: 0,
            work: 0,
            scope: Span::default(),
            sources: None,
            imports: None,
            path: String::new(),
            namespace: Vec::new(),
            module_depth: 0,
            module_inputs: BTreeMap::new(),
            module_outputs: BTreeMap::new(),
            parsed_modules: None,
            block_depth: 0,
        }
    }
    fn spend(&mut self, span: Span) -> Result<()> {
        self.work += 1;
        if self.work > MAX_WORK {
            Err(Diagnostic::new(span, "evaluation work limit exceeded"))
        } else {
            Ok(())
        }
    }
    pub(crate) fn block(&mut self, statements: &[Stmt], env: &mut Env) -> Result<()> {
        for s in statements {
            self.spend(s.span)?;
            if let Err(e) = self.statement(s, env) {
                if !self.checking {
                    return Err(e);
                }
                self.analysis.diagnostics.push(e);
                if self.analysis.diagnostics.len() >= 100 || self.work > MAX_WORK {
                    break;
                }
            }
        }
        Ok(())
    }
    fn bind(&mut self, env: &mut Env, name: &Name, value: Val) -> Result<()> {
        if env.len() >= 128
            || env.values().map(|b| value_size(&b.value)).sum::<usize>() + value_size(&value)
                > MAX_ENV
        {
            return Err(Diagnostic::new(name.span, "binding storage limit exceeded"));
        }
        if env.contains_key(&name.text) || matches!(name.text.as_str(), "linode" | "memory") {
            return Err(Diagnostic::new(
                name.span,
                "binding shadows an existing name",
            ));
        }
        if self.checking && !self.analysis.symbols.iter().any(|s| s.span == name.span) {
            self.analysis.symbols.push(Symbol {
                name: name.text.clone(),
                span: name.span,
                scope: self.scope,
                detail: describe(&value),
            });
        }
        env.insert(
            name.text.clone(),
            Binding {
                value,
                name: name.clone(),
            },
        );
        Ok(())
    }
    fn statement(&mut self, s: &Stmt, env: &mut Env) -> Result<()> {
        match &s.kind {
            StmtKind::Use { name, path } => {
                if !self.identity.is_empty() || self.block_depth != 0 {
                    return Err(Diagnostic::new(
                        s.span,
                        "use declarations belong at module scope",
                    ));
                }
                let target = self
                    .imports
                    .and_then(|all| all.get(&self.path))
                    .and_then(|visible| visible.get(path))
                    .filter(|target| {
                        self.sources
                            .is_some_and(|sources| sources.contains_key(*target))
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(
                            s.span,
                            format!(
                                "module `{path}` is not declared in this package's Ifx.toml scope"
                            ),
                        )
                    })?;
                self.bind(env, name, Val::ModuleDefinition(target.clone()))?;
            }
            StmtKind::Import { name, path } => {
                if !self.identity.is_empty() || self.block_depth != 0 {
                    return Err(Diagnostic::new(s.span, "imports belong at module scope"));
                }
                let path = module_path(&self.path, path).ok_or_else(|| {
                    Diagnostic::new(
                        s.span,
                        "imports must be relative .ifx paths without hidden or parent components",
                    )
                })?;
                if !self
                    .sources
                    .is_some_and(|sources| sources.contains_key(&path))
                {
                    return Err(Diagnostic::new(
                        s.span,
                        format!(
                            "module `{path}` is not supplied; open it in the editor or provide it to the CLI"
                        ),
                    ));
                }
                self.bind(env, name, Val::ModuleDefinition(path))?;
            }
            StmtKind::Bind {
                category,
                name,
                ty,
                value,
            } => {
                if !self.identity.is_empty() && category != "let" {
                    return Err(Diagnostic::new(
                        s.span,
                        "configuration can bind local values, but cannot declare resources, inputs or outputs",
                    ));
                }
                if category == "input"
                    && self.checking
                    && self.module_inputs.contains_key(&name.text)
                {
                    let default = data(self.expr(value, env)?, value.span)?;
                    let expected = ty
                        .as_deref()
                        .and_then(parse_type)
                        .ok_or_else(|| Diagnostic::new(name.span, "unknown input type"))?;
                    check_atom(&expected, &default, value.span)?;
                }
                let v = if category == "input" {
                    self.module_inputs.get(&name.text).cloned().map(Val::Data)
                } else {
                    None
                };
                let mut v = match v {
                    Some(v) => v,
                    None => self.expr(value, env)?,
                };
                if let Some(ty) = ty {
                    let expected = parse_type(ty).ok_or_else(|| {
                        Diagnostic::new(
                            name.span,
                            "unknown type (use String, Int, Bool, List[T], Map[T])",
                        )
                    })?;
                    check_atom(&expected, &data(v.clone(), value.span)?, value.span)?;
                    if let Val::Data(atom) = &mut v {
                        atom.ty = expected;
                    }
                }
                if category == "module" {
                    let Val::ModuleBuilder { path, key, inputs } = v else {
                        return Err(Diagnostic::new(
                            value.span,
                            "module requires an imported module builder",
                        ));
                    };
                    let outputs = self.instantiate(&path, &key, inputs, value.span)?;
                    self.bind(env, name, Val::ModuleOutputs(outputs))?;
                } else if category == "resource" {
                    if self.output.program.resources.len() >= 128 {
                        return Err(Diagnostic::new(s.span, "resource expansion limit exceeded"));
                    }
                    let Val::Builder { decl, configs } = v else {
                        return Err(Diagnostic::new(
                            value.span,
                            "resource requires a resource builder",
                        ));
                    };
                    let schema = self.schema(decl.type_name(), value.span)?;
                    let mut decl = decl;
                    schema.apply_defaults(&mut decl.inputs);
                    if let Err(errors) = schema.validate(&decl.inputs) {
                        return Err(Diagnostic::new(value.span, errors.join("; ")));
                    }
                    let mut keys = BTreeSet::new();
                    for (key, parameter, body, span) in configs {
                        if !keys.insert(key.clone()) {
                            return Err(Diagnostic::new(span, "duplicate configuration key"));
                        }
                        if self.checking {
                            let scope = std::mem::replace(&mut self.scope, span);
                            let mut captures = env.clone();
                            self.bind(
                                &mut captures,
                                &parameter,
                                Val::Host {
                                    urn: decl.urn.clone(),
                                },
                            )?;
                            let old = std::mem::replace(
                                &mut self.identity,
                                format!("{}/{key}", decl.urn),
                            );
                            let result = self.block(&body, &mut captures);
                            self.identity = old;
                            self.scope = scope;
                            result?;
                        } else {
                            if self.output.configurations.len() >= 128 {
                                return Err(Diagnostic::new(
                                    span,
                                    "configuration expansion limit exceeded",
                                ));
                            }
                            self.output.configurations.push(Configuration {
                                target: decl.urn.clone(),
                                key,
                                parameter,
                                body,
                                span,
                                captures: env.clone(),
                            });
                        }
                    }
                    let handle = Val::Resource {
                        urn: decl.urn.clone(),
                        kind: decl.type_name().into(),
                    };
                    self.output.program.resources.push(decl);
                    self.bind(env, name, handle)?;
                } else {
                    if !matches!(v, Val::Data(_)) {
                        return Err(Diagnostic::new(
                            value.span,
                            "local values and outputs must be data; builders finalize in resource declarations",
                        ));
                    }
                    if category == "output" {
                        let atom = data(v.clone(), value.span)?;
                        self.module_outputs.insert(name.text.clone(), atom.clone());
                        if self.module_depth == 0 {
                            self.output.outputs.insert(name.text.clone(), atom.json);
                        }
                    }
                    self.bind(env, name, v)?;
                }
            }
            StmtKind::Expr(e) => {
                if !matches!(self.expr(e, env)?, Val::Void) {
                    return Err(Diagnostic::new(
                        e.span,
                        "unused value; host builders require .ensure()",
                    ));
                }
            }
            StmtKind::If { condition, yes, no } => {
                let v = data(self.expr(condition, env)?, condition.span)?;
                check_atom(&FieldType::Bool, &v, condition.span)?;
                if self.checking {
                    self.scoped(yes, env)?;
                    self.scoped(no, env)?;
                } else {
                    let v = self.resolve(v, condition.span)?;
                    let Some(test) = v.json.as_bool() else {
                        return Err(Diagnostic::new(
                            condition.span,
                            "condition must be known before graph execution",
                        ));
                    };
                    self.scoped(if test { yes } else { no }, env)?;
                }
            }
            StmtKind::For {
                key,
                value,
                collection,
                body,
            } => {
                let v = data(self.expr(collection, env)?, collection.span)?;
                let v = self.resolve(v, collection.span)?;
                let entries: Vec<_> = match v.json {
                    Value::Array(a) if value.is_none() => {
                        a.into_iter().map(|x| (x, None)).collect()
                    }
                    Value::Object(o) if value.is_some() && v.concrete() => o
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                        .into_iter()
                        .map(|(k, v)| (Value::String(k), Some(v)))
                        .collect(),
                    _ => {
                        return Err(Diagnostic::new(
                            collection.span,
                            "iterate lists with one binding or known maps with key, value",
                        ));
                    }
                };
                if entries.len() > 256 {
                    return Err(Diagnostic::new(
                        collection.span,
                        "collection expansion exceeds 256 entries",
                    ));
                }
                // Empty collections are checked once with typed unknown values.
                let entries = if self.checking && entries.is_empty() {
                    let item = match v.ty {
                        FieldType::List { item } => *item,
                        FieldType::Map { value } => *value,
                        _ => FieldType::Any,
                    };
                    let mut inner = env.clone();
                    self.bind(
                        &mut inner,
                        key,
                        Val::Data(Atom {
                            json: unknown(),
                            ty: if value.is_some() {
                                FieldType::String
                            } else {
                                item.clone()
                            },
                        }),
                    )?;
                    if let Some(name) = value {
                        self.bind(
                            &mut inner,
                            name,
                            Val::Data(Atom {
                                json: unknown(),
                                ty: item,
                            }),
                        )?;
                    }
                    self.scoped(body, &inner)?;
                    Vec::new()
                } else {
                    entries
                };
                for (k, v) in entries {
                    let scope = self.scope;
                    if let (Some(first), Some(last)) = (body.first(), body.last()) {
                        self.scope = Span {
                            start: first.span.start,
                            end: last.span.end,
                        };
                    }
                    let mut inner = env.clone();
                    self.bind(&mut inner, key, Val::Data(Atom::known(k)))?;
                    if let (Some(name), Some(v)) = (value, v) {
                        self.bind(&mut inner, name, Val::Data(Atom::known(v)))?;
                    }
                    self.scoped(body, &inner)?;
                    self.scope = scope;
                }
            }
        }
        Ok(())
    }
    fn scoped(&mut self, body: &[Stmt], env: &Env) -> Result<()> {
        let old = self.scope;
        if let (Some(first), Some(last)) = (body.first(), body.last()) {
            self.scope = Span {
                start: first.span.start,
                end: last.span.end,
            };
        }
        self.block_depth += 1;
        let result = self.block(body, &mut env.clone());
        self.block_depth -= 1;
        self.scope = old;
        result
    }
    fn schema(&self, kind: &str, span: Span) -> Result<&ResourceSchema> {
        self.schemas
            .iter()
            .find(|s| s.type_name == kind)
            .ok_or_else(|| Diagnostic::new(span, "unsupported resource kind"))
    }
    pub(crate) fn resolve(&self, mut a: Atom, span: Span) -> Result<Atom> {
        if let Some(host) = self.host.as_ref() {
            a.json = ifx_program::model::resolve_refs(&a.json, &|r| {
                host.outputs
                    .get(r.urn.as_str())
                    .and_then(|o| ifx_program::model::get_path(o, &r.path).cloned())
            });
            if contains_unknown(&a.json) {
                return Err(Diagnostic::new(
                    span,
                    "simulation requires an output fixture for this reference",
                ));
            }
            check_atom(&a.ty, &a, span)?;
        }
        Ok(a)
    }
    fn expr(&mut self, e: &Expr, env: &Env) -> Result<Val> {
        self.spend(e.span)?;
        let value = match &e.kind {
            ExprKind::Literal(v) => Val::Data(Atom::known(v.clone())),
            ExprKind::Name(n) => {
                if let Some(binding) = env.get(&n.text) {
                    if self.checking {
                        self.analysis.occurrences.push(Occurrence {
                            span: n.span,
                            definition: Some(binding.name.span),
                            detail: describe(&binding.value),
                        });
                    }
                    binding.value.clone()
                } else if matches!(n.text.as_str(), "linode" | "memory") {
                    Val::Namespace(n.text.clone())
                } else {
                    return Err(Diagnostic::new(
                        n.span,
                        format!("unknown name `{}`", n.text),
                    ));
                }
            }
            ExprKind::List(items) => {
                let mut values = Vec::new();
                let mut size = 0;
                let mut ty = FieldType::Any;
                for item in items {
                    let a = data(self.expr(item, env)?, item.span)?;
                    size += json_size(&a.json) + 16;
                    if size > MAX_VALUE {
                        return Err(Diagnostic::new(e.span, "collection exceeds 64 KiB limit"));
                    }
                    if !values.is_empty() {
                        check_atom(&ty, &a, item.span)?;
                    } else {
                        ty = a.ty;
                    }
                    values.push(a.json);
                }
                Val::Data(Atom {
                    json: Value::Array(values),
                    ty: FieldType::list(ty),
                })
            }
            ExprKind::Map(items) => {
                let mut values = serde_json::Map::new();
                let mut fields = Vec::new();
                let mut size = 0;
                for (name, expr) in items {
                    let a = data(self.expr(expr, env)?, expr.span)?;
                    fields.push(ifx_program::schema::field(name.text.clone(), a.ty.clone()));
                    size += name.text.len() + json_size(&a.json) + 16;
                    if size > MAX_VALUE {
                        return Err(Diagnostic::new(e.span, "collection exceeds 64 KiB limit"));
                    }
                    if values.insert(name.text.clone(), a.json).is_some() {
                        return Err(Diagnostic::new(name.span, "duplicate map key"));
                    }
                }
                Val::Data(Atom {
                    json: Value::Object(values),
                    ty: FieldType::object(fields),
                })
            }
            ExprKind::Member(base, name) => {
                let base = self.expr(base, env)?;
                match base {
                    Val::ModuleOutputs(outputs) => Val::Data(
                        outputs
                            .get(&name.text)
                            .ok_or_else(|| Diagnostic::new(name.span, "unknown module output"))?
                            .clone(),
                    ),
                    Val::Host { urn } => {
                        let field = self
                            .schema(urn.type_name(), name.span)?
                            .outputs
                            .iter()
                            .find(|f| f.name == name.text)
                            .ok_or_else(|| Diagnostic::new(name.span, "unknown target output"))?
                            .clone();
                        if self.checking {
                            self.analysis.occurrences.push(Occurrence {
                                span: name.span,
                                definition: None,
                                detail: field_detail(&field),
                            });
                        }
                        Val::Data(Atom {
                            json: OutputRef::new(urn, name.text.clone()).to_value(),
                            ty: field.ty,
                        })
                    }
                    Val::Resource { urn, kind } => {
                        let field = self
                            .schema(&kind, name.span)?
                            .outputs
                            .iter()
                            .find(|f| f.name == name.text)
                            .ok_or_else(|| Diagnostic::new(name.span, "unknown output"))?
                            .clone();
                        if self.checking {
                            self.analysis.occurrences.push(Occurrence {
                                span: name.span,
                                definition: None,
                                detail: field_detail(&field),
                            });
                        }
                        Val::Data(Atom {
                            json: OutputRef::new(urn, name.text.clone()).to_value(),
                            ty: field.ty,
                        })
                    }
                    Val::Data(a) => {
                        if let Some(mut reference) = OutputRef::from_value(&a.json) {
                            let ty = match &a.ty {
                                FieldType::Object { fields, .. } => fields
                                    .iter()
                                    .find(|f| f.name == name.text)
                                    .map(|f| f.ty.clone()),
                                FieldType::Map { value } => Some(*value.clone()),
                                _ => None,
                            }
                            .ok_or_else(|| {
                                Diagnostic::new(name.span, "output type has no such field")
                            })?;
                            reference.path.push('.');
                            reference.path.push_str(&name.text);
                            return Ok(Val::Data(Atom {
                                json: reference.to_value(),
                                ty,
                            }));
                        }
                        let Some(v) = a.json.get(&name.text) else {
                            return Err(Diagnostic::new(name.span, "unknown map field"));
                        };
                        Val::Data(Atom {
                            json: v.clone(),
                            ty: member_type(&a.ty, &name.text)
                                .unwrap_or_else(|| Atom::known(v.clone()).ty),
                        })
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            name.span,
                            "expected an output or map field",
                        ));
                    }
                }
            }
            ExprKind::Index(base, index) => {
                let a = data(self.expr(base, env)?, base.span)?;
                let i = data(self.expr(index, env)?, index.span)?;
                if let Some(mut reference) = OutputRef::from_value(&a.json) {
                    let (path, ty) = match (&a.ty, &i.json) {
                        (FieldType::List { item }, Value::Number(n)) if n.as_u64().is_some() => {
                            (n.to_string(), *item.clone())
                        }
                        (FieldType::Map { value }, Value::String(k)) if !k.contains('.') => {
                            (k.clone(), *value.clone())
                        }
                        _ => {
                            return Err(Diagnostic::new(
                                e.span,
                                "output index must be known and match the collection type",
                            ));
                        }
                    };
                    reference.path.push('.');
                    reference.path.push_str(&path);
                    return Ok(Val::Data(Atom {
                        json: reference.to_value(),
                        ty,
                    }));
                }
                let got = match &i.json {
                    Value::String(k) => a.json.get(k),
                    Value::Number(n) => n
                        .as_u64()
                        .and_then(|i| usize::try_from(i).ok())
                        .and_then(|i| a.json.get(i)),
                    _ => None,
                };
                let ty = match (&a.ty, &i.json) {
                    (FieldType::List { item }, Value::Number(_)) => Some(*item.clone()),
                    (_, Value::String(key)) => member_type(&a.ty, key),
                    _ => None,
                };
                if self.checking && !a.concrete() && got.is_none() {
                    let Some(ty) = ty else {
                        return Err(Diagnostic::new(
                            e.span,
                            "index does not match the collection type",
                        ));
                    };
                    return Ok(Val::Data(Atom {
                        json: unknown(),
                        ty,
                    }));
                }
                let json = got
                    .ok_or_else(|| {
                        Diagnostic::new(e.span, "index is absent or has the wrong type")
                    })?
                    .clone();
                Val::Data(Atom {
                    ty: ty.unwrap_or_else(|| Atom::known(json.clone()).ty),
                    json,
                })
            }
            ExprKind::Not(inner) => {
                let a = data(self.expr(inner, env)?, inner.span)?;
                check_atom(&FieldType::Bool, &a, e.span)?;
                let a = self.resolve(a, e.span)?;
                if !a.concrete() && !self.checking && self.host.is_none() {
                    return Err(Diagnostic::new(
                        e.span,
                        "deferred boolean operations belong inside configure",
                    ));
                }
                Val::Data(Atom {
                    json: a.json.as_bool().map_or_else(unknown, |b| json!(!b)),
                    ty: FieldType::Bool,
                })
            }
            ExprKind::Binary(left, op, right) => {
                let a = data(self.expr(left, env)?, left.span)?;
                let b = data(self.expr(right, env)?, right.span)?;
                check_atom(&a.ty, &b, right.span)?;
                let a = self.resolve(a, left.span)?;
                let b = self.resolve(b, right.span)?;
                if op == "+" && json_size(&a.json) + json_size(&b.json) > MAX_VALUE {
                    return Err(Diagnostic::new(
                        e.span,
                        "expanded value exceeds 64 KiB limit",
                    ));
                }
                if (!a.concrete() || !b.concrete())
                    && !self.checking
                    && self.host.is_none()
                    && !(op == "+" && matches!(a.ty, FieldType::String | FieldType::Enum { .. }))
                {
                    return Err(Diagnostic::new(
                        e.span,
                        "deferred comparison/arithmetic belongs inside configure",
                    ));
                }
                let ty = if op == "+" {
                    a.ty.clone()
                } else {
                    FieldType::Bool
                };
                let json = if op == "+" {
                    match (&a.ty, &a.json, &b.json) {
                        (
                            FieldType::String | FieldType::Enum { .. },
                            Value::String(a),
                            Value::String(b),
                        ) => json!(format!("{a}{b}")),
                        (FieldType::String | FieldType::Enum { .. }, _, _) => {
                            concat(vec![a.json, b.json])
                        }
                        (FieldType::Int, _, _) => match (a.json.as_i64(), b.json.as_i64()) {
                            (Some(a), Some(b)) => json!(
                                a.checked_add(b)
                                    .ok_or_else(|| Diagnostic::new(e.span, "integer overflow"))?
                            ),
                            _ => unknown(),
                        },
                        _ => return Err(Diagnostic::new(e.span, "+ requires strings or integers")),
                    }
                } else if a.concrete() && b.concrete() {
                    json!((a.json == b.json) == (op == "=="))
                } else {
                    unknown()
                };
                Val::Data(Atom { json, ty })
            }
            ExprKind::Call(callee, args) => {
                if let ExprKind::Name(name) = &callee.kind {
                    let Some(Binding {
                        value: Val::ModuleDefinition(path),
                        ..
                    }) = env.get(&name.text)
                    else {
                        return Err(Diagnostic::new(
                            callee.span,
                            "only imported modules can be called by name",
                        ));
                    };
                    let key = self.key(args, env, e.span)?;
                    let value = Val::ModuleBuilder {
                        path: path.clone(),
                        key,
                        inputs: BTreeMap::new(),
                    };
                    if self.checking {
                        self.analysis.hints.push(Hint {
                            span: e.span,
                            items: self.completions(&value),
                        });
                    }
                    return Ok(value);
                }
                let ExprKind::Member(base, method) = &callee.kind else {
                    return Err(Diagnostic::new(
                        callee.span,
                        "call a resource constructor or typed method",
                    ));
                };
                let receiver = self.expr(base, env)?;
                self.call(receiver, method, args, env, e.span)?
            }
            ExprKind::Lambda(..) => {
                return Err(Diagnostic::new(
                    e.span,
                    "lambdas are accepted only by configure and policy methods",
                ));
            }
        };
        if value_size(&value) > MAX_VALUE {
            return Err(Diagnostic::new(
                e.span,
                "expanded value exceeds 64 KiB limit",
            ));
        }
        if self.checking && !self.analysis.hints.iter().any(|h| h.span == e.span) {
            let items = self.completions(&value);
            if !items.is_empty() {
                self.analysis.hints.push(Hint {
                    span: e.span,
                    items,
                });
            }
        }
        Ok(value)
    }
    fn call(
        &mut self,
        receiver: Val,
        method: &Name,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<Val> {
        match receiver {
            Val::ModuleBuilder {
                path,
                key,
                mut inputs,
            } => {
                let a = self.argument(args, env, span)?;
                if inputs.insert(method.text.clone(), a).is_some() {
                    return Err(Diagnostic::new(span, "module input set more than once"));
                }
                Ok(Val::ModuleBuilder { path, key, inputs })
            }
            Val::Namespace(provider) => {
                let kind = format!("{provider}.{}", method.text);
                self.schema(&kind, method.span)?;
                let key = self.key(args, env, span)?;
                let key = if self.namespace.is_empty() {
                    key
                } else {
                    let mut parts = self.namespace.clone();
                    parts.push(key);
                    json!(parts).to_string()
                };
                Ok(Val::Builder {
                    decl: ResourceDecl::new(&kind, &key, json!({})),
                    configs: Vec::new(),
                })
            }
            Val::Builder {
                mut decl,
                mut configs,
            } => {
                if method.text == "configure" {
                    if args.len() != 2 {
                        return Err(Diagnostic::new(
                            span,
                            "configure(key, |host| { ... }) requires two arguments",
                        ));
                    }
                    let key = self.key(&args[..1], env, span)?;
                    let ExprKind::Lambda(params, body) = &args[1].kind else {
                        return Err(Diagnostic::new(
                            args[1].span,
                            "expected configuration lambda",
                        ));
                    };
                    if params.len() != 1 {
                        return Err(Diagnostic::new(
                            args[1].span,
                            "configuration requires one host parameter",
                        ));
                    }
                    configs.push((key, params[0].clone(), body.clone(), args[1].span));
                } else {
                    let field = self
                        .schema(decl.type_name(), method.span)?
                        .input_field(&method.text)
                        .ok_or_else(|| Diagnostic::new(method.span, "unknown resource input"))?
                        .clone();
                    if self.checking {
                        if let Some(arg) = args.first().filter(|arg| {
                            !self
                                .analysis
                                .argument_hints
                                .iter()
                                .any(|h| h.span == arg.span)
                        }) {
                            let variants = match &field.ty {
                                FieldType::Enum { variants, .. } => variants.clone(),
                                FieldType::Bool => vec!["true".into(), "false".into()],
                                _ => Vec::new(),
                            };
                            self.analysis.argument_hints.push(Hint {
                                span: arg.span,
                                items: variants
                                    .into_iter()
                                    .map(|label| Completion {
                                        label,
                                        detail: field_detail(&field),
                                    })
                                    .collect(),
                            });
                        }
                        self.analysis.occurrences.push(Occurrence {
                            span: method.span,
                            definition: None,
                            detail: field_detail(&field),
                        });
                    }
                    let a = self.argument(args, env, span)?;
                    check_atom(&field.ty, &a, span)?;
                    let fields = decl
                        .inputs
                        .as_object_mut()
                        .expect("invariant: builder has object inputs");
                    if fields.insert(method.text.clone(), a.json).is_some() {
                        return Err(Diagnostic::new(method.span, "input set more than once"));
                    }
                }
                Ok(Val::Builder { decl, configs })
            }
            Val::Host { .. } => self.host_call(method, args, env, span),
            Val::Operation {
                kind,
                key,
                mut fields,
            } => {
                if method.text == "ensure" {
                    if !args.is_empty() {
                        return Err(Diagnostic::new(span, "ensure takes no arguments"));
                    }
                    if kind == "file" && !fields.contains_key("content") {
                        return Err(Diagnostic::new(span, "file requires content"));
                    }
                    if let Some(host) = self.host.as_mut() {
                        host.ensure(&kind, &key, fields, span)?;
                    }
                    Ok(Val::Void)
                } else {
                    let ty = operation_fields(&kind)
                        .into_iter()
                        .find(|(n, _)| *n == method.text)
                        .map(|(_, t)| t)
                        .ok_or_else(|| {
                            Diagnostic::new(method.span, "unknown host operation field")
                        })?;
                    let a = self.argument(args, env, span)?;
                    check_atom(&ty, &a, span)?;
                    let a = self.resolve(a, span)?;
                    if fields.insert(method.text.clone(), a.json).is_some() {
                        return Err(Diagnostic::new(
                            method.span,
                            "host field set more than once",
                        ));
                    }
                    Ok(Val::Operation { kind, key, fields })
                }
            }
            _ => Err(Diagnostic::new(
                method.span,
                "value has no callable methods",
            )),
        }
    }
    fn host_call(&mut self, method: &Name, args: &[Expr], env: &Env, span: Span) -> Result<Val> {
        match method.text.as_str() {
            "directory" | "file" | "package" | "service" => {
                let key = self.key(args, env, span)?;
                Ok(Val::Operation {
                    kind: method.text.clone(),
                    key,
                    fields: BTreeMap::new(),
                })
            }
            "exists" => {
                let key = self.key(args, env, span)?;
                let value = self
                    .host
                    .as_ref()
                    .map_or_else(unknown, |h| json!(h.exists(&key)));
                Ok(Val::Data(Atom {
                    json: value,
                    ty: FieldType::Bool,
                }))
            }
            "record" | "fail" => {
                let key = self.key(args, env, span)?;
                if let Some(host) = self.host.as_mut() {
                    if method.text == "fail" {
                        return Err(Diagnostic::new(span, "explicit simulated failure"));
                    }
                    host.record(&key);
                }
                Ok(Val::Void)
            }
            "once" | "always" | "on_change" => {
                let count = if method.text == "on_change" { 3 } else { 2 };
                if args.len() != count {
                    return Err(Diagnostic::new(
                        span,
                        "policy requires a stable key, optional watched value for on_change, and || { ... }",
                    ));
                }
                if self.policy_depth != 0 {
                    return Err(Diagnostic::new(
                        span,
                        "nested action policies are outside the initial subset",
                    ));
                }
                let key = self.key(&args[..1], env, span)?;
                let watched = if count == 3 {
                    let v = data(self.expr(&args[1], env)?, args[1].span)?;
                    self.resolve(v, span)?.json
                } else {
                    Value::Null
                };
                let ExprKind::Lambda(params, body) = &args[count - 1].kind else {
                    return Err(Diagnostic::new(span, "policy body must be a lambda"));
                };
                if !params.is_empty() {
                    return Err(Diagnostic::new(
                        span,
                        "policy lambda captures host and takes no parameters",
                    ));
                }
                let identity = json!([self.identity, key]).to_string();
                let run = if let Some(host) = self.host.as_mut() {
                    host.begin(&identity, &method.text, &watched, span)?
                } else {
                    true
                };
                if run {
                    self.policy_depth += 1;
                    let result = self.scoped(body, env);
                    self.policy_depth -= 1;
                    if let Some(host) = self.host.as_mut() {
                        host.finish(&identity, watched, result.is_ok());
                    }
                    result?;
                }
                Ok(Val::Void)
            }
            _ => Err(Diagnostic::new(method.span, "unknown host method")),
        }
    }
    fn argument(&mut self, args: &[Expr], env: &Env, span: Span) -> Result<Atom> {
        if args.len() != 1 {
            return Err(Diagnostic::new(span, "expected one argument"));
        }
        data(self.expr(&args[0], env)?, args[0].span)
    }
    fn key(&mut self, args: &[Expr], env: &Env, span: Span) -> Result<String> {
        let a = self.argument(args, env, span)?;
        check_atom(&FieldType::String, &a, span)?;
        let a = self.resolve(a, span)?;
        if self.checking && !a.concrete() {
            return Ok("<unresolved>".into());
        }
        let Some(s) = a.json.as_str() else {
            return Err(Diagnostic::new(
                span,
                "key must be a known string before this operation",
            ));
        };
        if s.is_empty() || s.len() > 512 {
            return Err(Diagnostic::new(span, "key must contain 1–512 bytes"));
        }
        Ok(s.into())
    }
    fn completions(&self, v: &Val) -> Vec<Completion> {
        match v {
            Val::ModuleOutputs(outputs) => outputs
                .iter()
                .map(|(name, a)| Completion {
                    label: name.clone(),
                    detail: a.ty.display(),
                })
                .collect(),
            Val::ModuleBuilder { path, .. } => self
                .parsed_modules
                .and_then(|sources| sources.get(path))
                .map(|parsed| {
                    parsed
                        .statements
                        .iter()
                        .filter_map(|s| {
                            if let StmtKind::Bind {
                                category, name, ty, ..
                            } = &s.kind
                            {
                                if category == "input" {
                                    return Some(Completion {
                                        label: name.text.clone(),
                                        detail: ty.clone().unwrap_or_default(),
                                    });
                                }
                            }
                            None
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Val::Namespace(provider) => self
                .schemas
                .iter()
                .filter_map(|s| {
                    s.type_name
                        .strip_prefix(&format!("{provider}."))
                        .map(|n| Completion {
                            label: n.into(),
                            detail: s.doc.clone(),
                        })
                })
                .collect(),
            Val::Builder { decl, .. } => self
                .schemas
                .iter()
                .find(|s| s.type_name == decl.type_name())
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|f| Completion {
                            label: f.name.clone(),
                            detail: field_detail(f),
                        })
                        .chain(std::iter::once(Completion {
                            label: "configure".into(),
                            detail: "(key: String, |host| { ... }): deferred ordered configuration"
                                .into(),
                        }))
                        .collect()
                })
                .unwrap_or_default(),
            Val::Resource { kind, .. } => self
                .schemas
                .iter()
                .find(|s| &s.type_name == kind)
                .map(|s| {
                    s.outputs
                        .iter()
                        .map(|f| Completion {
                            label: f.name.clone(),
                            detail: field_detail(f),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Val::Host { urn } => [
                "directory",
                "file",
                "package",
                "service",
                "exists",
                "once",
                "always",
                "on_change",
                "record",
                "fail",
            ]
            .into_iter()
            .map(|n| Completion {
                label: n.into(),
                detail: "In-memory host operation; runs only during simulation".into(),
            })
            .chain(
                self.schemas
                    .iter()
                    .filter(|s| s.type_name == urn.type_name())
                    .flat_map(|s| &s.outputs)
                    .map(|f| Completion {
                        label: f.name.clone(),
                        detail: field_detail(f),
                    }),
            )
            .collect(),
            Val::Operation { kind, .. } => operation_fields(kind)
                .into_iter()
                .map(|(n, t)| Completion {
                    label: n.into(),
                    detail: t.display(),
                })
                .chain(std::iter::once(Completion {
                    label: "ensure".into(),
                    detail: "Observe and converge before the next statement".into(),
                }))
                .collect(),
            _ => Vec::new(),
        }
    }
}
fn data(v: Val, span: Span) -> Result<Atom> {
    if let Val::Data(a) = v {
        Ok(a)
    } else {
        Err(Diagnostic::new(span, "expected a data value"))
    }
}
fn describe(v: &Val) -> String {
    match v {
        Val::Data(a) => a.ty.display(),
        Val::Resource { kind, .. } => format!("resource {kind}"),
        Val::Host { .. } => "Host (in-memory target)".into(),
        _ => "builder".into(),
    }
}
fn parse_type(s: &str) -> Option<FieldType> {
    match s {
        "String" => Some(FieldType::String),
        "Int" => Some(FieldType::Int),
        "Bool" => Some(FieldType::Bool),
        _ => {
            if let Some(t) = s.strip_prefix("List[").and_then(|s| s.strip_suffix(']')) {
                Some(FieldType::list(parse_type(t)?))
            } else if let Some(t) = s.strip_prefix("Map[").and_then(|s| s.strip_suffix(']')) {
                Some(FieldType::map(parse_type(t)?))
            } else {
                None
            }
        }
    }
}
fn check_atom(expected: &FieldType, a: &Atom, span: Span) -> Result<()> {
    let compatible = match (expected, &a.ty) {
        (FieldType::Any, _) => true,
        (
            FieldType::String | FieldType::Enum { .. },
            FieldType::String | FieldType::Enum { .. },
        ) => true,
        (FieldType::List { item: e }, FieldType::List { item: a }) => {
            e == a || **e == FieldType::Any || **a == FieldType::Any
        }
        (
            FieldType::Map { .. } | FieldType::Object { .. },
            FieldType::Any | FieldType::Object { .. },
        ) if a.json.is_object() => true,
        (e, t) => e == t,
    };
    if !compatible {
        return Err(Diagnostic::new(
            span,
            format!("expected {}, got {}", expected.display(), a.ty.display()),
        ));
    }
    if let (FieldType::Map { value }, FieldType::Object { fields, .. }, Value::Object(object)) =
        (expected, &a.ty, &a.json)
    {
        for field in fields {
            if let Some(json) = object.get(&field.name) {
                check_atom(
                    value,
                    &Atom {
                        json: json.clone(),
                        ty: field.ty.clone(),
                    },
                    span,
                )?;
            }
        }
    }
    let schema = ResourceSchema::new("value", "")
        .input(ifx_program::schema::field("value", expected.clone()));
    schema
        .validate(&json!({"value": a.json}))
        .map_err(|_| Diagnostic::new(span, format!("value does not match {}", expected.display())))
}
pub fn field_detail(f: &ifx_program::schema::FieldSchema) -> String {
    let default = if f.sensitive {
        String::new()
    } else {
        f.default
            .as_ref()
            .map(|v| format!(" (default: {v})"))
            .unwrap_or_default()
    };
    format!(
        "{}: {}{}{}{}{}\n{}",
        f.name,
        f.ty.display(),
        if f.required { " (required)" } else { "" },
        if f.replace {
            " (replaces resource)"
        } else {
            ""
        },
        if f.sensitive { " (sensitive)" } else { "" },
        default,
        f.doc
    )
}
fn operation_fields(kind: &str) -> Vec<(&'static str, FieldType)> {
    match kind {
        "directory" => vec![("mode", FieldType::String)],
        "file" => vec![("content", FieldType::String), ("mode", FieldType::String)],
        "package" => vec![("installed", FieldType::Bool)],
        "service" => vec![("enabled", FieldType::Bool), ("running", FieldType::Bool)],
        _ => Vec::new(),
    }
}
fn json_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(json_size).sum::<usize>() + a.len() * 16,
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + json_size(v) + 16).sum(),
        _ => 16,
    }
}
fn value_size(v: &Val) -> usize {
    match v {
        Val::ModuleBuilder { inputs, .. } | Val::ModuleOutputs(inputs) => inputs
            .iter()
            .map(|(k, a)| k.len() + json_size(&a.json))
            .sum(),
        Val::Data(a) => json_size(&a.json),
        Val::Builder { decl, configs } => {
            json_size(&decl.inputs)
                + configs
                    .iter()
                    .map(|(_, _, _, s)| s.end.saturating_sub(s.start))
                    .sum::<usize>()
        }
        Val::Operation { key, fields, .. } => {
            key.len()
                + fields
                    .iter()
                    .map(|(k, v)| k.len() + json_size(v))
                    .sum::<usize>()
        }
        Val::Resource { urn, kind } => urn.as_str().len() + kind.len(),
        Val::Namespace(n) => n.len(),
        _ => 16,
    }
}
/// Resolve a declared local module inside the caller's supplied source namespace.
pub fn module_path(base: &str, relative: &str) -> Option<String> {
    let relative = relative.strip_prefix("./").unwrap_or(relative);
    if !relative.ends_with(".ifx")
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|p| p.is_empty() || p.starts_with('.'))
    {
        return None;
    }
    Some(base.rsplit_once('/').map_or_else(
        || relative.into(),
        |(parent, _)| format!("{parent}/{relative}"),
    ))
}
impl Evaluator<'_> {
    fn instantiate(
        &mut self,
        path: &str,
        key: &str,
        inputs: BTreeMap<String, Atom>,
        span: Span,
    ) -> Result<BTreeMap<String, Atom>> {
        if self.module_depth >= 8 {
            return Err(Diagnostic::new(
                span,
                "module recursion/depth limit exceeded",
            ));
        }
        let parsed = self
            .parsed_modules
            .and_then(|s| s.get(path))
            .ok_or_else(|| Diagnostic::new(span, "module source is not supplied"))?;
        if let Some(d) = parsed.diagnostics.first() {
            return Err(Diagnostic::new(
                span,
                format!("module `{path}` at byte {}: {}", d.span.start, d.message),
            ));
        }
        let names: BTreeSet<_> = parsed
            .statements
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Bind { category, name, .. } if category == "input" => {
                    Some(name.text.clone())
                }
                _ => None,
            })
            .collect();
        if inputs.keys().any(|n| !names.contains(n)) {
            return Err(Diagnostic::new(span, "unknown module input"));
        }
        let old_path = std::mem::replace(&mut self.path, path.into());
        let old_inputs = std::mem::replace(&mut self.module_inputs, inputs);
        let old_outputs = std::mem::take(&mut self.module_outputs);
        let old_scope = self.scope;
        self.scope = span;
        let symbols = self.analysis.symbols.len();
        let occurrences = self.analysis.occurrences.len();
        let hints = self.analysis.hints.len();
        let argument_hints = self.analysis.argument_hints.len();
        let diagnostics = self.analysis.diagnostics.len();
        self.module_depth += 1;
        let old_block_depth = std::mem::replace(&mut self.block_depth, 0);
        self.namespace.push(key.into());
        let result = self.block(&parsed.statements, &mut Env::new());
        self.block_depth = old_block_depth;
        self.namespace.pop();
        self.module_depth -= 1;
        self.scope = old_scope;
        self.path = old_path;
        self.module_inputs = old_inputs;
        let outputs = std::mem::replace(&mut self.module_outputs, old_outputs);
        // Child byte offsets do not belong to the caller's document. Report a call-site
        // diagnostic with explicit child location until cross-file navigation is added.
        self.analysis.symbols.truncate(symbols);
        self.analysis.occurrences.truncate(occurrences);
        self.analysis.hints.truncate(hints);
        self.analysis.argument_hints.truncate(argument_hints);
        for d in &mut self.analysis.diagnostics[diagnostics..] {
            d.message = format!("module `{path}` at byte {}: {}", d.span.start, d.message);
            d.span = span;
        }
        result.map_err(|d| {
            Diagnostic::new(
                span,
                format!("module `{path}` at byte {}: {}", d.span.start, d.message),
            )
        })?;
        Ok(outputs)
    }
}
fn member_type(ty: &FieldType, key: &str) -> Option<FieldType> {
    match ty {
        FieldType::Object { fields, .. } => {
            fields.iter().find(|f| f.name == key).map(|f| f.ty.clone())
        }
        FieldType::Map { value } => Some(*value.clone()),
        _ => None,
    }
}
