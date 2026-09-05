//! Human-facing output: plan trees, apply progress, summaries.

use std::io::Write;

use crate::color::Paint;
use serde_json::Value;

use crate::engine::{Action, Event, Plan, PlannedOp, Report};
use crate::provider::{Diff, Registry};

fn redact(op: &PlannedOp, registry: &Registry, field: &str, v: &Value) -> String {
    if is_sensitive(op, registry, field) {
        return "<sensitive>".to_string();
    }
    if crate::model::contains_unknown(v) {
        return "<computed>".to_string();
    }
    let full = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let shown: String = full.lines().next().unwrap_or("").chars().take(80).collect();
    if shown.chars().count() < full.trim_end_matches('\n').chars().count() {
        format!("{shown}…")
    } else {
        shown
    }
}

fn render_diff(
    out: &mut impl Write,
    op: &PlannedOp,
    registry: &Registry,
    diff: &Diff,
) -> std::io::Result<()> {
    for c in &diff.changes {
        let tail = if c.forces_replace {
            " (forces replacement)"
        } else {
            ""
        };
        // Multi-line strings: point at the first line that differs.
        if let (Some(Value::String(a)), Some(Value::String(b))) = (&c.from, &c.to)
            && (a.contains('\n') || b.contains('\n'))
            && !is_sensitive(op, registry, &c.field)
        {
            let (i, la, lb) = first_diff_line(a, b);
            writeln!(
                out,
                "      {}: line {}: {} -> {}{}",
                c.field,
                i + 1,
                format!("{la:?}").red(),
                format!("{lb:?}").green(),
                tail.yellow()
            )?;
            continue;
        }
        let from = c
            .from
            .as_ref()
            .map(|v| redact(op, registry, &c.field, v))
            .unwrap_or_default();
        let to =
            c.to.as_ref()
                .map(|v| redact(op, registry, &c.field, v))
                .unwrap_or_default();
        writeln!(
            out,
            "      {}: {} -> {}{}",
            c.field,
            from.red(),
            to.green(),
            tail.yellow()
        )?;
    }
    Ok(())
}

fn is_sensitive(op: &PlannedOp, registry: &Registry, field: &str) -> bool {
    op.secrets.iter().any(|s| s == field)
        || registry
            .get(op.urn.type_name())
            .map(|h| h.schema().sensitive_fields().any(|f| f == field))
            .unwrap_or(false)
}

/// Index and text of the first differing line (`""` when one side has run out).
fn first_diff_line<'a>(a: &'a str, b: &'a str) -> (usize, &'a str, &'a str) {
    let mut la = a.lines();
    let mut lb = b.lines();
    let mut i = 0;
    loop {
        match (la.next(), lb.next()) {
            (Some(x), Some(y)) if x == y => i += 1,
            (x, y) => return (i, x.unwrap_or(""), y.unwrap_or("")),
        }
    }
}

fn render_inputs(out: &mut impl Write, op: &PlannedOp, registry: &Registry) -> std::io::Result<()> {
    if let Some(obj) = op.desired.as_object() {
        for (k, v) in obj {
            writeln!(out, "      {k}: {}", redact(op, registry, k, v).green())?;
        }
    }
    Ok(())
}

pub fn plan(
    out: &mut impl Write,
    plan: &Plan,
    registry: &Registry,
    verbose: bool,
) -> std::io::Result<()> {
    for w in &plan.warnings {
        writeln!(out, "{} {w}", "warning:".yellow().bold())?;
    }
    for op in &plan.ops {
        if op.skipped {
            continue;
        }
        if matches!(op.action, Action::NoOp) && !verbose {
            continue;
        }
        let sym = op.action.symbol();
        let line = format!("{sym} {}", op.urn);
        match &op.action {
            Action::Create => {
                writeln!(out, "  {}", line.green())?;
                render_inputs(out, op, registry)?;
            }
            Action::Update(d) => {
                writeln!(out, "  {}", line.yellow())?;
                render_diff(out, op, registry, d)?;
            }
            Action::Replace(d) => {
                writeln!(out, "  {}", line.magenta())?;
                render_diff(out, op, registry, d)?;
            }
            Action::Delete => writeln!(out, "  {}", line.red())?,
            Action::Trigger => writeln!(out, "  {} (triggered)", line.cyan())?,
            Action::Adopt => writeln!(
                out,
                "  {} (adopted: exists, matches, not in state)",
                line.cyan()
            )?,
            Action::NoOp => writeln!(out, "  {}", line.dimmed())?,
        }
        for approval in &op.approvals {
            writeln!(
                out,
                "      {} {}",
                "approval required:".red().bold(),
                approval.selector().bold()
            )?;
            writeln!(out, "        {}", approval.reason)?;
            match &approval.fingerprint {
                Some(fingerprint) => writeln!(out, "        fingerprint: {fingerprint}")?,
                None => writeln!(
                    out,
                    "        fingerprint: deferred until dependencies resolve"
                )?,
            }
        }
    }
    let s = plan.summary();
    writeln!(out)?;
    writeln!(
        out,
        "Plan: {} to create, {} to update, {} to replace, {} to delete, {} triggered, {} to adopt, {} unchanged.",
        s.create.to_string().green(),
        s.update.to_string().yellow(),
        s.replace.to_string().magenta(),
        s.delete.to_string().red(),
        s.trigger.to_string().cyan(),
        s.adopt.to_string().cyan(),
        s.unchanged,
    )
}

pub fn event(out: &mut impl Write, ev: &Event) -> std::io::Result<()> {
    match ev {
        Event::Started { urn, action } => {
            writeln!(
                out,
                "  {} {} {}…",
                action.symbol().bold(),
                urn.to_string().bold(),
                action.verb()
            )
        }
        Event::Finished { urn, action, .. } => {
            writeln!(
                out,
                "  {} {} {}",
                "✓".green(),
                urn,
                past_tense(action).green()
            )
        }
        Event::Failed { urn, action, error } => {
            writeln!(
                out,
                "  {} {} {} failed: {}",
                "✗".red(),
                urn,
                action.verb(),
                error.red()
            )
        }
        Event::ApprovalAccepted { approval } => writeln!(
            out,
            "  {} {}",
            "approval accepted".green(),
            approval.selector()
        ),
    }
}

pub fn report(out: &mut impl Write, r: &Report) -> std::io::Result<()> {
    writeln!(out)?;
    if !r.pending_approvals.is_empty() {
        writeln!(out, "{}", "Apply waiting for approval.".yellow().bold())?;
        for approval in &r.pending_approvals {
            writeln!(out, "  {} — {}", approval.selector(), approval.reason)?;
        }
    } else if r.ok() {
        writeln!(
            out,
            "{} {} resource(s) changed.",
            "Apply complete.".green().bold(),
            r.applied.len()
        )?;
    } else {
        writeln!(
            out,
            "{} {} succeeded, {} failed.",
            "Apply failed.".red().bold(),
            r.applied.len(),
            r.failed.len()
        )?;
        for (urn, e) in &r.failed {
            writeln!(out, "  {}: {e}", urn.to_string().red())?;
        }
    }
    Ok(())
}

pub fn outputs(out: &mut impl Write, r: &Report, registry: &Registry) -> std::io::Result<()> {
    let mut any = false;
    for (urn, o) in &r.outputs {
        let Some(obj) = o.as_object() else { continue };
        if obj.is_empty() {
            continue;
        }
        if !any {
            writeln!(out)?;
            writeln!(out, "{}", "Outputs:".bold())?;
            any = true;
        }
        let sensitive: Vec<String> = registry
            .get(urn.type_name())
            .map(|h| h.schema().sensitive_fields().map(String::from).collect())
            .unwrap_or_default();
        writeln!(out, "  {urn}")?;
        for (k, v) in obj {
            let shown = if sensitive.iter().any(|s| s == k) {
                "<sensitive>".to_string()
            } else {
                match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            };
            writeln!(out, "    {k} = {shown}")?;
        }
    }
    Ok(())
}

fn past_tense(action: &Action) -> &'static str {
    match action {
        Action::Create => "created",
        Action::Update(_) => "updated",
        Action::Replace(_) => "replaced",
        Action::Delete => "deleted",
        Action::Trigger => "triggered",
        Action::Adopt => "adopted",
        Action::NoOp => "unchanged",
    }
}
