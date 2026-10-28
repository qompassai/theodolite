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
use theodolite::shaping::{
    Diagnostic, DiagnosticChannel, diagnostic_json, parse_range, sort_diagnostics,
};

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

pub fn broken() -> u32 {
    "not a number"
}
"#;

/// The `initializationOptions` Diver's own bacon-ls config passes
/// (lsp/bacon_ls.lua in the diver repo). Without them bacon-ls stays
/// in its degraded mode: it answers initialize, publishes one empty
/// diagnostics set, and never runs its backend — a bare client sees
/// an empty, quiet server. Both sides of the gate use these verbatim
/// so the comparison covers the server's real operating mode.
fn bacon_initialization_options() -> serde_json::Value {
    serde_json::json!({
        "createBaconPreferencesFile": true,
        "locationsFile": ".bacon-locations",
        "runBaconInBackground": true,
        "runBaconInBackgroundCommandArguments": "--headless -j bacon-ls",
        "synchronizeAllOpenFilesWaitMillis": 2000,
        "updateOnChange": true,
        "updateOnSave": true,
        "updateOnSaveWaitMillis": 1000,
        "useBaconBackend": true,
        "validateBaconPreferences": true
    })
}

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
    messages: std::sync::mpsc::Receiver<theodolite::errors::BridgeResult<Option<LspMessage>>>,
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
        // Reader thread: `read_message` is a plain blocking read with
        // no timeout, so deadlines can only be honored by receiving
        // with a timeout. (The first primo run of this harness hung
        // 44 minutes at exactly this spot: the settle loop's deadline
        // was only consulted between messages, and a quiet server
        // never produced another one.)
        let stdout = child.stdout.take().expect("stdout");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let item = read_message(&mut reader, "bacon-ls");
                let terminal = matches!(item, Ok(None) | Err(_));
                if sender.send(item).is_err() || terminal {
                    break;
                }
            }
        });
        DirectClient {
            child,
            stdin,
            messages: receiver,
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
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let item = match self.messages.recv_timeout(remaining) {
                Ok(item) => item,
                Err(_) => return None,
            };
            match item {
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
                    channel: DiagnosticChannel::Push,
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
    // The initialize params mirror the bridge's own (session.rs):
    // an earlier harness revision sent a bare rootUri-only request
    // and bacon-ls never published real diagnostics to it — it
    // answers, syncs one empty set, and stays quiet. The comparison
    // is only meaningful when both clients present the same shape.
    let name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    client.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": client.next_id, "method": "initialize",
        "params": {
            "processId": null, "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": false},
                    "publishDiagnostics": {"relatedInformation": false},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {},
                    "references": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true}
                },
                "window": {"workDoneProgress": true},
                "experimental": {"serverStatusNotification": true}
            },
            "clientInfo": {"name": "theodolite-parity-harness", "version": "0.1.0"},
            "trace": "off",
            "initializationOptions": bacon_initialization_options()
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
    // Settle: bacon-ls publishes an empty set immediately, then the
    // real set only after its background backend has run (Diver's
    // init options carry a 2 s synchronize wait plus job time), so
    // a short stability window fires during the startup gap — the
    // naive-quiescence trap. Require a minimum observation period
    // after didOpen, then a full 2 s with no generation change.
    let opened_at = Instant::now();
    let deadline = opened_at + Duration::from_secs(120);
    let mut last_generation = client.generation;
    let mut last_change = Instant::now();
    loop {
        let _ = client.pump_until(None, Instant::now() + Duration::from_millis(500));
        if client.generation != last_generation {
            last_generation = client.generation;
            last_change = Instant::now();
        }
        if client.generation > 0
            && opened_at.elapsed() >= Duration::from_secs(6)
            && last_change.elapsed() >= Duration::from_secs(2)
        {
            break;
        }
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

    // Warm the fixture (measured behavior, primo 2026-10-26):
    // bacon-ls publishes, on open, the state of its `.bacon-locations`
    // file. On a cold directory the background backend computes the
    // real results into that file but the LSP does not republish them
    // to the already-open session (in editor use, the next edit/save
    // cycle republishes). A cold standalone client therefore sees an
    // empty set while any client opening the warmed directory sees
    // the full set — the gate compares two shapings of one server
    // truth, so both sides must observe the same server state. One
    // throwaway session brings the backend's first run to completion;
    // both measured sides then start fresh against the warmed dir.
    {
        let mut warm = DirectClient::spawn(&binary, &ws);
        warm.next_id += 1;
        let root_uri = theodolite::shaping::path_to_uri(&ws);
        let name = ws
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        warm.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": warm.next_id, "method": "initialize",
            "params": {
                "processId": null, "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": name}],
                "capabilities": {"textDocument": {"publishDiagnostics": {}}},
                "clientInfo": {"name": "theodolite-parity-warmup", "version": "0.1.0"},
                "initializationOptions": bacon_initialization_options()
            }
        }));
        let init_deadline = Instant::now() + Duration::from_secs(60);
        assert!(
            warm.pump_until(Some(warm.next_id), init_deadline).is_some(),
            "warm-up bacon-ls never answered initialize"
        );
        warm.send(&serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        let file = ws.join("src/lib.rs");
        let text = std::fs::read_to_string(&file).expect("read fixture lib");
        warm.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {
                "uri": theodolite::shaping::path_to_uri(&file),
                "languageId": "rust", "version": 1, "text": text
            }}
        }));
        let locations = ws.join(".bacon-locations");
        let warm_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let _ = warm.pump_until(None, Instant::now() + Duration::from_millis(500));
            let computed = std::fs::read_to_string(&locations)
                .map(|text| text.contains("mismatched types"))
                .unwrap_or(false);
            if computed {
                break;
            }
            assert!(
                Instant::now() < warm_deadline,
                "bacon backend never computed the fixture diagnostics"
            );
        }
        warm.next_id += 1;
        warm.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": warm.next_id, "method": "shutdown", "params": null
        }));
        let _ = warm.pump_until(Some(warm.next_id), Instant::now() + Duration::from_secs(10));
        warm.send(&serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
        let _ = warm.child.wait();
    }

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
             diagnostics_owner = true\n\
             initialization_options = {{ createBaconPreferencesFile = true, \
             locationsFile = \".bacon-locations\", runBaconInBackground = true, \
             runBaconInBackgroundCommandArguments = \"--headless -j bacon-ls\", \
             synchronizeAllOpenFilesWaitMillis = 2000, updateOnChange = true, \
             updateOnSave = true, updateOnSaveWaitMillis = 1000, \
             useBaconBackend = true, validateBaconPreferences = true }}\n",
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
    // Modern era (2026-07-28, WS2 W4): no handshake — discovery
    // opens the session and every request carries `_meta`.
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {"_meta": meta}
    }));
    let _ = recv(&mut stdout);
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "diagnostics", "_meta": meta, "arguments": {
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
