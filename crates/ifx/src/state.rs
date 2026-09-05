//! Persisted knowledge about applied resources. Only what cannot be observed is
//! authoritative here (ids, secrets, dependency order); everything else is refreshed
//! from the world on each plan.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::Urn;

pub const STATE_VERSION: u32 = 1;
static STATE_SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Option<String>,
    /// Inputs as applied (fully resolved).
    pub inputs: Value,
    pub outputs: Value,
    #[serde(default)]
    pub depends_on: Vec<Urn>,
    #[serde(default)]
    pub protect: bool,
}

/// Durable intent written before a replacement mutates its provider-side resource.
/// It is cleared atomically with the successor [`Entry`] after the provider commits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingReplacement {
    pub operation_id: String,
    pub execution_run_id: String,
    pub revision: String,
    /// Digest of raw declaration intent used by exact approval fingerprinting. The
    /// declaration itself may contain secrets and is never persisted here.
    pub intent_digest: String,
    pub old: Entry,
    pub old_in_state: bool,
    pub desired_inputs: Value,
    pub depends_on: Vec<Urn>,
    pub protect: bool,
    /// Exact risk name to approved operation fingerprint mappings.
    #[serde(default)]
    pub approval_fingerprints: BTreeMap<String, String>,
    pub prepared_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub stack: String,
    pub resources: BTreeMap<Urn, Entry>,
    #[serde(default)]
    pub pending_replacements: BTreeMap<Urn, PendingReplacement>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            stack: String::new(),
            resources: BTreeMap::new(),
            pending_replacements: BTreeMap::new(),
        }
    }
}

impl State {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let state: Self =
            serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            state.version == STATE_VERSION,
            "state version {} unsupported (expected {STATE_VERSION})",
            state.version
        );
        Ok(state)
    }

    /// Durable atomic write (unique same-directory temp, fsync, rename, directory
    /// fsync). Store implementations serialize competing writers around this call.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = (0..1_024)
            .find_map(|_| {
                let sequence = STATE_SAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let candidate =
                    dir.join(format!(".{filename}.tmp-{}-{sequence}", std::process::id()));
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&candidate)
                {
                    Ok(file) => Some(Ok((candidate, file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .transpose()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "could not allocate a unique state temporary file in {}",
                    dir.display()
                )
            })?;
        let (tmp_path, mut tmp_file) = tmp;
        let data = serde_json::to_vec_pretty(self)?;
        let result: anyhow::Result<()> = (|| {
            tmp_file
                .write_all(&data)
                .with_context(|| format!("writing {}", tmp_path.display()))?;
            tmp_file
                .sync_all()
                .with_context(|| format!("syncing {}", tmp_path.display()))?;
            drop(tmp_file);
            std::fs::rename(&tmp_path, path)
                .with_context(|| format!("renaming to {}", path.display()))?;
            std::fs::File::open(dir)
                .with_context(|| format!("opening state directory {}", dir.display()))?
                .sync_all()
                .with_context(|| format!("syncing state directory {}", dir.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        result
    }

    pub fn get(&self, urn: &Urn) -> Option<&Entry> {
        self.resources.get(urn)
    }

    pub fn upsert(&mut self, urn: Urn, entry: Entry) {
        self.resources.insert(urn, entry);
    }

    pub fn remove(&mut self, urn: &Urn) -> Option<Entry> {
        self.resources.remove(urn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ifx/state.json");
        let mut s = State::default();
        s.upsert(
            Urn::new("a", "b"),
            Entry {
                id: Some("1".into()),
                inputs: json!({"x": 1}),
                outputs: json!({}),
                depends_on: vec![],
                protect: false,
            },
        );
        s.pending_replacements.insert(
            Urn::new("a", "b"),
            PendingReplacement {
                operation_id: "op-1".into(),
                execution_run_id: "run-1".into(),
                revision: "revision-1".into(),
                intent_digest: "intent-digest".into(),
                old: s.get(&Urn::new("a", "b")).unwrap().clone(),
                old_in_state: true,
                desired_inputs: json!({"x": 2}),
                depends_on: vec![],
                protect: false,
                approval_fingerprints: BTreeMap::from([("data-loss".into(), "fingerprint".into())]),
                prepared_at: Utc::now(),
            },
        );
        s.save(&path).unwrap();
        let back = State::load(&path).unwrap();
        assert_eq!(back.resources, s.resources);
        assert_eq!(back.pending_replacements, s.pending_replacements);
        assert!(
            State::load(&dir.path().join("missing.json"))
                .unwrap()
                .resources
                .is_empty()
        );
    }
}
