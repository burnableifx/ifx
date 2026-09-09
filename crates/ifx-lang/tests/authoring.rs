use ifx_lang::{
    Compilation, authoring,
    language::Analysis,
    lsp::{Server, position},
    project,
    simulator::Simulator,
    syntax,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

fn analyze(source: &str) -> Analysis {
    authoring::analyze(
        "main",
        &BTreeMap::from([("main".into(), source.into())]),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}
fn compile(source: &str) -> Compilation {
    let a = analyze(source);
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
    a.compilation.expect("entry compiled")
}
fn rejects(source: &str, expected: &str) {
    let a = analyze(source);
    assert!(a.compilation.is_none());
    assert!(
        a.diagnostics.iter().any(|d| d.message.contains(expected)),
        "expected {expected}: {:?}",
        a.diagnostics
    );
}
fn examples() -> project::Snapshot {
    project::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/language/proposal"),
        &BTreeMap::new(),
    )
    .unwrap()
}
fn example(s: &project::Snapshot, file: &str, inputs: BTreeMap<String, Value>) -> Compilation {
    let id = s.paths.iter().find(|(_, p)| p.ends_with(file)).unwrap().0;
    let a = s.analyze_with_inputs(id, &inputs);
    assert!(a.diagnostics.is_empty(), "{file}: {:?}", a.diagnostics);
    a.compilation.unwrap()
}
#[test]
fn application_fleet_and_values_are_executable() {
    let s = examples();
    let app = example(
        &s,
        "01-application.ifx",
        BTreeMap::from([("name".into(), json!("custom"))]),
    );
    assert_eq!(app.program.resources.len(), 1);
    assert_eq!(app.configurations.len(), 1);
    let vm = &app.program.resources[0];
    assert_eq!(
        vm.urn.as_str(),
        r#"linode.instance:["production","web","vm"]"#
    );
    assert_eq!(vm.inputs["label"], "custom-web");
    assert_eq!(
        app.outputs["application"]["web"]["machine"]["$resource"],
        vm.urn.as_str()
    );
    assert!(app.outputs["website_url"].to_string().contains("$ref"));
    let fleet = example(&s, "02-environments.ifx", BTreeMap::new());
    assert_eq!(fleet.program.resources.len(), 2);
    let keys: Vec<_> = fleet
        .program
        .resources
        .iter()
        .map(|r| r.urn.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            r#"linode.instance:["fleet","production","vm"]"#,
            r#"linode.instance:["fleet","staging","vm"]"#
        ]
    );
    let values = example(&s, "04-values.ifx", BTreeMap::new());
    assert!(values.program.resources.is_empty());
    assert!(
        values.outputs["description"]
            .as_str()
            .unwrap()
            .contains("us-east")
    );
}
#[test]
fn lifecycle_preserves_convergence_drift_change_and_replacement() {
    let s = examples();
    let c = example(&s, "03-lifecycle.ifx", BTreeMap::new());
    let mut sim = Simulator::default();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let h = &sim.hosts[r#"memory.value:["worker"]"#];
    assert_eq!(h.records["initialized"], 1);
    assert_eq!(h.records["visited"], 2);
    assert_eq!(h.records["revision-changed"], 1);
    assert_eq!(h.records["directory-observed"], 2);
    let h = sim.hosts.get_mut(r#"memory.value:["worker"]"#).unwrap();
    h.remove_object("directory", "/srv/app");
    h.remove_object("file", "/etc/app-version");
    sim.apply(&c, &BTreeMap::new()).unwrap();
    let h = &sim.hosts[r#"memory.value:["worker"]"#];
    assert!(!h.exists("/srv/app"));
    assert!(h.exists("/etc/app-version"));
    assert_eq!(h.records["initialized"], 1);
    let changed = example(
        &s,
        "03-lifecycle.ifx",
        BTreeMap::from([("revision".into(), json!("v2"))]),
    );
    sim.apply(&changed, &BTreeMap::new()).unwrap();
    assert_eq!(
        sim.hosts[r#"memory.value:["worker"]"#].records["revision-changed"],
        2
    );
    sim.hosts
        .get_mut(r#"memory.value:["worker"]"#)
        .unwrap()
        .replace();
    sim.apply(&changed, &BTreeMap::new()).unwrap();
    assert!(sim.hosts[r#"memory.value:["worker"]"#].exists("/srv/app"));
    assert_eq!(sim.hosts[r#"memory.value:["worker"]"#].incarnation, 1);
}
const GRAPH: &str = r#"
use ifx::memory::Value;
pub struct Group { pub item: Value }
impl Group {
    pub fn new(label: String) -> Self {
        let item = Value::new().key("leaf").value(label);
        return Self { item: item };
    }
}
fn main() {
    let groups = {"west": "a", "east": "b"};
    for key, label in groups { let group = Group::new().key(key).label(label); }
}
"#;
#[test]
fn names_and_map_order_do_not_define_identity() {
    let c = compile(GRAPH);
    for source in [
        GRAPH.replace("Group", "Renamed"),
        GRAPH.replace("let group =", "let renamed ="),
        GRAPH.replace(r#""west": "a", "east": "b""#, r#""east": "b", "west": "a""#),
    ] {
        assert_eq!(
            serde_json::to_value(&c.program).unwrap(),
            serde_json::to_value(compile(&source).program).unwrap()
        );
    }
    let changed = compile(&GRAPH.replace(".key(\"leaf\")", ".key(\"changed\")"));
    assert_ne!(c.program.resources[0].urn, changed.program.resources[0].urn);
    rejects(
        &GRAPH.replace(".key(key)", ".key(\"same\")"),
        "duplicate constructor namespace",
    );
    rejects(&GRAPH.replace(".key(key)", ""), "requires .key");
    rejects(
        &GRAPH.replace(".label(label)", ""),
        "missing required parameter",
    );
    rejects(
        &GRAPH.replace(".label(label)", ".label(label).label(label)"),
        "more than once",
    );
    rejects(
        &GRAPH.replace("Group::new()", "Group::new(label)"),
        "positional constructor",
    );
}
#[test]
fn ordinary_calls_are_pure_and_builders_cannot_escape() {
    rejects(
        r#"use ifx::memory::Value; fn helper() { Value::new().key("x").value(1); } fn main() {}"#,
        "ordinary functions are pure",
    );
    rejects(
        r#"use ifx::memory::Value; fn main() { let xs = [Value::new().key("x")]; }"#,
        "pending builders",
    );
    rejects(
        r#"use ifx::memory::Value; fn main() { let xs = {"x": Value::new().key("x")}; }"#,
        "pending builders",
    );
    rejects(
        r#"use ifx::memory::Value; fn main() { let ty = Value; }"#,
        "pending builders",
    );
    rejects(
        r#"use ifx::memory::Value; struct Output { value: Value } fn main() -> Output { return Output { value: Value::new().key("x") }; }"#,
        "pending builders",
    );
    rejects(
        r#"struct Data {} impl Data { fn new() -> Self { return Self {}; } } fn main() { let d = Data::new().key("x"); }"#,
        "pure data constructors",
    );
    rejects(
        r#"struct Data {} impl Data { fn new() -> Self { return Self {}; } } fn main() { let d = Data::new(); d.name("x"); }"#,
        "unknown method",
    );
    rejects(
        r#"fn label(a: String = 1) -> String { return a; } fn main() { let name = label("override"); }"#,
        "expected",
    );
    rejects(
        r#"fn first(xs: List[String]) -> String { for x in xs { return x; } } fn main() {}"#,
        "expected",
    );
}
#[test]
fn typed_collections_defaults_and_associated_helpers_work() {
    let c = compile(
        r#"
struct Data { pub value: String = "default" }
impl Data { fn suffix(s: String) -> String { return s + "!"; } }
fn second(xs: List[String]) -> String { return xs[1]; }
fn lookup(xs: Map[String]) -> String { return xs["wanted"]; }
struct Output { pub result: String }
fn main() -> Output {
    let empty: List[String] = [];
    for s in empty { let same: String = s; }
    return Output { result: Data::suffix(second(["zero", "one"])) + lookup({"wanted": "two"}) };
}"#,
    );
    assert_eq!(c.outputs["result"], "one!two");
    let c = compile(
        r#"use ifx::linode::Instance; fn tags(xs: List[String]) -> List[String] { return xs; }
fn main() { Instance::new().key("web").label("web").region("us-east").type("g6-nanode-1").image("linode/debian12").tags(tags(["web"])); }"#,
    );
    assert_eq!(c.program.resources[0].inputs["tags"], json!(["web"]));
}
#[test]
fn modules_obey_visibility_and_imports_are_definition_only() {
    let sources=BTreeMap::from([("main".into(),"use library::Thing as Item; struct Output { pub result: String } fn main() -> Output { let data = Item::new(); return Output { result: data.value }; }".into()),("lib".into(),"pub struct Thing { pub value: String } impl Thing { pub fn new() -> Self { return Self {value: \"ok\"}; } } fn main() { let bad = 1; }".into())]);
    let imports = BTreeMap::from([(
        "main".into(),
        BTreeMap::from([("library".into(), "lib".into())]),
    )]);
    let a = authoring::analyze("main", &sources, &imports, &BTreeMap::new());
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
    assert_eq!(a.compilation.unwrap().outputs["result"], "ok");
    assert!(
        a.navigation
            .iter()
            .any(|n| n.source == "lib" && n.span.start > 30)
    );
    let private = BTreeMap::from([
        ("lib".into(), sources["lib"].replace("pub value", "value")),
        ("main".into(), sources["main"].clone()),
    ]);
    let a = authoring::analyze("main", &private, &imports, &BTreeMap::new());
    assert!(
        a.diagnostics
            .iter()
            .any(|d| d.message.contains("field is private"))
    );
    rejects(
        "struct Private {} pub fn expose() -> Private { return Private {}; } fn main() {}",
        "public interface",
    );
    let sources = BTreeMap::from([
        ("a".into(), "use b::B; pub struct A {}".into()),
        ("b".into(), "use a::A; pub struct B {}".into()),
    ]);
    let imports = BTreeMap::from([
        ("a".into(), BTreeMap::from([("b".into(), "b".into())])),
        ("b".into(), BTreeMap::from([("a".into(), "a".into())])),
    ]);
    let a = authoring::analyze("a", &sources, &imports, &BTreeMap::new());
    assert!(
        a.diagnostics
            .iter()
            .any(|d| d.message.contains("import cycle"))
    );
}
#[test]
fn recursion_and_partial_buffers_are_bounded() {
    rejects(
        "fn f() -> String { return f(); } fn main() {}",
        "recursive call",
    );
    rejects(
        "struct Node { next: Node } fn main() {}",
        "recursive struct",
    );
    let mut src = String::from("struct Leaf { value: String }\n");
    let mut last = String::from("Leaf");
    for n in 0..25 {
        let name = format!("Node{n}");
        src += &format!("struct {name} {{ left: {last}, right: {last} }}\n");
        last = name;
    }
    src += "fn main() {}";
    rejects(&src, "64 KiB");
    for (n, _) in GRAPH.char_indices() {
        let a = analyze(&GRAPH[..n]);
        assert!(a.diagnostics.len() <= 101);
    }
    assert!(syntax::parse(GRAPH).diagnostics.is_empty());
}
#[test]
fn lsp_completes_and_navigates_constructor_authoring() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/language/proposal")
        .canonicalize()
        .unwrap();
    let uri = format!("file://{}/01-application.ifx", root.display());
    let mut server = Server::default();
    let init=server.handle(json!({"id":1,"method":"initialize","params":{"rootUri":format!("file://{}",root.display())}}));
    assert!(
        init[0]["result"]["capabilities"]["semanticTokensProvider"]["full"]
            .as_bool()
            .unwrap()
    );
    let open = |text: &str, version: i32| json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"version":version,"text":text}}});
    let prefix = "use crate::application::Application;\nfn main() { let app = Application";
    for (n, tail, label) in [
        (1, "::", "new"),
        (2, "::new().", "key"),
        (3, "::new().key(\"x\").", "name"),
    ] {
        let text = format!("{prefix}{tail}");
        server.handle(open(&text, n));
        let out=server.handle(json!({"id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":uri},"position":position(&text,text.len())}}));
        assert!(
            out[0]["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["label"] == label),
            "{text}: {out:?}"
        );
    }
    let text = format!("{prefix}::new().key(\"x\").name(\"test\"); }}");
    let notifications = server.handle(open(&text, 4));
    assert!(
        notifications[0]["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{notifications:?}"
    );
    let out=server.handle(json!({"id":3,"method":"textDocument/definition","params":{"textDocument":{"uri":uri},"position":position(&text,prefix.len()+3)}}));
    assert!(
        out[0]["result"]["uri"]
            .as_str()
            .unwrap()
            .ends_with("/application.ifx")
    );
    let out=server.handle(json!({"id":4,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":uri}}}));
    assert!(!out[0]["result"]["data"].as_array().unwrap().is_empty());
    let app_uri = format!("file://{}/application.ifx", root.display());
    let original = std::fs::read_to_string(root.join("application.ifx")).unwrap();
    let changed = original
        .replace("new(name:", "new(title:")
        .replace(".name(name +", ".name(title +");
    let notices=server.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":app_uri,"version":1,"text":changed}}}));
    let importer = notices.iter().find(|n| n["params"]["uri"] == uri).unwrap();
    assert!(
        !importer["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "unsaved signature changes must invalidate the caller"
    );
    let notices = server.handle(
        json!({"method":"textDocument/didClose","params":{"textDocument":{"uri":app_uri}}}),
    );
    let importer = notices.iter().find(|n| n["params"]["uri"] == uri).unwrap();
    assert!(
        importer["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "closing restores the saved signature"
    );
}

#[test]
fn deferred_collection_elements_keep_types_through_helpers_and_captures() {
    let c = compile(
        r#"use ifx::linode::Instance;
fn first(xs: List[String]) -> String { return xs[0]; }
struct Output { pub result: String }
fn main() -> Output {
    let origin=Instance::new().key("origin").label("origin").region("us-east").type("g6-nanode-1").image("linode/debian12");
    let addresses=[origin.ipv4];
    let target=Instance::new().key("target").label("target").region("us-east").type("g6-nanode-1").image("linode/debian12")
        .configure("address", |host| { host.file("/etc/origin").content(addresses[0]).ensure(); });
    return Output { result: first(origin.ipv4s) };
}"#,
    );
    assert_eq!(
        c.outputs["result"],
        json!({"$ref":r#"linode.instance:["origin"]"#,"$path":"ipv4s.0"})
    );
    let mut sim = Simulator::default();
    sim.apply(
        &c,
        &BTreeMap::from([(
            r#"linode.instance:["origin"]"#.into(),
            json!({"ipv4":"192.0.2.8","ipv4s":["192.0.2.8"]}),
        )]),
    )
    .unwrap();
    assert_eq!(
        sim.hosts[r#"linode.instance:["target"]"#]
            .object("file", "/etc/origin")
            .unwrap()["content"],
        "192.0.2.8"
    );
}
#[test]
fn root_inputs_are_concrete_typed_and_optional_for_editor_checks() {
    let source = "struct Output { pub result: String } fn main(names: List[String]) -> Output { return Output { result: names[1] }; }";
    let sources = BTreeMap::from([("main".into(), source.into())]);
    assert!(
        authoring::check("main", &sources, &BTreeMap::new())
            .diagnostics
            .is_empty()
    );
    let inputs = BTreeMap::from([("names".into(), json!(["a", "b"]))]);
    let a = authoring::analyze("main", &sources, &BTreeMap::new(), &inputs);
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
    assert_eq!(a.compilation.unwrap().outputs["result"], "b");
    for inputs in [
        BTreeMap::from([("names".into(), json!({"$ref":"forged","$path":"value"}))]),
        BTreeMap::from([("names".into(), json!(["a", 1]))]),
        BTreeMap::from([("extra".into(), json!(true))]),
    ] {
        let a = authoring::analyze("main", &sources, &BTreeMap::new(), &inputs);
        assert!(a.compilation.is_none() && !a.diagnostics.is_empty());
    }
    let bad = format!("fn main() {{ let value = {}x; }}", "Type::".repeat(4000));
    assert!(!analyze(&bad).diagnostics.is_empty());
}
#[test]
fn cli_checks_libraries_and_loads_root_toml_without_echoing_invalid_values() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/language/proposal");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .args(["check"])
        .arg(root.join("shared/Ifx.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .arg("compile")
        .arg(root.join("Ifx.toml"))
        .arg("--inputs")
        .arg(root.join("production.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let c: Compilation = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(c.program.resources.len(), 1);
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("inputs.toml");
    std::fs::write(&input, "name = \"sentinel-private-value\" invalid\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .arg("check")
        .arg(root.join("Ifx.toml"))
        .arg("--inputs")
        .arg(input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid input TOML"));
    assert!(!stderr.contains("sentinel-private-value"));
}

#[test]
fn variable_keys_are_known_but_provider_outputs_cannot_choose_graph_shape() {
    let c = compile(
        r#"use ifx::memory::Value; fn main(name: String = "one") { Value::new().key("prefix-" + name).value(1); }"#,
    );
    assert_eq!(
        c.program.resources[0].urn.as_str(),
        r#"memory.value:["prefix-one"]"#
    );
    let base = r#"use ifx::linode::Instance; use ifx::memory::Value; fn main() {
let vm=Instance::new().key("vm").label("vm").region("us-east").type("g6-nanode-1").image("linode/debian12");
"#;
    rejects(
        &format!("{base} Value::new().key(vm.ipv4).value(1); }}"),
        "identity must be a known",
    );
    rejects(
        &format!("{base} if vm.ipv4 == \"x\" {{ Value::new().key(\"x\").value(1); }} }}"),
        "deferred comparison",
    );
    rejects(
        &format!(
            "{base} let other=Instance::new().key(\"other\").label(\"other\").region(\"us-east\").type(\"g6-nanode-1\").image(\"linode/debian12\").tags([vm.id]); }}"
        ),
        "expected string",
    );
}

#[test]
fn literal_keys_cannot_alias_nested_namespace_paths() {
    let source = GRAPH.replace(
        "fn main() {",
        r#"fn main() { Value::new().key("[\"east\",\"leaf\"]").value("literal");"#,
    );
    let c = compile(&source);
    assert_eq!(c.program.resources.len(), 3);
    let paths: Vec<Vec<String>> = c
        .program
        .resources
        .iter()
        .map(|r| {
            serde_json::from_str(r.urn.as_str().strip_prefix("memory.value:").unwrap()).unwrap()
        })
        .collect();
    assert!(paths.contains(&vec![r#"["east","leaf"]"#.into()]));
    assert!(paths.contains(&vec!["east".into(), "leaf".into()]));
}

#[test]
fn main_requires_named_outputs_while_helpers_keep_general_results() {
    for (ty, body) in [
        ("String", r#"return "value";"#),
        ("Int", "return 1;"),
        ("Bool", "return true;"),
        ("List[String]", r#"return ["value"];"#),
        ("Map[String]", r#"return {"name": "value"};"#),
        (
            "Value",
            r#"let vm = Value::new().key("vm").value(1); return vm;"#,
        ),
    ] {
        let source = format!("use ifx::memory::Value; fn main() -> {ty} {{ {body} }}");
        rejects(&source, "main must return Unit or a named output struct");
        let checked = authoring::check(
            "main",
            &BTreeMap::from([("main".into(), source)]),
            &BTreeMap::new(),
        );
        assert!(
            checked.diagnostics.iter().any(|d| d
                .message
                .contains("main must return Unit or a named output struct")),
            "editor accepted {ty}"
        );
    }
    for source in ["fn main() {}", "fn main() -> Unit { return; }"] {
        assert!(compile(source).outputs.is_empty());
    }
    let c = compile(
        r#"
use ifx::memory::Value;
struct Output { pub label: String, pub ports: List[Int], pub flags: Map[Bool], pub vm: Value }
fn label() -> String { return "web"; }
fn ports() -> List[Int] { return [80, 443]; }
fn flags() -> Map[Bool] { return {"enabled": true}; }
fn identity(vm: Value) -> Value { return vm; }
fn main() -> Output {
    let vm = Value::new().key("vm").value(1);
    return Output { label: label(), ports: ports(), flags: flags(), vm: identity(vm) };
}
"#,
    );
    assert_eq!(c.outputs["label"], "web");
    assert_eq!(c.outputs["ports"], json!([80, 443]));
    assert_eq!(c.outputs["flags"], json!({"enabled": true}));
    assert_eq!(
        c.outputs["vm"]["$resource"],
        c.program.resources[0].urn.as_str()
    );
    assert_eq!(c.program.resources.len(), 1);
}

#[test]
fn cli_and_lsp_report_the_same_main_result_contract() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("Ifx.toml"), "[language]\nedition = \"0.2-draft\"\n[package]\nname = \"root_outputs\"\nversion = \"0.1.0\"\nentry = \"main.ifx\"\n").unwrap();
    let source = "fn main() -> String { return \"website\"; }";
    let path = temp.path().join("main.ifx");
    std::fs::write(&path, source).unwrap();
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_ifx-lang"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!cli.status.success());
    let mut server = Server::default();
    server.handle(json!({"id":1,"method":"initialize","params":{"rootUri":format!("file://{}",temp.path().display())}}));
    let uri = format!("file://{}", path.display());
    let notices = server.handle(json!({"method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"version":1,"text":source}}}));
    let diagnostic = &notices[0]["params"]["diagnostics"][0];
    let message = diagnostic["message"].as_str().unwrap();
    assert!(message.contains("main must return Unit or a named output struct"));
    assert!(String::from_utf8_lossy(&cli.stderr).contains(message));
    assert_eq!(
        diagnostic["range"]["start"],
        json!({"line":0,"character":3})
    );
    let valid = "struct Output { pub website: String } fn main() -> Output { return Output { website: \"website\" }; }";
    let notices = server.handle(json!({"method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":valid}]}}));
    assert!(
        notices[0]["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
