//! Minimal LSP 3.17 stdio adapter. Full-document sync, UTF-16 positions, bounded frames.
//! Project files are read only within initialized workspaces; Git fetching is never implicit.
use crate::{
    analyze,
    language::{Analysis, analyze_workspace},
    project,
    syntax::{MAX_SOURCE, Span},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};

const MAX_FRAME: usize = 2 * 1024 * 1024;
struct Document {
    text: String,
    version: i64,
    analysis: Analysis,
}
#[derive(Default)]
pub struct Server {
    documents: BTreeMap<String, Document>,
    roots: Vec<PathBuf>,
    initialized: bool,
    shutdown: bool,
}
impl Server {
    /// JSON-RPC request/notification boundary, also used by protocol tests.
    pub fn handle(&mut self, message: Value) -> Vec<Value> {
        let id = message.get("id").cloned();
        let method = message["method"].as_str().unwrap_or("");
        let params = &message["params"];
        let reply = |result: Value| {
            id.as_ref()
                .map(|id| json!({"jsonrpc":"2.0", "id":id, "result":result}))
                .into_iter()
                .collect()
        };
        let error = |code: i32, text: &str| {
            id.as_ref()
                .map(|id| json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":text}}))
                .into_iter()
                .collect()
        };
        if self.shutdown {
            return error(-32600, "server has shut down");
        }
        if method == "initialize" {
            if self.initialized {
                return error(-32600, "already initialized");
            }
            self.roots = params["workspaceFolders"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|folder| folder["uri"].as_str())
                .chain(params["rootUri"].as_str())
                .take(8)
                .filter_map(file_path)
                .filter_map(|p| p.canonicalize().ok())
                .collect();
            self.initialized = true;
            return reply(json!({"capabilities":{
                "positionEncoding":"utf-16", "textDocumentSync":{"openClose":true,"change":1,"save":true},
                "completionProvider":{"triggerCharacters":[".",":"]},"hoverProvider":true,"definitionProvider":true,
                "documentSymbolProvider":true,"documentFormattingProvider":true,
                "semanticTokensProvider":{"legend":{"tokenTypes":["keyword","type","function","variable","property","string","number"],"tokenModifiers":[]},"full":true}
            },"serverInfo":{"name":"ifx-lang","version":env!("CARGO_PKG_VERSION")}}));
        }
        if !self.initialized {
            return error(-32002, "server not initialized");
        }
        if method == "shutdown" {
            self.shutdown = true;
            return reply(Value::Null);
        }
        match method {
            "initialized" | "$/cancelRequest" | "$/setTrace" => Vec::new(),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let doc = &params["textDocument"];
                let Some(uri) = doc["uri"].as_str() else {
                    return Vec::new();
                };
                let Some(version) = doc["version"].as_i64() else {
                    return Vec::new();
                };
                let text = if method.ends_with("didOpen") {
                    doc["text"].as_str()
                } else {
                    params["contentChanges"]
                        .as_array()
                        .filter(|a| a.len() == 1)
                        .and_then(|a| {
                            if a[0].get("range").is_some() {
                                None
                            } else {
                                a[0]["text"].as_str()
                            }
                        })
                };
                let Some(text) = text else {
                    return Vec::new();
                };
                if self
                    .documents
                    .get(uri)
                    .is_some_and(|d| d.version >= version)
                {
                    return Vec::new();
                }
                if self.documents.len() >= 32 && !self.documents.contains_key(uri) {
                    return Vec::new();
                }
                let sources: BTreeMap<String, String> = self
                    .documents
                    .iter()
                    .map(|(uri, d)| (uri.clone(), d.text.clone()))
                    .chain(std::iter::once((uri.to_string(), text.to_string())))
                    .collect();
                let mut analysis = if text.len() > MAX_SOURCE {
                    analyze(text)
                } else {
                    analyze_document(uri, &sources, &self.roots)
                };
                analysis.compilation = None;
                let diagnostics: Vec<_> = analysis.diagnostics.iter().map(|d| json!({"range":range(text,d.span),"severity":1,"source":"ifx","message":d.message})).collect();
                let notification = json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"version":version,"diagnostics":diagnostics}});
                // Oversized buffers get a diagnostic but are never retained.
                if text.len() <= MAX_SOURCE
                    && sources.values().map(String::len).sum::<usize>() <= 1024 * 1024
                {
                    self.documents.insert(
                        uri.into(),
                        Document {
                            text: text.into(),
                            version,
                            analysis,
                        },
                    );
                } else {
                    self.documents.remove(uri);
                }
                let mut notifications = vec![notification];
                for (other, doc) in self
                    .documents
                    .iter_mut()
                    .filter(|(key, _)| key.as_str() != uri)
                {
                    doc.analysis = analyze_document(other, &sources, &self.roots);
                    doc.analysis.compilation = None;
                    let diagnostics:Vec<_>=doc.analysis.diagnostics.iter().map(|d|json!({"range":range(&doc.text,d.span),"severity":1,"source":"ifx","message":d.message})).collect();
                    notifications.push(json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":other,"version":doc.version,"diagnostics":diagnostics}}));
                }
                notifications
            }
            "textDocument/didClose"
            | "textDocument/didSave"
            | "workspace/didChangeWatchedFiles" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let closing = method == "textDocument/didClose";
                if closing {
                    self.documents.remove(uri);
                }
                let mut notifications = if closing {
                    vec![
                        json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}),
                    ]
                } else {
                    Vec::new()
                };
                let sources: BTreeMap<String, String> = self
                    .documents
                    .iter()
                    .map(|(uri, d)| (uri.clone(), d.text.clone()))
                    .collect();
                for (uri, doc) in &mut self.documents {
                    doc.analysis = analyze_document(uri, &sources, &self.roots);
                    doc.analysis.compilation = None;
                    let diagnostics: Vec<_> = doc.analysis.diagnostics.iter().map(|d|json!({"range":range(&doc.text,d.span),"severity":1,"source":"ifx","message":d.message})).collect();
                    notifications.push(json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"version":doc.version,"diagnostics":diagnostics}}));
                }
                notifications
            }
            "textDocument/completion"
            | "textDocument/hover"
            | "textDocument/definition"
            | "textDocument/documentSymbol"
            | "textDocument/formatting"
            | "textDocument/semanticTokens/full" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let Some(doc) = self.documents.get(uri) else {
                    return reply(Value::Null);
                };
                let offset = offset(&doc.text, &params["position"]);
                match method {
                    "textDocument/semanticTokens/full" => reply(json!({"data":semantic_tokens(&doc.text)})),
                    "textDocument/completion" => {
                        let prefix = &doc.text[..offset];
                        let dot = prefix.rfind('.').filter(|&n| prefix[n+1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                        let associated = prefix.rfind("::").filter(|&n| prefix[n+2..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                        let dot = dot.into_iter().chain(associated).max();
                        let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
                        let line = prefix[line_start..].trim_start();
                        let use_prefix = line.strip_prefix("use ").map(str::trim_start);
                        let items: Vec<_> = if let Some(typed) = use_prefix.filter(|s| !s.chars().any(char::is_whitespace)) {
                            doc.analysis.modules.iter().filter(|p| p.starts_with(typed)).map(|p| json!({"label":p,"kind":9,"textEdit":{"range":range(&doc.text,Span {start:offset-typed.len(),end:offset}),"newText":p}})).collect()
                        } else if let Some(hint) = doc.analysis.argument_hints.iter().find(|h| h.span.start <= offset && offset <= h.span.end && !h.items.is_empty()) {
                            hint.items.iter().map(|c| json!({"label":c.label,"kind":12,"detail":c.detail})).collect()
                        } else if let Some(dot) = dot {
                            doc.analysis.hints.iter().filter(|h| h.span.end <= dot && doc.text[h.span.end..dot].trim().is_empty()).max_by_key(|h| h.span.end-h.span.start).map(|h| h.items.iter().map(|c| json!({"label":c.label,"kind":2,"detail":c.detail})).collect()).unwrap_or_default()
                        } else {
                            let mut items: Vec<_> = ["use", "pub", "struct", "impl", "fn", "return", "let", "for", "if", "else", "Self", "module", "resource", "input", "output", "linode", "memory"].iter().map(|s| json!({"label":s,"kind":14})).collect();
                            items.extend(doc.analysis.symbols.iter().filter(|s| s.span.start <= offset && s.scope.start <= offset && offset <= s.scope.end).map(|s| json!({"label":s.name,"kind":6,"detail":s.detail}))); items
                        };
                        reply(json!({"isIncomplete":false,"items":items}))
                    },
                    "textDocument/hover" | "textDocument/definition" => {
                        let occurrence = doc.analysis.occurrences.iter().find(|o| o.span.start <= offset && offset < o.span.end);
                        let symbol = doc.analysis.symbols.iter().find(|s| s.span.start <= offset && offset < s.span.end);
                        if method.ends_with("definition") {
                            if let Some(n) = doc.analysis.navigation.iter().find(|n| n.span.start <= offset && offset < n.span.end) {
                                return reply(json!({"uri":file_uri(&n.source),"range":{"start":{"line":n.start.0,"character":n.start.1},"end":{"line":n.end.0,"character":n.end.1}}}));
                            }
                            let span = occurrence.and_then(|o| o.definition).or_else(|| symbol.map(|s| s.span));
                            reply(span.map_or(Value::Null, |s| json!({"uri":uri,"range":range(&doc.text,s)})))
                        } else {
                            let text = occurrence.map(|o| (o.span, &o.detail)).or_else(|| symbol.map(|s| (s.span,&s.detail)));
                            if let Some((s,t)) = text { return reply(json!({"contents":{"kind":"plaintext","value":t},"range":range(&doc.text,s)})); }
                            let hint = doc.analysis.hints.iter().filter(|h| h.span.start <= offset && offset <= h.span.end).min_by_key(|h|h.span.end-h.span.start);
                            reply(hint.map_or(Value::Null, |h|json!({"contents":{"kind":"plaintext","value":h.items.iter().map(|i|format!("{}: {}",i.label,i.detail)).collect::<Vec<_>>().join("\n")},"range":range(&doc.text,h.span)})))
                        }
                    },
                    "textDocument/documentSymbol" => reply(json!(doc.analysis.symbols.iter().map(|s| json!({"name":s.name,"detail":s.detail,"kind":13,"range":range(&doc.text,s.span),"selectionRange":range(&doc.text,s.span)})).collect::<Vec<_>>())),
                    _ => {
                        // Whitespace-only formatter preserves all tokens/comments and existing vertical chains.
                        let formatted = format_source(&doc.text);
                        reply(json!([{"range":range(&doc.text,Span { start:0,end:doc.text.len() }),"newText":formatted}]))
                    }
                }
            }
            _ => error(-32601, "method not supported"),
        }
    }
}
fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(char::from(b));
        } else {
            use std::fmt::Write;
            write!(uri, "%{b:02X}").expect("String write");
        }
    }
    uri
}
fn semantic_tokens(source: &str) -> Vec<u32> {
    let parsed = crate::syntax::parse(source);
    let mut data = Vec::new();
    let mut previous = (0, 0);
    let (mut cursor, mut line, mut col) = (0, 0u32, 0u32);
    for (i, token) in parsed.tokens.iter().enumerate() {
        let text = token.text.as_str();
        let before = i.checked_sub(1).map(|i| parsed.tokens[i].text.as_str());
        let after = parsed.tokens.get(i + 1).map(|t| t.text.as_str());
        let kind = if matches!(
            text,
            "use"
                | "as"
                | "pub"
                | "struct"
                | "impl"
                | "fn"
                | "let"
                | "return"
                | "for"
                | "in"
                | "if"
                | "else"
                | "true"
                | "false"
                | "resource"
                | "module"
                | "input"
                | "output"
        ) {
            0
        } else if text.starts_with('"') {
            5
        } else if text.starts_with(|c: char| c.is_ascii_digit())
            || text.starts_with('-')
                && text.len() > 1
                && text[1..].starts_with(|c: char| c.is_ascii_digit())
        {
            6
        } else if text.starts_with(|c: char| c.is_ascii_uppercase())
            || matches!(before, Some("struct" | "impl"))
        {
            1
        } else if before == Some("fn") || after == Some("(") {
            2
        } else if before == Some(".") {
            4
        } else if text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            3
        } else {
            continue;
        };
        let mut byte = token.span.start;
        for part in source[token.span.start..token.span.end].split_inclusive('\n') {
            let length = part.trim_end_matches(['\n', '\r']).encode_utf16().count() as u32;
            if length > 0 {
                for c in source[cursor..byte].chars() {
                    if c == '\n' {
                        line += 1;
                        col = 0;
                    } else {
                        col += c.len_utf16() as u32;
                    }
                }
                cursor = byte;
                data.extend([
                    line - previous.0,
                    if line == previous.0 {
                        col - previous.1
                    } else {
                        col
                    },
                    length,
                    kind,
                    0,
                ]);
                previous = (line, col);
            }
            byte += part.len();
        }
    }
    data
}
/// Accept local absolute file URIs, including percent-encoded UTF-8. Never accept remote authorities.
fn file_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') || encoded.contains(['?', '#']) {
        return None;
    }
    let mut bytes = Vec::new();
    let mut input = encoded.bytes();
    while let Some(byte) = input.next() {
        bytes.push(if byte == b'%' {
            let high = char::from(input.next()?).to_digit(16)?;
            let low = char::from(input.next()?).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    let decoded = String::from_utf8(bytes).ok()?;
    if decoded.contains('\0') || decoded.split('/').any(|p| matches!(p, "." | "..")) {
        return None;
    }
    Some(PathBuf::from(decoded))
}
fn analyze_document(uri: &str, sources: &BTreeMap<String, String>, roots: &[PathBuf]) -> Analysis {
    let Some(path) = file_path(uri) else {
        return analyze_workspace(uri, sources);
    };
    let Some(boundary) = roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|p| p.components().count())
    else {
        return analyze_workspace(uri, sources);
    };
    let overlays = sources
        .iter()
        .filter_map(|(uri, text)| file_path(uri).map(|p| (p, text.clone())))
        .collect();
    let snapshot =
        match project::load_in_workspace(path.parent().unwrap_or(boundary), boundary, &overlays) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return analyze_workspace(uri, sources),
            Err(error) => return project::diagnostic(error),
        };
    let Some((entry, _)) = snapshot
        .paths
        .iter()
        .find(|(_, p)| p.as_path() == Path::new(&path))
    else {
        return project::diagnostic(project::Error::Invalid(
            "file is not declared in Ifx.toml".into(),
        ));
    };
    snapshot.check(entry)
}
/// Conservative formatting for the first grammar: trim line ends, normalize final newline.
/// No AST reprinting, which would discard comments and break incomplete buffers.
pub fn format_source(source: &str) -> String {
    if source.is_empty() {
        return String::new();
    }
    let mut out = source
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}
pub fn position(source: &str, byte: usize) -> Value {
    let mut byte = byte.min(source.len());
    while !source.is_char_boundary(byte) {
        byte -= 1;
    }
    let prefix = &source[..byte];
    let line = prefix.bytes().filter(|b| *b == b'\n').count();
    let column = prefix
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count();
    json!({"line":line,"character":column})
}
pub fn range(source: &str, span: Span) -> Value {
    json!({"start":position(source,span.start),"end":position(source,span.end)})
}
pub fn offset(source: &str, position: &Value) -> usize {
    let line = position["line"].as_u64().unwrap_or(0) as usize;
    let column = position["character"].as_u64().unwrap_or(0) as usize;
    let mut start = 0;
    for _ in 0..line.min(source.len() + 1) {
        let Some(end) = source[start..].find('\n') else {
            return source.len();
        };
        start += end + 1;
    }
    let text = source[start..].split('\n').next().unwrap_or("");
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units + c.len_utf16() > column {
            return start + byte;
        }
        units += c.len_utf16();
    }
    start + text.len()
}
pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut size = None;
    let mut total = 0;
    loop {
        let mut line = String::new();
        let n = reader.take(8193).read_line(&mut line)?;
        if n == 0 {
            if total == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete LSP header",
            ));
        }
        total += n;
        if total > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header exceeds limit",
            ));
        }
        if line == "\r\n" {
            break;
        }
        let Some((key, value)) = line.trim_end().split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid LSP header",
            ));
        };
        if key.eq_ignore_ascii_case("Content-Length") {
            if size.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate Content-Length",
                ));
            }
            size = Some(value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
            })?);
        }
    }
    let size = size.filter(|n| *n <= MAX_FRAME).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or oversized Content-Length",
        )
    })?;
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid LSP JSON"))
}
pub fn serve() -> io::Result<()> {
    let mut server = Server::default();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    while let Some(message) = read_frame(&mut input)? {
        if message["method"] == "exit" {
            if server.shutdown {
                return Ok(());
            }
            return Err(io::Error::other("exit before shutdown"));
        }
        for response in server.handle(message) {
            let body = serde_json::to_vec(&response)?;
            write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
            output.write_all(&body)?;
            output.flush()?;
        }
    }
    Ok(())
}
