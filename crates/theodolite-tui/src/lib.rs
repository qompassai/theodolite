//! theodolite-tui — the fleet TUI pattern (programme DESIGN SS D2)
//! applied to theodolite, the headless LSP-bridge MCP.
//!
//! Shape: the UI programs against a narrow [`facade::BridgeFacade`]
//! trait (the member's library crate underneath; [`facade::FakeFacade`]
//! in tests), panels are pure widget constructors with named
//! truncation bounds, screens are a small state machine in
//! [`app::App`], and the cross-MCP trigger layer (DESIGN SS D4) lives
//! in [`triggers`]: typed events, TOML rules, a depth cap,
//! event-hash idempotency, and a JSONL audit log.
//!
//! Modes: full-screen ratatui where a TTY exists, and a non-TTY line
//! mode sharing the same execute path (phlow precedent), so the TUI
//! is scriptable and testable headlessly.
//!
//! Theodolite has no media use case, so there is no Media screen
//! (DESIGN SS D2: absent, not stubbed), and it routes no model calls,
//! so the workflow provider control is honestly disabled with its
//! reason rather than faked.

#![forbid(unsafe_code)]

pub mod app;
pub mod events;
pub mod facade;
pub mod panels;
pub mod screens;
pub mod triggers;

/// Maximum event records retained in the app's event stream.
pub const EVENTS_MAX: usize = 512;
/// Maximum log lines retained for the Log screen.
pub const LOG_LINES_MAX: usize = 512;
/// Maximum diagnostics rendered as individual streamed log lines per
/// run; the remainder is summarized in one line.
pub const DIAGNOSTIC_LINES_MAX: usize = 128;
/// Maximum bytes in one line-mode input line.
pub const LINE_INPUT_BYTES_MAX: usize = 4096;
/// Maximum serialized bytes accepted from one facade call result;
/// larger payloads are a typed oversized error, never rendered.
pub const RESULT_BYTES_MAX: usize = 1_048_576;
/// Maximum workspace candidates the target picker holds.
pub const WORKSPACE_CANDIDATES_MAX: usize = 64;
/// Member name shown on the Overview screen.
pub const MEMBER_NAME: &str = "theodolite";
/// Member version (the bridge release this TUI ships beside).
pub const MEMBER_VERSION: &str = "1.0.0";
/// This TUI crate's own version.
pub const TUI_VERSION: &str = env!("CARGO_PKG_VERSION");
