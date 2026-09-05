use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let port: i64 = context.config("port")?;
        let on = Connection::local();
        let directory = stack
            .host_file("dir")
            .on(on.clone())
            .path(&base)
            .directory(true)
            .add()?;
        let index = stack
            .host_file("index")
            .on(on.clone())
            .path(format!("{base}/index.html"))
            .content("<h1>lab 4</h1>\n")
            .depends_on(&directory)
            .add()?;
        stack
            .check_exec("index-present")
            .on(on)
            .command(format!("test -s {base}/index.html"))
            .depends_on(&index)
            .add()?;
        stack
            .check_tcp("web-port")
            .host("127.0.0.1")
            .port(port)
            .timeout_secs(2)
            .depends_on(&index)
            .add()?;
        stack
            .check_http("web-http")
            .url(format!("http://127.0.0.1:{port}/index.html"))
            .expect_body("lab 4")
            .timeout_secs(2)
            .depends_on(&index)
            .add()?;
        Ok(())
    })
}
