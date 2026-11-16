//! The app: event loop state, the screen state machine, workflow
//! staging/execution, and the line-mode command path.
//!
//! One execute path serves both modes (DESIGN SS D2): the
//! full-screen TUI and the non-TTY line mode call the same
//! `run_workflow` / `approve` / `abort` methods, so plan mode's
//! guarantee — no facade call before approval — holds identically
//! in both. Diagnostics runs emit real typed events onto the stream
//! the Log screen renders and the trigger engine consumes.

use std::collections::{HashSet, VecDeque};

use serde_json::Value;

use crate::WORKSPACE_CANDIDATES_MAX;
use crate::events::{Event, EventKind, summary_from_result};
use crate::facade::{BridgeFacade, FacadeError, ServerInfo};
use crate::panels::truncate_text;
use crate::screens::ScreenModel;
use crate::triggers::{FireOutcome, TriggerEngine, TriggerTransport};
use crate::{DIAGNOSTIC_LINES_MAX, EVENTS_MAX, LINE_INPUT_BYTES_MAX, LOG_LINES_MAX};

/// The screens, in Tab order. No Media screen: theodolite has no
/// media use case (DESIGN SS D2 — absent, not stubbed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Member status, routes, session count.
    Overview,
    /// Target picker + diagnostics workflow + run controls.
    Workflows,
    /// Bounded scrolling event/log view.
    Log,
    /// Trigger rules + audit tail.
    Fleet,
}

/// Run control for workflows: plan stages the exact call for
/// approval first; auto executes directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Stage the call; nothing executes before approval.
    Plan,
    /// Execute declared operations directly.
    Auto,
}

/// Why the provider control is disabled on the Workflows screen:
/// theodolite routes no model calls, so there is nothing to route.
/// Shown verbatim in the UI — never a fake control (DESIGN SS D3).
pub const PROVIDER_NOTE: &str = "disabled — theodolite routes no model calls";

/// A typed app failure (bad operator input, or a facade failure the
/// caller should see rather than have hidden).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    /// Operator input violated a contract.
    Invalid(String),
    /// The facade refused the call.
    Facade(FacadeError),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Invalid(detail) => write!(f, "invalid: {detail}"),
            AppError::Facade(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AppError {}

/// One diagnostics workflow request: a workspace, a configured
/// server, and an optional single-file scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsRequest {
    /// Workspace root directory.
    pub workspace: String,
    /// Configured server id.
    pub server: String,
    /// Optional file to scope to (absolute or workspace-relative).
    pub path: Option<String>,
}

impl DiagnosticsRequest {
    /// The exact tool arguments this request executes with — the
    /// value plan mode shows for approval.
    pub fn args(&self) -> Value {
        let mut args = serde_json::json!({
            "workspace": self.workspace,
            "server": self.server,
        });
        if let Some(path) = &self.path {
            args["path"] = Value::String(path.clone());
        }
        args
    }
}

/// The application state machine over a facade, a trigger engine,
/// and a trigger transport.
pub struct App<F: BridgeFacade, T: TriggerTransport> {
    facade: F,
    transport: T,
    engine: TriggerEngine,
    screen: Screen,
    mode: RunMode,
    events: VecDeque<Event>,
    log_lines: VecDeque<String>,
    next_seq: u64,
    staged: Option<(String, Value)>,
    workspaces: Vec<String>,
    workspace_idx: usize,
    servers: Vec<ServerInfo>,
    server_idx: usize,
    path: Option<String>,
    sessions_seen: HashSet<String>,
    last_summary: Option<crate::events::DiagnosticsSummary>,
    quit: bool,
}

impl<F: BridgeFacade, T: TriggerTransport> App<F, T> {
    /// Build the app: the server list is read from the facade once
    /// (routes are config data; a restart reloads them).
    pub fn new(
        mut facade: F,
        transport: T,
        engine: TriggerEngine,
        workspaces: Vec<String>,
    ) -> App<F, T> {
        let servers = facade.servers();
        let mut workspaces: Vec<String> = workspaces
            .into_iter()
            .take(WORKSPACE_CANDIDATES_MAX)
            .collect();
        if workspaces.is_empty() {
            workspaces.push(String::new());
        }
        App {
            facade,
            transport,
            engine,
            screen: Screen::Overview,
            mode: RunMode::Plan,
            events: VecDeque::new(),
            log_lines: VecDeque::new(),
            next_seq: 1,
            staged: None,
            workspaces,
            workspace_idx: 0,
            servers,
            server_idx: 0,
            path: None,
            sessions_seen: HashSet::new(),
            last_summary: None,
            quit: false,
        }
    }

    /// The facade (tests inspect the fake's recorded calls here).
    pub fn facade(&self) -> &F {
        &self.facade
    }

    /// Mutable facade access (tests script the fake's results).
    pub fn facade_mut(&mut self) -> &mut F {
        &mut self.facade
    }

    /// The trigger transport (tests inspect recorded firings here).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The trigger engine.
    pub fn engine(&self) -> &TriggerEngine {
        &self.engine
    }

    /// The active screen.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Switch to a screen.
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Advance to the next screen in Tab order.
    pub fn cycle_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Overview => Screen::Workflows,
            Screen::Workflows => Screen::Log,
            Screen::Log => Screen::Fleet,
            Screen::Fleet => Screen::Overview,
        };
    }

    /// The run mode.
    pub fn mode(&self) -> RunMode {
        self.mode
    }

    /// Set the run mode.
    pub fn set_mode(&mut self, mode: RunMode) {
        self.mode = mode;
    }

    /// Whether line mode asked to exit.
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// The currently picked workspace directory.
    pub fn workspace(&self) -> &str {
        &self.workspaces[self.workspace_idx]
    }

    /// The currently picked server id ("" when none configured).
    pub fn server(&self) -> &str {
        self.servers
            .get(self.server_idx)
            .map(|server| server.id.as_str())
            .unwrap_or("")
    }

    /// The configured servers.
    pub fn servers(&self) -> &[ServerInfo] {
        &self.servers
    }

    /// Move the workspace picker by `delta` candidates (wrapping).
    pub fn cycle_workspace(&mut self, delta: i32) {
        let count = self.workspaces.len() as i32;
        if count > 0 {
            self.workspace_idx = (self.workspace_idx as i32 + delta).rem_euclid(count) as usize;
        }
    }

    /// Move the server picker by `delta` entries (wrapping).
    pub fn cycle_server(&mut self, delta: i32) {
        let count = self.servers.len() as i32;
        if count > 0 {
            self.server_idx = (self.server_idx as i32 + delta).rem_euclid(count) as usize;
        }
    }

    /// Set the workspace explicitly (typed entry in the picker):
    /// added to the candidates when new, then selected.
    pub fn set_workspace(&mut self, workspace: String) {
        if let Some(index) = self.workspaces.iter().position(|item| item == &workspace) {
            self.workspace_idx = index;
        } else if self.workspaces.len() == 1 && self.workspaces[0].is_empty() {
            self.workspaces[0] = workspace;
            self.workspace_idx = 0;
        } else if self.workspaces.len() < WORKSPACE_CANDIDATES_MAX {
            self.workspaces.push(workspace);
            self.workspace_idx = self.workspaces.len() - 1;
        }
    }

    /// Select a server by id; unknown ids are a typed refusal.
    pub fn set_server(&mut self, server_id: &str) -> Result<(), AppError> {
        match self
            .servers
            .iter()
            .position(|server| server.id == server_id)
        {
            Some(index) => {
                self.server_idx = index;
                Ok(())
            }
            None => Err(AppError::Invalid(format!(
                "no configured server {server_id:?}"
            ))),
        }
    }

    /// Set or clear the single-file scope.
    pub fn set_path(&mut self, path: Option<String>) {
        self.path = path;
    }

    /// Distinct (workspace, server) sessions this app has opened.
    pub fn session_count(&self) -> usize {
        self.sessions_seen.len()
    }

    /// The most recent diagnostics summary, when a run completed.
    pub fn last_summary(&self) -> Option<&crate::events::DiagnosticsSummary> {
        self.last_summary.as_ref()
    }

    /// The retained event stream, oldest first.
    pub fn events(&self) -> Vec<Event> {
        self.events.iter().cloned().collect()
    }

    /// The retained log lines, oldest first.
    pub fn log_lines(&self) -> Vec<String> {
        self.log_lines.iter().cloned().collect()
    }

    /// A human description of the staged plan, when one is staged.
    pub fn staged_description(&self) -> Option<String> {
        self.staged
            .as_ref()
            .map(|(tool, args)| format!("{tool} {}", truncate_text(&args.to_string(), 200)))
    }

    fn log(&mut self, line: String) {
        self.log_lines.push_back(truncate_text(&line, 300));
        while self.log_lines.len() > LOG_LINES_MAX {
            self.log_lines.pop_front();
        }
    }

    fn push_event(&mut self, kind: EventKind, depth: u8) -> Event {
        let event = Event {
            seq: self.next_seq,
            depth,
            kind,
        };
        self.next_seq += 1;
        self.log(event.describe());
        self.events.push_back(event.clone());
        while self.events.len() > EVENTS_MAX {
            self.events.pop_front();
        }
        event
    }

    /// Stage a workflow call for approval (the plan-mode step).
    /// Guaranteed: this performs no facade call.
    pub fn stage(&mut self, tool: &str, args: Value) {
        self.staged = Some((tool.to_string(), args));
        self.log(format!("staged (plan): {tool} — awaiting approval"));
    }

    /// Run one workflow under the current mode: plan stages it,
    /// auto executes it. Returns a one-line outcome description.
    pub fn run_workflow(&mut self, tool: &str, args: Value) -> Result<String, AppError> {
        match self.mode {
            RunMode::Plan => {
                self.stage(tool, args);
                Ok(format!("staged {tool} (plan mode)"))
            }
            RunMode::Auto => self.execute(tool, args),
        }
    }

    /// Run the diagnostics workflow for `request` under the
    /// current mode.
    pub fn run_diagnostics(&mut self, request: &DiagnosticsRequest) -> Result<String, AppError> {
        if request.workspace.is_empty() || request.server.is_empty() {
            return Err(AppError::Invalid(
                "diagnostics needs a workspace and a server".to_string(),
            ));
        }
        self.run_workflow("diagnostics", request.args())
    }

    /// Run diagnostics for the current picker selection.
    pub fn run_diagnostics_current(&mut self) -> Result<String, AppError> {
        let request = DiagnosticsRequest {
            workspace: self.workspace().to_string(),
            server: self.server().to_string(),
            path: self.path.clone(),
        };
        self.run_diagnostics(&request)
    }

    /// Approve whatever is staged: a staged workflow run first
    /// (executing it may stage plan-mode trigger firings), then
    /// staged trigger firings. Nothing staged is a typed refusal.
    pub fn approve(&mut self) -> Result<String, AppError> {
        if let Some((tool, args)) = self.staged.take() {
            self.log(format!("approved staged run: {tool}"));
            return self.execute(&tool, args);
        }
        if !self.engine.pending().is_empty() {
            let outcomes = self.engine.approve_pending(&mut self.transport);
            return Ok(format!("trigger firings: {}", describe_outcomes(&outcomes)));
        }
        Err(AppError::Invalid("nothing staged to approve".to_string()))
    }

    /// Abort whatever is staged, executing nothing.
    pub fn abort(&mut self) -> String {
        if let Some((tool, _)) = self.staged.take() {
            self.log(format!("aborted staged run: {tool}"));
            return format!("aborted staged {tool}");
        }
        let discarded = self.engine.discard_pending();
        if discarded > 0 {
            self.log(format!("aborted {discarded} staged trigger firing(s)"));
            return format!("aborted {discarded} staged trigger firing(s)");
        }
        "nothing staged to abort".to_string()
    }

    /// Execute one tool call now, folding the outcome into the
    /// event stream. A diagnostics result streams one log line per
    /// diagnostic (bounded), then emits the real
    /// `diagnostics.completed` event the trigger engine consumes.
    pub fn execute(&mut self, tool: &str, args: Value) -> Result<String, AppError> {
        self.push_event(
            EventKind::RunStarted {
                workflow: tool.to_string(),
            },
            0,
        );
        let result = match self.facade.call(tool, &args) {
            Ok(result) => result,
            Err(error) => {
                self.push_event(
                    EventKind::RunFailed {
                        workflow: tool.to_string(),
                        error: error.to_string(),
                    },
                    0,
                );
                return Err(AppError::Facade(error));
            }
        };
        if let (Some(workspace), Some(server)) = (
            args.get("workspace").and_then(Value::as_str),
            args.get("server").and_then(Value::as_str),
        ) {
            self.sessions_seen.insert(format!("{workspace}@{server}"));
        }
        if tool == "diagnostics" {
            return Ok(self.finish_diagnostics(&args, &result));
        }
        self.push_event(
            EventKind::WorkflowCompleted {
                workflow: tool.to_string(),
            },
            0,
        );
        Ok(format!("{tool} completed"))
    }

    fn finish_diagnostics(&mut self, args: &Value, result: &Value) -> String {
        let workspace = args.get("workspace").and_then(Value::as_str).unwrap_or("");
        let server = args.get("server").and_then(Value::as_str).unwrap_or("");
        let path = args.get("path").and_then(Value::as_str);
        self.stream_diagnostic_lines(result);
        let summary = summary_from_result(workspace, server, path, result);
        self.last_summary = Some(summary.clone());
        let event = self.push_event(EventKind::DiagnosticsCompleted(summary), 0);
        let outcomes = self.engine.evaluate(&event, &mut self.transport);
        for outcome in &outcomes {
            self.log(format!("trigger: {}", describe_outcome(outcome)));
        }
        format!("diagnostics completed: {}", event.describe())
    }

    fn stream_diagnostic_lines(&mut self, result: &Value) {
        let Some(items) = result.get("diagnostics").and_then(Value::as_array) else {
            return;
        };
        for item in items.iter().take(DIAGNOSTIC_LINES_MAX) {
            let uri = item.get("uri").and_then(Value::as_str).unwrap_or("?");
            let severity = item.get("severity").and_then(Value::as_u64).unwrap_or(0);
            let message = item.get("message").and_then(Value::as_str).unwrap_or("");
            self.log(format!("diag sev={severity} {uri}: {message}"));
        }
        if items.len() > DIAGNOSTIC_LINES_MAX {
            self.log(format!(
                "diag: {} more diagnostics not streamed (bound {DIAGNOSTIC_LINES_MAX})",
                items.len() - DIAGNOSTIC_LINES_MAX
            ));
        }
    }

    /// Fold the current state into a render model.
    pub fn screen_model(&mut self) -> ScreenModel {
        let status_lines = match self.facade.status() {
            Ok(status) => status_lines(&status),
            Err(error) => vec![format!("status unavailable: {error}")],
        };
        let mut status_lines = status_lines;
        status_lines.push(format!("sessions opened: {}", self.session_count()));
        let route_lines = self
            .servers
            .iter()
            .map(|server| {
                format!(
                    "{}: {} [{}]",
                    server.id,
                    server.languages.join(", "),
                    server.extensions.join(", ")
                )
            })
            .collect();
        ScreenModel {
            screen: self.screen,
            status_lines,
            route_lines,
            rule_count: self.engine.rules().len(),
            last_event: self.events.back().map(Event::describe).unwrap_or_default(),
            workspace: self.workspace().to_string(),
            server: self.server().to_string(),
            path: self.path.clone(),
            mode_label: match self.mode {
                RunMode::Plan => "plan".to_string(),
                RunMode::Auto => "auto".to_string(),
            },
            provider_note: PROVIDER_NOTE.to_string(),
            staged: self.staged_description(),
            log_lines: self.log_lines(),
            rules: self.engine.rules().to_vec(),
            pending_triggers: self.engine.pending().len(),
            audit_tail: self.engine.audit_tail(),
        }
    }

    /// Execute one line-mode command through the same paths the
    /// full-screen TUI uses. Returns the text to print.
    pub fn execute_line_command(&mut self, line: &str) -> Result<String, AppError> {
        if line.len() > LINE_INPUT_BYTES_MAX {
            return Err(AppError::Invalid(format!(
                "line of {} bytes exceeds LINE_INPUT_BYTES_MAX {LINE_INPUT_BYTES_MAX}",
                line.len()
            )));
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some((head, rest)) = parts.split_first() else {
            return Ok(String::new());
        };
        match *head {
            "status" => {
                let status = self.facade.status().map_err(AppError::Facade)?;
                Ok(status.to_string())
            }
            "tools" => {
                let tools = self.facade.tools().map_err(AppError::Facade)?;
                let names: Vec<String> = tools
                    .iter()
                    .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect();
                Ok(names.join("\n"))
            }
            "servers" => Ok(self
                .servers
                .iter()
                .map(|server| format!("{}: {}", server.id, server.languages.join(", ")))
                .collect::<Vec<String>>()
                .join("\n")),
            "screen" => self.line_screen(rest),
            "overview" => {
                self.set_screen(Screen::Overview);
                Ok(self.screen_model().status_lines.join("\n"))
            }
            "fleet" => {
                self.set_screen(Screen::Fleet);
                let model = self.screen_model();
                let mut lines: Vec<String> = model
                    .rules
                    .iter()
                    .map(|rule| {
                        format!(
                            "rule {} -> {} {}",
                            rule.id, rule.target_mcp, rule.target_tool
                        )
                    })
                    .collect();
                lines.extend(model.audit_tail);
                Ok(lines.join("\n"))
            }
            "log" => {
                self.set_screen(Screen::Log);
                Ok(self.log_lines().join("\n"))
            }
            "mode" => self.line_mode(rest),
            "workspace" => self.line_workspace(rest),
            "server" => self.line_server(rest),
            "path" => {
                self.set_path(
                    rest.first()
                        .filter(|p| **p != "-")
                        .map(|p| (*p).to_string()),
                );
                Ok(format!(
                    "path: {}",
                    self.path.as_deref().unwrap_or("(whole workspace)")
                ))
            }
            "diag" => self.line_diag(rest),
            "call" => self.line_call(rest),
            "approve" => self.approve(),
            "abort" => Ok(self.abort()),
            "rules" => Ok(self
                .engine
                .rules()
                .iter()
                .map(|rule| {
                    format!(
                        "{} [{}] on {} mode={}",
                        rule.id,
                        if rule.enabled { "on" } else { "off" },
                        rule.event,
                        match rule.mode {
                            crate::triggers::RuleMode::Auto => "auto",
                            crate::triggers::RuleMode::Plan => "plan",
                        }
                    )
                })
                .collect::<Vec<String>>()
                .join("\n")),
            "rule" => self.line_rule(rest),
            "audit" => Ok(self.engine.audit_tail().join("\n")),
            "quit" | "exit" => {
                self.quit = true;
                Ok("bye".to_string())
            }
            other => Err(AppError::Invalid(format!("unknown command {other:?}"))),
        }
    }

    fn line_screen(&mut self, rest: &[&str]) -> Result<String, AppError> {
        match rest.first() {
            None => Ok(format!("screen: {:?}", self.screen)),
            Some(&"overview") => {
                self.set_screen(Screen::Overview);
                Ok("screen: Overview".to_string())
            }
            Some(&"workflows") => {
                self.set_screen(Screen::Workflows);
                Ok("screen: Workflows".to_string())
            }
            Some(&"log") => {
                self.set_screen(Screen::Log);
                Ok("screen: Log".to_string())
            }
            Some(&"fleet") => {
                self.set_screen(Screen::Fleet);
                Ok("screen: Fleet".to_string())
            }
            Some(other) => Err(AppError::Invalid(format!("unknown screen {other:?}"))),
        }
    }

    fn line_mode(&mut self, rest: &[&str]) -> Result<String, AppError> {
        match rest.first() {
            None => Ok(format!("mode: {:?}", self.mode)),
            Some(&"plan") => {
                self.set_mode(RunMode::Plan);
                Ok("mode: Plan".to_string())
            }
            Some(&"auto") => {
                self.set_mode(RunMode::Auto);
                Ok("mode: Auto".to_string())
            }
            Some(other) => Err(AppError::Invalid(format!("unknown mode {other:?}"))),
        }
    }

    fn line_workspace(&mut self, rest: &[&str]) -> Result<String, AppError> {
        if rest.is_empty() {
            return Ok(format!("workspace: {}", self.workspace()));
        }
        self.set_workspace(rest.join(" "));
        Ok(format!("workspace: {}", self.workspace()))
    }

    fn line_server(&mut self, rest: &[&str]) -> Result<String, AppError> {
        match rest.first() {
            None => Ok(format!("server: {}", self.server())),
            Some(id) => {
                self.set_server(id)?;
                Ok(format!("server: {}", self.server()))
            }
        }
    }

    fn line_diag(&mut self, rest: &[&str]) -> Result<String, AppError> {
        let request = match rest {
            [] => DiagnosticsRequest {
                workspace: self.workspace().to_string(),
                server: self.server().to_string(),
                path: self.path.clone(),
            },
            [workspace, server] => DiagnosticsRequest {
                workspace: (*workspace).to_string(),
                server: (*server).to_string(),
                path: None,
            },
            [workspace, server, path] => DiagnosticsRequest {
                workspace: (*workspace).to_string(),
                server: (*server).to_string(),
                path: Some((*path).to_string()),
            },
            _ => {
                return Err(AppError::Invalid(
                    "usage: diag [workspace server [path]]".to_string(),
                ));
            }
        };
        self.run_diagnostics(&request)
    }

    fn line_call(&mut self, rest: &[&str]) -> Result<String, AppError> {
        let Some((tool, json_parts)) = rest.split_first() else {
            return Err(AppError::Invalid(
                "usage: call <tool> <json-args>".to_string(),
            ));
        };
        let args: Value = serde_json::from_str(&json_parts.join(" "))
            .map_err(|error| AppError::Invalid(format!("args are not JSON: {error}")))?;
        self.run_workflow(tool, args)
    }

    fn line_rule(&mut self, rest: &[&str]) -> Result<String, AppError> {
        match rest {
            [id, state] => {
                let enabled = match *state {
                    "on" => true,
                    "off" => false,
                    other => {
                        return Err(AppError::Invalid(format!(
                            "rule state must be on|off, got {other:?}"
                        )));
                    }
                };
                if self.engine.set_enabled(id, enabled) {
                    Ok(format!("rule {id}: {state}"))
                } else {
                    Err(AppError::Invalid(format!("no rule {id:?}")))
                }
            }
            _ => Err(AppError::Invalid("usage: rule <id> on|off".to_string())),
        }
    }
}

fn status_lines(status: &Value) -> Vec<String> {
    match status.as_object() {
        Some(object) => object
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect(),
        None => vec![status.to_string()],
    }
}

fn describe_outcome(outcome: &FireOutcome) -> String {
    match outcome {
        FireOutcome::Fired { rule_id } => format!("fired {rule_id}"),
        FireOutcome::Staged { rule_id } => format!("staged {rule_id} (plan)"),
        FireOutcome::Dropped { rule_id, reason } => format!("dropped {rule_id} ({reason})"),
        FireOutcome::Failed { rule_id, error } => format!("failed {rule_id}: {error}"),
    }
}

fn describe_outcomes(outcomes: &[FireOutcome]) -> String {
    outcomes
        .iter()
        .map(describe_outcome)
        .collect::<Vec<String>>()
        .join(", ")
}
