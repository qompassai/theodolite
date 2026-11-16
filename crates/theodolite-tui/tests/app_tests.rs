//! App tests over the fake facade: screen transitions, workflow
//! staging/execution, the real diagnostics event path, and line
//! mode — plus the adversarial half: server death, oversized
//! payloads, aborts, and malformed results.

use serde_json::Value;
use theodolite_tui::app::{App, AppError, DiagnosticsRequest, RunMode, Screen};
use theodolite_tui::events::EventKind;
use theodolite_tui::facade::{FacadeError, FakeFacade};
use theodolite_tui::triggers::{AuditSink, FakeTransport, TriggerEngine, load_rules};
use theodolite_tui::{LINE_INPUT_BYTES_MAX, LOG_LINES_MAX, screens};

const RULE_AUTO: &str = r#"
[[rule]]
id = "theodolite-diag-to-bloch-lesson"
origin_lane = "qompass"
content_label = "sfw"
on = { event = "diagnostics.completed", where = "error_count > 0" }
then = { mcp = "bloch", tool = "lesson_candidate", args_from = "event" }
mode = "auto"
idempotency = "event-hash"
max_depth = 1
"#;

fn make_app(rule_text: &str) -> App<FakeFacade, FakeTransport> {
    let rules = load_rules(rule_text).expect("rules load");
    let engine = TriggerEngine::new(rules, AuditSink::memory());
    App::new(
        FakeFacade::new(),
        FakeTransport::new(),
        engine,
        vec!["/ws".to_string()],
    )
}

fn diag_result(severities: &[u64]) -> Value {
    let items: Vec<Value> = severities
        .iter()
        .enumerate()
        .map(|(index, severity)| {
            serde_json::json!({
                "uri": format!("src/file{index}.rs"),
                "severity": severity,
                "message": format!("problem {index}"),
                "source": "test",
                "code": "E0000",
            })
        })
        .collect();
    serde_json::json!({"count": items.len(), "diagnostics": items})
}

fn request() -> DiagnosticsRequest {
    DiagnosticsRequest {
        workspace: "/ws".to_string(),
        server: "bacon-ls".to_string(),
        path: None,
    }
}

// ---- validation ----

#[test]
fn overview_model_shows_status_routes_and_sessions() {
    let mut app = make_app("");
    let model = app.screen_model();
    assert_eq!(model.screen, Screen::Overview);
    assert!(
        model
            .status_lines
            .iter()
            .any(|line| line.contains("theodolite"))
    );
    assert!(
        model
            .route_lines
            .iter()
            .any(|line| line.contains("bacon-ls"))
    );
    assert!(
        model
            .status_lines
            .iter()
            .any(|line| line.contains("sessions opened: 0"))
    );
}

#[test]
fn screen_cycle_visits_all_four_screens() {
    let mut app = make_app("");
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(app.screen());
        app.cycle_screen();
    }
    assert_eq!(
        seen,
        vec![
            Screen::Overview,
            Screen::Workflows,
            Screen::Log,
            Screen::Fleet
        ]
    );
    assert_eq!(app.screen(), Screen::Overview);
}

#[test]
fn plan_mode_stages_without_any_facade_call() {
    let mut app = make_app("");
    assert_eq!(app.mode(), RunMode::Plan);
    let outcome = app.run_diagnostics(&request()).expect("staging succeeds");
    assert!(outcome.contains("staged"));
    assert_eq!(
        app.facade().call_count(),
        0,
        "plan mode must not call before approval"
    );
    assert!(
        app.staged_description()
            .is_some_and(|d| d.contains("diagnostics"))
    );
}

#[test]
fn approve_executes_the_staged_call_exactly_once() {
    let mut app = make_app("");
    app.facade_mut().queue_call_result(Ok(diag_result(&[1])));
    app.run_diagnostics(&request()).expect("staged");
    let outcome = app.approve().expect("approval executes");
    assert!(outcome.contains("diagnostics completed"));
    assert_eq!(app.facade().call_count(), 1);
    let (tool, args) = &app.facade().calls[0];
    assert_eq!(tool, "diagnostics");
    assert_eq!(args["workspace"], "/ws");
    assert_eq!(args["server"], "bacon-ls");
}

#[test]
fn auto_mode_executes_immediately() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    app.facade_mut().queue_call_result(Ok(diag_result(&[])));
    app.run_diagnostics(&request()).expect("auto run");
    assert_eq!(app.facade().call_count(), 1);
    assert!(app.staged_description().is_none());
}

#[test]
fn diagnostics_run_emits_completed_event_with_counts() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    app.facade_mut()
        .queue_call_result(Ok(diag_result(&[1, 1, 2, 3])));
    app.run_diagnostics(&request()).expect("run");
    let summary = app.last_summary().expect("summary recorded");
    assert_eq!(summary.error_count, 2);
    assert_eq!(summary.warning_count, 1);
    assert_eq!(summary.total_count, 4);
    assert!(
        app.events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::DiagnosticsCompleted(_)))
    );
    assert!(
        app.log_lines()
            .iter()
            .any(|line| line.contains("diag sev=1"))
    );
    assert_eq!(app.session_count(), 1);
}

#[test]
fn workflow_event_fires_the_bloch_trigger() {
    let mut app = make_app(RULE_AUTO);
    app.set_mode(RunMode::Auto);
    app.facade_mut().queue_call_result(Ok(diag_result(&[1])));
    app.run_diagnostics(&request()).expect("run");
    assert_eq!(app.transport().calls.len(), 1);
    let (mcp, tool, args) = &app.transport().calls[0];
    assert_eq!(mcp, "bloch");
    assert_eq!(tool, "lesson_candidate");
    assert_eq!(args["error_count"], 1);
    assert_eq!(args["origin_lane"], "qompass");
    assert_eq!(args["content_label"], "sfw");
}

#[test]
fn clean_run_does_not_fire_the_trigger() {
    let mut app = make_app(RULE_AUTO);
    app.set_mode(RunMode::Auto);
    app.facade_mut().queue_call_result(Ok(diag_result(&[2])));
    app.run_diagnostics(&request()).expect("run");
    assert_eq!(app.transport().calls.len(), 0);
}

#[test]
fn line_mode_status_diag_and_fleet() {
    let mut app = make_app(RULE_AUTO);
    let status = app.execute_line_command("status").expect("status");
    assert!(status.contains("theodolite"));
    app.execute_line_command("mode auto").expect("mode");
    app.facade_mut().queue_call_result(Ok(diag_result(&[1])));
    let outcome = app
        .execute_line_command("diag /ws bacon-ls")
        .expect("diag via line mode");
    assert!(outcome.contains("diagnostics completed"));
    assert_eq!(
        app.transport().calls.len(),
        1,
        "line mode shares the trigger path"
    );
    let fleet = app.execute_line_command("fleet").expect("fleet");
    assert!(fleet.contains("theodolite-diag-to-bloch-lesson"));
}

#[test]
fn first_screen_renders_headlessly() {
    let mut app = make_app(RULE_AUTO);
    let model = app.screen_model();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| screens::render(frame, &model))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(
        text.contains("theodolite"),
        "overview shows the member name"
    );
    assert!(text.contains("Overview"));
}

#[test]
fn fleet_screen_renders_rules_and_audit() {
    let mut app = make_app(RULE_AUTO);
    app.set_screen(Screen::Fleet);
    let model = app.screen_model();
    let backend = ratatui::backend::TestBackend::new(100, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| screens::render(frame, &model))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("theodolite-diag-to-bloch-lesson"));
}

#[test]
fn workspace_picker_sets_and_cycles() {
    let mut app = make_app("");
    app.set_workspace("/other".to_string());
    assert_eq!(app.workspace(), "/other");
    app.cycle_workspace(1);
    assert_eq!(app.workspace(), "/ws");
    app.cycle_workspace(-1);
    assert_eq!(app.workspace(), "/other");
}

// ---- adversarial ----

#[test]
fn approve_with_nothing_staged_is_a_typed_refusal() {
    let mut app = make_app("");
    let error = app.approve().expect_err("nothing staged");
    assert!(matches!(error, AppError::Invalid(_)));
    assert_eq!(app.facade().call_count(), 0);
}

#[test]
fn abort_after_staging_executes_nothing() {
    let mut app = make_app("");
    app.run_diagnostics(&request()).expect("staged");
    let outcome = app.abort();
    assert!(outcome.contains("aborted"));
    assert_eq!(app.facade().call_count(), 0);
    assert!(app.staged_description().is_none());
    assert!(
        app.approve().is_err(),
        "aborted staging cannot be approved later"
    );
}

#[test]
fn server_death_mid_call_is_typed_and_recoverable() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    app.facade_mut()
        .queue_call_result(Err(FacadeError::Unavailable(
            "server process exited mid-call".to_string(),
        )));
    let error = app.run_diagnostics(&request()).expect_err("death surfaces");
    assert!(matches!(
        error,
        AppError::Facade(FacadeError::Unavailable(_))
    ));
    assert!(
        app.events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::RunFailed { .. }))
    );
    // The app is not wedged: a later run succeeds.
    app.facade_mut().queue_call_result(Ok(diag_result(&[])));
    app.run_diagnostics(&request()).expect("recovery run");
    assert!(app.last_summary().is_some());
}

#[test]
fn oversized_result_is_rejected_not_rendered() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    let huge = serde_json::json!({"diagnostics": "x".repeat(1_100_000)});
    app.facade_mut().queue_call_result(Ok(huge));
    let error = app
        .run_diagnostics(&request())
        .expect_err("oversized refused");
    assert!(matches!(error, AppError::Facade(FacadeError::Oversized(_))));
}

#[test]
fn malformed_diagnostics_payload_counts_zero_without_panic() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    app.facade_mut()
        .queue_call_result(Ok(serde_json::json!({"diagnostics": "junk"})));
    app.run_diagnostics(&request())
        .expect("run tolerates malformed payload");
    let summary = app.last_summary().expect("summary");
    assert_eq!(summary.total_count, 0);
    assert_eq!(summary.error_count, 0);
}

#[test]
fn oversized_line_input_is_rejected() {
    let mut app = make_app("");
    let line = "x".repeat(LINE_INPUT_BYTES_MAX + 1);
    let error = app.execute_line_command(&line).expect_err("oversized line");
    assert!(matches!(error, AppError::Invalid(_)));
}

#[test]
fn unknown_command_and_unknown_server_are_refused() {
    let mut app = make_app("");
    assert!(app.execute_line_command("frobnicate").is_err());
    assert!(app.execute_line_command("server nope-ls").is_err());
    assert!(app.execute_line_command("mode sideways").is_err());
}

#[test]
fn empty_workspace_diagnostics_is_refused_before_any_call() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    let bad = DiagnosticsRequest {
        workspace: String::new(),
        server: "bacon-ls".to_string(),
        path: None,
    };
    assert!(app.run_diagnostics(&bad).is_err());
    assert_eq!(app.facade().call_count(), 0);
}

#[test]
fn log_buffer_stays_bounded_under_a_diagnostic_flood() {
    let mut app = make_app("");
    app.set_mode(RunMode::Auto);
    let flood: Vec<u64> = vec![1; 600];
    app.facade_mut().queue_call_result(Ok(diag_result(&flood)));
    app.run_diagnostics(&request()).expect("flood run");
    assert!(app.log_lines().len() <= LOG_LINES_MAX);
    assert_eq!(app.last_summary().expect("summary").total_count, 600);
}
