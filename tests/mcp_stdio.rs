//! End-to-end gates: the real bridge binary over MCP stdio, driven
//! against the scripted fake language server (SPEC §9 gates 2–11 in
//! miniature) plus one real rust-analyzer smoke. The parity gate
//! against bacon-ls lives in `tests/parity.rs` and skips loudly
//! where bacon-ls is unavailable.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const FAKE_VERSION: &str = "theodolite-fake-lsp 1.0.0";
const FIXTURE_MAIN: &str = include_str!("fixtures/fake-workspace/main.rs");

fn bridge_bin() -> &'static str {
    env!("CARGO_BIN_EXE_theodolite")
}

fn fake_bin() -> &'static str {
    env!("CARGO_BIN_EXE_theodolite-fake-lsp")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "theodolite-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn make_workspace(dir: &Path) -> PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::write(ws.join("main.rs"), FIXTURE_MAIN).expect("write fixture");
    ws.canonicalize().expect("canonical ws")
}

/// Write a bridge config naming one fake server in `mode`.
fn write_fake_config(
    dir: &Path,
    mode: &str,
    extra_top: &str,
    extra_env: &[(&str, &str)],
) -> PathBuf {
    write_fake_config_version(dir, mode, extra_top, extra_env, FAKE_VERSION)
}

fn write_fake_config_version(
    dir: &Path,
    mode: &str,
    extra_top: &str,
    extra_env: &[(&str, &str)],
    expect_version: &str,
) -> PathBuf {
    let env_entries: Vec<String> = extra_env
        .iter()
        .map(|(k, v)| format!("{k} = \"{v}\""))
        .collect();
    let body = format!(
        "{extra_top}\n[[server]]\nid = \"fake\"\nbinary = \"{}\"\nargs = [\"--mode\", \"{mode}\"]\n\
         version_probe = [\"--version\"]\nexpect_version = \"{expect_version}\"\n\
         languages = [\"rust\"]\nextensions = [\"rs\"]\nlanguage_ids = {{ rs = \"rust\" }}\n\
         readiness = \"immediate\"\nenv = {{ {} }}\n",
        fake_bin(),
        env_entries.join(", ")
    );
    let path = dir.join(format!("bridge-{mode}.toml"));
    std::fs::write(&path, body).expect("write config");
    path
}

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Bridge {
    fn spawn(config: &Path) -> Bridge {
        let mut child = Command::new(bridge_bin())
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        let mut bridge = Bridge {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let response = bridge.request(
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05"}),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "theodolite");
        bridge
    }

    fn send(&mut self, value: &serde_json::Value) {
        let line = value.to_string();
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read frame");
        assert!(read > 0, "bridge closed stdout unexpectedly");
        serde_json::from_str(&line).expect("bridge frame is JSON")
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        loop {
            let frame = self.recv();
            if frame.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    /// Call a tool; returns (is_error, parsed result-text JSON).
    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> (bool, serde_json::Value) {
        let frame = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        );
        let result = &frame["result"];
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(text).expect("tool text is JSON");
        (is_error, parsed)
    }

    fn error_code(parsed: &serde_json::Value) -> String {
        parsed["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        panic!("bridge did not exit within 20 s of transport EOF");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("waiting on bridge: {error}"),
            }
        }
    }
}

fn hover_args(ws: &Path) -> serde_json::Value {
    serde_json::json!({
        "workspace": ws.to_string_lossy(), "server": "fake",
        "path": "main.rs", "line": 0, "character": 3
    })
}

// --- Validation --------------------------------------------------------

#[test]
fn hover_returns_verbatim_contents() {
    let dir = temp_dir("hover");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(!is_error, "{result}");
    assert_eq!(result["contents"]["kind"], "markdown");
    assert_eq!(result["contents"]["value"], "fake hover for `foo`");
    assert_eq!(result["range"]["start"]["line"], 0);
    bridge.shutdown();
}

#[test]
fn definition_results_are_sorted() {
    let dir = temp_dir("definition");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("definition", hover_args(&ws));
    assert!(!is_error, "{result}");
    // The fake returns other.rs before lib.rs; the bridge must sort.
    assert_eq!(result["count"], 2);
    assert_eq!(result["locations"][0]["uri"], "file:///src/lib.rs");
    assert_eq!(result["locations"][1]["uri"], "file:///src/other.rs");
    bridge.shutdown();
}

#[test]
fn references_sorted_with_declaration_default() {
    let dir = temp_dir("references");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("references", hover_args(&ws));
    assert!(!is_error, "{result}");
    let lines: Vec<u64> = result["locations"]
        .as_array()
        .expect("locations array")
        .iter()
        .map(|loc| loc["range"]["start"]["line"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(lines, vec![3, 7, 9]);
    bridge.shutdown();
}

#[test]
fn document_symbols_flattened_sorted_with_containers() {
    let dir = temp_dir("symbols");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool(
        "document_symbols",
        serde_json::json!({
            "workspace": ws.to_string_lossy(), "server": "fake", "path": "main.rs"
        }),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["count"], 3);
    let names: Vec<&str> = result["symbols"]
        .as_array()
        .expect("symbols")
        .iter()
        .map(|s| s["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["aaa_first", "outer_fn", "inner_value"]);
    assert_eq!(result["symbols"][2]["container"], "outer_fn");
    assert_eq!(result["uri"], "file:///main.rs");
    bridge.shutdown();
}

#[test]
fn diagnostics_waits_for_second_wave_and_sorts() {
    let dir = temp_dir("diagnostics");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    // Sync the file first so the fake publishes its two waves.
    let _ = bridge.call_tool("hover", hover_args(&ws));
    let (is_error, result) = bridge.call_tool(
        "diagnostics",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["count"], 2, "{result}");
    // Sorted by range: the wave-2 error (line 1) precedes line 4.
    assert_eq!(result["diagnostics"][0]["message"], "second wave error");
    assert_eq!(result["diagnostics"][0]["source"], "clippy");
    assert_eq!(result["diagnostics"][1]["message"], "first wave warning");
    assert_eq!(result["diagnostics"][0]["uri"], "file:///main.rs");
    bridge.shutdown();
}

#[test]
fn pull_diagnostics_merge_with_push_provenance() {
    let dir = temp_dir("pull-merge");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "pull", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    // Open the file so the fake pushes its push half.
    let _ = bridge.call_tool("hover", hover_args(&ws));
    let (is_error, result) = bridge.call_tool(
        "diagnostics",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(!is_error, "{result}");
    // 2 pushed + 2 pulled − 1 duplicate = 3 merged entries.
    assert_eq!(result["count"], 3, "{result}");
    let channels: Vec<&str> = result["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .map(|c| c.as_str().unwrap_or(""))
        .collect();
    assert_eq!(channels, vec!["pull", "push"]);
    let by_message = |message: &str| {
        result["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .find(|d| d["message"] == message)
            .unwrap_or_else(|| panic!("diagnostic {message:?} missing from {result}"))
            .clone()
    };
    // Sorted by range line: shared (1) < push only (4) < pull only (7).
    let lines: Vec<u64> = result["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["range"]["start"]["line"].as_u64().unwrap_or(99))
        .collect();
    assert_eq!(lines, vec![1, 4, 7]);
    assert_eq!(by_message("in both channels")["channel"], "push+pull");
    assert_eq!(by_message("push only warning")["channel"], "push");
    assert_eq!(by_message("pull only error")["channel"], "pull");
    bridge.shutdown();
}

#[test]
fn session_status_reports_the_pinned_transcript() {
    let dir = temp_dir("status");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let _ = bridge.call_tool("hover", hover_args(&ws));
    let (is_error, result) = bridge.call_tool(
        "session_status",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["actual_version"], FAKE_VERSION);
    assert_eq!(result["expected_version"], FAKE_VERSION);
    assert_eq!(result["phase"], "ready");
    assert_eq!(result["alive"], true);
    assert_eq!(result["open_documents"], 1);
    assert_eq!(result["config_hash"].as_str().map(str::len), Some(64));
    assert_eq!(result["toolchain_pin"], "nightly-2026-09-25");
    bridge.shutdown();
}

#[test]
fn double_run_is_byte_identical() {
    let run = |tag: &str| -> (String, String) {
        let dir = temp_dir(tag);
        let ws = make_workspace(&dir);
        let config = write_fake_config(&dir, "normal", "", &[]);
        let mut bridge = Bridge::spawn(&config);
        let (_, hover) = bridge.call_tool("hover", hover_args(&ws));
        let frame = bridge.request(
            "tools/call",
            serde_json::json!({
                "name": "diagnostics",
                "arguments": {"workspace": ws.to_string_lossy(), "server": "fake"}
            }),
        );
        let diagnostics_text = frame["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let frame = bridge.request(
            "tools/call",
            serde_json::json!({"name": "definition", "arguments": hover_args(&ws)}),
        );
        let definition_text = frame["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let _ = hover;
        bridge.shutdown();
        (diagnostics_text, definition_text)
    };
    let (diag_a, def_a) = run("det-a");
    let (diag_b, def_b) = run("det-b");
    assert_eq!(diag_a, diag_b, "diagnostics differ across cold runs");
    assert_eq!(def_a, def_b, "definitions differ across cold runs");
}

#[test]
fn session_restart_returns_a_fresh_transcript() {
    let dir = temp_dir("restart-ok");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let _ = bridge.call_tool("hover", hover_args(&ws));
    let (is_error, result) = bridge.call_tool(
        "session_restart",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["restarted"], true);
    assert_eq!(result["open_documents"], 0);
    let (is_error, _) = bridge.call_tool("hover", hover_args(&ws));
    assert!(!is_error);
    bridge.shutdown();
}

#[test]
fn rust_analyzer_real_server_smoke() {
    let Some((binary, version)) = find_rust_analyzer() else {
        eprintln!(
            "SKIP rust_analyzer_real_server_smoke: no rust-analyzer binary found \
             (checked $THEODOLITE_RA, rustup toolchains, ~/.cargo/bin)"
        );
        return;
    };
    let dir = temp_dir("ra");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust-workspace");
    let ws = dir.join("ws");
    copy_tree(&fixture, &ws);
    let ws = ws.canonicalize().expect("canonical ws");
    let config_body = format!(
        "ready_timeout_ms = 180000\n[[server]]\nid = \"rust-analyzer\"\nbinary = \"{binary}\"\n\
         version_probe = [\"--version\"]\nexpect_version = \"{version}\"\n\
         languages = [\"rust\"]\nextensions = [\"rs\"]\nlanguage_ids = {{ rs = \"rust\" }}\n\
         readiness = \"quiescent\"\n\
         env = {{ PATH = \"{}:/usr/local/bin:/usr/bin:/bin\" }}\n",
        std::env::var("HOME").unwrap_or_default() + "/.cargo/bin"
    );
    let config = dir.join("ra.toml");
    std::fs::write(&config, config_body).expect("write ra config");
    let mut bridge = Bridge::spawn(&config);
    let position = serde_json::json!({
        "workspace": ws.to_string_lossy(), "server": "rust-analyzer",
        "path": "src/lib.rs", "line": 6, "character": 6
    });
    let (is_error, hover) = bridge.call_tool("hover", position.clone());
    assert!(!is_error, "hover failed: {hover}");
    assert!(
        hover["contents"] != serde_json::Value::Null,
        "hover was null: {hover}"
    );
    let (is_error, definition) = bridge.call_tool("definition", position);
    assert!(!is_error, "definition failed: {definition}");
    assert_eq!(definition["count"], 1, "{definition}");
    assert_eq!(definition["locations"][0]["uri"], "file:///src/lib.rs");
    assert_eq!(definition["locations"][0]["range"]["start"]["line"], 1);
    bridge.shutdown();
}

fn find_rust_analyzer() -> Option<(String, String)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("THEODOLITE_RA") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rust-analyzer"),
        );
        candidates.push(PathBuf::from(&home).join(".cargo/bin/rust-analyzer"));
    }
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        // A rustup proxy fails under the bridge's scrubbed env; the
        // probe here uses the ambient env only to learn the version
        // string — a proxy would then mismatch at bridge runtime and
        // the smoke would fail loudly, which is the honest outcome.
        // Prefer candidates whose scrubbed-env probe also succeeds.
        let scrubbed = Command::new(&candidate)
            .arg("--version")
            .env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .output();
        if let Ok(output) = scrubbed
            && output.status.success()
        {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Some((candidate.to_string_lossy().into_owned(), version));
        }
    }
    None
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir copy target");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

// --- Adversarial -------------------------------------------------------

#[test]
fn oversized_frame_is_rejected_as_malformed() {
    let dir = temp_dir("oversized");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "oversized", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    // In-flight callers get the malformed error itself; a caller
    // arriving after the death is recorded gets ServerDied carrying
    // the same detail. Both are typed; neither hangs.
    let code = Bridge::error_code(&result);
    assert!(
        code == "MalformedServerTraffic" || code == "ServerDied",
        "unexpected code {code}: {result}"
    );
    let (_, status) = bridge.call_tool(
        "session_status",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert_eq!(status["phase"], "dead", "{status}");
    let detail = status["dead_detail"].as_str().unwrap_or("");
    assert!(detail.contains("MAX_FRAME_BYTES"), "detail: {detail}");
    bridge.shutdown();
}

#[test]
fn garbage_frame_marks_the_session_dead() {
    let dir = temp_dir("garbage");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "garbage", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    let code = Bridge::error_code(&result);
    assert!(
        code == "MalformedServerTraffic" || code == "ServerDied",
        "unexpected code {code}: {result}"
    );
    let (_, status) = bridge.call_tool(
        "session_status",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert_eq!(status["phase"], "dead", "{status}");
    bridge.shutdown();
}

#[test]
fn truncated_frame_marks_the_session_dead() {
    let dir = temp_dir("truncated");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "truncated", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    let code = Bridge::error_code(&result);
    assert!(
        code == "MalformedServerTraffic" || code == "ServerDied",
        "unexpected code {code}: {result}"
    );
    bridge.shutdown();
}

#[test]
fn unknown_response_id_kills_the_session() {
    let dir = temp_dir("unknown-id");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "unknown-id", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    let code = Bridge::error_code(&result);
    assert!(
        code == "MalformedServerTraffic" || code == "ServerDied",
        "unexpected code {code}: {result}"
    );
    let (_, status) = bridge.call_tool(
        "session_status",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert_eq!(status["phase"], "dead", "{status}");
    let detail = status["dead_detail"].as_str().unwrap_or("");
    assert!(detail.contains("unknown id 9999"), "detail: {detail}");
    bridge.shutdown();
}

#[test]
fn crash_mid_request_then_dead_then_restart_recovers() {
    let dir = temp_dir("crash");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "crash", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "ServerDied");
    // No silent respawn: the next call gets ServerDied too.
    let (is_error, result) = bridge.call_tool(
        "diagnostics",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "ServerDied");
    // The explicit restart is the only respawn path.
    let (is_error, result) = bridge.call_tool(
        "session_restart",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["restarted"], true);
    bridge.shutdown();
}

#[test]
fn slow_server_request_times_out_with_typed_error() {
    let dir = temp_dir("slow");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "slow", "request_timeout_ms = 1500", &[]);
    let mut bridge = Bridge::spawn(&config);
    let started = Instant::now();
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "ServerTimeout");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "timeout path must be bounded"
    );
    bridge.shutdown();
}

#[test]
fn oversized_references_yield_result_too_large_with_true_count() {
    let dir = temp_dir("bigrefs");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "bigrefs", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("references", hover_args(&ws));
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "ResultTooLarge");
    assert_eq!(result["error"]["items"], 1500);
    assert_eq!(result["error"]["budget_bytes"], 65_536);
    bridge.shutdown();
}

#[test]
fn workspace_escape_arguments_are_refused() {
    let dir = temp_dir("escape-args");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    for bad_path in ["../outside.rs", "/etc/passwd", "main.rs/../../etc/passwd"] {
        let (is_error, result) = bridge.call_tool(
            "hover",
            serde_json::json!({
                "workspace": ws.to_string_lossy(), "server": "fake",
                "path": bad_path, "line": 0, "character": 0
            }),
        );
        assert!(is_error, "path {bad_path} was not refused: {result}");
        assert_eq!(
            Bridge::error_code(&result),
            "WorkspaceEscape",
            "path {bad_path}"
        );
    }
    bridge.shutdown();
}

#[test]
fn mutation_tools_are_absent_and_unregistered() {
    let dir = temp_dir("gate-posture");
    let ws = make_workspace(&dir);
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let frame = bridge.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = frame["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        vec![
            "session_status",
            "diagnostics",
            "hover",
            "definition",
            "references",
            "document_symbols",
            "session_restart"
        ]
    );
    for forbidden in [
        "code_action_apply",
        "rename_preview",
        "rename_symbol",
        "edit_file",
    ] {
        let (is_error, result) = bridge.call_tool(forbidden, hover_args(&ws));
        assert!(is_error, "{forbidden} unexpectedly succeeded");
        assert_eq!(Bridge::error_code(&result), "UnsupportedMethod");
    }
    bridge.shutdown();
}

#[test]
fn apply_edit_push_is_refused_and_logged() {
    let dir = temp_dir("applyedit");
    let ws = make_workspace(&dir);
    let state_file = dir.join("fake-state.log");
    let config = write_fake_config(
        &dir,
        "applyedit",
        "",
        &[("FAKE_LSP_STATE_FILE", state_file.to_str().expect("utf8"))],
    );
    let mut bridge = Bridge::spawn(&config);
    let _ = bridge.call_tool("hover", hover_args(&ws));
    // The fake logs the bridge's refusal asynchronously; poll briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut logged = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&state_file) {
            logged = text;
            if logged.contains("applyedit_response") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        logged.contains("applyedit_response error:-32001"),
        "fake never logged a refusal; state file: {logged:?}"
    );
    let (_, status) = bridge.call_tool(
        "session_status",
        serde_json::json!({"workspace": ws.to_string_lossy(), "server": "fake"}),
    );
    assert!(
        status["protocol_violations"].as_u64().unwrap_or(0) >= 1,
        "violation not recorded in transcript: {status}"
    );
    bridge.shutdown();
}

#[test]
fn fifth_session_key_is_a_loud_error_not_an_eviction() {
    let dir = temp_dir("session-limit");
    let config = write_fake_config(&dir, "normal", "", &[]);
    let mut bridge = Bridge::spawn(&config);
    let mut workspaces: Vec<PathBuf> = Vec::new();
    for index in 0..5 {
        let sub = dir.join(format!("case-{index}"));
        std::fs::create_dir_all(&sub).expect("mkdir case");
        workspaces.push(make_workspace(&sub));
    }
    for ws in &workspaces[..4] {
        let (is_error, result) = bridge.call_tool("hover", hover_args(ws));
        assert!(!is_error, "{result}");
    }
    let (is_error, result) = bridge.call_tool("hover", hover_args(&workspaces[4]));
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "SessionLimit");
    bridge.shutdown();
}

#[test]
fn version_mismatch_is_refused_before_any_session() {
    let dir = temp_dir("version");
    let ws = make_workspace(&dir);
    let config = write_fake_config_version(&dir, "normal", "", &[], "theodolite-fake-lsp 9.9.9");
    let mut bridge = Bridge::spawn(&config);
    let (is_error, result) = bridge.call_tool("hover", hover_args(&ws));
    assert!(is_error, "{result}");
    assert_eq!(Bridge::error_code(&result), "ServerVersionMismatch");
    bridge.shutdown();
}

#[test]
fn orphan_gate_no_survivors_after_bridge_sigkill() {
    let dir = temp_dir("orphan");
    let ws = make_workspace(&dir);
    let pid_file = dir.join("fake.pid");
    let config = write_fake_config(
        &dir,
        "normal",
        "",
        &[("FAKE_LSP_PID_FILE", pid_file.to_str().expect("utf8"))],
    );
    let mut bridge = Bridge::spawn(&config);
    let (is_error, _) = bridge.call_tool("hover", hover_args(&ws));
    assert!(!is_error);
    // Wait for the fake to record its pid, then SIGKILL the bridge.
    let deadline = Instant::now() + Duration::from_secs(5);
    let fake_pid: u32 = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "fake never wrote its pid file");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(
        Path::new(&format!("/proc/{fake_pid}")).exists(),
        "fake server is not running before the kill"
    );
    bridge.child.kill().expect("SIGKILL the bridge");
    let _ = bridge.child.wait();
    // The pdeathsig discipline must take the fake down with it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{fake_pid}")).exists() {
        assert!(
            Instant::now() < deadline,
            "fake server (pid {fake_pid}) survived the bridge's SIGKILL by >5 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
