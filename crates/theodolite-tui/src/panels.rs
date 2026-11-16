//! Pure widget constructors for the screens.
//!
//! Every panel is a function from plain data to a ratatui widget:
//! no I/O, no facade, no state beyond its arguments, so tests render
//! them through ratatui's `TestBackend` and assert on the buffer.
//! All text passes through named truncation bounds — a hostile or
//! broken server result can widen a line, never the layout.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::triggers::Rule;

/// Maximum characters in one rendered panel line.
pub const LINE_CHARS_MAX: usize = 120;
/// Maximum lines a panel renders before summarizing the rest.
pub const PANEL_LINES_MAX: usize = 64;
/// Maximum characters in a panel title.
pub const TITLE_CHARS_MAX: usize = 60;

/// Truncate `text` to at most `max_chars` characters, ending with
/// an ellipsis marker when anything was cut. Char-boundary safe.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn panel(title: &str, lines: Vec<String>) -> Paragraph<'static> {
    let mut rendered: Vec<Line> = lines
        .into_iter()
        .take(PANEL_LINES_MAX)
        .map(|line| Line::from(truncate_text(&line, LINE_CHARS_MAX)))
        .collect();
    if rendered.is_empty() {
        rendered.push(Line::from("(nothing yet)"));
    }
    Paragraph::new(rendered)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(truncate_text(title, TITLE_CHARS_MAX)),
        )
        .wrap(Wrap { trim: false })
}

fn header_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(truncate_text(value, LINE_CHARS_MAX)),
    ])
}

/// The Overview panel: member identity, bridge status fields,
/// route summary, and trigger state.
pub fn overview_panel(
    member: &str,
    version: &str,
    status_lines: &[String],
    route_lines: &[String],
    rule_count: usize,
    last_event: &str,
) -> Paragraph<'static> {
    let mut lines: Vec<Line> = vec![
        header_line("member", &format!("{member} {version}")),
        header_line("trigger rules", &rule_count.to_string()),
        header_line("last event", last_event),
    ];
    lines.extend(
        status_lines
            .iter()
            .map(|line| Line::from(truncate_text(line, LINE_CHARS_MAX))),
    );
    lines.push(Line::from("routes:"));
    lines.extend(
        route_lines
            .iter()
            .map(|line| Line::from(truncate_text(line, LINE_CHARS_MAX))),
    );
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Overview"))
        .wrap(Wrap { trim: false })
}

/// The Workflows panel: current target selection, run mode, the
/// provider control (honestly disabled for theodolite), and any
/// staged plan awaiting approval.
pub fn workflows_panel(
    workspace: &str,
    server: &str,
    path: Option<&str>,
    mode: &str,
    provider_note: &str,
    staged: Option<&str>,
) -> Paragraph<'static> {
    let mut lines = vec![
        format!("workspace: {}", truncate_text(workspace, LINE_CHARS_MAX)),
        format!("server: {server}"),
        format!("path: {}", path.unwrap_or("(whole workspace)")),
        format!("mode: {mode}"),
        format!("provider: {provider_note}"),
        "run: diagnostics (Enter in TUI, `diag` in line mode)".to_string(),
    ];
    match staged {
        Some(description) => {
            lines.push(format!("STAGED (plan): {description}"));
            lines.push("approve with `approve`, discard with `abort`".to_string());
        }
        None => lines.push("staged: (none)".to_string()),
    }
    panel("Workflows", lines)
}

/// The Log panel: the bounded, scrolling event/log view.
pub fn log_panel(lines: &[String]) -> Paragraph<'static> {
    panel("Log / Live", lines.to_vec())
}

/// The Fleet panel: trigger rules with enable state, staged
/// trigger firings, and the audit tail.
pub fn fleet_panel(
    rules: &[Rule],
    pending_count: usize,
    audit_tail: &[String],
) -> Paragraph<'static> {
    let mut lines: Vec<String> = rules
        .iter()
        .map(|rule| {
            format!(
                "rule {} [{}] on {} -> {} {} mode={} depth<={}",
                rule.id,
                if rule.enabled { "on" } else { "OFF" },
                rule.event,
                rule.target_mcp,
                rule.target_tool,
                match rule.mode {
                    crate::triggers::RuleMode::Auto => "auto",
                    crate::triggers::RuleMode::Plan => "plan",
                },
                rule.max_depth,
            )
        })
        .collect();
    if rules.is_empty() {
        lines.push("rules: (none loaded)".to_string());
    }
    lines.push(format!("staged trigger firings: {pending_count}"));
    lines.push("audit tail:".to_string());
    lines.extend(audit_tail.iter().cloned());
    panel("Fleet / Triggers", lines)
}
