use std::process::ExitCode;

use ifx_program::emit_program;
use ifx_program::{host, qemu};

const DEFAULT_IMAGE_URL: &str = "https://cloud.debian.org/images/cloud/trixie/20260712-2537/debian-13-genericcloud-amd64-20260712-2537.qcow2";
const DEFAULT_IMAGE_SHA256: &str =
    "2cab162ddebb1ef083cca8f8261f77c93ae70f98252aabfeb1d8a28c30b191b1";

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let lab_root = std::env::var("LAB").unwrap_or_else(|_| "/tmp/ifx-lab".to_string());
        let base: String = context
            .config_optional("base")?
            .unwrap_or_else(|| format!("{lab_root}/08"));
        let cache = std::env::var("IFX_QEMU_CACHE_DIR")
            .unwrap_or_else(|_| format!("{lab_root}/qemu-cache"));
        let memory_mb: i64 = context.config_optional("memory_mb")?.unwrap_or(512);
        let http_port: i64 = context.config_optional("http_port")?.unwrap_or(38_080);
        let image_url: String = context
            .config_optional("image_url")?
            .unwrap_or_else(|| DEFAULT_IMAGE_URL.to_string());
        let image_sha256: String = context
            .config_optional("image_sha256")?
            .unwrap_or_else(|| DEFAULT_IMAGE_SHA256.to_string());
        let qemu_dir = format!("{base}/qemu");

        let image = stack
            .qemu_image("debian-13")
            .url(image_url)
            .sha256(image_sha256)
            .dir(&cache)
            .add()?;
        let router_to_middle = stack
            .qemu_network("router-to-middle")
            .name("lab08-front")
            .dir(&qemu_dir)
            .add()?;
        let middle_to_leaf = stack
            .qemu_network("middle-to-leaf")
            .name("lab08-back")
            .dir(&qemu_dir)
            .add()?;

        let router = stack
            .qemu_instance("router")
            .dir(&qemu_dir)
            .image(image.path())
            .memory_mb(memory_mb)
            .disk_gb(4)
            .management(qemu::InstanceManagement::Direct)
            .egress(qemu::InstanceEgress::UserNat)
            .networks([router_to_middle.endpoint()])
            .port_forwards([(http_port.to_string(), 80)])
            .connect_timeout_secs(300)
            .add()?;
        let middle = stack
            .qemu_instance("middle")
            .dir(&qemu_dir)
            .image(image.path())
            .memory_mb(memory_mb)
            .disk_gb(4)
            .management(qemu::InstanceManagement::DirectThenVia)
            .management_via(router.connection())
            .egress(qemu::InstanceEgress::UserNat)
            .networks([router_to_middle.endpoint(), middle_to_leaf.endpoint()])
            .connect_timeout_secs(300)
            .add()?;
        let leaf = stack
            .qemu_instance("leaf")
            .dir(&qemu_dir)
            .image(image.path())
            .memory_mb(memory_mb)
            .disk_gb(4)
            .management(qemu::InstanceManagement::DirectThenVia)
            .management_via(middle.connection())
            .egress(qemu::InstanceEgress::UserNat)
            .networks([middle_to_leaf.endpoint()])
            .connect_timeout_secs(300)
            .add()?;

        let router_packages = stack
            .host_package("web-router")
            .on(router.connection())
            .names(["nginx", "curl"])
            .update_cache(true)
            .add()?;
        let middle_packages = stack
            .host_package("web-middle")
            .on(middle.connection())
            .names(["nginx", "curl"])
            .update_cache(true)
            .add()?;
        let leaf_packages = stack
            .host_package("web-leaf")
            .on(leaf.connection())
            .names(["nginx", "curl"])
            .update_cache(true)
            .add()?;

        let router_page = stack
            .host_file("page-router")
            .on(router.connection())
            .path("/var/www/html/index.html")
            .content(ifx_program::concat!(
                "<h1>router</h1>\n<p>front=",
                router.ips().at(0)?,
                "</p>\n",
            ))
            .privileged(true)
            .depends_on(&router_packages)
            .add()?;
        let middle_page = stack
            .host_file("page-middle")
            .on(middle.connection())
            .path("/var/www/html/index.html")
            .content(ifx_program::concat!(
                "<h1>middle</h1>\n<p>front=",
                middle.ips().at(0)?,
                " back=",
                middle.ips().at(1)?,
                "</p>\n",
            ))
            .privileged(true)
            .depends_on(&middle_packages)
            .add()?;
        let leaf_page = stack
            .host_file("page-leaf")
            .on(leaf.connection())
            .path("/var/www/html/index.html")
            .content(ifx_program::concat!(
                "<h1>leaf</h1>\n<p>back=",
                leaf.ips().at(0)?,
                "</p>\n",
            ))
            .privileged(true)
            .depends_on(&leaf_packages)
            .add()?;

        let router_service = stack
            .host_service("nginx-router")
            .on(router.connection())
            .name("nginx")
            .enabled(true)
            .state(host::ServiceState::Running)
            .triggered_by(&router_page)
            .add()?;
        let middle_service = stack
            .host_service("nginx-middle")
            .on(middle.connection())
            .name("nginx")
            .enabled(true)
            .state(host::ServiceState::Running)
            .triggered_by(&middle_page)
            .add()?;
        let leaf_service = stack
            .host_service("nginx-leaf")
            .on(leaf.connection())
            .name("nginx")
            .enabled(true)
            .state(host::ServiceState::Running)
            .triggered_by(&leaf_page)
            .add()?;

        stack
            .check_tcp("host-to-router")
            .host("127.0.0.1")
            .port(http_port)
            .required(true)
            .depends_on(&router_service)
            .add()?;
        stack
            .check_http("router-http")
            .url(format!("http://127.0.0.1:{http_port}/"))
            .expect_body("<h1>router</h1>")
            .required(true)
            .depends_on(&router_service)
            .add()?;
        stack
            .check_exec("router-to-middle")
            .on(router.connection())
            .command(ifx_program::concat!(
                "curl -fsS http://",
                middle.ips().at(0)?,
                "/ | grep -q '<h1>middle</h1>'",
            ))
            .required(true)
            .timeout_secs(20)
            .depends_on(&middle_service)
            .add()?;
        stack
            .check_exec("middle-to-leaf")
            .on(middle.connection())
            .command(ifx_program::concat!(
                "curl -fsS http://",
                leaf.ips().at(0)?,
                "/ | grep -q '<h1>leaf</h1>'",
            ))
            .required(true)
            .timeout_secs(20)
            .depends_on(&leaf_service)
            .add()?;
        stack
            .check_exec("leaf-to-middle")
            .on(leaf.connection())
            .command(ifx_program::concat!(
                "curl -fsS http://",
                middle.ips().at(1)?,
                "/ | grep -q '<h1>middle</h1>'",
            ))
            .required(true)
            .timeout_secs(20)
            .depends_on(&middle_service)
            .add()?;
        Ok(())
    })
}
