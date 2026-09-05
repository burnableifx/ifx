//! JSON file store: resources only. Runs, events and health are accepted and dropped,
//! so `ifx` works without a database but `ifxd` has nothing to show.

use std::path::PathBuf;
use std::{fs::File, os::fd::AsRawFd as _};

use anyhow::Context as _;
use async_trait::async_trait;
use serde_json::Value;

use super::{EventRecord, HealthRecord, Result, RunKind, RunRecord, Store};
use crate::model::Urn;
use crate::state::State;

pub struct FileStore {
    path: PathBuf,
}

struct FileStoreLock(File);

impl FileStoreLock {
    fn acquire(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        let lock_path = path.with_file_name(lock_name);
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening state lock {}", lock_path.display()))?;
        // SAFETY: flock only reads the valid file descriptor and does not retain a
        // pointer. The descriptor remains owned by Self until Drop unlocks it.
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking state file {}", path.display()));
        }
        Ok(Self(file))
    }
}

impl Drop for FileStoreLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid for the duration of this call.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    async fn lock(&self) -> Result<FileStoreLock> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || FileStoreLock::acquire(&path))
            .await
            .context("state file lock task failed")?
    }
}

#[async_trait]
impl Store for FileStore {
    fn describe(&self) -> String {
        format!("file://{}", self.path.display())
    }

    async fn load_state(&self, stack: &str) -> Result<State> {
        let _lock = self.lock().await?;
        let mut s = State::load(&self.path)?;
        s.stack = stack.to_string();
        Ok(s)
    }

    async fn save_state(&self, state: &State) -> Result<()> {
        let _lock = self.lock().await?;
        let current = State::load(&self.path)?;
        anyhow::ensure!(
            current.pending_replacements == state.pending_replacements,
            "refusing to add, remove, or rewrite a replacement journal through a general state save"
        );
        for urn in current.pending_replacements.keys() {
            anyhow::ensure!(
                current.resources.get(urn) == state.resources.get(urn),
                "refusing to overwrite state for {urn} while its replacement journal guards it"
            );
        }
        state.save(&self.path)
    }

    async fn prepare_replacement(&self, state: &State, urn: &Urn) -> Result<()> {
        let _lock = self.lock().await?;
        let current = State::load(&self.path)?;
        let replacement = state
            .pending_replacements
            .get(urn)
            .with_context(|| format!("preparing missing replacement intent for {urn}"))?;
        anyhow::ensure!(
            !current.pending_replacements.contains_key(urn),
            "replacement journal already exists for {urn}"
        );
        let mut expected_pending = state.pending_replacements.clone();
        expected_pending.remove(urn);
        anyhow::ensure!(
            current.pending_replacements == expected_pending,
            "replacement journals changed before preparing {urn}"
        );
        if replacement.old_in_state {
            anyhow::ensure!(
                current.resources.get(urn) == Some(&replacement.old)
                    && state.resources.get(urn) == Some(&replacement.old),
                "resource changed before replacement preparation for {urn}"
            );
        } else {
            anyhow::ensure!(
                !current.resources.contains_key(urn) && !state.resources.contains_key(urn),
                "unmanaged resource state appeared before replacement preparation for {urn}"
            );
        }
        state.save(&self.path)
    }

    async fn reauthorize_replacement(
        &self,
        state: &State,
        urn: &Urn,
        previous_operation_id: &str,
    ) -> Result<()> {
        let _lock = self.lock().await?;
        let current = State::load(&self.path)?;
        let previous = current.pending_replacements.get(urn).with_context(|| {
            format!("replacement journal disappeared before reauthorizing {urn}")
        })?;
        anyhow::ensure!(
            previous.operation_id == previous_operation_id,
            "replacement journal operation changed before reauthorizing {urn}"
        );
        let next = state
            .pending_replacements
            .get(urn)
            .with_context(|| format!("reauthorizing missing replacement intent for {urn}"))?;
        anyhow::ensure!(
            next.operation_id != previous_operation_id,
            "replacement approval identity for {urn} did not change"
        );
        let mut expected = state.pending_replacements.clone();
        expected.insert(urn.clone(), previous.clone());
        anyhow::ensure!(
            current.pending_replacements == expected && current.resources == state.resources,
            "state changed before reauthorizing replacement for {urn}"
        );
        state.save(&self.path)
    }

    async fn commit_replacement(&self, state: &State, urn: &Urn, operation_id: &str) -> Result<()> {
        let _lock = self.lock().await?;
        let current = State::load(&self.path)?;
        let replacement = current
            .pending_replacements
            .get(urn)
            .with_context(|| format!("replacement journal disappeared before committing {urn}"))?;
        anyhow::ensure!(
            replacement.operation_id == operation_id,
            "replacement journal operation changed before committing {urn}"
        );
        anyhow::ensure!(
            !state.pending_replacements.contains_key(urn) && state.resources.contains_key(urn),
            "successor state for {urn} is incomplete"
        );
        let mut expected_pending = state.pending_replacements.clone();
        expected_pending.insert(urn.clone(), replacement.clone());
        anyhow::ensure!(
            current.pending_replacements == expected_pending,
            "other replacement journals changed before committing {urn}"
        );
        let mut expected_resources = state.resources.clone();
        if replacement.old_in_state {
            expected_resources.insert(urn.clone(), replacement.old.clone());
        } else {
            expected_resources.remove(urn);
        }
        anyhow::ensure!(
            current.resources == expected_resources,
            "resource state changed before committing replacement for {urn}"
        );
        state.save(&self.path)
    }

    async fn abort_replacement(&self, state: &State, urn: &Urn, operation_id: &str) -> Result<()> {
        let _lock = self.lock().await?;
        let current = State::load(&self.path)?;
        let replacement = current
            .pending_replacements
            .get(urn)
            .with_context(|| format!("replacement journal disappeared before aborting {urn}"))?;
        anyhow::ensure!(
            replacement.operation_id == operation_id,
            "replacement journal operation changed before aborting {urn}"
        );
        anyhow::ensure!(
            !state.pending_replacements.contains_key(urn),
            "replacement journal for {urn} remains in rollback state"
        );
        if replacement.old_in_state {
            anyhow::ensure!(
                state.resources.get(urn) == Some(&replacement.old),
                "rollback state for {urn} does not restore the previous resource"
            );
        } else {
            anyhow::ensure!(
                !state.resources.contains_key(urn),
                "rollback state for unmanaged {urn} unexpectedly contains a resource"
            );
        }
        let mut expected_pending = state.pending_replacements.clone();
        expected_pending.insert(urn.clone(), replacement.clone());
        anyhow::ensure!(
            current.pending_replacements == expected_pending,
            "other replacement journals changed before aborting {urn}"
        );
        let mut expected_resources = state.resources.clone();
        if replacement.old_in_state {
            expected_resources.insert(urn.clone(), replacement.old.clone());
        }
        anyhow::ensure!(
            current.resources == expected_resources,
            "resource state changed before aborting replacement for {urn}"
        );
        state.save(&self.path)
    }

    async fn begin_run(&self, _stack: &str, _kind: RunKind) -> Result<String> {
        Ok(String::new())
    }
    async fn finish_run(
        &self,
        _run_id: &str,
        _ok: bool,
        _summary: Value,
        _error: Option<String>,
    ) -> Result<()> {
        Ok(())
    }
    async fn runs(&self, _stack: &str, _limit: usize) -> Result<Vec<RunRecord>> {
        Ok(vec![])
    }
    async fn record_event(&self, _ev: &EventRecord) -> Result<()> {
        Ok(())
    }
    async fn events(&self, _run_id: &str) -> Result<Vec<EventRecord>> {
        Ok(vec![])
    }
    async fn record_health(&self, _h: &HealthRecord) -> Result<()> {
        Ok(())
    }
    async fn latest_health(&self, _stack: &str) -> Result<Vec<HealthRecord>> {
        Ok(vec![])
    }
    async fn health_history(
        &self,
        _stack: &str,
        _urn: &Urn,
        _limit: usize,
    ) -> Result<Vec<HealthRecord>> {
        Ok(vec![])
    }
    async fn stacks(&self) -> Result<Vec<String>> {
        let s = State::load(&self.path)?;
        Ok(if s.resources.is_empty() {
            vec![]
        } else {
            vec![s.stack]
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::state::{Entry, PendingReplacement};

    fn state(stack: &str, value: i64) -> State {
        let mut state = State {
            stack: stack.into(),
            ..State::default()
        };
        state.upsert(
            Urn::new("memory.value", "value"),
            Entry {
                id: Some(format!("value-{value}")),
                inputs: json!({"value": value}),
                outputs: json!({"value": value}),
                depends_on: vec![],
                protect: false,
            },
        );
        state
    }

    #[tokio::test]
    async fn concurrent_writers_leave_one_complete_durable_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let store = Arc::new(FileStore::new(&path));
        let first = state("stack", 1);
        let second = state("stack", 2);
        let (first_result, second_result) =
            tokio::join!(store.save_state(&first), store.save_state(&second),);
        first_result.unwrap();
        second_result.unwrap();

        let loaded = store.load_state("stack").await.unwrap();
        let value = loaded.resources[&Urn::new("memory.value", "value")].inputs["value"]
            .as_i64()
            .unwrap();
        assert!(matches!(value, 1 | 2));
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp-"))
        );
    }

    #[tokio::test]
    async fn competing_replacement_prepares_use_compare_and_swap() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::new(directory.path().join("state.json")));
        let base = state("stack", 1);
        store.save_state(&base).await.unwrap();
        let urn = Urn::new("memory.value", "value");
        let pending = |operation_id: &str| PendingReplacement {
            operation_id: operation_id.into(),
            execution_run_id: "run".into(),
            revision: "revision".into(),
            intent_digest: "intent".into(),
            old: base.resources[&urn].clone(),
            old_in_state: true,
            desired_inputs: json!({"value": 2}),
            depends_on: Vec::new(),
            protect: false,
            approval_fingerprints: std::collections::BTreeMap::new(),
            prepared_at: chrono::Utc::now(),
        };
        let mut first = base.clone();
        first
            .pending_replacements
            .insert(urn.clone(), pending("operation-1"));
        let mut second = base.clone();
        second
            .pending_replacements
            .insert(urn.clone(), pending("operation-2"));

        let (first_result, second_result) = tokio::join!(
            store.prepare_replacement(&first, &urn),
            store.prepare_replacement(&second, &urn),
        );
        assert_ne!(first_result.is_ok(), second_result.is_ok());
        let loaded = store.load_state("stack").await.unwrap();
        assert!(matches!(
            loaded.pending_replacements[&urn].operation_id.as_str(),
            "operation-1" | "operation-2"
        ));
    }
}
