//! Trigger-engine tests against the fake transport: the shipped
//! rule #1, predicate semantics, plan staging, audit stamps — plus
//! the adversarial half: duplicates, depth, disabled rules, dead
//! targets, and malformed rule files.

use theodolite_tui::events::{DiagnosticsSummary, Event, EventKind};
use theodolite_tui::triggers::{
    AuditSink, FakeTransport, FireOutcome, TriggerEngine, TriggerError, load_rules,
};

fn rule_text(mode: &str, where_clause: &str, max_depth: u8) -> String {
    format!(
        r#"
[[rule]]
id = "theodolite-diag-to-bloch-lesson"
origin_lane = "qompass"
content_label = "sfw"
on = {{ event = "diagnostics.completed", where = "{where_clause}" }}
then = {{ mcp = "bloch", tool = "lesson_candidate", args_from = "event" }}
mode = "{mode}"
idempotency = "event-hash"
max_depth = {max_depth}
"#
    )
}

fn summary(errors: u64, warnings: u64) -> DiagnosticsSummary {
    DiagnosticsSummary {
        workspace: "/ws".to_string(),
        server: "bacon-ls".to_string(),
        path: None,
        error_count: errors,
        warning_count: warnings,
        total_count: errors + warnings,
    }
}

fn event(errors: u64, warnings: u64, depth: u8) -> Event {
    Event {
        seq: 7,
        depth,
        kind: EventKind::DiagnosticsCompleted(summary(errors, warnings)),
    }
}

fn engine(rule_text: &str) -> TriggerEngine {
    let rules = load_rules(rule_text).expect("rules load");
    TriggerEngine::new(rules, AuditSink::memory())
}

// ---- validation ----

#[test]
fn shipped_rule_file_loads_as_rule_one() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../triggers.toml");
    let text = std::fs::read_to_string(path).expect("shipped triggers.toml exists");
    let rules = load_rules(&text).expect("shipped rules are valid");
    assert_eq!(rules.len(), 1);
    let rule = &rules[0];
    assert_eq!(rule.id, "theodolite-diag-to-bloch-lesson");
    assert_eq!(rule.origin_lane, "qompass");
    assert_eq!(rule.content_label, "sfw");
    assert_eq!(rule.event, "diagnostics.completed");
    assert_eq!(rule.target_mcp, "bloch");
    assert_eq!(rule.target_tool, "lesson_candidate");
    assert_eq!(rule.max_depth, 1);
}

#[test]
fn matching_event_fires_with_event_args_and_birth_stamps() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    let outcomes = engine.evaluate(&event(3, 1, 0), &mut transport);
    assert_eq!(
        outcomes,
        vec![FireOutcome::Fired {
            rule_id: "theodolite-diag-to-bloch-lesson".to_string()
        }]
    );
    assert_eq!(transport.calls.len(), 1);
    let (mcp, tool, args) = &transport.calls[0];
    assert_eq!(mcp, "bloch");
    assert_eq!(tool, "lesson_candidate");
    assert_eq!(args["error_count"], 3);
    assert_eq!(args["warning_count"], 1);
    assert_eq!(args["workspace"], "/ws");
    assert_eq!(args["origin_lane"], "qompass");
    assert_eq!(args["content_label"], "sfw");
}

#[test]
fn conjunction_predicate_requires_all_comparisons() {
    let mut engine = engine(&rule_text(
        "auto",
        "error_count > 0 && warning_count > 1",
        1,
    ));
    let mut transport = FakeTransport::new();
    let outcomes = engine.evaluate(&event(2, 1, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Dropped { .. }));
    assert_eq!(transport.calls.len(), 0);
    let outcomes = engine.evaluate(&event(2, 2, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Fired { .. }));
    assert_eq!(transport.calls.len(), 1);
}

#[test]
fn text_predicate_matches_server_field() {
    let mut engine = engine(&rule_text("auto", "server == bacon-ls", 1));
    let mut transport = FakeTransport::new();
    let outcomes = engine.evaluate(&event(0, 0, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Fired { .. }));
}

#[test]
fn plan_rule_stages_and_only_fires_on_approval() {
    let mut engine = engine(&rule_text("plan", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    let outcomes = engine.evaluate(&event(1, 0, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Staged { .. }));
    assert_eq!(
        transport.calls.len(),
        0,
        "plan mode must not fire before approval"
    );
    assert_eq!(engine.pending().len(), 1);
    assert_eq!(engine.pending()[0].tool, "lesson_candidate");
    let outcomes = engine.approve_pending(&mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Fired { .. }));
    assert_eq!(transport.calls.len(), 1);
    assert!(engine.pending().is_empty());
}

#[test]
fn audit_lines_carry_action_and_birth_stamps() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    engine.evaluate(&event(1, 0, 0), &mut transport);
    engine.evaluate(&event(0, 0, 0), &mut transport);
    let tail = engine.audit_tail();
    assert_eq!(tail.len(), 2);
    let fired: serde_json::Value = serde_json::from_str(&tail[0]).expect("audit is JSON");
    assert_eq!(fired["action"], "fired");
    assert_eq!(fired["origin_lane"], "qompass");
    assert_eq!(fired["content_label"], "sfw");
    assert_eq!(fired["rule_id"], "theodolite-diag-to-bloch-lesson");
    let dropped: serde_json::Value = serde_json::from_str(&tail[1]).expect("audit is JSON");
    assert_eq!(dropped["action"], "dropped");
    assert_eq!(dropped["detail"], "predicate");
}

// ---- adversarial ----

#[test]
fn duplicate_event_never_refires() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    engine.evaluate(&event(1, 0, 0), &mut transport);
    let outcomes = engine.evaluate(&event(1, 0, 0), &mut transport);
    assert_eq!(
        outcomes,
        vec![FireOutcome::Dropped {
            rule_id: "theodolite-diag-to-bloch-lesson".to_string(),
            reason: "duplicate".to_string()
        }]
    );
    assert_eq!(transport.calls.len(), 1);
}

#[test]
fn event_at_max_depth_is_dropped() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    let outcomes = engine.evaluate(&event(1, 0, 1), &mut transport);
    assert!(matches!(
        outcomes[0],
        FireOutcome::Dropped { ref reason, .. } if reason == "depth"
    ));
    assert_eq!(transport.calls.len(), 0);
}

#[test]
fn depth_above_hard_ceiling_is_rejected_at_load() {
    let error = load_rules(&rule_text("auto", "error_count > 0", 3)).expect_err("depth 3");
    assert!(matches!(error, TriggerError::Invalid(_)));
    let error = load_rules(&rule_text("auto", "error_count > 0", 0)).expect_err("depth 0");
    assert!(matches!(error, TriggerError::Invalid(_)));
}

#[test]
fn disabled_rule_drops_and_reenabling_fires() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    assert!(engine.set_enabled("theodolite-diag-to-bloch-lesson", false));
    let outcomes = engine.evaluate(&event(1, 0, 0), &mut transport);
    assert!(matches!(
        outcomes[0],
        FireOutcome::Dropped { ref reason, .. } if reason == "disabled"
    ));
    assert_eq!(transport.calls.len(), 0);
    assert!(engine.set_enabled("theodolite-diag-to-bloch-lesson", true));
    let outcomes = engine.evaluate(&event(1, 0, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Fired { .. }));
    assert!(!engine.set_enabled("no-such-rule", true));
}

#[test]
fn dead_target_is_a_failed_outcome_and_is_audited() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    transport.queue_result(Err(TriggerError::Transport(
        "bloch server unreachable".to_string(),
    )));
    let outcomes = engine.evaluate(&event(1, 0, 0), &mut transport);
    assert!(matches!(outcomes[0], FireOutcome::Failed { .. }));
    let tail = engine.audit_tail();
    let record: serde_json::Value = serde_json::from_str(&tail[0]).expect("audit is JSON");
    assert_eq!(record["action"], "fire_failed");
}

#[test]
fn approval_then_abort_discards_without_firing() {
    let mut engine = engine(&rule_text("plan", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    engine.evaluate(&event(1, 0, 0), &mut transport);
    assert_eq!(engine.discard_pending(), 1);
    assert!(engine.pending().is_empty());
    assert_eq!(transport.calls.len(), 0);
    assert!(engine.approve_pending(&mut transport).is_empty());
    assert_eq!(transport.calls.len(), 0);
}

#[test]
fn malformed_rule_files_are_rejected() {
    assert!(matches!(
        load_rules("[[rule]]\nid = "),
        Err(TriggerError::Parse(_))
    ));
    assert!(load_rules(&rule_text("sideways", "error_count > 0", 1)).is_err());
    assert!(load_rules(&rule_text("auto", "bogus_field > 0", 1)).is_err());
    assert!(load_rules(&rule_text("auto", "error_count >> 0", 1)).is_err());
    let bad_scheme = rule_text("auto", "error_count > 0", 1).replace("event-hash", "at-least-once");
    assert!(load_rules(&bad_scheme).is_err());
    let bad_args = rule_text("auto", "error_count > 0", 1)
        .replace("args_from = \"event\"", "args_from = \"payload\"");
    assert!(load_rules(&bad_args).is_err());
}

#[test]
fn unrelated_event_names_do_not_match() {
    let mut engine = engine(&rule_text("auto", "error_count > 0", 1));
    let mut transport = FakeTransport::new();
    let other = Event {
        seq: 1,
        depth: 0,
        kind: EventKind::WorkflowCompleted {
            workflow: "hover".to_string(),
        },
    };
    assert!(engine.evaluate(&other, &mut transport).is_empty());
    assert_eq!(transport.calls.len(), 0);
    assert!(engine.audit_tail().is_empty(), "no match, no audit noise");
}
