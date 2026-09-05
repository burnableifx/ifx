use std::process::ExitCode;

use ifx_program::memory;
use ifx_program::{Connection, Handle, IntoConcatPart as _, Stack, emit_program};
use serde_json::json;

fn declare_node(
    stack: &mut Stack,
    index: usize,
    base: &str,
    registry: &Handle<memory::Value>,
) -> anyhow::Result<Handle<memory::Value>> {
    let node = stack
        .memory_value(format!("vm-{index}"))
        .value(json!({"name": format!("node-{index}"), "port": 8000 + index}))
        .add()?;
    let content = ifx_program::concat!(
        "id=",
        node.id(),
        "\nport=",
        node.value().select::<i64>("port")?,
        "\nregistry=",
        registry.value(),
        "\n",
    );
    stack
        .host_file(format!("cfg-{index}"))
        .on(Connection::local())
        .path(format!("{base}/node-{index}.conf"))
        .content(content)
        .add()?;
    Ok(node)
}

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let nodes: usize = context.config_optional("nodes")?.unwrap_or(2);
        let directory = stack
            .host_file("dir")
            .on(Connection::local())
            .path(&base)
            .directory(true)
            .add()?;
        let registry = stack
            .memory_value("registry")
            .value("registry.internal:5000")
            .add()?;
        let nodes = (0..nodes)
            .map(|index| declare_node(stack, index, &base, &registry))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut inventory = Vec::new();
        for node in &nodes {
            inventory.push(node.id().into_concat_part()?);
            inventory.push("\n".into_concat_part()?);
        }
        stack
            .host_file("inventory")
            .on(Connection::local())
            .path(format!("{base}/inventory.txt"))
            .content(ifx_program::stack::concat(inventory))
            .depends_on(&directory)
            .add()?;
        Ok(())
    })
}
