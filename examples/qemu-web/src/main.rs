use std::process::ExitCode;

use ifx_program::{Handle, emit_program};
use ifx_program::{host, qemu};

const IMAGE_URL: &str = "https://cloud.debian.org/images/cloud/trixie/20260712-2537/debian-13-genericcloud-amd64-20260712-2537.qcow2";
const IMAGE_SHA256: &str = "2cab162ddebb1ef083cca8f8261f77c93ae70f98252aabfeb1d8a28c30b191b1";

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let root =
            std::env::var("IFX_QEMU_DIR").unwrap_or_else(|_| "/tmp/ifx-example-qemu".to_string());
        let cache = std::env::var("IFX_QEMU_CACHE_DIR")
            .unwrap_or_else(|_| "/tmp/ifx-qemu-cache".to_string());
        let memory_mb: i64 = context.config_optional("memory_mb")?.unwrap_or(512);
        let http_port: i64 = context.config_optional("http_port")?.unwrap_or(38_180);
        let image = stack
            .qemu_image("debian")
            .url(IMAGE_URL)
            .sha256(IMAGE_SHA256)
            .dir(&cache)
            .add()?;
        let lan = stack
            .qemu_network("lan")
            .name("qemu-web")
            .dir(&root)
            .add()?;
        let names = ["web-a", "web-b"];
        let mut vms: Vec<Handle<qemu::Instance>> = Vec::new();
        let mut services: Vec<Handle<host::Service>> = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let port = http_port + index as i64;
            let vm = stack
                .qemu_instance(*name)
                .dir(&root)
                .image(image.path())
                .memory_mb(memory_mb)
                .disk_gb(4)
                .networks([lan.endpoint()])
                .port_forwards([(port.to_string(), 80)])
                .add()?;
            let packages = stack
                .host_package(format!("packages-{name}"))
                .on(vm.connection())
                .names(["nginx", "curl"])
                .update_cache(true)
                .add()?;
            let page = stack
                .host_file(format!("page-{name}"))
                .on(vm.connection())
                .path("/var/www/html/index.html")
                .content(format!("<h1>{name}</h1>\n"))
                .privileged(true)
                .depends_on(&packages)
                .add()?;
            let service = stack
                .host_service(format!("nginx-{name}"))
                .on(vm.connection())
                .name("nginx")
                .enabled(true)
                .state(host::ServiceState::Running)
                .triggered_by(&page)
                .add()?;
            stack
                .check_http(format!("http-{name}"))
                .url(format!("http://127.0.0.1:{port}/"))
                .expect_body(format!("<h1>{name}</h1>"))
                .required(true)
                .depends_on(&service)
                .add()?;
            vms.push(vm);
            services.push(service);
        }
        for (index, vm) in vms.iter().enumerate() {
            let peer_index = (index + 1) % vms.len();
            stack
                .check_exec(format!("{}-to-{}", names[index], names[peer_index]))
                .on(vm.connection())
                .command(ifx_program::concat!(
                    "curl -fsS http://",
                    vms[peer_index].ips().at(0)?,
                    "/ | grep -q ",
                    names[peer_index],
                ))
                .required(true)
                .timeout_secs(20)
                .depends_on(&services[peer_index])
                .add()?;
        }
        Ok(())
    })
}
