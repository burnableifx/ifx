use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let on = Connection::local();

        let directory = stack
            .host_file("dir")
            .on(on.clone())
            .path(&base)
            .directory(true)
            .add()?;

        stack
            .host_file("greeting")
            .on(on.clone())
            .path(format!("{base}/greeting.txt"))
            .content("hello from ifx\n")
            .mode("0644")
            .depends_on(&directory)
            .add()?;

        stack
            .host_file("secret")
            .on(on)
            .path(format!("{base}/secret.txt"))
            .content("s3cret\n")
            .mode("0600")
            .depends_on(&directory)
            .add()?;

        Ok(())
    })
}
