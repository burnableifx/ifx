use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let protect: bool = context.config_optional("protect")?.unwrap_or(true);
        let on = Connection::local();
        let directory = stack
            .host_file("dir")
            .on(on.clone())
            .path(&base)
            .directory(true)
            .add()?;
        let app = stack
            .host_file("app-config")
            .on(on.clone())
            .path(format!("{base}/app.conf"))
            .content("workers = 2\n")
            .depends_on(&directory)
            .add()?;
        stack
            .host_exec("init")
            .on(on.clone())
            .command(format!("echo initialised > {base}/init.done"))
            .creates(format!("{base}/init.done"))
            .depends_on(&directory)
            .add()?;
        stack
            .host_exec("reload")
            .on(on.clone())
            .command(format!("date +%s >> {base}/reloads.log"))
            .triggered_by(&app)
            .add()?;
        let keep = stack
            .host_file("keep")
            .on(on)
            .path(format!("{base}/keep.txt"))
            .content("do not lose me\n")
            .depends_on(&directory);
        if protect {
            keep.protect().add()?;
        } else {
            keep.add()?;
        }
        Ok(())
    })
}
