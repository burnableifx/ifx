//! SurrealDB-backed store. Tables: `resource`, `run`, `event`, `health`, `lease`, and the
//! daemon's revision and execution records.

use std::collections::BTreeMap;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use super::{EventRecord, Health, HealthRecord, Result, RunKind, RunRecord, Store};
use crate::control::{ExecutionEvent, ExecutionRun, ProgramRevision, StackLease};
use crate::model::Urn;
use crate::state::{Entry, PendingReplacement, State};

pub struct SurrealStore {
    db: Surreal<Any>,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
struct ResourceRow {
    stack: String,
    urn: Urn,
    #[serde(rename = "type")]
    type_name: String,
    provider_id: Option<String>,
    inputs: Value,
    outputs: Value,
    depends_on: Vec<Urn>,
    protect: bool,
    #[serde(default)]
    replacement_operation_id: Option<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct ReplacementRow {
    stack: String,
    urn: Urn,
    replacement: PendingReplacement,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RunRow {
    run_id: String,
    stack: String,
    kind: RunKind,
    started_at: DateTime<Utc>,
    #[serde(default)]
    finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    summary: Value,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct HealthRow {
    stack: String,
    urn: Urn,
    status: Health,
    #[serde(default)]
    message: String,
    #[serde(default)]
    latency_ms: Option<u64>,
    at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct StackRow {
    stack: String,
}

#[derive(Serialize, Deserialize)]
struct DeploymentRow {
    stack: String,
    active_revision: String,
    updated_at: DateTime<Utc>,
}

const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS resource SCHEMALESS;
DEFINE INDEX IF NOT EXISTS resource_stack ON TABLE resource COLUMNS stack;
DEFINE TABLE IF NOT EXISTS replacement SCHEMALESS;
DEFINE INDEX IF NOT EXISTS replacement_stack ON TABLE replacement COLUMNS stack;
DEFINE TABLE IF NOT EXISTS run SCHEMALESS;
DEFINE INDEX IF NOT EXISTS run_stack ON TABLE run COLUMNS stack, started_at;
DEFINE TABLE IF NOT EXISTS event SCHEMALESS;
DEFINE INDEX IF NOT EXISTS event_run ON TABLE event COLUMNS run_id;
DEFINE TABLE IF NOT EXISTS health SCHEMALESS;
DEFINE INDEX IF NOT EXISTS health_stack ON TABLE health COLUMNS stack, urn, at;
DEFINE TABLE IF NOT EXISTS program_revision SCHEMALESS;
DEFINE INDEX IF NOT EXISTS program_revision_stack ON TABLE program_revision COLUMNS stack, submitted_at;
DEFINE TABLE IF NOT EXISTS deployment SCHEMALESS;
DEFINE INDEX IF NOT EXISTS deployment_stack ON TABLE deployment COLUMNS stack UNIQUE;
DEFINE TABLE IF NOT EXISTS execution SCHEMALESS;
DEFINE INDEX IF NOT EXISTS execution_stack ON TABLE execution COLUMNS stack, requested_at;
DEFINE TABLE IF NOT EXISTS execution_event SCHEMALESS;
DEFINE INDEX IF NOT EXISTS execution_event_run ON TABLE execution_event COLUMNS run_id, at;
DEFINE TABLE IF NOT EXISTS lease SCHEMALESS;
DEFINE INDEX IF NOT EXISTS lease_stack ON TABLE lease COLUMNS stack UNIQUE;
";

impl SurrealStore {
    pub async fn connect(endpoint: &str, namespace: &str, database: &str) -> Result<Self> {
        let db = surrealdb::engine::any::connect(endpoint)
            .await
            .with_context(|| format!("connecting to {endpoint}"))?;
        if endpoint.starts_with("ws") || endpoint.starts_with("http") {
            let username = std::env::var("IFX_DB_USER").unwrap_or_else(|_| "root".into());
            let password = std::env::var("IFX_DB_PASS").unwrap_or_else(|_| "root".into());
            db.signin(Root {
                username: &username,
                password: &password,
            })
            .await
            .with_context(|| format!("signing in to {endpoint} as {username}"))?;
        }
        db.use_ns(namespace).use_db(database).await?;
        db.query(SCHEMA).await?.check().context("defining schema")?;
        Ok(Self {
            db,
            endpoint: endpoint.to_string(),
        })
    }

    pub fn db(&self) -> &Surreal<Any> {
        &self.db
    }

    fn resource_key(stack: &str, urn: &Urn) -> String {
        format!("{stack}|{urn}")
    }

    fn replacement_key(stack: &str, urn: &Urn) -> String {
        format!("{stack}|{urn}")
    }

    fn revision_key(stack: &str, revision: &str) -> String {
        format!("{stack}|{revision}")
    }
}

#[async_trait]
impl Store for SurrealStore {
    fn describe(&self) -> String {
        self.endpoint.clone()
    }

    async fn load_state(&self, stack: &str) -> Result<State> {
        let mut response = self
            .db
            .query(
                "BEGIN TRANSACTION; \
                 SELECT * OMIT id FROM resource WHERE stack = $stack; \
                 SELECT * OMIT id FROM replacement WHERE stack = $stack; \
                 COMMIT TRANSACTION;",
            )
            .bind(("stack", stack.to_string()))
            .await?;
        let resources: Vec<ResourceRow> = response.take(0)?;
        let replacements: Vec<ReplacementRow> = response.take(1)?;
        let mut state = State {
            stack: stack.to_string(),
            ..State::default()
        };
        for r in resources {
            state.resources.insert(
                r.urn,
                Entry {
                    id: r.provider_id,
                    inputs: r.inputs,
                    outputs: r.outputs,
                    depends_on: r.depends_on,
                    protect: r.protect,
                },
            );
        }
        for row in replacements {
            state.pending_replacements.insert(row.urn, row.replacement);
        }
        Ok(state)
    }

    async fn save_state(&self, state: &State) -> Result<()> {
        let stack = state.stack.clone();
        let existing: Vec<StackRow> = self
            .db
            .query("SELECT urn AS stack FROM resource WHERE stack = $stack")
            .bind(("stack", stack.clone()))
            .await?
            .take(0)?;
        let stale: Vec<String> = existing
            .into_iter()
            .map(|r| r.stack)
            .filter(|u| {
                Urn::parse(u)
                    .map(|u| !state.resources.contains_key(&u))
                    .unwrap_or(true)
            })
            .collect();
        for u in stale {
            let key = Self::resource_key(&stack, &Urn::parse(&u)?);
            let response = self
                .db
                .query(
                    "DELETE type::thing('resource', $key) \
                     WHERE replacement_operation_id = NONE RETURN BEFORE",
                )
                .bind(("key", key))
                .await?;
            let mut response = response.check()?;
            let deleted: Vec<ResourceRow> = response.take(0)?;
            anyhow::ensure!(
                deleted.len() == 1,
                "refusing to delete state for {u} while a replacement journal guards it"
            );
        }
        let now = Utc::now();
        for (urn, e) in &state.resources {
            let row = ResourceRow {
                stack: stack.clone(),
                urn: urn.clone(),
                type_name: urn.type_name().to_string(),
                provider_id: e.id.clone(),
                inputs: e.inputs.clone(),
                outputs: e.outputs.clone(),
                depends_on: e.depends_on.clone(),
                protect: e.protect,
                replacement_operation_id: state
                    .pending_replacements
                    .get(urn)
                    .map(|replacement| replacement.operation_id.clone()),
                updated_at: now,
            };
            let expected_operation = row.replacement_operation_id.clone();
            let has_expected_operation = expected_operation.is_some();
            let response = self
                .db
                .query(
                    "UPSERT type::thing('resource', $key) CONTENT $row \
                     WHERE ($has_expected_operation = false AND replacement_operation_id = NONE) \
                        OR replacement_operation_id = $expected_operation \
                     RETURN AFTER",
                )
                .bind(("key", Self::resource_key(&stack, urn)))
                .bind(("row", row))
                .bind(("expected_operation", expected_operation.unwrap_or_default()))
                .bind(("has_expected_operation", has_expected_operation))
                .await?;
            let mut response = response.check().with_context(|| format!("saving {urn}"))?;
            let saved: Vec<ResourceRow> = response.take(0)?;
            anyhow::ensure!(
                saved.len() == 1,
                "refusing to overwrite state for {urn} while another replacement journal guards it"
            );
        }
        Ok(())
    }

    async fn prepare_replacement(&self, state: &State, urn: &Urn) -> Result<()> {
        let replacement = state
            .pending_replacements
            .get(urn)
            .cloned()
            .with_context(|| format!("preparing missing replacement intent for {urn}"))?;
        if replacement.old_in_state {
            anyhow::ensure!(
                state.resources.get(urn) == Some(&replacement.old),
                "replacement journal for {urn} does not match its in-memory resource entry"
            );
        } else {
            anyhow::ensure!(
                !state.resources.contains_key(urn),
                "unmanaged replacement journal for {urn} unexpectedly has a state entry"
            );
        }
        let row = ReplacementRow {
            stack: state.stack.clone(),
            urn: urn.clone(),
            replacement,
            updated_at: Utc::now(),
        };
        let query = if row.replacement.old_in_state {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal != NONE { THROW 'replacement journal already exists'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource = NONE \
                OR ($old_has_provider_id = true AND $resource.provider_id != $old_provider_id) \
                OR ($old_has_provider_id = false AND $resource.provider_id != NONE) \
                OR $resource.inputs != $old_inputs \
                OR $resource.outputs != $old_outputs \
                OR $resource.depends_on != $old_depends_on \
                OR $resource.protect != $old_protect \
                OR $resource.replacement_operation_id != NONE \
             { THROW 'resource changed before replacement preparation'; }; \
             UPDATE type::thing('resource', $resource_key) \
                SET replacement_operation_id = $operation_id; \
             CREATE ONLY type::thing('replacement', $replacement_key) CONTENT $row; \
             COMMIT TRANSACTION;"
        } else {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal != NONE { THROW 'replacement journal already exists'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource != NONE { THROW 'resource state appeared before replacement preparation'; }; \
             CREATE ONLY type::thing('replacement', $replacement_key) CONTENT $row; \
             COMMIT TRANSACTION;"
        };
        self.db
            .query(query)
            .bind(("replacement_key", Self::replacement_key(&state.stack, urn)))
            .bind(("resource_key", Self::resource_key(&state.stack, urn)))
            .bind(("operation_id", row.replacement.operation_id.clone()))
            .bind((
                "old_provider_id",
                row.replacement.old.id.clone().unwrap_or_default(),
            ))
            .bind(("old_has_provider_id", row.replacement.old.id.is_some()))
            .bind(("old_inputs", row.replacement.old.inputs.clone()))
            .bind(("old_outputs", row.replacement.old.outputs.clone()))
            .bind(("old_depends_on", row.replacement.old.depends_on.clone()))
            .bind(("old_protect", row.replacement.old.protect))
            .bind(("row", row))
            .await?
            .check()
            .with_context(|| format!("preparing replacement journal for {urn}"))?;
        Ok(())
    }

    async fn reauthorize_replacement(
        &self,
        state: &State,
        urn: &Urn,
        previous_operation_id: &str,
    ) -> Result<()> {
        let replacement = state
            .pending_replacements
            .get(urn)
            .cloned()
            .with_context(|| format!("reauthorizing missing replacement intent for {urn}"))?;
        anyhow::ensure!(
            replacement.operation_id != previous_operation_id,
            "replacement approval identity for {urn} did not change"
        );
        let row = ReplacementRow {
            stack: state.stack.clone(),
            urn: urn.clone(),
            replacement,
            updated_at: Utc::now(),
        };
        let query = if row.replacement.old_in_state {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal = NONE OR $journal.replacement.operation_id != $previous_operation_id \
             { THROW 'replacement journal operation does not match'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource = NONE OR $resource.replacement_operation_id != $previous_operation_id \
             { THROW 'guarded resource changed before replacement reauthorization'; }; \
             UPDATE type::thing('resource', $resource_key) \
                SET replacement_operation_id = $operation_id; \
             UPDATE type::thing('replacement', $replacement_key) CONTENT $row; \
             COMMIT TRANSACTION;"
        } else {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal = NONE OR $journal.replacement.operation_id != $previous_operation_id \
             { THROW 'replacement journal operation does not match'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource != NONE \
             { THROW 'unmanaged resource state appeared before replacement reauthorization'; }; \
             UPDATE type::thing('replacement', $replacement_key) CONTENT $row; \
             COMMIT TRANSACTION;"
        };
        self.db
            .query(query)
            .bind(("resource_key", Self::resource_key(&state.stack, urn)))
            .bind(("replacement_key", Self::replacement_key(&state.stack, urn)))
            .bind(("previous_operation_id", previous_operation_id.to_string()))
            .bind(("operation_id", row.replacement.operation_id.clone()))
            .bind(("row", row))
            .await?
            .check()
            .with_context(|| format!("reauthorizing replacement journal for {urn}"))?;
        Ok(())
    }

    async fn commit_replacement(&self, state: &State, urn: &Urn, operation_id: &str) -> Result<()> {
        anyhow::ensure!(
            !state.pending_replacements.contains_key(urn),
            "cannot commit {urn} while its replacement intent remains in memory"
        );
        let entry = state
            .resources
            .get(urn)
            .with_context(|| format!("committing missing replacement result for {urn}"))?;
        let row = ResourceRow {
            stack: state.stack.clone(),
            urn: urn.clone(),
            type_name: urn.type_name().to_string(),
            provider_id: entry.id.clone(),
            inputs: entry.inputs.clone(),
            outputs: entry.outputs.clone(),
            depends_on: entry.depends_on.clone(),
            protect: entry.protect,
            replacement_operation_id: None,
            updated_at: Utc::now(),
        };
        self.db
            .query(
                "BEGIN TRANSACTION; \
                 LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
                 IF $journal = NONE OR $journal.replacement.operation_id != $operation_id \
                 { THROW 'replacement journal operation does not match'; }; \
                 LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
                 IF $journal.replacement.old_in_state = true \
                    AND ($resource = NONE OR $resource.replacement_operation_id != $operation_id) \
                 { THROW 'guarded resource changed before replacement commit'; }; \
                 IF $journal.replacement.old_in_state = false AND $resource != NONE \
                 { THROW 'unmanaged resource state appeared before replacement commit'; }; \
                 UPSERT type::thing('resource', $resource_key) CONTENT $row; \
                 DELETE type::thing('replacement', $replacement_key) \
                    WHERE replacement.operation_id = $operation_id; \
                 COMMIT TRANSACTION;",
            )
            .bind(("resource_key", Self::resource_key(&state.stack, urn)))
            .bind(("replacement_key", Self::replacement_key(&state.stack, urn)))
            .bind(("operation_id", operation_id.to_string()))
            .bind(("row", row))
            .await?
            .check()
            .with_context(|| format!("committing replacement journal for {urn}"))?;
        Ok(())
    }

    async fn abort_replacement(&self, state: &State, urn: &Urn, operation_id: &str) -> Result<()> {
        anyhow::ensure!(
            !state.pending_replacements.contains_key(urn),
            "cannot abort {urn} while its replacement intent remains in memory"
        );
        let restored = state.resources.get(urn).map(|entry| ResourceRow {
            stack: state.stack.clone(),
            urn: urn.clone(),
            type_name: urn.type_name().to_string(),
            provider_id: entry.id.clone(),
            inputs: entry.inputs.clone(),
            outputs: entry.outputs.clone(),
            depends_on: entry.depends_on.clone(),
            protect: entry.protect,
            replacement_operation_id: None,
            updated_at: Utc::now(),
        });
        let query = if restored.is_some() {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal = NONE OR $journal.replacement.operation_id != $operation_id \
                OR $journal.replacement.old_in_state != true \
             { THROW 'replacement journal operation does not match a managed rollback'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource = NONE OR $resource.replacement_operation_id != $operation_id \
             { THROW 'guarded resource changed before replacement rollback'; }; \
             UPDATE type::thing('resource', $resource_key) CONTENT $row; \
             DELETE type::thing('replacement', $replacement_key) \
                WHERE replacement.operation_id = $operation_id; \
             COMMIT TRANSACTION;"
        } else {
            "BEGIN TRANSACTION; \
             LET $journal = SELECT * FROM ONLY type::thing('replacement', $replacement_key); \
             IF $journal = NONE OR $journal.replacement.operation_id != $operation_id \
                OR $journal.replacement.old_in_state != false \
             { THROW 'replacement journal operation does not match an unmanaged rollback'; }; \
             LET $resource = SELECT * FROM ONLY type::thing('resource', $resource_key); \
             IF $resource != NONE \
             { THROW 'unmanaged resource state appeared before replacement rollback'; }; \
             DELETE type::thing('replacement', $replacement_key) \
                WHERE replacement.operation_id = $operation_id; \
             COMMIT TRANSACTION;"
        };
        let mut request = self
            .db
            .query(query)
            .bind(("resource_key", Self::resource_key(&state.stack, urn)))
            .bind(("replacement_key", Self::replacement_key(&state.stack, urn)))
            .bind(("operation_id", operation_id.to_string()));
        if let Some(row) = restored {
            request = request.bind(("row", row));
        }
        request
            .await?
            .check()
            .with_context(|| format!("aborting replacement journal for {urn}"))?;
        Ok(())
    }

    async fn begin_run(&self, stack: &str, kind: RunKind) -> Result<String> {
        let run_id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%3f"),
            rand_suffix()
        );
        let row = RunRecord {
            run_id: run_id.clone(),
            stack: stack.to_string(),
            kind,
            started_at: Utc::now(),
            finished_at: None,
            ok: None,
            summary: Value::Null,
            error: None,
        };
        self.db
            .query("CREATE type::thing('run', $key) CONTENT $row")
            .bind(("key", run_id.clone()))
            .bind(("row", row))
            .await?
            .check()?;
        Ok(run_id)
    }

    async fn finish_run(
        &self,
        run_id: &str,
        ok: bool,
        summary: Value,
        error: Option<String>,
    ) -> Result<()> {
        self.db
            .query(
                "UPDATE type::thing('run', $key) MERGE { finished_at: $at, ok: $ok, summary: $summary, error: $error }",
            )
            .bind(("key", run_id.to_string()))
            .bind(("at", Utc::now()))
            .bind(("ok", ok))
            .bind(("summary", summary))
            .bind(("error", error))
            .await?
            .check()?;
        Ok(())
    }

    async fn runs(&self, stack: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let rows: Vec<RunRow> = self
            .db
            .query("SELECT * OMIT id FROM run WHERE stack = $stack ORDER BY started_at DESC LIMIT $limit")
            .bind(("stack", stack.to_string()))
            .bind(("limit", limit as i64))
            .await?
            .take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| RunRecord {
                run_id: r.run_id,
                stack: r.stack,
                kind: r.kind,
                started_at: r.started_at,
                finished_at: r.finished_at,
                ok: r.ok,
                summary: r.summary,
                error: r.error,
            })
            .collect())
    }

    async fn record_event(&self, ev: &EventRecord) -> Result<()> {
        self.db
            .query("CREATE event CONTENT $row")
            .bind(("row", ev.clone()))
            .await?
            .check()?;
        Ok(())
    }

    async fn events(&self, run_id: &str) -> Result<Vec<EventRecord>> {
        Ok(self
            .db
            .query("SELECT * OMIT id FROM event WHERE run_id = $run ORDER BY at ASC")
            .bind(("run", run_id.to_string()))
            .await?
            .take(0)?)
    }

    async fn record_health(&self, h: &HealthRecord) -> Result<()> {
        self.db
            .query("CREATE health CONTENT $row")
            .bind(("row", h.clone()))
            .await?
            .check()?;
        Ok(())
    }

    async fn latest_health(&self, stack: &str) -> Result<Vec<HealthRecord>> {
        let rows: Vec<HealthRow> = self
            .db
            .query("SELECT * OMIT id FROM health WHERE stack = $stack ORDER BY at DESC LIMIT 5000")
            .bind(("stack", stack.to_string()))
            .await?
            .take(0)?;
        let mut latest: BTreeMap<Urn, HealthRecord> = BTreeMap::new();
        for r in rows {
            latest.entry(r.urn.clone()).or_insert(HealthRecord {
                stack: r.stack,
                urn: r.urn,
                status: r.status,
                message: r.message,
                latency_ms: r.latency_ms,
                at: r.at,
            });
        }
        Ok(latest.into_values().collect())
    }

    async fn health_history(
        &self,
        stack: &str,
        urn: &Urn,
        limit: usize,
    ) -> Result<Vec<HealthRecord>> {
        let rows: Vec<HealthRow> = self
            .db
            .query("SELECT * OMIT id FROM health WHERE stack = $stack AND urn = $urn ORDER BY at DESC LIMIT $limit")
            .bind(("stack", stack.to_string()))
            .bind(("urn", urn.clone()))
            .bind(("limit", limit as i64))
            .await?
            .take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| HealthRecord {
                stack: r.stack,
                urn: r.urn,
                status: r.status,
                message: r.message,
                latency_ms: r.latency_ms,
                at: r.at,
            })
            .collect())
    }

    async fn stacks(&self) -> Result<Vec<String>> {
        let mut res = self
            .db
            .query("SELECT stack FROM resource GROUP BY stack")
            .query("SELECT stack FROM run GROUP BY stack")
            .query("SELECT stack FROM deployment GROUP BY stack")
            .await?;
        let a: Vec<StackRow> = res.take(0)?;
        let b: Vec<StackRow> = res.take(1)?;
        let c: Vec<StackRow> = res.take(2)?;
        let mut all: Vec<String> = a.into_iter().chain(b).chain(c).map(|r| r.stack).collect();
        all.sort();
        all.dedup();
        Ok(all)
    }

    async fn put_program_revision(&self, revision: &ProgramRevision, activate: bool) -> Result<()> {
        let key = Self::revision_key(&revision.stack, &revision.revision);
        let existing: Option<ProgramRevision> = self
            .db
            .query("SELECT * OMIT id FROM type::thing('program_revision', $key)")
            .bind(("key", key.clone()))
            .await?
            .take(0)?;
        if existing.is_none() {
            self.db
                .query("CREATE type::thing('program_revision', $key) CONTENT $row")
                .bind(("key", key))
                .bind(("row", revision.clone()))
                .await?
                .check()?;
        }
        if activate {
            let row = DeploymentRow {
                stack: revision.stack.clone(),
                active_revision: revision.revision.clone(),
                updated_at: Utc::now(),
            };
            self.db
                .query("UPSERT type::thing('deployment', $key) CONTENT $row")
                .bind(("key", revision.stack.clone()))
                .bind(("row", row))
                .await?
                .check()?;
        }
        Ok(())
    }

    async fn program_revision(
        &self,
        stack: &str,
        revision: &str,
    ) -> Result<Option<ProgramRevision>> {
        Ok(self
            .db
            .query("SELECT * OMIT id FROM type::thing('program_revision', $key)")
            .bind(("key", Self::revision_key(stack, revision)))
            .await?
            .take(0)?)
    }

    async fn active_program_revision(&self, stack: &str) -> Result<Option<ProgramRevision>> {
        let deployment: Option<DeploymentRow> = self
            .db
            .query("SELECT * OMIT id FROM type::thing('deployment', $key)")
            .bind(("key", stack.to_string()))
            .await?
            .take(0)?;
        match deployment {
            Some(row) => self.program_revision(stack, &row.active_revision).await,
            None => Ok(None),
        }
    }

    async fn lease(&self, stack: &str) -> Result<Option<StackLease>> {
        Ok(self
            .db
            .query("SELECT * OMIT id FROM type::thing('lease', $key)")
            .bind(("key", stack.to_string()))
            .await?
            .take(0)?)
    }

    async fn put_lease(&self, lease: &StackLease) -> Result<()> {
        self.db
            .query("UPSERT type::thing('lease', $key) CONTENT $row")
            .bind(("key", lease.stack.clone()))
            .bind(("row", lease.clone()))
            .await?
            .check()?;
        Ok(())
    }

    async fn clear_lease(&self, stack: &str) -> Result<()> {
        self.db
            .query("DELETE type::thing('lease', $key)")
            .bind(("key", stack.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    async fn save_execution(&self, run: &ExecutionRun) -> Result<()> {
        self.db
            .query("UPSERT type::thing('execution', $key) CONTENT $row")
            .bind(("key", run.run_id.clone()))
            .bind(("row", run.clone()))
            .await?
            .check()?;
        Ok(())
    }

    async fn execution(&self, run_id: &str) -> Result<Option<ExecutionRun>> {
        Ok(self
            .db
            .query("SELECT * OMIT id FROM type::thing('execution', $key)")
            .bind(("key", run_id.to_string()))
            .await?
            .take(0)?)
    }

    async fn executions(&self, stack: &str, limit: usize) -> Result<Vec<ExecutionRun>> {
        Ok(self
            .db
            .query(
                "SELECT * OMIT id FROM execution WHERE stack = $stack ORDER BY requested_at DESC LIMIT $limit",
            )
            .bind(("stack", stack.to_string()))
            .bind(("limit", limit as i64))
            .await?
            .take(0)?)
    }

    async fn record_execution_event(&self, event: &ExecutionEvent) -> Result<()> {
        self.db
            .query("CREATE execution_event CONTENT $row")
            .bind(("row", event.clone()))
            .await?
            .check()?;
        Ok(())
    }

    async fn execution_events(&self, run_id: &str) -> Result<Vec<ExecutionEvent>> {
        Ok(self
            .db
            .query("SELECT * OMIT id FROM execution_event WHERE run_id = $run ORDER BY at ASC")
            .bind(("run", run_id.to_string()))
            .await?
            .take(0)?)
    }
}

fn rand_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let h = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{:06x}", h & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn state_roundtrip_and_history() {
        let store = SurrealStore::connect("mem://", "ifx", "test")
            .await
            .unwrap();
        let mut state = State {
            stack: "s".into(),
            ..State::default()
        };
        let a = Urn::new("memory.value", "a");
        let b = Urn::new("memory.value", "b");
        state.upsert(
            a.clone(),
            Entry {
                id: Some("1".into()),
                inputs: json!({"value": {"n": 1}}),
                outputs: json!({"value": 1}),
                depends_on: vec![],
                protect: false,
            },
        );
        state.upsert(
            b.clone(),
            Entry {
                id: None,
                inputs: json!({"value": "x"}),
                outputs: json!({}),
                depends_on: vec![a.clone()],
                protect: true,
            },
        );
        store.save_state(&state).await.unwrap();
        let back = store.load_state("s").await.unwrap();
        assert_eq!(back.resources, state.resources);
        assert!(
            store
                .load_state("other")
                .await
                .unwrap()
                .resources
                .is_empty()
        );

        state.remove(&b);
        store.save_state(&state).await.unwrap();
        let back = store.load_state("s").await.unwrap();
        assert_eq!(back.resources.len(), 1);

        let run = store.begin_run("s", RunKind::Apply).await.unwrap();
        store
            .record_event(&EventRecord {
                run_id: run.clone(),
                stack: "s".into(),
                urn: a.clone(),
                action: "create".into(),
                ok: true,
                error: None,
                at: Utc::now(),
            })
            .await
            .unwrap();
        store
            .finish_run(&run, true, json!({"create": 1}), None)
            .await
            .unwrap();
        let runs = store.runs("s", 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].ok, Some(true));
        assert_eq!(runs[0].summary["create"], 1);
        assert_eq!(store.events(&run).await.unwrap().len(), 1);

        for (i, st) in [Health::Healthy, Health::Unhealthy].iter().enumerate() {
            store
                .record_health(&HealthRecord {
                    stack: "s".into(),
                    urn: a.clone(),
                    status: *st,
                    message: format!("m{i}"),
                    latency_ms: Some(i as u64),
                    at: Utc::now() + chrono::Duration::seconds(i as i64),
                })
                .await
                .unwrap();
        }
        let latest = store.latest_health("s").await.unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].status, Health::Unhealthy);
        assert_eq!(store.health_history("s", &a, 10).await.unwrap().len(), 2);

        let revision = crate::control::ProgramRevision::from_submission(
            "s",
            crate::control::ProgramSubmission::new(
                "test",
                crate::model::Program {
                    resources: vec![crate::model::ResourceDecl::new(
                        "memory.value",
                        "a",
                        json!({"value": 1}),
                    )],
                },
            ),
        )
        .unwrap();
        store.put_program_revision(&revision, true).await.unwrap();
        let active = store.active_program_revision("s").await.unwrap().unwrap();
        assert_eq!(active.revision, revision.revision);

        let request = crate::control::RunRequest::new(RunKind::Apply);
        let mut execution =
            crate::control::ExecutionRun::queued("exec-1", "s", revision.revision, request);
        store.save_execution(&execution).await.unwrap();
        execution.status = crate::control::ExecutionStatus::Succeeded;
        execution.result = json!({"applied": 1});
        store.save_execution(&execution).await.unwrap();
        assert_eq!(
            store.execution("exec-1").await.unwrap().unwrap().status,
            crate::control::ExecutionStatus::Succeeded
        );
        let execution_event = crate::control::ExecutionEvent {
            run_id: "exec-1".into(),
            stack: "s".into(),
            kind: "finished".into(),
            urn: Some(a.clone()),
            action: Some("create".into()),
            message: String::new(),
            at: Utc::now(),
        };
        store
            .record_execution_event(&execution_event)
            .await
            .unwrap();
        assert_eq!(store.execution_events("exec-1").await.unwrap().len(), 1);
        assert_eq!(store.stacks().await.unwrap(), vec!["s".to_string()]);
    }

    #[tokio::test]
    async fn replacement_journal_commits_with_successor_state() {
        let store = SurrealStore::connect("mem://", "ifx", "replacement-journal")
            .await
            .unwrap();
        let urn = Urn::new("memory.value", "database");
        let old = Entry {
            id: Some("old-id".into()),
            inputs: json!({"value": 1, "key": "old"}),
            outputs: json!({"value": 1}),
            depends_on: vec![],
            protect: false,
        };
        let mut state = State {
            stack: "s".into(),
            ..State::default()
        };
        state.upsert(urn.clone(), old.clone());
        store.save_state(&state).await.unwrap();
        let stale_state = state.clone();
        state.pending_replacements.insert(
            urn.clone(),
            PendingReplacement {
                operation_id: "operation-1".into(),
                execution_run_id: "run-1".into(),
                revision: "revision-1".into(),
                intent_digest: "intent-digest".into(),
                old,
                old_in_state: true,
                desired_inputs: json!({"value": 2, "key": "new"}),
                depends_on: vec![],
                protect: false,
                approval_fingerprints: BTreeMap::new(),
                prepared_at: Utc::now(),
            },
        );
        store.prepare_replacement(&state, &urn).await.unwrap();
        assert!(store.save_state(&stale_state).await.is_err());

        let mut competing = state.clone();
        competing
            .pending_replacements
            .get_mut(&urn)
            .unwrap()
            .operation_id = "operation-2".into();
        assert!(store.prepare_replacement(&competing, &urn).await.is_err());

        let prepared = store.load_state("s").await.unwrap();
        assert_eq!(
            prepared.pending_replacements[&urn].operation_id,
            "operation-1"
        );
        let mut reauthorized = prepared;
        reauthorized
            .pending_replacements
            .get_mut(&urn)
            .unwrap()
            .operation_id = "operation-2".into();
        assert!(
            store
                .reauthorize_replacement(&reauthorized, &urn, "stale-operation")
                .await
                .is_err()
        );
        store
            .reauthorize_replacement(&reauthorized, &urn, "operation-1")
            .await
            .unwrap();
        let mut committed = reauthorized;
        committed.pending_replacements.remove(&urn);
        committed.upsert(
            urn.clone(),
            Entry {
                id: Some("new-id".into()),
                inputs: json!({"value": 2, "key": "new"}),
                outputs: json!({"value": 2}),
                depends_on: vec![],
                protect: false,
            },
        );
        assert!(
            store
                .commit_replacement(&committed, &urn, "operation-1")
                .await
                .is_err()
        );
        store
            .commit_replacement(&committed, &urn, "operation-2")
            .await
            .unwrap();

        let loaded = store.load_state("s").await.unwrap();
        assert!(loaded.pending_replacements.is_empty());
        assert_eq!(loaded.resources[&urn].id.as_deref(), Some("new-id"));
        assert_eq!(loaded.resources[&urn].inputs["key"], "new");

        let old = loaded.resources[&urn].clone();
        let mut aborting = loaded;
        aborting.pending_replacements.insert(
            urn.clone(),
            PendingReplacement {
                operation_id: "operation-3".into(),
                execution_run_id: "run-2".into(),
                revision: "revision-2".into(),
                intent_digest: "second-intent".into(),
                old,
                old_in_state: true,
                desired_inputs: json!({"value": 3, "key": "third"}),
                depends_on: vec![],
                protect: false,
                approval_fingerprints: BTreeMap::new(),
                prepared_at: Utc::now(),
            },
        );
        store.prepare_replacement(&aborting, &urn).await.unwrap();
        let mut restored = aborting;
        restored.pending_replacements.remove(&urn);
        assert!(
            store
                .abort_replacement(&restored, &urn, "stale-operation")
                .await
                .is_err()
        );
        store
            .abort_replacement(&restored, &urn, "operation-3")
            .await
            .unwrap();
        let loaded = store.load_state("s").await.unwrap();
        assert!(loaded.pending_replacements.is_empty());
        assert_eq!(loaded.resources[&urn].id.as_deref(), Some("new-id"));
        assert_eq!(loaded.resources[&urn].inputs["key"], "new");
    }

    #[tokio::test]
    async fn lease_roundtrip_is_keyed_by_stack() {
        let store = SurrealStore::connect("mem://", "ifx", "lease")
            .await
            .unwrap();
        assert!(store.lease("s").await.unwrap().is_none());
        let now = Utc::now();
        let lease = StackLease {
            stack: "s".into(),
            deadline: now + chrono::TimeDelta::minutes(5),
            grace: std::time::Duration::from_secs(30),
            created_at: now,
            history: vec![],
            fired: None,
        };
        store.put_lease(&lease).await.unwrap();
        assert_eq!(store.lease("s").await.unwrap(), Some(lease.clone()));
        assert!(store.lease("other").await.unwrap().is_none());

        let extended = StackLease {
            deadline: lease.deadline + chrono::TimeDelta::minutes(1),
            ..lease.clone()
        };
        store.put_lease(&extended).await.unwrap();
        assert_eq!(store.lease("s").await.unwrap(), Some(extended));

        store.clear_lease("s").await.unwrap();
        assert!(store.lease("s").await.unwrap().is_none());
    }
}
