use std::process::ExitCode;

use ifx_program::linode;
use ifx_program::{Handle, Stack, emit_program, secret};

fn web(
    stack: &mut Stack,
    index: usize,
    stack_name: &str,
    region: &str,
    key: &str,
    root_pass: &str,
) -> anyhow::Result<Handle<linode::Instance>> {
    let vm = stack
        .linode_instance(format!("web-{index}"))
        .label(format!("{stack_name}-web-{index}"))
        .region(region)
        .r#type(linode::InstanceType::G6Nanode1)
        .image(linode::InstanceImage::LinodeDebian12)
        .authorized_keys([key])
        .root_pass(secret(root_pass))
        .tags(["web", stack_name])
        .connect_timeout_secs(300)
        .add()?;
    let nginx = stack
        .host_package(format!("nginx-{index}"))
        .on(vm.connection())
        .names(["nginx"])
        .update_cache(true)
        .add()?;
    let site = stack
        .host_file(format!("site-{index}"))
        .on(vm.connection())
        .path("/var/www/html/index.html")
        .content(ifx_program::concat!(
            "<h1>",
            vm.label(),
            format!(" in {region}</h1>\n"),
        ))
        .mode("0644")
        .depends_on(&nginx)
        .add()?;
    stack
        .host_service(format!("nginx-{index}"))
        .on(vm.connection())
        .name("nginx")
        .enabled(true)
        .state(ifx_program::host::ServiceState::Running)
        .triggered_by(&site)
        .add()?;
    stack
        .check_http(format!("web-{index}-http"))
        .url(ifx_program::concat!("http://", vm.ipv4(), "/"))
        .expect_body("<h1>")
        .add()?;
    stack
        .check_exec(format!("web-{index}-nginx"))
        .on(vm.connection())
        .command("systemctl is-active nginx")
        .add()?;
    Ok(vm)
}

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let nodes: usize = context.config_optional("nodes")?.unwrap_or(2);
        let region: String = context
            .config_optional("region")?
            .unwrap_or_else(|| "us-east".to_string());
        let root_pass: String = context.config("root_pass")?;
        let key = std::fs::read_to_string("id_ed25519.pub")?;
        let vms = (0..nodes)
            .map(|index| {
                web(
                    stack,
                    index,
                    context.stack(),
                    &region,
                    key.trim(),
                    &root_pass,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        stack
            .linode_firewall("fw")
            .label(format!("{}-web", context.stack()))
            .linodes(vms.iter().map(|vm| vm.id()))
            .inbound([
                linode::Rule::allow("ssh", linode::RuleProtocol::Tcp, "22"),
                linode::Rule::allow("http", linode::RuleProtocol::Tcp, "80,443"),
            ])
            .add()?;
        Ok(())
    })
}
