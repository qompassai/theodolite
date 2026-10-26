//! The parity gate (SPEC §9 gate 1): the bridge's `diagnostics`
//! output for a fixture Rust workspace must equal, byte-for-byte
//! after canonicalization, the diagnostics bacon-ls itself produces
//! when driven standalone over the same fixture at the same pinned
//! version.
//!
//! This harness runs wherever a bacon-ls binary is available
//! (primo). Where none exists it prints the SKIP reason and passes
//! vacuously — the gate is never faked: BUILD-REPORT.md records it
//! as SKIPPED in the sandbox with the exact primo steps to run it.
//!
//! Find bacon-ls via, in order: `$THEODOLITE_BACON_LS`,
//! `$HOME/.cargo/bin/bacon-ls`, or a `PATH` lookup.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use theodolite::lsp::{LspMessage, read_message, write_message};
use theodolite::shaping::{Diagnostic, diagnostic_json, parse_range, sort_diagnostics};

const FIXTURE_LIB: &str = r#"pub fn used() -> u32 {
    1
}

fn never_called() -> u32 {
    2
}

pub fn with_warning() -> bool {
    let value = 1;
    value == 1
}
"#;

fn find_bacon_ls() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("THEODOLITE_BACON_LS") {
        let path = PathBuf::from(explicit);
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(home).join(".cargo/bin/bacon-ls");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let output = Command::new("which").arg("bacon-ls").output().ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !text.is_empty() {
            return Some(PathBuf::from(text));
        }
    }
    None
}

fn probe_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn make_fixture(dir: &Path) -> PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join("src")).expect("mkdir fixture");
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"theodolite-parity-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(ws.join("src/lib.rs"), FIXTURE_LIB).expect("write lib.rs");
    ws.canonicalize().expect("canonical fixture")
}

/// A minimal standalone LSP client used to drive bacon-ls directly,
/// the same way the bridge drives it — so the comparison is between
/// two shapings of one server's truth, not two clients' guesses.
struct DirectClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    diagnostics: Vec<Diagnostic>,
    generation: u64,
}

impl DirectClient {
    fn spawn(binary: &Path, workspace: &Path) -> DirectClient {
        let mut child = Command::new(binary)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn standalone bacon-ls");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        DirectClient {
            child,
            stdin,
            stdout,
            next_id: 0,
            diagnostics: Vec::new(),
            generation: 0,
        }
    }

    fn send(&mut self, value: &serde_json::Value) {
        write_message(&mut self.stdin, "bacon-ls", value).expect("write to bacon-ls");
    }

    fn pump_until(&mut self, want_id: Option<i64>, deadline: Instant) -> Option<serde_json::Value> {
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            match read_message(&mut self.stdout, "bacon-ls") {
                Ok(Some(LspMessage::Response { id, result })) => {
                    if want_id == Some(id) {
                        return Some(result.ok().unwrap_or(serde_json::Value::Null));
                    }
                }
                Ok(Some(LspMessage::Notification { method, params })) => {
                    if method == "textDocument/publishDiagnostics" {
                        self.absorb(&params);
                    }
                }
                Ok(Some(LspMessage::Request { id, .. })) => {
                    self.send(&serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": null
                    }));
                }
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
    }

    fn absorb(&mut self, params: &serde_json::Value) {
        let uri = params
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if let Some(items) = params
            .get("diagnostics")
            .and_then(serde_json::Value::as_array)
        {
            self.diagnostics.retain(|d| d.uri != uri);
            for item in items {
                self.diagnostics.push(Diagnostic {
                    uri: uri.to_string(),
                    range: parse_range(item.get("range").unwrap_or(&serde_json::Value::Null)),
                    severity: item
                        .get("severity")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    source: item
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    code: match item.get("code") {
                        Some(serde_json::Value::String(text)) => text.clone(),
                        Some(serde_json::Value::Number(number)) => number.to_string(),
                        _ => String::new(),
                    },
                    message: item
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            self.generation += 1;
        }
    }
}

/// Drive bacon-ls standalone: initialize, open the fixture file,
/// wait for the diagnostics generation to settle, return the
/// canonical diagnostics JSON (absolute URIs relativized by caller).
fn standalone_diagnostics(binary: &Path, workspace: &Path) -> Vec<Diagnostic> {
    let mut client = DirectClient::spawn(binary, workspace);
    client.next_id += 1;
    let root_uri = theodolite::shaping::path_to_uri(workspace);
    client.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": client.next_id, "method": "initialize",
        "params": {
            "processId": null, "rootUri": root_uri,
            "capabilities": {"textDocument": {"publishDiagnostics": {}}},
            "clientInfo": {"name": "theodolite-parity-harness", "version": "0.1.0"}
        }
    }));
    let deadline = Instant::now() + Duration::from_secs(60);
    let init = client.pump_until(Some(client.next_id), deadline);
    assert!(init.is_some(), "bacon-ls never answered initialize");
    client.send(&serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    let file = workspace.join("src/lib.rs");
    let text = std::fs::read_to_string(&file).expect("read fixture lib");
    client.send(&serde_json::json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {
            "uri": theodolite::shaping::path_to_uri(&file),
            "languageId": "rust", "version": 1, "text": text
        }}
    }));
    // Settle: generation stable across two pumps 250 ms apart.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_generation: Option<u64> = None;
    loop {
        let _ = client.pump_until(None, Instant::now() + Duration::from_millis(250));
        if last_generation == Some(client.generation) && client.generation > 0 {
            break;
        }
        last_generation = Some(client.generation);
        assert!(
            Instant::now() < deadline,
            "standalone bacon-ls diagnostics never settled"
        );
    }
    client.next_id += 1;
    client.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": client.next_id, "method": "shutdown", "params": null
    }));
    let _ = client.pump_until(
        Some(client.next_id),
        Instant::now() + Duration::from_secs(10),
    );
    client.send(&serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
    let _ = client.child.wait();
    client.diagnostics.clone()
}

#[test]
fn parity_gate_bridge_diagnostics_equal_standalone_bacon_ls() {
    let Some(binary) = find_bacon_ls() else {
        eprintln!(
            "SKIP parity gate: bacon-ls not found (checked $THEODOLITE_BACON_LS, \
             $HOME/.cargo/bin/bacon-ls, PATH). Run on primo per BUILD-REPORT.md."
        );
        return;
    };
    let Some(version) = probe_version(&binary) else {
        eprintln!(
            "SKIP parity gate: bacon-ls at {} failed its --version probe",
            binary.display()
        );
        return;
    };
    let dir = std::env::temp_dir().join(format!("theodolite-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir parity dir");
    let ws = make_fixture(&dir);

    // Side A: bacon-ls driven standalone.
    let mut standalone = standalone_diagnostics(&binary, &ws);
    for diagnostic in &mut standalone {
        diagnostic.uri =
            theodolite::shaping::relativize_uri(&diagnostic.uri, &ws).expect("relativize");
    }
    sort_diagnostics(&mut standalone);
    let standalone_json: Vec<serde_json::Value> = standalone.iter().map(diagnostic_json).collect();

    // Side B: the bridge's diagnostics tool over the same fixture.
    let bridge = env!("CARGO_BIN_EXE_theodolite");
    let config = dir.join("bridge.toml");
    std::fs::write(
        &config,
        format!(
            "ready_timeout_ms = 120000\n[[server]]\nid = \"bacon-ls\"\nbinary = \"{}\"\n\
             version_probe = [\"--version\"]\nexpect_version = \"{version}\"\n\
             languages = [\"rust\"]\nextensions = [\"rs\"]\n\
             language_ids = {{ rs = \"rust\" }}\nreadiness = \"immediate\"\n\
             diagnostics_owner = true\n",
            binary.display()
        ),
    )
    .expect("write parity config");
    let mut child = Command::new(bridge)
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge for parity");
    let mut stdin = child.stdin.take().expect("bridge stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
    let mut send = |value: serde_json::Value| {
        writeln!(stdin, "{value}").expect("write bridge frame");
        stdin.flush().expect("flush bridge");
    };
    let recv = |stdout: &mut BufReader<ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read bridge frame");
        serde_json::from_str(&line).expect("bridge frame JSON")
    };
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05"}
    }));
    let _ = recv(&mut stdout);
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "diagnostics", "arguments": {
            "workspace": ws.to_string_lossy(), "server": "bacon-ls", "path": "src/lib.rs"
        }}
    }));
    let frame = recv(&mut stdout);
    drop(stdin);
    let _ = child.wait();
    let text = frame["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text");
    let bridge_result: serde_json::Value = serde_json::from_str(text).expect("tool JSON");
    assert_eq!(
        bridge_result["isError"],
        serde_json::Value::Null,
        "{bridge_result}"
    );
    let bridge_json = bridge_result["diagnostics"].clone();

    assert_eq!(
        bridge_json,
        serde_json::Value::Array(standalone_json),
        "bridge diagnostics diverge from standalone bacon-ls"
    );
}
