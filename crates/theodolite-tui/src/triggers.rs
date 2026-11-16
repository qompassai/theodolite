//! The cross-MCP auto-trigger layer (DESIGN SS D4).
//!
//! Rules are data (`triggers.toml`), loaded at startup and listed
//! in the Fleet panel; a rule that is not in the file does not
//! exist. Each rule matches one typed event name, applies a bounded
//! predicate over named event fields (comparisons joined by `&&`
//! only — no expression language, no eval), and fires one MCP tool
//! call on a target member through a [`TriggerTransport`].
//!
//! Safety semantics, by construction:
//! - depth cap: an event at or beyond a rule's `max_depth` never
//!   fires it; `max_depth` is validated to 1..=[`DEPTH_HARD_MAX`].
//! - idempotency: a firing is keyed by the SHA-256 of (rule id,
//!   canonical event payload); a key that already fired is dropped
//!   and logged, so duplicate events never re-fire.
//! - plan mode: a matching rule stages its call for operator
//!   approval instead of firing; nothing reaches the transport
//!   before approval.
//! - audit: every match, firing, staging, gate refusal, and drop
//!   appends one JSON line carrying the rule's `origin_lane` and
//!   `content_label` from birth (BLOCH lane law, D-1030).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::events::{DiagnosticsSummary, Event, FieldValue};

/// Hard ceiling on any rule's `max_depth` (DESIGN SS D4: default 1,
/// hard ceiling 2; cycles are impossible by construction).
pub const DEPTH_HARD_MAX: u8 = 2;
/// Maximum rules loaded from one `triggers.toml`.
pub const RULES_MAX: usize = 64;
/// Maximum remembered idempotency keys; beyond this the oldest are
/// forgotten in FIFO order (bounded memory over perfect recall).
pub const FIRED_KEYS_MAX: usize = 4096;
/// Maximum audit lines retained in memory for the Fleet panel tail.
pub const AUDIT_TAIL_MAX: usize = 256;
/// Maximum audit file size in bytes before one rotation to `.1`.
pub const AUDIT_BYTES_MAX: u64 = 1_048_576;
/// The only idempotency scheme this rung implements.
pub const IDEMPOTENCY_EVENT_HASH: &str = "event-hash";

/// A typed trigger-layer failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerError {
    /// The rule file did not parse.
    Parse(String),
    /// The rule file parsed but a rule violates the contract.
    Invalid(String),
    /// A transport call failed (target unreachable, server died).
    Transport(String),
    /// The audit log could not be written.
    Io(String),
}

impl std::fmt::Display for TriggerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerError::Parse(detail) => write!(f, "parse: {detail}"),
            TriggerError::Invalid(detail) => write!(f, "invalid: {detail}"),
            TriggerError::Transport(detail) => write!(f, "transport: {detail}"),
            TriggerError::Io(detail) => write!(f, "io: {detail}"),
        }
    }
}

impl std::error::Error for TriggerError {}

/// Whether a rule fires directly or stages for approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMode {
    /// Execute the declared call directly.
    Auto,
    /// Stage the exact call; a human approves before it fires.
    Plan,
}

/// One comparison operator in a predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `==`
    Eq,
    /// `!=`
    Ne,
}

impl Op {
    /// Apply the operator to two field values. Cross-kind
    /// comparisons (number vs text) never match, except `!=`.
    pub fn apply(&self, lhs: &FieldValue, rhs: &FieldValue) -> bool {
        match (lhs, rhs) {
            (FieldValue::Num(a), FieldValue::Num(b)) => match self {
                Op::Gt => a > b,
                Op::Ge => a >= b,
                Op::Lt => a < b,
                Op::Le => a <= b,
                Op::Eq => a == b,
                Op::Ne => a != b,
            },
            (FieldValue::Text(a), FieldValue::Text(b)) => match self {
                Op::Eq => a == b,
                Op::Ne => a != b,
                _ => false,
            },
            _ => matches!(self, Op::Ne),
        }
    }
}

/// One `field op value` comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// Named event field.
    pub field: String,
    /// Comparison operator.
    pub op: Op,
    /// Literal compared against.
    pub value: FieldValue,
}

/// A bounded predicate: a conjunction of comparisons over named
/// event fields. An empty predicate matches every event of the
/// named kind.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Predicate {
    /// Comparisons, all of which must hold.
    pub comparisons: Vec<Comparison>,
}

/// Fields a predicate may name. Anything else is rejected at load,
/// so a typo'd rule fails loudly instead of silently never firing.
const KNOWN_FIELDS: [&str; 6] = [
    "error_count",
    "warning_count",
    "total_count",
    "depth",
    "workspace",
    "server",
];

fn parse_comparison(raw: &str) -> Result<Comparison, TriggerError> {
    let invalid = |detail: &str| TriggerError::Invalid(format!("predicate {raw:?}: {detail}"));
    let op_index = raw
        .find(|c: char| ['>', '<', '=', '!'].contains(&c))
        .ok_or_else(|| invalid("no operator"))?;
    let field = raw[..op_index].trim().to_string();
    if !KNOWN_FIELDS.contains(&field.as_str()) {
        return Err(invalid("unknown field"));
    }
    let rest = raw[op_index..].trim();
    let (op, value_raw) = if let Some(v) = rest.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = rest.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = rest.strip_prefix("==") {
        (Op::Eq, v)
    } else if let Some(v) = rest.strip_prefix("!=") {
        (Op::Ne, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (Op::Gt, v)
    } else if let Some(v) = rest.strip_prefix('<') {
        (Op::Lt, v)
    } else {
        return Err(invalid("unknown operator"));
    };
    let value_raw = value_raw.trim().trim_matches('"');
    if value_raw.is_empty() || value_raw.starts_with(['>', '<', '=', '!']) {
        return Err(invalid("malformed value"));
    }
    let value = match value_raw.parse::<i64>() {
        Ok(number) => FieldValue::Num(number),
        Err(_) => FieldValue::Text(value_raw.to_string()),
    };
    Ok(Comparison { field, op, value })
}

/// Parse a `where` string: comparisons joined by `&&` only.
pub fn parse_predicate(raw: &str) -> Result<Predicate, TriggerError> {
    let mut comparisons = Vec::new();
    for part in raw.split("&&") {
        let part = part.trim();
        if part.is_empty() {
            return Err(TriggerError::Invalid(format!(
                "predicate {raw:?}: empty comparison"
            )));
        }
        comparisons.push(parse_comparison(part)?);
    }
    if comparisons.len() > 8 {
        return Err(TriggerError::Invalid(format!(
            "predicate {raw:?}: more than 8 comparisons"
        )));
    }
    Ok(Predicate { comparisons })
}

impl Predicate {
    /// Evaluate against an event's named fields. A comparison
    /// naming a field the event does not carry never matches.
    pub fn matches(&self, fields: &BTreeMap<String, FieldValue>) -> bool {
        self.comparisons.iter().all(|comparison| {
            fields
                .get(&comparison.field)
                .is_some_and(|actual| comparison.op.apply(actual, &comparison.value))
        })
    }
}

/// One trigger rule, as declared in `triggers.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Stable rule id (audit + idempotency key input).
    pub id: String,
    /// BLOCH lane stamp from birth (D-1030).
    pub origin_lane: String,
    /// BLOCH content label from birth (D-1030).
    pub content_label: String,
    /// Dotted event name to match (`on.event`).
    pub event: String,
    /// Bounded predicate over the event's fields (`on.where`).
    pub predicate: Predicate,
    /// Target MCP member name (`then.mcp`).
    pub target_mcp: String,
    /// Target tool (`then.tool`).
    pub target_tool: String,
    /// `args_from` — this rung implements `"event"` only.
    pub args_from: String,
    /// Fire directly or stage for approval.
    pub mode: RuleMode,
    /// Idempotency scheme name; must be `"event-hash"`.
    pub idempotency: String,
    /// Depth cap for this rule, 1..=[`DEPTH_HARD_MAX`].
    pub max_depth: u8,
    /// Operator enable state; disabling in the Fleet panel takes
    /// effect without restart.
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    origin_lane: String,
    content_label: String,
    on: RawOn,
    then: RawThen,
    mode: String,
    idempotency: String,
    max_depth: u8,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOn {
    event: String,
    #[serde(default)]
    r#where: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawThen {
    mcp: String,
    tool: String,
    #[serde(default = "default_args_from")]
    args_from: String,
}

fn default_args_from() -> String {
    "event".to_string()
}

/// Load and validate rules from TOML text. Every contract violation
/// is a typed refusal at load: unknown fields, an unknown predicate
/// field, a depth above the hard ceiling, or an unimplemented
/// idempotency / args scheme all stop the load, so the Fleet panel
/// can never list a rule the engine would interpret differently.
pub fn load_rules(text: &str) -> Result<Vec<Rule>, TriggerError> {
    let raw: RawRules =
        toml::from_str(text).map_err(|error| TriggerError::Parse(error.to_string()))?;
    if raw.rule.len() > RULES_MAX {
        return Err(TriggerError::Invalid(format!(
            "{} rules exceeds RULES_MAX {RULES_MAX}",
            raw.rule.len()
        )));
    }
    let mut rules = Vec::with_capacity(raw.rule.len());
    for item in raw.rule {
        if item.id.is_empty() || item.on.event.is_empty() {
            return Err(TriggerError::Invalid(
                "rule id and on.event are required".into(),
            ));
        }
        if item.max_depth == 0 || item.max_depth > DEPTH_HARD_MAX {
            return Err(TriggerError::Invalid(format!(
                "rule {:?}: max_depth {} outside 1..={DEPTH_HARD_MAX}",
                item.id, item.max_depth
            )));
        }
        if item.idempotency != IDEMPOTENCY_EVENT_HASH {
            return Err(TriggerError::Invalid(format!(
                "rule {:?}: idempotency {:?} not implemented",
                item.id, item.idempotency
            )));
        }
        if item.then.args_from != "event" {
            return Err(TriggerError::Invalid(format!(
                "rule {:?}: args_from {:?} not implemented",
                item.id, item.then.args_from
            )));
        }
        let mode = match item.mode.as_str() {
            "auto" => RuleMode::Auto,
            "plan" => RuleMode::Plan,
            other => {
                return Err(TriggerError::Invalid(format!(
                    "rule {:?}: unknown mode {other:?}",
                    item.id
                )));
            }
        };
        let predicate = if item.on.r#where.trim().is_empty() {
            Predicate::default()
        } else {
            parse_predicate(&item.on.r#where)?
        };
        rules.push(Rule {
            id: item.id,
            origin_lane: item.origin_lane,
            content_label: item.content_label,
            event: item.on.event,
            predicate,
            target_mcp: item.then.mcp,
            target_tool: item.then.tool,
            args_from: item.then.args_from,
            mode,
            idempotency: item.idempotency,
            max_depth: item.max_depth,
            enabled: item.enabled,
        });
    }
    Ok(rules)
}

/// The transport a trigger firing travels over: one MCP tool call
/// to a target member. The real deployment wires this to the
/// member's MCP client transport; tests use [`FakeTransport`].
pub trait TriggerTransport {
    /// Call `tool` on member `mcp` with `args`.
    fn call(&mut self, mcp: &str, tool: &str, args: &Value) -> Result<Value, TriggerError>;
}

/// An in-memory [`TriggerTransport`] for tests: records calls,
/// returns scripted results, and can simulate a dead target.
#[derive(Debug, Default)]
pub struct FakeTransport {
    /// Every `(mcp, tool, args)` call, in order.
    pub calls: Vec<(String, String, Value)>,
    /// Results for successive calls; empty queue echoes the args.
    pub results: VecDeque<Result<Value, TriggerError>>,
}

impl FakeTransport {
    /// A fake transport with an empty script.
    pub fn new() -> FakeTransport {
        FakeTransport::default()
    }

    /// Queue one result for the next call.
    pub fn queue_result(&mut self, result: Result<Value, TriggerError>) {
        self.results.push_back(result);
    }
}

impl TriggerTransport for FakeTransport {
    fn call(&mut self, mcp: &str, tool: &str, args: &Value) -> Result<Value, TriggerError> {
        self.calls
            .push((mcp.to_string(), tool.to_string(), args.clone()));
        self.results
            .pop_front()
            .unwrap_or_else(|| Ok(serde_json::json!({"accepted": true})))
    }
}

/// One staged plan-mode firing awaiting operator approval.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingFiring {
    /// Rule that staged the call.
    pub rule_id: String,
    /// Target member.
    pub mcp: String,
    /// Target tool.
    pub tool: String,
    /// Exact arguments that will be sent on approval.
    pub args: Value,
    /// The staging rule's birth stamp, carried so the approval
    /// audit line keeps it even if the rule is later edited.
    pub origin_lane: String,
    /// The staging rule's content label (see `origin_lane`).
    pub content_label: String,
}

/// What one rule evaluation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// The call reached the transport.
    Fired {
        /// Rule id.
        rule_id: String,
    },
    /// Plan mode: the call is staged, awaiting approval.
    Staged {
        /// Rule id.
        rule_id: String,
    },
    /// The rule matched (or would have) but was dropped.
    Dropped {
        /// Rule id.
        rule_id: String,
        /// Why: `disabled`, `depth`, `duplicate`, or `predicate`.
        reason: String,
    },
    /// The call was attempted and the transport refused it.
    Failed {
        /// Rule id.
        rule_id: String,
        /// Transport error text.
        error: String,
    },
}

/// The audit sink: an in-memory bounded tail for the Fleet panel
/// plus an optional JSONL file with one bounded rotation.
#[derive(Debug, Default)]
pub struct AuditSink {
    path: Option<PathBuf>,
    tail: VecDeque<String>,
}

impl AuditSink {
    /// An in-memory-only sink (tests, line mode without `--audit`).
    pub fn memory() -> AuditSink {
        AuditSink::default()
    }

    /// A sink appending JSONL to `path` (created on first write).
    pub fn file(path: PathBuf) -> AuditSink {
        AuditSink {
            path: Some(path),
            tail: VecDeque::new(),
        }
    }

    /// The most recent audit lines, oldest first.
    pub fn tail(&self) -> Vec<String> {
        self.tail.iter().cloned().collect()
    }

    fn append(&mut self, record: &Value) -> Result<(), TriggerError> {
        let line = record.to_string();
        if let Some(path) = &self.path {
            rotate_if_needed(path)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| TriggerError::Io(error.to_string()))?;
            writeln!(file, "{line}").map_err(|error| TriggerError::Io(error.to_string()))?;
        }
        self.tail.push_back(line);
        while self.tail.len() > AUDIT_TAIL_MAX {
            self.tail.pop_front();
        }
        Ok(())
    }
}

fn rotate_if_needed(path: &std::path::Path) -> Result<(), TriggerError> {
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size > AUDIT_BYTES_MAX {
        let rotated = path.with_extension("jsonl.1");
        std::fs::rename(path, rotated).map_err(|error| TriggerError::Io(error.to_string()))?;
    }
    Ok(())
}

/// Build the tool arguments for a firing from the event
/// (`args_from = "event"`): the diagnostics summary plus the rule's
/// birth stamps, so the target's intake sees the lane and label the
/// rule was born with.
pub fn args_from_event(rule: &Rule, summary: &DiagnosticsSummary) -> Value {
    serde_json::json!({
        "workspace": summary.workspace,
        "server": summary.server,
        "path": summary.path,
        "error_count": summary.error_count,
        "warning_count": summary.warning_count,
        "total_count": summary.total_count,
        "summary": summary.describe(),
        "origin_lane": rule.origin_lane,
        "content_label": rule.content_label,
    })
}

fn idempotency_key(rule_id: &str, event: &Event) -> String {
    let mut hasher = Sha256::new();
    hasher.update(rule_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(event.to_json().to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The rule engine: evaluates events against loaded rules, fires
/// through a transport, and audits everything it decides.
#[derive(Debug)]
pub struct TriggerEngine {
    rules: Vec<Rule>,
    fired_keys: HashSet<String>,
    fired_order: VecDeque<String>,
    pending: Vec<PendingFiring>,
    audit: AuditSink,
    audit_seq: u64,
}

impl TriggerEngine {
    /// An engine over `rules`, auditing into `audit`.
    pub fn new(rules: Vec<Rule>, audit: AuditSink) -> TriggerEngine {
        TriggerEngine {
            rules,
            fired_keys: HashSet::new(),
            fired_order: VecDeque::new(),
            pending: Vec::new(),
            audit,
            audit_seq: 0,
        }
    }

    /// The loaded rules, as the Fleet panel lists them.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Enable or disable one rule by id; takes effect on the next
    /// evaluation, without restart. Returns false for unknown ids.
    pub fn set_enabled(&mut self, rule_id: &str, enabled: bool) -> bool {
        match self.rules.iter_mut().find(|rule| rule.id == rule_id) {
            Some(rule) => {
                rule.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Staged plan-mode firings awaiting approval.
    pub fn pending(&self) -> &[PendingFiring] {
        &self.pending
    }

    /// The audit tail for the Fleet panel.
    pub fn audit_tail(&self) -> Vec<String> {
        self.audit.tail()
    }

    fn record(
        &mut self,
        rule: &Rule,
        event: &Event,
        action: &str,
        detail: &str,
    ) -> Result<(), TriggerError> {
        self.record_parts(
            &rule.id,
            &rule.origin_lane,
            &rule.content_label,
            event.name(),
            action,
            detail,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_parts(
        &mut self,
        rule_id: &str,
        origin_lane: &str,
        content_label: &str,
        event_name: &str,
        action: &str,
        detail: &str,
    ) -> Result<(), TriggerError> {
        self.audit_seq += 1;
        let record = serde_json::json!({
            "seq": self.audit_seq,
            "action": action,
            "rule_id": rule_id,
            "event": event_name,
            "detail": detail,
            "origin_lane": origin_lane,
            "content_label": content_label,
        });
        self.audit.append(&record)
    }

    fn remember_key(&mut self, key: String) {
        self.fired_keys.insert(key.clone());
        self.fired_order.push_back(key);
        while self.fired_order.len() > FIRED_KEYS_MAX {
            if let Some(oldest) = self.fired_order.pop_front() {
                self.fired_keys.remove(&oldest);
            }
        }
    }

    /// Evaluate one event against every rule. Diagnostics events
    /// are the only ones carrying summary payloads this rung fires
    /// on; other events are evaluated for name/predicate matches but
    /// only rules on `diagnostics.completed` produce calls, because
    /// `args_from = "event"` is defined over the summary.
    pub fn evaluate(
        &mut self,
        event: &Event,
        transport: &mut dyn TriggerTransport,
    ) -> Vec<FireOutcome> {
        let mut outcomes = Vec::new();
        let summary = match &event.kind {
            crate::events::EventKind::DiagnosticsCompleted(summary) => Some(summary.clone()),
            _ => None,
        };
        for index in 0..self.rules.len() {
            let rule = self.rules[index].clone();
            if rule.event != event.name() {
                continue;
            }
            if !rule.enabled {
                outcomes.push(self.drop(&rule, event, "disabled"));
                continue;
            }
            if !rule.predicate.matches(&event.fields()) {
                outcomes.push(self.drop(&rule, event, "predicate"));
                continue;
            }
            if event.depth >= rule.max_depth {
                outcomes.push(self.drop(&rule, event, "depth"));
                continue;
            }
            let Some(summary) = &summary else {
                outcomes.push(self.drop(&rule, event, "no-payload"));
                continue;
            };
            let key = idempotency_key(&rule.id, event);
            if self.fired_keys.contains(&key) {
                outcomes.push(self.drop(&rule, event, "duplicate"));
                continue;
            }
            self.remember_key(key);
            let args = args_from_event(&rule, summary);
            match rule.mode {
                RuleMode::Plan => {
                    self.pending.push(PendingFiring {
                        rule_id: rule.id.clone(),
                        mcp: rule.target_mcp.clone(),
                        tool: rule.target_tool.clone(),
                        args,
                        origin_lane: rule.origin_lane.clone(),
                        content_label: rule.content_label.clone(),
                    });
                    let _ = self.record(&rule, event, "staged", "plan mode: awaiting approval");
                    outcomes.push(FireOutcome::Staged { rule_id: rule.id });
                }
                RuleMode::Auto => outcomes.push(self.fire(&rule, event, &args, transport)),
            }
        }
        outcomes
    }

    fn drop(&mut self, rule: &Rule, event: &Event, reason: &str) -> FireOutcome {
        let _ = self.record(rule, event, "dropped", reason);
        FireOutcome::Dropped {
            rule_id: rule.id.clone(),
            reason: reason.to_string(),
        }
    }

    fn fire(
        &mut self,
        rule: &Rule,
        event: &Event,
        args: &Value,
        transport: &mut dyn TriggerTransport,
    ) -> FireOutcome {
        match transport.call(&rule.target_mcp, &rule.target_tool, args) {
            Ok(_) => {
                let _ = self.record(
                    rule,
                    event,
                    "fired",
                    &format!("{} {}", rule.target_mcp, rule.target_tool),
                );
                FireOutcome::Fired {
                    rule_id: rule.id.clone(),
                }
            }
            Err(error) => {
                let _ = self.record(rule, event, "fire_failed", &error.to_string());
                FireOutcome::Failed {
                    rule_id: rule.id.clone(),
                    error: error.to_string(),
                }
            }
        }
    }

    /// Fire every staged plan-mode call, in staging order. Called
    /// only on explicit operator approval; aborting instead is
    /// [`TriggerEngine::discard_pending`].
    pub fn approve_pending(&mut self, transport: &mut dyn TriggerTransport) -> Vec<FireOutcome> {
        let staged = std::mem::take(&mut self.pending);
        let mut outcomes = Vec::with_capacity(staged.len());
        for firing in staged {
            let outcome = match transport.call(&firing.mcp, &firing.tool, &firing.args) {
                Ok(_) => {
                    let _ = self.record_parts(
                        &firing.rule_id,
                        &firing.origin_lane,
                        &firing.content_label,
                        "trigger.approved",
                        "fired",
                        "approved plan firing",
                    );
                    FireOutcome::Fired {
                        rule_id: firing.rule_id.clone(),
                    }
                }
                Err(error) => {
                    let _ = self.record_parts(
                        &firing.rule_id,
                        &firing.origin_lane,
                        &firing.content_label,
                        "trigger.approved",
                        "fire_failed",
                        &error.to_string(),
                    );
                    FireOutcome::Failed {
                        rule_id: firing.rule_id.clone(),
                        error: error.to_string(),
                    }
                }
            };
            outcomes.push(outcome);
        }
        outcomes
    }

    /// Discard every staged plan-mode call without firing any of
    /// them (the approval-then-abort path).
    pub fn discard_pending(&mut self) -> usize {
        let count = self.pending.len();
        self.pending.clear();
        count
    }
}
