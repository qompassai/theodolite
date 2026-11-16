//! theodolite-tui entry point: parse args, build the real facade
//! over the member's library crate, load trigger rules, and run
//! either the full-screen TUI (TTY) or the non-TTY line mode —
//! both over the same execute path in [`theodolite_tui::app`].
//!
//! Usage:
//! `theodolite-tui [--config <bridge.toml>] [--triggers <triggers.toml>]`
//! `               [--audit <audit.jsonl>] [--workspace <dir>] [--line]`
//!
//! Keys (full-screen): Tab cycle screens, 1-4 jump, `r` run
//! diagnostics for the picked target, `p`/`a` plan/auto mode,
//! Enter approve staged, `x` abort staged, `w`/`s` cycle the
//! workspace/server picker, `q` quit.

#![forbid(unsafe_code)]

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use theodolite_tui::app::{App, RunMode, Screen};
use theodolite_tui::facade::LibFacade;
use theodolite_tui::triggers::{AuditSink, TriggerEngine, load_rules};
use theodolite_tui::{WORKSPACE_CANDIDATES_MAX, screens};

/// The trigger transport for this rung: firings are recorded
/// through the audit log and reported, but the live bloch MCP
/// client transport is a named follow-on — the engine is gated
/// against a fake transport in this rung's tests, and this
/// transport refuses honestly rather than pretending to deliver.
struct RefusingTransport;

impl theodolite_tui::triggers::TriggerTransport for RefusingTransport {
    fn call(
        &mut self,
        mcp: &str,
        tool: &str,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, theodolite_tui::triggers::TriggerError> {
        Err(theodolite_tui::triggers::TriggerError::Transport(format!(
            "no live MCP client transport wired in rung 1 for {mcp}.{tool}"
        )))
    }
}

struct Args {
    config: Option<PathBuf>,
    triggers: Option<PathBuf>,
    audit: Option<PathBuf>,
    workspace: Option<String>,
    line: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: None,
        triggers: None,
        audit: None,
        workspace: None,
        line: false,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < raw.len() {
        let take = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            raw.get(*index)
                .cloned()
                .ok_or_else(|| "flag needs a value".to_string())
        };
        match raw[index].as_str() {
            "--config" => args.config = Some(PathBuf::from(take(&mut index)?)),
            "--triggers" => args.triggers = Some(PathBuf::from(take(&mut index)?)),
            "--audit" => args.audit = Some(PathBuf::from(take(&mut index)?)),
            "--workspace" => args.workspace = Some(take(&mut index)?),
            "--line" => args.line = true,
            "--help" | "-h" => {
                return Err(
                    "usage: theodolite-tui [--config <bridge.toml>] [--triggers <path>] \
                     [--audit <path>] [--workspace <dir>] [--line]"
                        .to_string(),
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        index += 1;
    }
    Ok(args)
}

fn default_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("THEODOLITE_CONFIG") {
        return PathBuf::from(path);
    }
    home_dir().join(".local/share/theodolite/bridge.toml")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

fn load_engine(args: &Args) -> Result<TriggerEngine, String> {
    let candidates: Vec<PathBuf> = match &args.triggers {
        Some(path) => vec![path.clone()],
        None => vec![
            PathBuf::from("triggers.toml"),
            home_dir().join(".local/share/theodolite-tui/triggers.toml"),
        ],
    };
    let mut rules = Vec::new();
    for candidate in &candidates {
        if candidate.exists() {
            let text = std::fs::read_to_string(candidate)
                .map_err(|error| format!("cannot read {}: {error}", candidate.display()))?;
            rules =
                load_rules(&text).map_err(|error| format!("{}: {error}", candidate.display()))?;
            break;
        }
    }
    if rules.is_empty() && args.triggers.is_none() {
        eprintln!("theodolite-tui: no triggers.toml found; running with zero rules");
    }
    let audit = match &args.audit {
        Some(path) => AuditSink::file(path.clone()),
        None => {
            let path = home_dir().join(".local/share/theodolite-tui/trigger-audit.jsonl");
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            AuditSink::file(path)
        }
    };
    Ok(TriggerEngine::new(rules, audit))
}

fn workspace_candidates(first: Option<String>) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(dir) = first {
        candidates.push(dir);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_text = cwd.display().to_string();
        if !candidates.contains(&cwd_text) {
            candidates.push(cwd_text);
        }
        if let Ok(entries) = std::fs::read_dir(&cwd) {
            let mut subdirs: Vec<String> = entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.path().display().to_string())
                .collect();
            subdirs.sort();
            for subdir in subdirs {
                if candidates.len() >= WORKSPACE_CANDIDATES_MAX {
                    break;
                }
                if !candidates.contains(&subdir) {
                    candidates.push(subdir);
                }
            }
        }
    }
    candidates
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("theodolite-tui: {message}");
            std::process::exit(2);
        }
    };
    let config_path = args.config.clone().unwrap_or_else(default_config_path);
    let facade = match LibFacade::from_config(config_path) {
        Ok(facade) => facade,
        Err(error) => {
            eprintln!("theodolite-tui: {error}");
            std::process::exit(2);
        }
    };
    let engine = match load_engine(&args) {
        Ok(engine) => engine,
        Err(message) => {
            eprintln!("theodolite-tui: {message}");
            std::process::exit(2);
        }
    };
    let candidates = workspace_candidates(args.workspace.clone());
    let mut app = App::new(facade, RefusingTransport, engine, candidates);
    let line_mode =
        args.line || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal();
    let code = if line_mode {
        run_line_mode(&mut app)
    } else {
        run_fullscreen(&mut app)
    };
    app.facade_shutdown();
    std::process::exit(code);
}

fn run_line_mode(app: &mut App<LibFacade, RefusingTransport>) -> i32 {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => return 1,
        };
        match app.execute_line_command(&line) {
            Ok(output) => {
                if !output.is_empty() {
                    let _ = writeln!(stdout, "{output}");
                }
            }
            Err(error) => {
                let _ = writeln!(stdout, "error: {error}");
            }
        }
        let _ = stdout.flush();
        if app.quit_requested() {
            break;
        }
    }
    0
}

fn run_fullscreen(app: &mut App<LibFacade, RefusingTransport>) -> i32 {
    match run_tui(app) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("theodolite-tui: {error}");
            1
        }
    }
}

fn run_tui(app: &mut App<LibFacade, RefusingTransport>) -> std::io::Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = loop {
        let model = app.screen_model();
        if let Err(error) = terminal.draw(|frame| screens::render(frame, &model)) {
            break Err(error);
        }
        if !crossterm::event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break Ok(()),
            KeyCode::Tab => app.cycle_screen(),
            KeyCode::Char('1') => app.set_screen(Screen::Overview),
            KeyCode::Char('2') => app.set_screen(Screen::Workflows),
            KeyCode::Char('3') => app.set_screen(Screen::Log),
            KeyCode::Char('4') => app.set_screen(Screen::Fleet),
            KeyCode::Char('p') => app.set_mode(RunMode::Plan),
            KeyCode::Char('a') => app.set_mode(RunMode::Auto),
            KeyCode::Char('r') => {
                let _ = app.run_diagnostics_current();
                app.set_screen(Screen::Log);
            }
            KeyCode::Enter => {
                let _ = app.approve();
            }
            KeyCode::Char('x') => {
                let _ = app.abort();
            }
            KeyCode::Char('w') => app.cycle_workspace(1),
            KeyCode::Char('W') => app.cycle_workspace(-1),
            KeyCode::Char('s') => app.cycle_server(1),
            KeyCode::Char('S') => app.cycle_server(-1),
            _ => {}
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

/// Small extension seam so `main` can shut the bridge sessions
/// down through the app's facade without widening `App`'s API.
trait FacadeShutdown {
    /// Shut down the underlying facade's sessions, if any.
    fn facade_shutdown(&self);
}

impl FacadeShutdown for App<LibFacade, RefusingTransport> {
    fn facade_shutdown(&self) {
        self.facade().shutdown();
    }
}
