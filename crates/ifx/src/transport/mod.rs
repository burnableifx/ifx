//! How commands and files reach a target: locally, over SSH, or (later) into a
//! container or network namespace. Host-scoped providers are written once against
//! [`Transport`] and work on every target.

mod local;
mod ssh;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::model::Connection;
pub use local::Local;
pub use ssh::Ssh;
pub(crate) use ssh::{
    activation_pending as ssh_activation_pending,
    clear_activation_pending as clear_ssh_activation_pending,
    close_activation as close_ssh_activation,
    mark_activation_pending as mark_ssh_activation_pending,
};

/// A command to run on the target. Executed through `sh -c` on the far side so the
/// same escaping works locally and over SSH.
#[derive(Clone, Debug, Default)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
    /// Request `sudo -n` when the connection has `sudo` enabled.
    pub privileged: bool,
    /// Run through the shell as-is instead of escaping `program`/`args`.
    pub raw: Option<String>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, it: I) -> Self {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    /// A raw shell snippet, run verbatim via `sh -c`.
    pub fn sh(script: impl Into<String>) -> Self {
        Self {
            raw: Some(script.into()),
            ..Default::default()
        }
    }

    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn privileged(mut self) -> Self {
        self.privileged = true;
        self
    }

    /// Render as a single shell line (before any sudo wrapping).
    pub fn shell_line(&self) -> String {
        let mut line = String::new();
        for (k, v) in &self.env {
            line.push_str(&format!("{k}={} ", shell_escape::unix::escape(v.into())));
        }
        match &self.raw {
            Some(raw) => line.push_str(raw),
            None => {
                line.push_str(&shell_escape::unix::escape((&self.program).into()));
                for a in &self.args {
                    line.push(' ');
                    line.push_str(&shell_escape::unix::escape(a.into()));
                }
            }
        }
        line
    }

    /// Full line including `sudo -n` if requested and permitted.
    pub fn wrapped(&self, sudo_allowed: bool) -> String {
        let line = self.shell_line();
        if self.privileged && sudo_allowed {
            format!("sudo -n sh -c {}", shell_escape::unix::escape(line.into()))
        } else {
            line
        }
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Error if non-zero, with stderr in the message.
    pub fn ok(self, what: &str) -> anyhow::Result<Self> {
        if self.success() {
            Ok(self)
        } else {
            anyhow::bail!(
                "{what} failed (exit {}): {}",
                self.status,
                self.stderr_str().trim()
            )
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    fn connection(&self) -> &Connection;

    /// Whether this transport owns access that must be closed after an execution.
    fn execution_scoped(&self) -> bool {
        false
    }

    /// Close execution-scoped access. Persistent transports have nothing to do.
    async fn finish(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Wait until the target accepts commands (fresh VMs), then run.
    async fn exec(&self, cmd: &Cmd) -> anyhow::Result<Output>;

    /// `None` when the file does not exist.
    async fn read_file(&self, path: &str, privileged: bool) -> anyhow::Result<Option<Vec<u8>>> {
        let script = format!(
            "p={}; if [ -e \"$p\" ]; then base64 < \"$p\"; else exit 44; fi",
            shell_escape::unix::escape(path.into())
        );
        let mut cmd = Cmd::sh(script);
        cmd.privileged = privileged;
        let out = self.exec(&cmd).await?;
        match out.status {
            0 => {
                use base64::Engine;
                let text: String = out
                    .stdout_str()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                Ok(Some(
                    base64::engine::general_purpose::STANDARD.decode(text)?,
                ))
            }
            44 => Ok(None),
            _ => Err(anyhow::anyhow!("read {path}: {}", out.stderr_str().trim())),
        }
    }

    /// Atomic write: temp file in the same directory, chmod/chown, rename.
    async fn write_file(&self, path: &str, data: &[u8], opts: &WriteOpts) -> anyhow::Result<()> {
        let p = shell_escape::unix::escape(path.into());
        let mut script = format!(
            "p={p}; d=$(dirname \"$p\"); t=$(mktemp \"$d/.ifx.XXXXXX\") || exit 1; \
             cat > \"$t\" || exit 1;"
        );
        if let Some(m) = &opts.mode {
            script.push_str(&format!(
                " chmod {} \"$t\" || exit 1;",
                shell_escape::unix::escape(m.into())
            ));
        }
        if opts.owner.is_some() || opts.group.is_some() {
            let spec = format!(
                "{}:{}",
                opts.owner.clone().unwrap_or_default(),
                opts.group.clone().unwrap_or_default()
            );
            script.push_str(&format!(
                " chown {} \"$t\" || exit 1;",
                shell_escape::unix::escape(spec.into())
            ));
        }
        script.push_str(" mv -f \"$t\" \"$p\"");
        let mut cmd = Cmd::sh(script).stdin(data.to_vec());
        cmd.privileged = opts.privileged;
        self.exec(&cmd).await?.ok(&format!("write {path}"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct WriteOpts {
    pub mode: Option<String>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub privileged: bool,
}

/// Caches one transport per distinct connection so SSH sessions are reused.
#[derive(Default)]
pub struct TransportPool {
    inner: Mutex<HashMap<Connection, Arc<dyn Transport>>>,
    /// Directory for SSH control sockets.
    control_dir: Option<PathBuf>,
}

impl TransportPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_control_dir(dir: PathBuf) -> Self {
        Self {
            inner: Mutex::default(),
            control_dir: Some(dir),
        }
    }

    pub async fn get(&self, conn: &Connection) -> Arc<dyn Transport> {
        let mut map = self.inner.lock().await;
        if let Some(t) = map.get(conn) {
            return t.clone();
        }
        let t: Arc<dyn Transport> = match conn {
            Connection::Unavailable { .. } => Arc::new(Unavailable::new(conn.clone())),
            Connection::Local { .. } => Arc::new(Local::new(conn.clone())),
            Connection::Ssh { .. } => Arc::new(Ssh::new(conn.clone(), self.control_dir.clone())),
        };
        map.insert(conn.clone(), t.clone());
        t
    }

    /// Parse a connection value out of inputs and fetch its transport.
    pub async fn from_value(&self, v: &serde_json::Value) -> anyhow::Result<Arc<dyn Transport>> {
        let conn: Connection = serde_json::from_value(v.clone())
            .map_err(|e| anyhow::anyhow!("invalid connection {v}: {e}"))?;
        Ok(self.get(&conn).await)
    }

    /// Close and evict every execution-scoped transport so the next run starts sealed.
    pub async fn finish_execution(&self) -> anyhow::Result<()> {
        let transports = {
            let map = self.inner.lock().await;
            map.iter()
                .filter(|(_, transport)| transport.execution_scoped())
                .map(|(connection, transport)| (connection.clone(), transport.clone()))
                .collect::<Vec<_>>()
        };
        let mut errors = Vec::new();
        let mut finished = Vec::new();
        for (connection, transport) in transports {
            if let Err(error) = transport.finish().await {
                errors.push(format!("{}: {error:#}", transport.connection().label()));
            } else {
                finished.push((connection, transport));
            }
        }
        if !finished.is_empty() {
            let mut map = self.inner.lock().await;
            for (connection, transport) in finished {
                if map
                    .get(&connection)
                    .is_some_and(|current| Arc::ptr_eq(current, &transport))
                {
                    map.remove(&connection);
                }
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "closing execution-scoped management:\n  {}",
            errors.join("\n  ")
        );
        Ok(())
    }

    /// Close and evict one execution-scoped transport before its provider disappears.
    pub async fn finish_connection(&self, connection: &Connection) -> anyhow::Result<()> {
        let transport = self.inner.lock().await.get(connection).cloned();
        let Some(transport) = transport else {
            return Ok(());
        };
        if !transport.execution_scoped() {
            return Ok(());
        }
        transport.finish().await?;
        let mut map = self.inner.lock().await;
        if map
            .get(connection)
            .is_some_and(|current| Arc::ptr_eq(current, &transport))
        {
            map.remove(connection);
        }
        Ok(())
    }
}

struct Unavailable {
    conn: Connection,
}

impl Unavailable {
    fn new(conn: Connection) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl Transport for Unavailable {
    fn connection(&self) -> &Connection {
        &self.conn
    }

    async fn exec(&self, _cmd: &Cmd) -> anyhow::Result<Output> {
        let Connection::Unavailable { reason } = &self.conn else {
            unreachable!("Unavailable transport requires an unavailable connection")
        };
        anyhow::bail!("management connection unavailable: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct RetryableCleanup {
        connection: Connection,
        fail: AtomicBool,
    }

    #[async_trait]
    impl Transport for RetryableCleanup {
        fn connection(&self) -> &Connection {
            &self.connection
        }

        fn execution_scoped(&self) -> bool {
            true
        }

        async fn finish(&self) -> anyhow::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("transient cleanup failure")
            }
            Ok(())
        }

        async fn exec(&self, _cmd: &Cmd) -> anyhow::Result<Output> {
            unreachable!()
        }
    }

    #[test]
    fn shell_line_escapes() {
        let c = Cmd::new("echo").arg("a b").arg("it's").env("X", "1 2");
        assert_eq!(c.shell_line(), "X='1 2' echo 'a b' 'it'\\''s'");
        assert_eq!(c.wrapped(false), c.shell_line());
        assert!(
            c.clone()
                .privileged()
                .wrapped(true)
                .starts_with("sudo -n sh -c ")
        );
    }

    #[tokio::test]
    async fn local_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = TransportPool::new();
        let t = pool.get(&Connection::local()).await;
        let path = dir.path().join("f.txt");
        let ps = path.to_str().unwrap();
        assert_eq!(t.read_file(ps, false).await.unwrap(), None);
        t.write_file(
            ps,
            b"hello\n",
            &WriteOpts {
                mode: Some("0600".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            t.read_file(ps, false).await.unwrap().as_deref(),
            Some(&b"hello\n"[..])
        );
        let out = t.exec(&Cmd::new("cat").arg(ps)).await.unwrap();
        assert_eq!(out.stdout_str(), "hello\n");
        let out = t.exec(&Cmd::sh("exit 3")).await.unwrap();
        assert_eq!(out.status, 3);
    }

    #[tokio::test]
    async fn failed_execution_cleanup_remains_retryable() {
        let pool = TransportPool::new();
        let connection = Connection::ssh("127.0.0.1", None);
        let transport = Arc::new(RetryableCleanup {
            connection: connection.clone(),
            fail: AtomicBool::new(true),
        });
        pool.inner
            .lock()
            .await
            .insert(connection.clone(), transport.clone());

        assert!(pool.finish_execution().await.is_err());
        assert!(pool.inner.lock().await.contains_key(&connection));

        transport.fail.store(false, Ordering::SeqCst);
        pool.finish_execution().await.unwrap();
        assert!(!pool.inner.lock().await.contains_key(&connection));
    }
}
