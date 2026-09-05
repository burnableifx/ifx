use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use super::{Cmd, Output, Transport};
use crate::model::Connection;

pub struct Local {
    conn: Connection,
}

impl Local {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl Transport for Local {
    fn connection(&self) -> &Connection {
        &self.conn
    }

    async fn exec(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        let line = cmd.wrapped(self.conn.sudo());
        tracing::debug!(target: "ifx::transport", conn = "local", %line, "exec");
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(data) = &cmd.stdin {
            let mut stdin = child.stdin.take().expect("piped");
            let data = data.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&data).await;
                let _ = stdin.shutdown().await;
            });
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}
