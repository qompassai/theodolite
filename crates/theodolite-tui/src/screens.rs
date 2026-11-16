//! Screen rendering: a snapshot model plus one draw function.
//!
//! The app folds its state into a [`ScreenModel`] (plain data), and
//! [`render`] draws exactly one panel per screen. Rendering is a
//! pure function of the model, so the headless smoke test renders
//! the first screen through ratatui's `TestBackend` with no
//! terminal at all.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::app::Screen;
use crate::panels;
use crate::triggers::Rule;

/// Everything one frame needs, as plain data.
#[derive(Debug, Clone)]
pub struct ScreenModel {
    /// Active screen.
    pub screen: Screen,
    /// Overview status lines (`key: value`).
    pub status_lines: Vec<String>,
    /// Overview route lines (one per configured server).
    pub route_lines: Vec<String>,
    /// Trigger-rule count for the Overview.
    pub rule_count: usize,
    /// Most recent event description, or "" when none yet.
    pub last_event: String,
    /// Selected workspace (target picker value).
    pub workspace: String,
    /// Selected server id.
    pub server: String,
    /// Selected file scope, when set.
    pub path: Option<String>,
    /// Run mode label: `plan` or `auto`.
    pub mode_label: String,
    /// Provider control note (disabled with reason for theodolite).
    pub provider_note: String,
    /// Staged plan description awaiting approval, when any.
    pub staged: Option<String>,
    /// Log lines for the Log screen, oldest first.
    pub log_lines: Vec<String>,
    /// Rules for the Fleet screen.
    pub rules: Vec<Rule>,
    /// Staged trigger firings count for the Fleet screen.
    pub pending_triggers: usize,
    /// Audit tail lines for the Fleet screen.
    pub audit_tail: Vec<String>,
}

/// Draw the model's active screen into `frame`.
pub fn render(frame: &mut Frame, model: &ScreenModel) {
    let area: Rect = frame.area();
    let widget: Paragraph = match model.screen {
        Screen::Overview => panels::overview_panel(
            crate::MEMBER_NAME,
            crate::MEMBER_VERSION,
            &model.status_lines,
            &model.route_lines,
            model.rule_count,
            &model.last_event,
        ),
        Screen::Workflows => panels::workflows_panel(
            &model.workspace,
            &model.server,
            model.path.as_deref(),
            &model.mode_label,
            &model.provider_note,
            model.staged.as_deref(),
        ),
        Screen::Log => panels::log_panel(&model.log_lines),
        Screen::Fleet => {
            panels::fleet_panel(&model.rules, model.pending_triggers, &model.audit_tail)
        }
    };
    frame.render_widget(widget, area);
}
