//! Typed events on the member's own event stream.
//!
//! One stream feeds three consumers: the Log screen renders it, the
//! app state machine folds it into screen state, and the trigger
//! engine (DESIGN SS D4) evaluates rules against it. Events are
//! typed values, never free-form strings, so a rule's `on.event`
//! name and `where` fields resolve against a closed field set.

use std::collections::BTreeMap;

use serde_json::Value;

/// A field value a trigger predicate can compare against: numbers
/// and text only (DESIGN SS D4: comparisons and boolean
/// combinations, no expression language).
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// An integer event field (counts, depth).
    Num(i64),
    /// A text event field (workspace, server).
    Text(String),
}

/// The counts one completed diagnostics run reports — the payload of
/// the canonical cross-MCP trigger source event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsSummary {
    /// Workspace root the run covered.
    pub workspace: String,
    /// Configured server id that produced the diagnostics.
    pub server: String,
    /// Single file the run was scoped to, when it was.
    pub path: Option<String>,
    /// Severity-1 (error) diagnostic count.
    pub error_count: u64,
    /// Severity-2 (warning) diagnostic count.
    pub warning_count: u64,
    /// Total diagnostic count across all severities.
    pub total_count: u64,
}

impl DiagnosticsSummary {
    /// The named fields trigger predicates may read, sorted by name
    /// so predicate evaluation and idempotency hashing are
    /// deterministic.
    pub fn fields(&self) -> BTreeMap<String, FieldValue> {
        let mut fields = BTreeMap::new();
        fields.insert(
            "error_count".to_string(),
            FieldValue::Num(i64::try_from(self.error_count).unwrap_or(i64::MAX)),
        );
        fields.insert(
            "warning_count".to_string(),
            FieldValue::Num(i64::try_from(self.warning_count).unwrap_or(i64::MAX)),
        );
        fields.insert(
            "total_count".to_string(),
            FieldValue::Num(i64::try_from(self.total_count).unwrap_or(i64::MAX)),
        );
        fields.insert(
            "workspace".to_string(),
            FieldValue::Text(self.workspace.clone()),
        );
        fields.insert("server".to_string(), FieldValue::Text(self.server.clone()));
        fields
    }

    /// Canonical JSON payload: the idempotency hash input and the
    /// audit record's event body.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "workspace": self.workspace,
            "server": self.server,
            "path": self.path,
            "error_count": self.error_count,
            "warning_count": self.warning_count,
            "total_count": self.total_count,
        })
    }

    /// One-line human summary, used in log lines and trigger args.
    pub fn describe(&self) -> String {
        format!(
            "{} errors, {} warnings, {} total via {} in {}",
            self.error_count, self.warning_count, self.total_count, self.server, self.workspace
        )
    }
}

/// Build a summary from one bridge `diagnostics` result: the result
/// carries `diagnostics` items with LSP `severity` (1 error,
/// 2 warning); a missing or malformed array yields zero counts
/// rather than a panic — the caller's typed result already proved
/// the envelope, and counts are derived data.
pub fn summary_from_result(
    workspace: &str,
    server: &str,
    path: Option<&str>,
    result: &Value,
) -> DiagnosticsSummary {
    let mut error_count: u64 = 0;
    let mut warning_count: u64 = 0;
    let mut total_count: u64 = 0;
    if let Some(items) = result.get("diagnostics").and_then(Value::as_array) {
        total_count = items.len() as u64;
        for item in items {
            match item.get("severity").and_then(Value::as_u64) {
                Some(1) => error_count += 1,
                Some(2) => warning_count += 1,
                _ => {}
            }
        }
    }
    DiagnosticsSummary {
        workspace: workspace.to_string(),
        server: server.to_string(),
        path: path.map(str::to_string),
        error_count,
        warning_count,
        total_count,
    }
}

/// The event kinds the member's workflows genuinely produce.
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// A workflow run started.
    RunStarted {
        /// Workflow name (the tool being run).
        workflow: String,
    },
    /// A workflow reported progress.
    Progress {
        /// Progress message.
        message: String,
    },
    /// One diagnostics run completed; the trigger source event.
    DiagnosticsCompleted(DiagnosticsSummary),
    /// A workflow run completed (non-diagnostics tools).
    WorkflowCompleted {
        /// Workflow name.
        workflow: String,
    },
    /// A workflow run failed; the error is bounded text.
    RunFailed {
        /// Workflow name.
        workflow: String,
        /// Failure description.
        error: String,
    },
}

/// One event on the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Monotonic sequence number within the owning app.
    pub seq: u64,
    /// Trigger depth: 0 for operator-initiated runs; events produced
    /// by a triggered call carry their depth so rules can cap chains
    /// (DESIGN SS D4).
    pub depth: u8,
    /// The event payload.
    pub kind: EventKind,
}

impl Event {
    /// The dotted event name rules match against (`on.event`).
    pub fn name(&self) -> &'static str {
        match &self.kind {
            EventKind::RunStarted { .. } => "workflow.started",
            EventKind::Progress { .. } => "workflow.progress",
            EventKind::DiagnosticsCompleted(_) => "diagnostics.completed",
            EventKind::WorkflowCompleted { .. } => "workflow.completed",
            EventKind::RunFailed { .. } => "workflow.failed",
        }
    }

    /// Named predicate fields for this event: the diagnostics fields
    /// plus `depth`. Non-diagnostics events expose only `depth`, so
    /// a rule naming a count field simply never matches them.
    pub fn fields(&self) -> BTreeMap<String, FieldValue> {
        let mut fields = BTreeMap::new();
        fields.insert("depth".to_string(), FieldValue::Num(i64::from(self.depth)));
        if let EventKind::DiagnosticsCompleted(summary) = &self.kind {
            fields.extend(summary.fields());
        }
        fields
    }

    /// Canonical JSON for idempotency hashing and audit records.
    pub fn to_json(&self) -> Value {
        let body = match &self.kind {
            EventKind::DiagnosticsCompleted(summary) => summary.to_json(),
            EventKind::RunStarted { workflow } => serde_json::json!({"workflow": workflow}),
            EventKind::Progress { message } => serde_json::json!({"message": message}),
            EventKind::WorkflowCompleted { workflow } => {
                serde_json::json!({"workflow": workflow})
            }
            EventKind::RunFailed { workflow, error } => {
                serde_json::json!({"workflow": workflow, "error": error})
            }
        };
        serde_json::json!({"name": self.name(), "depth": self.depth, "payload": body})
    }

    /// One log line rendering of the event.
    pub fn describe(&self) -> String {
        match &self.kind {
            EventKind::RunStarted { workflow } => format!("run started: {workflow}"),
            EventKind::Progress { message } => format!("progress: {message}"),
            EventKind::DiagnosticsCompleted(summary) => {
                format!("diagnostics completed: {}", summary.describe())
            }
            EventKind::WorkflowCompleted { workflow } => format!("run completed: {workflow}"),
            EventKind::RunFailed { workflow, error } => {
                format!("run failed: {workflow}: {error}")
            }
        }
    }
}
