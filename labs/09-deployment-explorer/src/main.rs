use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let port: i64 = context.config("port")?;
        let version: String = context.config("release")?;
        let on = Connection::local();
        let release = stack
            .memory_value("release")
            .value(version.as_str())
            .add()?;
        let site = stack
            .host_file("site")
            .on(on.clone())
            .path(format!("{base}/site"))
            .directory(true)
            .add()?;
        let page = stack
            .host_file("page")
            .on(on.clone())
            .path(format!("{base}/site/index.html"))
            .content(ifx_program::concat!(
                "<h1>ifx tutorial</h1>\n<p>release ",
                release.value(),
                " is healthy</p>\n",
            ))
            .mode("0644")
            .depends_on(&site)
            .add()?;
        let metadata = stack
            .host_file("metadata")
            .on(on.clone())
            .path(format!("{base}/site/release.txt"))
            .content(ifx_program::concat!(release.value(), "\n"))
            .mode("0644")
            .depends_on(&site)
            .add()?;
        stack
            .check_exec("files-ready")
            .on(on)
            .command(format!(
                "test -s {base}/site/index.html && test -s {base}/site/release.txt"
            ))
            .depends_on(&page)
            .depends_on(&metadata)
            .add()?;
        stack
            .check_tcp("web-port")
            .host("127.0.0.1")
            .port(port)
            .timeout_secs(2)
            .depends_on(&page)
            .add()?;
        stack
            .check_http("web-page")
            .url(format!("http://127.0.0.1:{port}/index.html"))
            .expect_body(format!("release {version}"))
            .timeout_secs(2)
            .depends_on(&page)
            .add()?;
        Ok(())
    })
}
