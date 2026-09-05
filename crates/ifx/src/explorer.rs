//! Secret-safe deployment snapshots shared by the CLI's offline explorer and `ifxd`'s
//! live HTTP surface.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::engine::{Action, Engine, Options, PlannedOp};
use crate::model::{OutputRef, Program, Urn};
use crate::provider::Diff;
use crate::schema::ResourceSchema;
use crate::state::State;
use crate::store::{EventRecord, Health, HealthRecord, RunRecord};
use crate::{ExecutionRun, StackBuildStatus};

pub const SNAPSHOT_VERSION: u32 = 2;
const HTML_TEMPLATE: &str = include_str!("explorer.html");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub version: u32,
    pub stack: String,
    pub generated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<StackBuildStatus>,
    pub summary: TopologySummary,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
    /// Daemon-owned executions. This is the primary operator-facing run history.
    #[serde(default)]
    pub executions: Vec<ExecutionRun>,
    /// Legacy engine sub-runs retained for resource-operation history and backwards
    /// compatibility with version 1 snapshots.
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub events: Vec<EventRecord>,
}

/// Render a self-contained explorer document with the snapshot embedded. Replacing
/// `<` prevents stack-provided strings from terminating the JSON script element.
pub fn offline_html(snapshot: &TopologySnapshot) -> anyhow::Result<String> {
    let json = serde_json::to_string(snapshot)?.replace('<', "\\u003c");
    Ok(render_html(false, &json))
}

/// Render the live explorer shell. It discovers watched stacks and fetches snapshots
/// from the same `ifxd` origin.
pub fn live_html() -> String {
    render_html(true, "null")
}

fn render_html(live: bool, snapshot: &str) -> String {
    HTML_TEMPLATE
        .replace("__IFX_LIVE__", if live { "true" } else { "false" })
        .replace("__IFX_SNAPSHOT__", snapshot)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopologySummary {
    pub resources: usize,
    pub changes: usize,
    pub checks: usize,
    pub issues: usize,
    pub actions: BTreeMap<String, usize>,
    pub health: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopologyNode {
    pub urn: Urn,
    pub type_name: String,
    pub provider: String,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub action: TopologyAction,
    pub changes: Vec<TopologyChange>,
    pub skipped: bool,
    pub protect: bool,
    pub is_check: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub desired: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
    pub outputs: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<EventRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyAction {
    Create,
    Update,
    Replace,
    Delete,
    Trigger,
    Adopt,
    Unchanged,
}

impl TopologyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Replace => "replace",
            Self::Delete => "delete",
            Self::Trigger => "trigger",
            Self::Adopt => "adopt",
            Self::Unchanged => "unchanged",
        }
    }

    pub fn is_change(self) -> bool {
        self != Self::Unchanged
    }
}

impl From<&Action> for TopologyAction {
    fn from(action: &Action) -> Self {
        match action {
            Action::Create => Self::Create,
            Action::Update(_) => Self::Update,
            Action::Replace(_) => Self::Replace,
            Action::Delete => Self::Delete,
            Action::Trigger => Self::Trigger,
            Action::Adopt => Self::Adopt,
            Action::NoOp => Self::Unchanged,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopologyChange {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Value>,
    pub forces_replace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TopologyEdge {
    pub from: Urn,
    pub to: Urn,
    pub kind: EdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Reference,
    Dependency,
    Trigger,
    Stored,
}

/// Capture a current, read-only view of the desired graph, observed resources, health,
/// and recent history. Provider observation happens through [`Engine::preview`], which
/// deliberately does not create a run record.
pub async fn capture(
    engine: &Engine,
    stack: &str,
    program: &Program,
    state: &State,
    options: &Options,
) -> anyhow::Result<TopologySnapshot> {
    let plan = engine.preview(program, state, options).await?;
    let store = engine.store();
    let health = store.latest_health(stack).await?;
    let active_revision = match store.active_program_revision(stack).await {
        Ok(revision) => revision.map(|revision| revision.revision),
        Err(error) => {
            tracing::debug!(stack, "program revision history unavailable: {error:#}");
            None
        }
    };
    let executions = match store.executions(stack, 20).await {
        Ok(executions) => executions,
        Err(error) => {
            tracing::debug!(stack, "daemon execution history unavailable: {error:#}");
            Vec::new()
        }
    };
    let runs = store.runs(stack, 20).await?;
    let mut events = Vec::new();
    for run in runs.iter().take(10) {
        events.extend(store.events(&run.run_id).await?);
    }
    events.sort_by_key(|event| std::cmp::Reverse(event.at));

    let health_by_urn: BTreeMap<Urn, HealthRecord> = health
        .into_iter()
        .map(|record| (record.urn.clone(), record))
        .collect();
    let mut event_by_urn = BTreeMap::new();
    for event in &events {
        event_by_urn
            .entry(event.urn.clone())
            .or_insert_with(|| event.clone());
    }

    let mut nodes = Vec::with_capacity(plan.ops.len());
    for op in &plan.ops {
        let decl = program.get(&op.urn);
        let schema = engine
            .registry()
            .get(op.urn.type_name())
            .map(|h| h.schema());
        nodes.push(node(
            op,
            decl.map(|d| d.protect)
                .or_else(|| state.get(&op.urn).map(|entry| entry.protect))
                .unwrap_or(false),
            state,
            schema.as_ref(),
            engine.registry().is_check(op.urn.type_name()),
            health_by_urn.get(&op.urn).cloned(),
            event_by_urn.get(&op.urn).cloned(),
        ));
    }

    let edges = edges(program, state, &nodes);
    let mut summary = TopologySummary {
        resources: nodes.len(),
        checks: nodes.iter().filter(|node| node.is_check).count(),
        ..TopologySummary::default()
    };
    for node in &nodes {
        *summary
            .actions
            .entry(node.action.as_str().to_string())
            .or_default() += 1;
        if node.action.is_change() {
            summary.changes += 1;
        }
        if let Some(record) = &node.health {
            let status = health_name(record.status);
            *summary.health.entry(status.to_string()).or_default() += 1;
            if record.status != Health::Healthy {
                summary.issues += 1;
            }
        }
    }

    Ok(TopologySnapshot {
        version: SNAPSHOT_VERSION,
        stack: stack.to_string(),
        generated_at: Utc::now(),
        active_revision,
        build: None,
        summary,
        nodes,
        edges,
        executions,
        runs,
        events,
    })
}

fn node(
    op: &PlannedOp,
    protect: bool,
    state: &State,
    schema: Option<&ResourceSchema>,
    is_check: bool,
    health: Option<HealthRecord>,
    last_event: Option<EventRecord>,
) -> TopologyNode {
    let input_sensitive: BTreeSet<&str> = schema
        .into_iter()
        .flat_map(|schema| &schema.inputs)
        .filter(|field| field.sensitive)
        .map(|field| field.name.as_str())
        .chain(op.secrets.iter().map(String::as_str))
        .collect();
    let output_sensitive: BTreeSet<&str> = schema
        .into_iter()
        .flat_map(|schema| &schema.outputs)
        .filter(|field| field.sensitive)
        .map(|field| field.name.as_str())
        .collect();
    let action = TopologyAction::from(&op.action);
    let changes = match &op.action {
        Action::Update(diff) | Action::Replace(diff) => changes(diff, &input_sensitive),
        _ => Vec::new(),
    };
    let (provider, kind) = op
        .urn
        .type_name()
        .split_once('.')
        .unwrap_or((op.urn.type_name(), "resource"));

    TopologyNode {
        urn: op.urn.clone(),
        type_name: op.urn.type_name().to_string(),
        provider: provider.to_string(),
        kind: kind.to_string(),
        name: op.urn.name().to_string(),
        description: schema.map(|schema| schema.doc.clone()).unwrap_or_default(),
        action,
        changes,
        skipped: op.skipped,
        protect,
        is_check,
        id: op
            .actual
            .as_ref()
            .and_then(|actual| actual.id.clone())
            .or_else(|| state.get(&op.urn).and_then(|entry| entry.id.clone())),
        desired: redact(&op.desired, &input_sensitive),
        observed: op
            .actual
            .as_ref()
            .map(|actual| redact(&actual.props, &input_sensitive)),
        outputs: redact(&op.outputs, &output_sensitive),
        health,
        last_event,
    }
}

fn changes(diff: &Diff, sensitive: &BTreeSet<&str>) -> Vec<TopologyChange> {
    diff.changes
        .iter()
        .map(|change| {
            let hidden = sensitive.contains(change.field.as_str());
            TopologyChange {
                field: change.field.clone(),
                from: change.from.as_ref().map(|value| {
                    if hidden {
                        Value::String("<sensitive>".into())
                    } else {
                        value.clone()
                    }
                }),
                to: change.to.as_ref().map(|value| {
                    if hidden {
                        Value::String("<sensitive>".into())
                    } else {
                        value.clone()
                    }
                }),
                forces_replace: change.forces_replace,
            }
        })
        .collect()
}

fn redact(value: &Value, sensitive: &BTreeSet<&str>) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    Value::Object(
        object
            .iter()
            .map(|(key, value)| {
                let value = if sensitive.contains(key.as_str()) {
                    Value::String("<sensitive>".into())
                } else {
                    value.clone()
                };
                (key.clone(), value)
            })
            .collect::<Map<_, _>>(),
    )
}

fn edges(program: &Program, state: &State, nodes: &[TopologyNode]) -> Vec<TopologyEdge> {
    let included: BTreeSet<&Urn> = nodes.iter().map(|node| &node.urn).collect();
    let mut out = BTreeSet::new();
    for decl in &program.resources {
        if !included.contains(&decl.urn) {
            continue;
        }
        let mut refs = Vec::new();
        collect_refs_with_paths(&decl.inputs, "", &mut refs);
        for (reference, input_path) in refs {
            if included.contains(&reference.urn) {
                let label = match (input_path.is_empty(), reference.path.is_empty()) {
                    (true, true) => None,
                    (false, true) => Some(input_path),
                    (true, false) => Some(reference.path),
                    (false, false) => Some(format!("{input_path} ← {}", reference.path)),
                };
                out.insert(TopologyEdge {
                    from: reference.urn,
                    to: decl.urn.clone(),
                    kind: EdgeKind::Reference,
                    label,
                });
            }
        }
        for dependency in &decl.depends_on {
            if included.contains(dependency) {
                out.insert(TopologyEdge {
                    from: dependency.clone(),
                    to: decl.urn.clone(),
                    kind: EdgeKind::Dependency,
                    label: None,
                });
            }
        }
        for trigger in &decl.triggers {
            if included.contains(trigger) {
                out.insert(TopologyEdge {
                    from: trigger.clone(),
                    to: decl.urn.clone(),
                    kind: EdgeKind::Trigger,
                    label: None,
                });
            }
        }
    }
    for node in nodes.iter().filter(|node| program.get(&node.urn).is_none()) {
        if let Some(entry) = state.get(&node.urn) {
            for dependency in &entry.depends_on {
                if included.contains(dependency) {
                    out.insert(TopologyEdge {
                        from: dependency.clone(),
                        to: node.urn.clone(),
                        kind: EdgeKind::Stored,
                        label: None,
                    });
                }
            }
        }
    }
    out.into_iter().collect()
}

fn collect_refs_with_paths(value: &Value, path: &str, out: &mut Vec<(OutputRef, String)>) {
    if let Some(reference) = OutputRef::from_value(value) {
        out.push((reference, path.to_string()));
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let child = if key == crate::model::CONCAT_KEY {
                    path.to_string()
                } else if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_refs_with_paths(value, &child, out);
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                let child = if path.is_empty() {
                    index.to_string()
                } else {
                    format!("{path}[{index}]")
                };
                collect_refs_with_paths(value, &child, out);
            }
        }
        _ => {}
    }
}

fn health_name(health: Health) -> &'static str {
    match health {
        Health::Healthy => "healthy",
        Health::Degraded => "degraded",
        Health::Unhealthy => "unhealthy",
        Health::Drifted => "drifted",
        Health::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PlannedOp;
    use crate::model::ResourceDecl;
    use crate::provider::{Actual, FieldChange};
    use crate::schema::{FieldType, ResourceSchema, field};
    use serde_json::json;

    fn op(action: Action) -> PlannedOp {
        PlannedOp {
            urn: Urn::new("test.secret", "one"),
            action,
            desired: json!({"password": "plain", "token": "dynamic", "name": "shown"}),
            secrets: vec!["token".into()],
            actual: Some(Actual {
                id: Some("id-1".into()),
                props: json!({"password": "old", "token": "old-dynamic", "name": "shown"}),
                outputs: json!({"password": "output-secret", "name": "shown"}),
            }),
            outputs: json!({"password": "output-secret", "name": "shown"}),
            skipped: false,
            approvals: Vec::new(),
        }
    }

    fn schema() -> ResourceSchema {
        ResourceSchema::new("test.secret", "Secret test resource")
            .input(field("password", FieldType::String).sensitive())
            .input(field("token", FieldType::String))
            .input(field("name", FieldType::String))
            .output(field("password", FieldType::String).sensitive())
            .output(field("name", FieldType::String))
    }

    #[test]
    fn redacts_schema_and_dynamic_secrets_everywhere() {
        let action = Action::Update(Diff {
            changes: vec![
                FieldChange {
                    field: "password".into(),
                    from: Some(json!("old")),
                    to: Some(json!("new")),
                    forces_replace: false,
                },
                FieldChange {
                    field: "name".into(),
                    from: Some(json!("old-name")),
                    to: Some(json!("new-name")),
                    forces_replace: false,
                },
            ],
        });
        let node = node(
            &op(action),
            false,
            &State::default(),
            Some(&schema()),
            false,
            None,
            None,
        );
        assert_eq!(node.desired["password"], "<sensitive>");
        assert_eq!(node.desired["token"], "<sensitive>");
        assert_eq!(node.observed.unwrap()["password"], "<sensitive>");
        assert_eq!(node.outputs["password"], "<sensitive>");
        assert_eq!(node.changes[0].from, Some(json!("<sensitive>")));
        assert_eq!(node.changes[1].from, Some(json!("old-name")));
    }

    #[test]
    fn preserves_reference_dependency_and_trigger_edges() {
        let a = ResourceDecl::new("memory.value", "a", json!({"value": 1}));
        let mut b = ResourceDecl::new(
            "memory.value",
            "b",
            json!({"value": {"$ref": "memory.value:a", "$path": "value"}}),
        );
        b.depends_on.push(a.urn.clone());
        b.triggers.push(a.urn.clone());
        let program = Program {
            resources: vec![a.clone(), b.clone()],
        };
        let nodes = vec![
            TopologyNode {
                urn: a.urn.clone(),
                type_name: "memory.value".into(),
                provider: "memory".into(),
                kind: "value".into(),
                name: "a".into(),
                description: String::new(),
                action: TopologyAction::Unchanged,
                changes: vec![],
                skipped: false,
                protect: false,
                is_check: false,
                id: None,
                desired: Value::Null,
                observed: None,
                outputs: Value::Null,
                health: None,
                last_event: None,
            },
            TopologyNode {
                urn: b.urn.clone(),
                type_name: "memory.value".into(),
                provider: "memory".into(),
                kind: "value".into(),
                name: "b".into(),
                description: String::new(),
                action: TopologyAction::Unchanged,
                changes: vec![],
                skipped: false,
                protect: false,
                is_check: false,
                id: None,
                desired: Value::Null,
                observed: None,
                outputs: Value::Null,
                health: None,
                last_event: None,
            },
        ];
        let edges = edges(&program, &State::default(), &nodes);
        assert_eq!(edges.len(), 3);
        assert!(edges.iter().any(|edge| edge.kind == EdgeKind::Reference));
        assert!(edges.iter().any(|edge| edge.kind == EdgeKind::Dependency));
        assert!(edges.iter().any(|edge| edge.kind == EdgeKind::Trigger));
    }

    #[test]
    fn offline_document_cannot_be_broken_out_of_its_json_script() {
        let snapshot = TopologySnapshot {
            version: SNAPSHOT_VERSION,
            stack: "</script><script>window.bad = true</script>".into(),
            generated_at: Utc::now(),
            active_revision: None,
            build: None,
            summary: TopologySummary::default(),
            nodes: vec![],
            edges: vec![],
            executions: vec![],
            runs: vec![],
            events: vec![],
        };
        let html = offline_html(&snapshot).unwrap();

        assert!(!html.contains("</script><script>window.bad"));
        assert!(html.contains(r#"\u003c/script>\u003cscript>window.bad"#));
    }

    #[test]
    fn explorer_offers_operational_graph_shapes() {
        let html = live_html();
        for view in [
            "Build dependencies",
            "Resource utilization",
            "Logical vs physical",
            "Human vs computer facing",
        ] {
            assert!(html.contains(view), "missing explorer view {view}");
        }
    }
}
