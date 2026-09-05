use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context
            .config_optional("base")?
            .unwrap_or_else(|| "/tmp/ifx-example-local".to_string());
        let on = Connection::local();
        let directory = stack
            .host_file("directory")
            .on(on.clone())
            .path(&base)
            .directory(true)
            .add()?;
        let settings = stack
            .host_file("config")
            .on(on.clone())
            .path(format!("{base}/app.conf"))
            .content("environment=local\n")
            .depends_on(&directory)
            .add()?;
        let inventory = stack
            .host_file("checksum")
            .on(on.clone())
            .path(format!("{base}/app.conf.sha256"))
            .content(settings.sha256())
            .depends_on(&directory)
            .add()?;
        stack
            .check_exec("files-ready")
            .on(on)
            .command(format!(
                "test -s {base}/app.conf && test -s {base}/app.conf.sha256"
            ))
            .required(true)
            .depends_on(&inventory)
            .add()?;
        Ok(())
    })
}
