use ifx_program::{Connection, Stack};
use serde_json::json;

#[test]
fn generated_builders_emit_dependency_references() {
    let mut stack = Stack::new();
    let on = Connection::local();

    let directory = stack
        .host_file("directory")
        .on(on.clone())
        .path("/tmp/ifx-fast")
        .directory(true)
        .add()
        .expect("directory declaration should be valid");
    let file = stack
        .host_file("file")
        .on(on)
        .path("/tmp/ifx-fast/value")
        .content(directory.sha256())
        .depends_on(&directory)
        .add()
        .expect("file declaration should be valid");

    assert_eq!(stack.program().resources.len(), 2);
    assert_eq!(
        stack.program().resources[1].depends_on,
        [directory.urn().clone()]
    );
    assert_eq!(
        stack.program().resources[1].inputs["content"],
        json!({"$ref": "host.file:directory", "$path": "sha256"})
    );
    assert_eq!(file.urn().as_str(), "host.file:file");
}

#[test]
fn deferred_concat_and_index_preserve_output_paths() {
    let ips: ifx_program::Input<Vec<String>> =
        ifx_program::Input::reference(ifx_program::Urn::new("qemu.instance", "node"), "ips");
    let first = ips.at(0).expect("an output reference can select an index");
    let value =
        (|| -> anyhow::Result<_> { Ok(ifx_program::concat!("http://", first, "/health")) })()
            .expect("concat should build");

    assert_eq!(
        serde_json::to_value(value).unwrap(),
        json!({"$concat": ["http://", {"$ref": "qemu.instance:node", "$path": "ips.0"}, "/health"]})
    );
}
