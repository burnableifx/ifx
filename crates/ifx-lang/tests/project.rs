//! Component tier: local filesystem, offline CLI and JSON-RPC. No network or provider execution.
use ifx_lang::{lsp::Server, project, syntax};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Ifx.toml",
        r#"
[package]
name = "app"
version = "0.1.0"
entry = "main.ifx"
[modules]
local = "local.ifx"
[dependencies]
shared = { path = "shared" }
"#,
    );
    write(root.path(), "local.ifx", "output value: Int = 7;");
    write(
        root.path(),
        "main.ifx",
        "use shared::nested::web as server; module app = server(\"app\").value(42); output result: Int = app.result;",
    );
    write(
        root.path(),
        "shared/Ifx.toml",
        r#"
[package]
name = "shared_package"
version = "0.1.0"
[modules]
"nested::web" = "src/web.ifx"
base = "src/base.ifx"
"#,
    );
    write(
        root.path(),
        "shared/src/web.ifx",
        "use crate::base; input value: Int = 1; module child = base(\"child\").value(value); output result: Int = child.result;",
    );
    write(
        root.path(),
        "shared/src/base.ifx",
        "input value: Int = 2; output result: Int = value;",
    );
    root
}
fn errors(root: &Path) -> String {
    match project::load(root, &BTreeMap::new()) {
        Ok(snapshot) => snapshot
            .analyze(snapshot.entry.as_ref().unwrap())
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => error.to_string(),
    }
}
#[test]
fn aliases_and_package_local_uses_preserve_inputs_and_outputs() {
    let root = fixture();
    let snapshot = project::load(root.path(), &BTreeMap::new()).unwrap();
    let analysis = snapshot.analyze(snapshot.entry.as_ref().unwrap());
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    assert_eq!(analysis.compilation.unwrap().outputs["result"], 42);
    let library = project::load(&root.path().join("shared"), &BTreeMap::new()).unwrap();
    assert!(library.entry.is_none());
    assert!(
        library.analyze("crate/src/web.ifx").diagnostics.is_empty(),
        "library exports must not require a default main.ifx"
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .args(["compile", "Ifx.toml"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["outputs"]["result"],
        42
    );
}
#[test]
fn dependency_cannot_import_consumer_modules_or_undeclared_exports() {
    let root = fixture();
    write(
        root.path(),
        "shared/src/web.ifx",
        "use crate::local; input value: Int = 1; output result: Int = 0;",
    );
    assert!(errors(root.path()).contains("not declared"));
    write(
        root.path(),
        "main.ifx",
        "use shared::private; module app = private(\"app\");",
    );
    write(
        root.path(),
        "shared/private.ifx",
        "output secret: String = \"unexported\";",
    );
    assert!(errors(root.path()).contains("not declared"));
    let result = ifx_lang::analyze("if true { use crate::local; }");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("module scope"))
    );
}
#[test]
fn invalid_manifests_reject_ambiguous_dependencies_and_escapes_without_echoing_values() {
    for addition in [
        "[modules]\nweb = '../outside.ifx'",
        "[modules]\nweb = '/outside.ifx'",
        "[dependencies]\nx = { path = '../outside' }",
        "[dependencies]\nx = { git = 'file:///outside', rev = 'main' }",
        "[dependencies]\nx = { git = 'https://example.com/repo', rev = 'main' }",
        "[dependencies]\nx = { git = 'https://user:secret@example.com/repo', rev = '0000000000000000000000000000000000000000' }",
        "[dependencies]\nx = { path = 'shared', git = 'https://example.com/repo', rev = '0000000000000000000000000000000000000000' }",
        "[dependencies]\nx = { path = 'shared', typo = 'secret-value' }",
    ] {
        let text = format!("[package]\nname = 'app'\nversion = '1'\n{addition}");
        let error = project::parse_manifest(&text).unwrap_err().to_string();
        assert!(
            !error.contains("secret-value"),
            "diagnostics must not echo manifest values"
        );
    }
    let root = fixture();
    write(
        root.path(),
        "Ifx.toml",
        "[package]\nname = 'app'\nversion = '1'\nentry = 'main.ifx'\n[dependencies]\nshared = { git = 'https://example.com/repo', rev = '0000000000000000000000000000000000000000' }",
    );
    assert!(errors(root.path()).contains("run `ifx-lang fetch"));
    assert!(
        !root.path().join(".ifx").exists(),
        "offline loading must not fetch or create cache state"
    );
}
#[test]
fn module_loads_and_editor_discovery_refuse_symlinks_and_nonregular_files() {
    use std::os::unix::fs::symlink;
    let root = fixture();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "Ifx.toml", "this must never be parsed");
    symlink(outside.path(), root.path().join("escape")).unwrap();
    let result =
        project::load_in_workspace(&root.path().join("escape"), root.path(), &BTreeMap::new());
    assert!(
        matches!(result, Err(project::Error::Fs(_))),
        "discovery must stop at the directory symlink"
    );
    fs::remove_file(root.path().join("local.ifx")).unwrap();
    symlink(
        outside.path().join("Ifx.toml"),
        root.path().join("local.ifx"),
    )
    .unwrap();
    assert!(project::load(root.path(), &BTreeMap::new()).is_err());
    fs::remove_file(root.path().join("local.ifx")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        root.path().join("local.ifx"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR,
        0,
    )
    .unwrap();
    assert!(errors(root.path()).contains("regular file"));
}
fn uri(path: &Path) -> String {
    format!("file://{}", path.display()).replace(' ', "%20")
}
#[test]
fn lsp_loads_declared_disk_modules_completes_use_paths_and_overlays_unsaved_edits() {
    let root = fixture();
    let mut server = Server::default();
    server.handle(json!({"id":1,"method":"initialize","params":{"rootUri":uri(root.path())}}));
    let main = uri(&root.path().join("main.ifx"));
    let open = |server: &mut Server, uri: &str, text: &str, version: i32| {
        server.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"version":version,"text":text}}}))
    };
    let notifications = open(
        &mut server,
        &main,
        &fs::read_to_string(root.path().join("main.ifx")).unwrap(),
        1,
    );
    assert_eq!(
        notifications[0]["params"]["diagnostics"],
        json!([]),
        "dependency need not be open"
    );
    let child = uri(&root.path().join("shared/src/base.ifx"));
    let changed = open(
        &mut server,
        &child,
        "input value: String = \"wrong type\"; output result: String = value;",
        1,
    );
    assert!(
        changed.iter().any(|n| n["params"]["uri"] == main
            && !n["params"]["diagnostics"].as_array().unwrap().is_empty()),
        "unsaved dependency edits must invalidate the consumer"
    );
    let closed = server
        .handle(json!({"method":"textDocument/didClose","params":{"textDocument":{"uri":child}}}));
    assert!(
        closed
            .iter()
            .any(|n| n["params"]["uri"] == main && n["params"]["diagnostics"] == json!([])),
        "closing a project buffer restores the saved module"
    );
    open(&mut server, &main, "use shared::", 2);
    let complete = server.handle(json!({"id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":main},"position":{"line":0,"character":12}}}));
    assert!(
        complete[0]["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["label"] == "shared::nested::web")
    );
    assert!(
        !complete[0]["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["label"] == "crate::local")
    );
}
#[test]
fn partial_use_syntax_recovers_without_accepting_globs_or_losing_later_statements() {
    let text = "use shared::nested::web as server;\noutput value: Int = 1;";
    for end in 0..=text.len() {
        let parsed = syntax::parse(&text[..end]);
        assert!(parsed.diagnostics.iter().all(|d| d.span.end <= end));
    }
    assert!(!syntax::parse("use shared::*;").diagnostics.is_empty());
    let parsed = syntax::parse("use shared::;\noutput value: Int = 1;");
    assert!(
        parsed.statements.iter().any(
            |s| matches!(&s.kind, syntax::StmtKind::Bind { name, .. } if name.text == "value")
        )
    );
}

#[test]
fn git_url_and_revision_rules_distinguish_each_invalid_boundary_from_valid_pins() {
    let pin = "0123456789abcdef0123456789abcdef01234567";
    let cases = [
        ("https://example.com/modules.git", pin, true),
        (
            "https://git.example.com/team/modules",
            "ffffffffffffffffffffffffffffffffffffffff",
            true,
        ),
        ("https:///modules", pin, false),
        ("https://example.com/", pin, false),
        ("https://example.com:443/modules", pin, false),
        ("https://example.com/modules?query", pin, false),
        ("https://example.com/has space", pin, false),
        ("https://example.com/control\u{7f}", pin, false),
        ("https://example.com/modules", "", false),
        (
            "https://example.com/modules",
            "0123456789abcdef0123456789abcdef0123456",
            false,
        ),
        (
            "https://example.com/modules",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            false,
        ),
        (
            "https://example.com/modules",
            "gggggggggggggggggggggggggggggggggggggggg",
            false,
        ),
    ];
    for (url, rev, accepted) in cases {
        let url_literal = toml::Value::String(url.into()).to_string();
        let rev_literal = toml::Value::String(rev.into()).to_string();
        let manifest = format!(
            "[package]\nname = 'app'\nversion = '1'\n[dependencies]\nshared = {{ git = {url_literal}, rev = {rev_literal} }}"
        );
        assert_eq!(
            project::parse_manifest(&manifest).is_ok(),
            accepted,
            "URL/revision boundary: {url:?}, {rev:?}"
        );
    }
}
#[test]
fn lsp_reports_manifest_load_failures_without_echoing_manifest_lines() {
    let root = fixture();
    write(root.path(), "Ifx.toml", "secret-looking-value = [invalid");
    let mut server = Server::default();
    server.handle(json!({"id":1,"method":"initialize","params":{"rootUri":uri(root.path())}}));
    let notifications = server.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":uri(&root.path().join("main.ifx")),"version":1,"text":"output x: Int = 1;"}}}));
    let diagnostics = notifications[0]["params"]["diagnostics"]
        .as_array()
        .unwrap();
    assert!(
        !diagnostics.is_empty(),
        "manifest failures must block project analysis visibly"
    );
    assert!(
        !diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("secret-looking-value")
    );
}
