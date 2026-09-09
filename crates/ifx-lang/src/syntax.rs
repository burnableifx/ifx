//! Bounded, recovering syntax parser. Offsets are UTF-8 bytes; protocol conversion is separate.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_SOURCE: usize = 256 * 1024;
const MAX_TOKENS: usize = 24_000;
const MAX_DEPTH: usize = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}
impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Name {
    pub text: String,
    pub span: Span,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Expr {
    pub span: Span,
    pub kind: ExprKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ExprKind {
    Record(Name, Vec<(Name, Expr)>),
    Literal(Value),
    Name(Name),
    List(Vec<Expr>),
    Map(Vec<(Name, Expr)>),
    Member(Box<Expr>, Name),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Binary(Box<Expr>, String, Box<Expr>),
    Not(Box<Expr>),
    Lambda(Vec<Name>, Vec<Stmt>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stmt {
    pub span: Span,
    pub kind: StmtKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StmtKind {
    Struct {
        public: bool,
        name: Name,
        fields: Vec<Field>,
    },
    Function(Function),
    Impl {
        name: Name,
        functions: Vec<Function>,
    },
    Return(Option<Expr>),
    Use {
        name: Name,
        path: String,
    },
    Import {
        name: Name,
        path: String,
    },
    Bind {
        category: String,
        name: Name,
        ty: Option<String>,
        value: Expr,
    },
    For {
        key: Name,
        value: Option<Name>,
        collection: Expr,
        body: Vec<Stmt>,
    },
    If {
        condition: Expr,
        yes: Vec<Stmt>,
        no: Vec<Stmt>,
    },
    Expr(Expr),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Field {
    pub public: bool,
    pub name: Name,
    pub ty: String,
    pub default: Option<Expr>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Function {
    pub public: bool,
    pub name: Name,
    pub parameters: Vec<Field>,
    pub result: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub span: Span,
}
#[derive(Default)]
pub struct Parsed {
    pub statements: Vec<Stmt>,
    pub diagnostics: Vec<Diagnostic>,
    pub tokens: Vec<Token>,
}

pub fn parse(source: &str) -> Parsed {
    if source.len() > MAX_SOURCE {
        return Parsed {
            diagnostics: vec![Diagnostic::new(
                Span::default(),
                "source exceeds 256 KiB limit",
            )],
            ..Parsed::default()
        };
    }
    let (tokens, mut diagnostics) = lex(source);
    let mut p = Parser {
        tokens: &tokens,
        at: 0,
        depth: 0,
        records: true,
        diagnostics: Vec::new(),
    };
    let statements = p.block(false);
    diagnostics.extend(p.diagnostics);
    diagnostics.truncate(100);
    Parsed {
        statements,
        diagnostics,
        tokens,
    }
}
fn lex(s: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let start = i;
        let c = s[i..]
            .chars()
            .next()
            .expect("invariant: character boundary before end");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if s[i..].starts_with("//") {
            i = s[i..].find('\n').map_or(s.len(), |n| i + n);
            continue;
        }
        if c == '"' {
            i += 1;
            let mut escaped = false;
            let mut closed = false;
            while i < s.len() {
                let ch = s[i..]
                    .chars()
                    .next()
                    .expect("invariant: character boundary before end");
                i += ch.len_utf8();
                if ch == '"' && !escaped {
                    closed = true;
                    break;
                }
                if ch == '\n' {
                    break;
                }
                escaped = ch == '\\' && !escaped;
            }
            if !closed {
                errors.push(Diagnostic::new(
                    Span { start, end: i },
                    "unterminated string",
                ));
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            i += 1;
            while i < s.len()
                && (s.as_bytes()[i].is_ascii_alphanumeric() || s.as_bytes()[i] == b'_')
            {
                i += 1;
            }
        } else if c.is_ascii_digit()
            || (c == '-' && s.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            i += 1;
            while i < s.len() && s.as_bytes()[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            i += c.len_utf8();
            if (matches!(c, '=' | '!' | '<' | '>') && s.as_bytes().get(i) == Some(&b'='))
                || (c == '-' && s.as_bytes().get(i) == Some(&b'>'))
            {
                i += 1;
            }
        }
        tokens.push(Token {
            text: s[start..i].into(),
            span: Span { start, end: i },
        });
        if tokens.len() >= MAX_TOKENS {
            errors.push(Diagnostic::new(
                Span { start, end: i },
                "token limit exceeded",
            ));
            break;
        }
    }
    tokens.push(Token {
        text: String::new(),
        span: Span {
            start: s.len(),
            end: s.len(),
        },
    });
    (tokens, errors)
}
struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    depth: usize,
    records: bool,
    diagnostics: Vec<Diagnostic>,
}
type Result<T> = std::result::Result<T, Diagnostic>;
impl Parser<'_> {
    fn token(&self) -> &Token {
        &self.tokens[self.at.min(self.tokens.len() - 1)]
    }
    fn is(&self, text: &str) -> bool {
        self.token().text == text
    }
    fn take(&mut self) -> Token {
        let t = self.token().clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        t
    }
    fn eat(&mut self, text: &str) -> bool {
        if self.is(text) {
            self.take();
            true
        } else {
            false
        }
    }
    fn need(&mut self, text: &str) -> Result<()> {
        if self.eat(text) {
            return Ok(());
        }
        let error = Diagnostic::new(self.token().span, format!("expected `{text}`"));
        if matches!(text, ";" | ")" | "]") {
            self.diagnostics.push(error);
            Ok(())
        } else {
            Err(error)
        }
    }
    fn name(&mut self) -> Result<Name> {
        let t = self.token();
        if !t
            .text
            .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        {
            return Err(Diagnostic::new(t.span, "expected identifier"));
        }
        let t = self.take();
        Ok(Name {
            text: t.text,
            span: t.span,
        })
    }
    fn block(&mut self, braced: bool) -> Vec<Stmt> {
        let mut out = Vec::new();
        while !self.is("") && !(braced && self.is("}")) && self.diagnostics.len() < 100 {
            let before = self.at;
            match self.statement() {
                Ok(s) => out.push(s),
                Err(e) => {
                    self.diagnostics.push(e);
                    if self.at == before {
                        self.take();
                    }
                    while !self.is("")
                        && !self.is("}")
                        && !self.is(";")
                        && !matches!(
                            self.token().text.as_str(),
                            "let"
                                | "resource"
                                | "input"
                                | "output"
                                | "if"
                                | "for"
                                | "use"
                                | "pub"
                                | "fn"
                                | "struct"
                                | "impl"
                                | "return"
                        )
                    {
                        self.take();
                    }
                    self.eat(";");
                }
            }
        }
        if braced && !self.eat("}") {
            self.diagnostics
                .push(Diagnostic::new(self.token().span, "expected `}`"));
        }
        out
    }
    fn body(&mut self) -> Result<Vec<Stmt>> {
        self.need("{")?;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(Diagnostic::new(self.token().span, "nesting limit exceeded"));
        }
        let result = self.block(true);
        self.depth -= 1;
        Ok(result)
    }
    fn statement(&mut self) -> Result<Stmt> {
        let start = self.token().span.start;
        let public = self.eat("pub");
        if public && !matches!(self.token().text.as_str(), "fn" | "struct") {
            return Err(Diagnostic::new(
                self.token().span,
                "pub requires fn or struct",
            ));
        }
        let kind = match self.token().text.as_str() {
            "struct" => {
                self.take();
                let name = self.name()?;
                self.need("{")?;
                let fields = self.fields("}", true)?;
                StmtKind::Struct {
                    public,
                    name,
                    fields,
                }
            }
            "fn" => StmtKind::Function(self.function(public)?),
            "impl" => {
                self.take();
                let name = self.path_name()?;
                self.need("{")?;
                let mut functions = Vec::new();
                while !self.is("}") && !self.is("") {
                    let public = self.eat("pub");
                    functions.push(self.function(public)?);
                }
                self.need("}")?;
                StmtKind::Impl { name, functions }
            }
            "return" => {
                self.take();
                let value = if self.is(";") {
                    None
                } else {
                    Some(self.expr(0)?)
                };
                self.need(";")?;
                StmtKind::Return(value)
            }
            "use" => {
                self.take();
                let mut name = self.name()?;
                let mut parts = vec![name.text.clone()];
                while self.eat(":") {
                    self.need(":")?;
                    name = self.name()?;
                    parts.push(name.text.clone());
                }
                if parts.len() < 2 {
                    return Err(Diagnostic::new(name.span, "use requires package::module"));
                }
                if self.eat("as") {
                    name = self.name()?;
                }
                self.need(";")?;
                StmtKind::Use {
                    name,
                    path: parts.join("::"),
                }
            }
            "import" => {
                self.take();
                let name = self.name()?;
                self.need("from")?;
                let token = self.take();
                let path = serde_json::from_str::<String>(&token.text).map_err(|_| {
                    Diagnostic::new(token.span, "expected local module path string")
                })?;
                self.need(";")?;
                StmtKind::Import { name, path }
            }
            "let" | "resource" | "input" | "output" | "module" => {
                let category = self.take().text;
                let name = self.name()?;
                let ty = if self.eat(":") {
                    Some(self.type_name()?)
                } else {
                    None
                };
                if matches!(category.as_str(), "input" | "output") && ty.is_none() {
                    return Err(Diagnostic::new(name.span, "input/output requires a type"));
                }
                self.need("=")?;
                let value = self.expr(0)?;
                self.need(";")?;
                StmtKind::Bind {
                    category,
                    name,
                    ty,
                    value,
                }
            }
            "for" => {
                self.take();
                let key = self.name()?;
                let value = if self.eat(",") {
                    Some(self.name()?)
                } else {
                    None
                };
                self.need("in")?;
                let collection = self.header_expr()?;
                let body = self.body()?;
                StmtKind::For {
                    key,
                    value,
                    collection,
                    body,
                }
            }
            "if" => {
                self.take();
                let condition = self.header_expr()?;
                let yes = self.body()?;
                let no = if self.eat("else") {
                    self.body()?
                } else {
                    Vec::new()
                };
                StmtKind::If { condition, yes, no }
            }
            _ => {
                let e = self.expr(0)?;
                self.need(";")?;
                StmtKind::Expr(e)
            }
        };
        Ok(Stmt {
            span: Span {
                start,
                end: self.tokens[self.at.saturating_sub(1)].span.end,
            },
            kind,
        })
    }
    fn header_expr(&mut self) -> Result<Expr> {
        let records = std::mem::replace(&mut self.records, false);
        let result = self.expr(0);
        self.records = records;
        result
    }
    fn path_name(&mut self) -> Result<Name> {
        let mut name = self.name()?;
        while self.eat(":") {
            self.need(":")?;
            match self.name() {
                Ok(next) => {
                    name.text.push_str("::");
                    name.text.push_str(&next.text);
                    name.span.end = next.span.end;
                }
                Err(error) => {
                    self.diagnostics.push(error);
                    name.text.push_str("::");
                    break;
                }
            }
        }
        Ok(name)
    }
    fn fields(&mut self, end: &str, record: bool) -> Result<Vec<Field>> {
        let mut fields = Vec::new();
        while !self.is(end) && !self.is("") {
            let public = record && self.eat("pub");
            let name = self.name()?;
            let ty = if !record && name.text == "self" {
                "Self".into()
            } else {
                self.need(":")?;
                self.type_name()?
            };
            let default = if self.eat("=") {
                Some(self.expr(0)?)
            } else {
                None
            };
            fields.push(Field {
                public,
                name,
                ty,
                default,
            });
            if !self.eat(",") {
                break;
            }
        }
        self.need(end)?;
        Ok(fields)
    }
    fn function(&mut self, public: bool) -> Result<Function> {
        let start = self.token().span.start;
        self.need("fn")?;
        let name = self.name()?;
        self.need("(")?;
        let parameters = self.fields(")", false)?;
        let result = if self.eat("->") {
            self.type_name()?
        } else {
            "Unit".into()
        };
        let body = self.body()?;
        Ok(Function {
            public,
            name,
            parameters,
            result,
            body,
            span: Span {
                start,
                end: self.tokens[self.at.saturating_sub(1)].span.end,
            },
        })
    }
    fn type_name(&mut self) -> Result<String> {
        let mut name = self.path_name()?.text;
        if self.eat("[") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                self.depth -= 1;
                return Err(Diagnostic::new(
                    self.token().span,
                    "type nesting limit exceeded",
                ));
            }
            let inner = self.type_name();
            self.depth -= 1;
            name.push('[');
            name.push_str(&inner?);
            self.need("]")?;
            name.push(']');
        }
        Ok(name)
    }
    fn expr(&mut self, min: u8) -> Result<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(Diagnostic::new(
                self.token().span,
                "expression nesting limit exceeded",
            ));
        }
        let result = self.expression(min);
        self.depth -= 1;
        result
    }
    fn expression(&mut self, min: u8) -> Result<Expr> {
        let t = self.take();
        let start = t.span.start;
        let kind = match t.text.as_str() {
            "" => return Err(Diagnostic::new(t.span, "expected expression")),
            "true" => ExprKind::Literal(Value::Bool(true)),
            "false" => ExprKind::Literal(Value::Bool(false)),
            "!" => ExprKind::Not(Box::new(self.expr(4)?)),
            "(" => {
                let records = std::mem::replace(&mut self.records, true);
                let result = self.expr(0);
                self.records = records;
                let e = result?;
                self.need(")")?;
                e.kind
            }
            "[" => ExprKind::List(self.args("]")?),
            "{" => {
                let mut fields = Vec::new();
                while !self.is("}") && !self.is("") {
                    let k = if self.token().text.starts_with('"') {
                        let t = self.take();
                        Name {
                            text: serde_json::from_str(&t.text)
                                .map_err(|_| Diagnostic::new(t.span, "invalid map key"))?,
                            span: t.span,
                        }
                    } else {
                        self.name()?
                    };
                    self.need(":")?;
                    fields.push((k, self.expr(0)?));
                    if !self.eat(",") {
                        break;
                    }
                }
                self.need("}")?;
                ExprKind::Map(fields)
            }
            "|" => {
                let mut params = Vec::new();
                while !self.is("|") && !self.is("") {
                    params.push(self.name()?);
                    if !self.eat(",") {
                        break;
                    }
                }
                self.need("|")?;
                ExprKind::Lambda(params, self.body()?)
            }
            s if s.starts_with('"') || s.starts_with(|c: char| c.is_ascii_digit() || c == '-') => {
                match serde_json::from_str(&t.text) {
                    Ok(Value::Number(number)) if !number.is_i64() => {
                        return Err(Diagnostic::new(
                            t.span,
                            "integer outside signed 64-bit range",
                        ));
                    }
                    Ok(value) => ExprKind::Literal(value),
                    Err(_) if t.text.starts_with('"') => {
                        self.diagnostics.push(Diagnostic::new(
                            t.span,
                            "invalid or incomplete string literal",
                        ));
                        ExprKind::Literal(Value::String(String::new()))
                    }
                    Err(_) => return Err(Diagnostic::new(t.span, "invalid literal")),
                }
            }
            s if s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') => {
                self.at = self.at.saturating_sub(1);
                let name = self.path_name()?;
                if self.records && self.eat("{") {
                    let mut fields = Vec::new();
                    while !self.is("}") && !self.is("") {
                        let field = self.name()?;
                        self.need(":")?;
                        fields.push((field, self.expr(0)?));
                        if !self.eat(",") {
                            break;
                        }
                    }
                    self.need("}")?;
                    ExprKind::Record(name, fields)
                } else {
                    ExprKind::Name(name)
                }
            }
            _ => return Err(Diagnostic::new(t.span, "expected expression")),
        };
        let mut left = Expr {
            span: Span {
                start,
                end: self.tokens[self.at.saturating_sub(1)].span.end,
            },
            kind,
        };
        let mut chain = 0;
        loop {
            chain += 1;
            if chain > MAX_DEPTH {
                return Err(Diagnostic::new(
                    left.span,
                    "expression chain limit exceeded",
                ));
            }
            let kind = if self.eat(".") {
                let name = match self.name() {
                    Ok(n) => n,
                    Err(e) => {
                        self.diagnostics.push(e);
                        Name {
                            text: String::new(),
                            span: self.token().span,
                        }
                    }
                };
                Some(ExprKind::Member(Box::new(left.clone()), name))
            } else if self.eat("(") {
                Some(ExprKind::Call(Box::new(left.clone()), self.args(")")?))
            } else if self.eat("[") {
                let index = self.expr(0)?;
                self.need("]")?;
                Some(ExprKind::Index(Box::new(left.clone()), Box::new(index)))
            } else {
                let precedence = match self.token().text.as_str() {
                    "==" | "!=" => 1,
                    "+" => 2,
                    _ => 0,
                };
                if precedence == 0 || precedence < min {
                    None
                } else {
                    let op = self.take().text;
                    Some(ExprKind::Binary(
                        Box::new(left.clone()),
                        op,
                        Box::new(self.expr(precedence + 1)?),
                    ))
                }
            };
            let Some(kind) = kind else { break };
            left = Expr {
                span: Span {
                    start,
                    end: self.tokens[self.at.saturating_sub(1)].span.end,
                },
                kind,
            };
        }
        Ok(left)
    }
    fn args(&mut self, end: &str) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        while !self.is(end) && !self.is("") {
            args.push(self.expr(0)?);
            if !self.eat(",") {
                break;
            }
        }
        self.need(end)?;
        Ok(args)
    }
}
