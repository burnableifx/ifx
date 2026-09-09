//! Minimal LSP 3.17 stdio adapter. Full-document sync, UTF-16 positions, bounded frames.
//! Analysis never reads a workspace file: only explicitly supplied open buffers.
use crate::{
    analyze,
    language::{Analysis, analyze_workspace},
    syntax::{MAX_SOURCE, Span},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Read, Write},
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
            self.initialized = true;
            return reply(json!({"capabilities":{
                "positionEncoding":"utf-16", "textDocumentSync":{"openClose":true,"change":1},
                "completionProvider":{"triggerCharacters":["."]},"hoverProvider":true,"definitionProvider":true,
                "documentSymbolProvider":true,"documentFormattingProvider":true
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
                    analyze_workspace(uri, &sources)
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
                    doc.analysis = analyze_workspace(other, &sources);
                    doc.analysis.compilation = None;
                    let diagnostics:Vec<_>=doc.analysis.diagnostics.iter().map(|d|json!({"range":range(&doc.text,d.span),"severity":1,"source":"ifx","message":d.message})).collect();
                    notifications.push(json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":other,"version":doc.version,"diagnostics":diagnostics}}));
                }
                notifications
            }
            "textDocument/didClose" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                self.documents.remove(uri);
                let mut notifications = vec![
                    json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}),
                ];
                let sources: BTreeMap<String, String> = self
                    .documents
                    .iter()
                    .map(|(uri, d)| (uri.clone(), d.text.clone()))
                    .collect();
                for (uri, doc) in &mut self.documents {
                    doc.analysis = analyze_workspace(uri, &sources);
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
            | "textDocument/formatting" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let Some(doc) = self.documents.get(uri) else {
                    return reply(Value::Null);
                };
                let offset = offset(&doc.text, &params["position"]);
                match method {
                    "textDocument/completion" => {
                        let prefix = &doc.text[..offset];
                        let dot = prefix.rfind('.').filter(|&n| prefix[n+1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                        let items: Vec<_> = if let Some(hint) = doc.analysis.argument_hints.iter().find(|h| h.span.start <= offset && offset <= h.span.end && !h.items.is_empty()) {
                            hint.items.iter().map(|c| json!({"label":c.label,"kind":12,"detail":c.detail})).collect()
                        } else if let Some(dot) = dot {
                            doc.analysis.hints.iter().filter(|h| h.span.end <= dot && doc.text[h.span.end..dot].trim().is_empty()).max_by_key(|h| h.span.end-h.span.start).map(|h| h.items.iter().map(|c| json!({"label":c.label,"kind":2,"detail":c.detail})).collect()).unwrap_or_default()
                        } else {
                            let mut items: Vec<_> = ["resource", "let", "input", "output", "for", "if", "linode", "memory"].iter().map(|s| json!({"label":s,"kind":14})).collect();
                            items.extend(doc.analysis.symbols.iter().filter(|s| s.span.start <= offset && s.scope.start <= offset && offset <= s.scope.end).map(|s| json!({"label":s.name,"kind":6,"detail":s.detail}))); items
                        };
                        reply(json!({"isIncomplete":false,"items":items}))
                    },
                    "textDocument/hover" | "textDocument/definition" => {
                        let occurrence = doc.analysis.occurrences.iter().find(|o| o.span.start <= offset && offset < o.span.end);
                        let symbol = doc.analysis.symbols.iter().find(|s| s.span.start <= offset && offset < s.span.end);
                        if method.ends_with("definition") {
                            let span = occurrence.and_then(|o| o.definition).or_else(|| symbol.map(|s| s.span));
                            reply(span.map_or(Value::Null, |s| json!({"uri":uri,"range":range(&doc.text,s)})))
                        } else {
                            let text = occurrence.map(|o| (o.span, &o.detail)).or_else(|| symbol.map(|s| (s.span,&s.detail)));
                            reply(text.map_or(Value::Null, |(s,t)| json!({"contents":{"kind":"plaintext","value":t},"range":range(&doc.text,s)})))
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
