use ifx_lang::{
    Compilation, analyze,
    lsp::{Server, offset, position},
    simulator::{Completion, Simulator},
    syntax,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn compile(source: &str) -> Compilation {
    let result = analyze(source);
    assert!(
        result.diagnostics.is_empty(),
        "valid fixture rejected: {:?}",
        result.diagnostics
    );
    result.compilation.expect("valid source lowers")
}
fn errors(source: &str) -> String {
    analyze(source)
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
const CONFIGURE: &str = include_str!("../../../examples/language/configure.ifx");
const LINODE: &str = include_str!("../../../examples/language/linode.ifx");

#[test]
fn lowers_existing_program_with_stable_keys_and_typed_deferred_outputs() {
    let c = compile(LINODE);
    assert_eq!(c.language, "ifx/0.1-experimental");
    assert_eq!(c.program.resources.len(), 3);
    assert_eq!(c.program.resources[0].urn.as_str(), "linode.instance:web-a");
    assert_eq!(c.program.resources[0].inputs["type"], "g6-nanode-1");
    assert_eq!(
        c.outputs["address"],
        json!({"$ref":"linode.instance:origin","$path":"ipv4"})
    );
    let renamed = compile(
        &LINODE
            .replace("resource origin =", "resource symbol =")
            .replace("origin.ipv4", "symbol.ipv4"),
    );
    assert_eq!(
        c.outputs, renamed.outputs,
        "local rename must preserve identity"
    );
    let reordered = compile(&LINODE.replace(
        "\"web-a\": \"g6-nanode-1\", \"web-b\": \"g6-nanode-1\"",
        "\"web-b\": \"g6-nanode-1\", \"web-a\": \"g6-nanode-1\"",
    ));
    assert_eq!(
        serde_json::to_value(c.program).unwrap(),
        serde_json::to_value(reordered.program).unwrap()
    );
}
#[test]
fn converges_in_source_order_and_selects_each_policy() {
    let c = compile(CONFIGURE);
    let mut sim = Simulator::default();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let h = &sim.hosts["memory.value:primary-web"];
    assert_eq!(h.records["initialized"], 1);
    assert_eq!(h.records["visited"], 2);
    assert_eq!(h.records["reloaded"], 1);
    assert_eq!(
        h.records["observed-after-creation"], 2,
        "conditions must observe earlier ensures"
    );
    assert_eq!(
        &h.events[..4],
        &[
            "converged package",
            "converged file",
            "converged service",
            "converged directory"
        ]
    );
    assert_eq!(
        h.events
            .iter()
            .filter(|e| e.starts_with("converged"))
            .count(),
        4,
        "unchanged ensure must not mutate"
    );
    let changed = compile(&CONFIGURE.replace("\"v1\"", "\"v2\""));
    sim.apply(&changed, &BTreeMap::new()).unwrap();
    assert_eq!(sim.hosts["memory.value:primary-web"].records["reloaded"], 2);
}
#[test]
fn once_does_not_repair_drift_but_ordinary_ensure_does() {
    let c = compile(CONFIGURE);
    let mut sim = Simulator::default();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let host = sim.hosts.get_mut("memory.value:primary-web").unwrap();
    host.remove_object("directory", "/srv/app");
    host.remove_object("file", "/etc/nginx/app.conf");
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let host = &sim.hosts["memory.value:primary-web"];
    assert!(!host.exists("/srv/app"));
    assert!(host.exists("/etc/nginx/app.conf"));
    assert_eq!(host.records["initialized"], 1);
}
#[test]
fn replacement_gets_new_once_identity() {
    let c = compile(CONFIGURE);
    let mut sim = Simulator::default();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    sim.hosts
        .get_mut("memory.value:primary-web")
        .unwrap()
        .replace();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let host = &sim.hosts["memory.value:primary-web"];
    assert_eq!(host.incarnation, 1);
    assert!(host.exists("/srv/app"));
    assert_eq!(host.records["initialized"], 1);
    assert_eq!(
        host.journal.len(),
        6,
        "completion history must distinguish incarnations"
    );
}
#[test]
fn failure_stops_body_and_uncertainty_blocks_replay_until_reconciled() {
    let source = r#"resource node = memory.value("node").value(1).configure("setup", |host| {
        host.once("init", || { host.record("before"); host.fail("failure"); host.record("after"); });
        host.record("dependent");
    });"#;
    let c = compile(source);
    let mut sim = Simulator::default();
    assert!(
        sim.apply(&c, &BTreeMap::new())
            .unwrap_err()
            .message
            .contains("failure")
    );
    assert!(
        sim.apply(&c, &BTreeMap::new())
            .unwrap_err()
            .message
            .contains("uncertain")
    );
    let host = sim.hosts.get_mut("memory.value:node").unwrap();
    assert_eq!(host.records["before"], 1);
    assert!(!host.records.contains_key("after"));
    assert!(!host.records.contains_key("dependent"));
    let key = host.journal.keys().next().unwrap().clone();
    assert_eq!(host.journal[&key], Completion::Uncertain);
    assert!(host.reconcile_success(&key, Value::Null));
    sim.apply(&c, &BTreeMap::new()).unwrap();
    assert_eq!(sim.hosts["memory.value:node"].records["dependent"], 1);
}
#[test]
fn simulator_requires_explicit_provider_facts_and_resolves_array_paths() {
    let source = format!(
        "{}\nresource target = memory.value(\"target\").value(1).configure(\"write\", |host| {{ host.file(\"/address\").content(origin.ipv4s[0]).ensure(); }});",
        LINODE
    );
    let c = compile(&source);
    let mut sim = Simulator::default();
    assert!(
        sim.apply(&c, &BTreeMap::new())
            .unwrap_err()
            .message
            .contains("output fixture")
    );
    let fixtures = BTreeMap::from([(
        "linode.instance:origin".into(),
        json!({"ipv4s":["192.0.2.1"]}),
    )]);
    sim.apply(&c, &fixtures).unwrap();
    assert_eq!(
        sim.hosts["memory.value:target"]
            .object("file", "/address")
            .unwrap()["content"],
        "192.0.2.1"
    );
}
#[test]
fn rejects_wrong_fields_types_duplicate_identity_and_runtime_graph_expansion() {
    let cases = [
        (
            "resource x = memory.value(\"x\").vaue(1);",
            "unknown resource input",
        ),
        ("resource x = memory.value(\"x\");", "required"),
        ("input x: String = 3;", "expected string"),
        (
            "resource x = memory.value(\"same\").value(1); resource y = memory.value(\"same\").value(2);",
            "duplicate resource",
        ),
        (
            "resource x = memory.value(\"x\").value(1).configure(\"c\", |host| { resource y = memory.value(\"y\").value(1); });",
            "cannot declare resources",
        ),
        ("if false { let x: Bool = 1; }", "expected bool"),
        ("for x in [] { let y: Bool = 1; }", "expected bool"),
        (
            "resource x = memory.value(\"x\").value(1).configure(\"c\", |host| { host.once(\"a\", || { host.always(\"b\", || {}); }); });",
            "nested action policies",
        ),
    ];
    for (source, want) in cases {
        let actual = errors(source);
        assert!(
            actual.contains(want),
            "{source}: expected {want}, got {actual}"
        );
    }
}
#[test]
fn rejects_unrepresentable_deferred_computation_before_emitting_program() {
    let source = format!("{LINODE}\nlet flag = origin.id == 1;");
    assert!(errors(&source).contains("belongs inside configure"));
    let key = format!("{LINODE}\nresource second = memory.value(origin.ipv4).value(1);");
    assert!(errors(&key).contains("known string"));
    let wrong = format!("{LINODE}\noutput wrong: Int = origin.ipv4;");
    assert!(errors(&wrong).contains("expected int"));
}
#[test]
fn static_analysis_does_not_execute_a_configuration() {
    let c = compile(
        "resource x = memory.value(\"x\").value(1).configure(\"c\", |host| { host.fail(\"would fail if run\"); });",
    );
    assert_eq!(c.configurations.len(), 1);
    assert!(Simulator::default().apply(&c, &BTreeMap::new()).is_err());
}
#[test]
fn bounds_nested_types_expansion_and_exponential_strings() {
    let nested = format!(
        "input x: {}Int{} = 1;",
        "List[".repeat(1000),
        "]".repeat(1000)
    );
    assert!(errors(&nested).contains("nesting limit"));
    let mut source = String::from("let x0 = \"abc\";\n");
    for n in 1..30 {
        source.push_str(&format!("let x{n} = x{} + x{};\n", n - 1, n - 1));
    }
    assert!(errors(&source).contains("limit"));
    let source = format!(
        "let xs = [{}]; for x in xs {{ let y = x; }}",
        vec!["1"; 257].join(",")
    );
    assert!(errors(&source).contains("256 entries"));
}
#[test]
fn recovers_after_syntax_error_and_survives_generated_partial_buffers() {
    assert!(
        syntax::parse("let broken = ; let good = 1;")
            .statements
            .iter()
            .any(|s| matches!(&s.kind,syntax::StmtKind::Bind {name,..} if name.text=="good"))
    );
    for boundary in CONFIGURE.char_indices().map(|(i, _)| i) {
        let result = analyze(&CONFIGURE[..boundary]);
        assert!(result.diagnostics.len() <= 100);
    }
    let alphabet = [
        "{", "}", "(", ")", "|", "[", "]", ".", ";", "input", "List", "\"x\"", "💡",
    ];
    let mut seed = 42u64;
    for _ in 0..200 {
        let mut source = String::new();
        for _ in 0..64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            source.push_str(alphabet[(seed as usize) % alphabet.len()]);
            source.push(' ');
        }
        let _ = analyze(&source);
    }
}
fn open(server: &mut Server, text: &str, version: i64) -> Vec<Value> {
    server.handle(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///workspace/test.ifx","languageId":"ifx","version":version,"text":text}}}))
}
fn server() -> Server {
    let mut s = Server::default();
    s.handle(json!({"id":1,"method":"initialize","params":{}}));
    s
}
fn request(server: &mut Server, method: &str, source: &str, at: usize) -> Value {
    server.handle(json!({"id":2,"method":method,"params":{"textDocument":{"uri":"file:///workspace/test.ifx"},"position":position(source,at)}})).remove(0)["result"].clone()
}
#[test]
fn lsp_and_cli_agree_on_ranges_and_ignore_older_versions() {
    let mut s = server();
    let source = "// 💡\ninput a: Int = \"wrong\";";
    let diagnostics = open(&mut s, source, 2);
    let analysis = analyze(source);
    assert_eq!(
        diagnostics[0]["params"]["diagnostics"][0]["message"],
        analysis.diagnostics[0].message
    );
    assert_eq!(
        diagnostics[0]["params"]["diagnostics"][0]["range"],
        ifx_lang::lsp::range(source, analysis.diagnostics[0].span)
    );
    assert!(open(&mut s, "", 1).is_empty());
    for (byte, _) in source.char_indices() {
        assert_eq!(offset(source, &position(source, byte)), byte);
    }
}
#[test]
fn completes_partial_namespace_and_builder_chains() {
    for (source, label) in [
        ("resource node = linode.", "instance"),
        ("resource node = linode.instance(\"web\").", "region"),
    ] {
        let mut s = server();
        open(&mut s, source, 1);
        let result = request(&mut s, "textDocument/completion", source, source.len());
        assert!(
            result["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["label"] == label),
            "{source}: {result}"
        );
    }
}
#[test]
fn navigates_local_names_without_confusing_stable_resource_keys() {
    let source = "let key = \"stable\"; resource symbol = memory.value(key).value(1);";
    let mut s = server();
    open(&mut s, source, 1);
    let at = source.rfind("key").unwrap();
    let result = request(&mut s, "textDocument/definition", source, at);
    assert_eq!(
        result["range"]["start"],
        position(source, source.find("key").unwrap())
    );
    let formatted = ifx_lang::lsp::format_source(CONFIGURE);
    assert_eq!(
        serde_json::to_value(compile(CONFIGURE).program).unwrap(),
        serde_json::to_value(compile(&formatted).program).unwrap()
    );
    assert!(formatted.starts_with("// Simulation only"));
    assert_eq!(formatted, ifx_lang::lsp::format_source(&formatted));
}
#[test]
fn rejects_oversized_and_ambiguous_lsp_frames() {
    for input in [
        "Content-Length: 999999999\r\n\r\n",
        "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        "Content-Length: 9\r\n\r\n{}",
    ] {
        assert!(ifx_lang::lsp::read_frame(&mut std::io::Cursor::new(input)).is_err());
    }
}
#[test]
fn typed_local_modules_namespace_resources_and_return_deferred_outputs() {
    let sources = BTreeMap::from([
        (
            "main.ifx".into(),
            include_str!("../../../examples/language/modules.ifx").into(),
        ),
        (
            "machine.ifx".into(),
            include_str!("../../../examples/language/machine.ifx").into(),
        ),
    ]);
    let imports = BTreeMap::from([(
        "main.ifx".into(),
        BTreeMap::from([("crate::machine".into(), "machine.ifx".into())]),
    )]);
    let a = ifx_lang::language::analyze_project("main.ifx", &sources, &imports);
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
    let c = a.compilation.unwrap();
    assert_eq!(c.program.resources.len(), 2);
    assert_ne!(c.program.resources[0].urn, c.program.resources[1].urn);
    assert_eq!(
        c.outputs["east_address"]["$ref"],
        c.program.resources[0].urn.as_str()
    );
    assert_eq!(c.program.resources[0].inputs["label"], "web-east");
    let mut bad = sources.clone();
    bad.get_mut("main.ifx")
        .unwrap()
        .push_str("\nmodule bad = machine(\"bad\").region(42);");
    assert!(
        ifx_lang::language::analyze_project("main.ifx", &bad, &imports)
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expected string"))
    );
    let recursive = BTreeMap::from([(
        "main.ifx".into(),
        "import again from \"./main.ifx\"; module recurse = again(\"recurse\");".into(),
    )]);
    assert!(
        ifx_lang::language::analyze_workspace("main.ifx", &recursive)
            .diagnostics
            .iter()
            .any(|d| d.message.contains("recursion/depth"))
    );
}
#[test]
fn lsp_rechecks_importers_from_unsaved_module_buffers() {
    let mut s = server();
    let source = "import child from \"./child.ifx\"; module instance = child(\"child\"); output result: Int = instance.result;";
    let before = open(&mut s, source, 1);
    assert!(
        !before[0]["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let after=s.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///workspace/child.ifx","version":1,"text":"output result: Int = 42;"}}}));
    let parent = after
        .iter()
        .find(|m| m["params"]["uri"] == "file:///workspace/test.ifx")
        .unwrap();
    assert!(
        parent["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn host_bindings_do_not_leak_to_top_level_completion() {
    let source =
        "resource x = memory.value(\"x\").value(1).configure(\"c\", |host| { let inside = 1; });\n";
    let mut s = server();
    open(&mut s, source, 1);
    let result = request(&mut s, "textDocument/completion", source, source.len());
    assert!(
        !result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["label"] == "host" || i["label"] == "inside")
    );
}
#[test]
fn stdio_round_trip_keeps_protocol_output_clean() {
    use std::io::{BufReader, Write};
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    for (id, method) in [(1, "initialize"), (2, "shutdown")] {
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":{}}).to_string();
        write!(input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        input.flush().unwrap();
        let response = ifx_lang::lsp::read_frame(&mut output).unwrap().unwrap();
        assert_eq!(response["id"], id);
        if id == 1 {
            assert_eq!(
                response["result"]["capabilities"]["positionEncoding"],
                "utf-16"
            );
        }
    }
    let body = json!({"jsonrpc":"2.0","method":"exit"}).to_string();
    write!(input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    drop(input);
    assert!(child.wait().unwrap().success());
}
#[test]
fn incremental_collection_limits_reject_many_references_to_a_large_value() {
    let source = format!(
        "let large = \"{}\"; let many = [{}];",
        "a".repeat(32_768),
        vec!["large"; 1000].join(",")
    );
    assert!(errors(&source).contains("collection exceeds"));
}
#[test]
fn module_defaults_are_checked_even_when_overridden_and_nested_imports_are_rejected() {
    let sources = BTreeMap::from([
        (
            "main.ifx".into(),
            "import child from \"./child.ifx\"; module x = child(\"x\").port(80);".into(),
        ),
        ("child.ifx".into(), "input port: Int = \"bad\";".into()),
    ]);
    assert!(
        ifx_lang::language::analyze_workspace("main.ifx", &sources)
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expected int"))
    );
    assert!(errors("if true { import child from \"./child.ifx\"; }").contains("module scope"));
}
#[test]
fn closing_imported_document_invalidates_importer() {
    let mut s = server();
    open(
        &mut s,
        "import child from \"./child.ifx\"; module x = child(\"x\");",
        1,
    );
    s.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///workspace/child.ifx","version":1,"text":"output value: Int = 1;"}}}));
    let closed=s.handle(json!({"method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///workspace/child.ifx"}}}));
    let parent = closed
        .iter()
        .find(|m| m["params"]["uri"] == "file:///workspace/test.ifx")
        .unwrap();
    assert!(
        !parent["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn completion_offers_schema_values_in_unfinished_string() {
    let source = "resource node = linode.instance(\"node\").region(\"";
    let mut s = server();
    open(&mut s, source, 1);
    let result = request(&mut s, "textDocument/completion", source, source.len());
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["label"] == "us-east"),
        "{result}"
    );
}
#[test]
fn cli_accepts_bare_filenames_and_refuses_nonregular_or_symlink_imports() {
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("main.ifx"),
        "resource x = memory.value(\"x\").value(1);",
    )
    .unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
            .current_dir(temp.path())
            .args(["check", "main.ifx"])
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    std::fs::write(
        temp.path().join("main.ifx"),
        "import child from \"./child.ifx\"; module x = child(\"x\");",
    )
    .unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        temp.path().join("child.ifx"),
        rustix::fs::Mode::RUSR,
    )
    .unwrap();
    let fifo = run();
    assert!(!fifo.status.success());
    assert!(String::from_utf8_lossy(&fifo.stderr).contains("regular file"));
    std::fs::write(temp.path().join("real.ifx"), "output value: Int = 1;").unwrap();
    std::os::unix::fs::symlink("real.ifx", temp.path().join("linked.ifx")).unwrap();
    std::fs::write(
        temp.path().join("main.ifx"),
        "import child from \"./linked.ifx\"; module x = child(\"x\");",
    )
    .unwrap();
    assert!(!run().status.success());
}
#[test]
fn configuration_can_read_its_own_typed_target_outputs() {
    let source = "resource node = linode.instance(\"node\").label(\"node\").region(\"us-east\").type(\"g6-nanode-1\").configure(\"c\", |host| { host.file(\"/ip\").content(host.ipv4).ensure(); });";
    let c = compile(source);
    let mut sim = Simulator::default();
    sim.apply(
        &c,
        &BTreeMap::from([("linode.instance:node".into(), json!({"ipv4":"192.0.2.1"}))]),
    )
    .unwrap();
    assert_eq!(
        sim.hosts["linode.instance:node"]
            .object("file", "/ip")
            .unwrap()["content"],
        "192.0.2.1"
    );
}
#[test]
fn collection_access_preserves_deferred_types_and_annotations() {
    let source = format!(
        "{LINODE}\nlet ips = [origin.ipv4]; let fields = {{address: origin.ipv4}}; output one: String = ips[0]; output two: String = fields.address; input empty: List[List[String]] = []; for item in empty {{ let first: String = item[0]; }}"
    );
    let c = compile(&source);
    assert_eq!(c.outputs["one"], c.outputs["two"]);
    let wrong = format!("{LINODE}\nlet bad: Map[Int] = {{address: origin.ipv4}};");
    assert!(errors(&wrong).contains("expected int"));
    assert!(errors("let overflow: Int = 18446744073709551615;").contains("64-bit"));
}
