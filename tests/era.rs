//! Era gates (WS2 Wave 4, D-113): the migrated bridge speaks the
//! modern MCP era only (2026-07-28). Legacy contact is refused
//! with the designed typed errors and the server survives every
//! refusal; framing failures keep the baseline's exact shapes
//! wherever the baseline had one. Frames are raw strings and
//! responses are correlated by JSON-RPC id (rmcp dispatches
//! pipelined requests concurrently — the WS2 W1 §3.3 rule).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const FAKE_VERSION: &str = "theodolite-fake-lsp 1.0.0";
const META: &str = r#""_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "era-test", "version": "0"}, "io.modelcontextprotocol/clientCapabilities": {}}"#;

fn bridge_bin() -> &'static str {
    env!("CARGO_BIN_EXE_theodolite")
}

fn fake_bin() -> &'static str {
    env!("CARGO_BIN_EXE_theodolite-fake-lsp")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "theodolite-era-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_config(dir: &Path, extra_top: &str) -> PathBuf {
    let body = format!(
        "{extra_top}\n[[server]]\nid = \"fake\"\nbinary = \"{}\"\nargs = [\"--mode\", \"normal\"]\n\
         version_probe = [\"--version\"]\nexpect_version = \"{FAKE_VERSION}\"\n\
         languages = [\"rust\"]\nextensions = [\"rs\"]\nlanguage_ids = {{ rs = \"rust\" }}\n\
         readiness = \"immediate\"\nenv = {{ }}\n",
        fake_bin()
    );
    let path = dir.join("bridge.toml");
    std::fs::write(&path, body).expect("write config");
    path
}

struct RawBridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawBridge {
    fn spawn(config: &Path) -> RawBridge {
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
        RawBridge {
            child,
            stdin,
            stdout,
        }
    }

    /// Send raw lines; read exactly `expect` frames. Returns them
    /// in arrival order (callers correlate by id).
    fn exchange(&mut self, lines: &[String], expect: usize) -> Vec<serde_json::Value> {
        for line in lines {
            self.stdin.write_all(line.as_bytes()).expect("write frame");
            self.stdin.write_all(b"\n").expect("write newline");
        }
        self.stdin.flush().expect("flush");
        let mut frames = Vec::new();
        for _ in 0..expect {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read frame");
            assert!(read > 0, "bridge closed stdout unexpectedly");
            frames.push(serde_json::from_str(&line).expect("bridge frame is JSON"));
        }
        frames
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "bridge did not exit within 20 s of transport EOF"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => panic!("waiting on bridge: {error}"),
            }
        }
    }
}

fn by_id(frames: &[serde_json::Value], id: i64) -> &serde_json::Value {
    frames
        .iter()
        .find(|f| f.get("id").and_then(serde_json::Value::as_i64) == Some(id))
        .unwrap_or_else(|| panic!("no frame with id {id} in {frames:?}"))
}

fn discover(id: i64) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"server/discover","params":{{{META}}}}}"#)
}

fn initialize(id: i64, version: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"{version}","clientInfo":{{"name":"legacy-probe","version":"0"}},"capabilities":{{}}}}}}"#
    )
}

#[test]
fn discover_advertises_only_the_modern_era() {
    let dir = temp_dir("discover");
    let config = write_config(&dir, "mutation = \"off\"");
    let mut bridge = RawBridge::spawn(&config);
    let frames = bridge.exchange(&[discover(0)], 1);
    let result = &frames[0]["result"];
    assert_eq!(
        result["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "theodolite"
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    bridge.shutdown();
}

#[test]
fn legacy_contact_is_refused_and_the_server_survives() {
    let dir = temp_dir("refusal");
    let config = write_config(&dir, "mutation = \"off\"");
    let mut bridge = RawBridge::spawn(&config);
    let no_meta = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#.to_string();
    let legacy_meta = r#"{"jsonrpc":"2.0","id":5,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientInfo":{"name":"era-test","version":"0"},"io.modelcontextprotocol/clientCapabilities":{}}}}"#.to_string();
    let list = format!(r#"{{"jsonrpc":"2.0","id":6,"method":"tools/list","params":{{{META}}}}}"#);
    let frames = bridge.exchange(
        &[
            initialize(1, "2025-06-18"),
            initialize(2, "2024-11-05"),
            initialize(3, "2026-07-28"),
            no_meta,
            legacy_meta,
            discover(0),
            list,
            initialize(7, "2025-06-18"),
        ],
        8,
    );
    // Legacy initialize at any version: the designed -32022, with
    // the requested version named and 2026-07-28 the one support.
    for (id, requested) in [
        (1, "2025-06-18"),
        (2, "2024-11-05"),
        (3, "2026-07-28"),
        (7, "2025-06-18"),
    ] {
        let frame = by_id(&frames, id);
        assert_eq!(frame["error"]["code"], -32022, "{frame}");
        assert_eq!(frame["error"]["data"]["requested"], requested);
        assert_eq!(
            frame["error"]["data"]["supported"],
            serde_json::json!(["2026-07-28"])
        );
    }
    // A first request without _meta meets the SDK's metadata gate.
    assert_eq!(by_id(&frames, 4)["error"]["code"], -32602);
    // A legacy-version _meta meets the version refusal.
    assert_eq!(by_id(&frames, 5)["error"]["code"], -32022);
    // And after every refusal the server still serves.
    let list_frame = by_id(&frames, 6);
    assert_eq!(
        list_frame["result"]["tools"].as_array().map(Vec::len),
        Some(13)
    );
    bridge.shutdown();
}

#[test]
fn adversarial_frames_keep_the_framing_contract() {
    let dir = temp_dir("adversarial");
    let config = write_config(&dir, "mutation = \"off\"");
    let mut bridge = RawBridge::spawn(&config);
    let unknown =
        format!(r#"{{"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{{{META}}}}}"#);
    let oversized = "x".repeat(2_300_000);
    // 6 responses: parse error, -32600 (non-object), -32601
    // (unknown method), oversize -32700, discover, list after all.
    let frames = bridge.exchange(
        &[
            "this is not json".to_string(),
            "[1,2,3]".to_string(),
            unknown,
            oversized,
            discover(0),
            format!(r#"{{"jsonrpc":"2.0","id":9,"method":"tools/list","params":{{{META}}}}}"#),
        ],
        6,
    );
    let null_frames: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|f| f.get("id").is_some_and(serde_json::Value::is_null))
        .collect();
    // The baseline's parse-error shape, serde detail included.
    assert!(
        null_frames.iter().any(|f| f["error"]["code"] == -32700
            && f["error"]["message"] == "parse error: expected ident at line 1 column 2"),
        "{null_frames:?}"
    );
    // The fleet framing contract for a non-object request.
    assert!(
        null_frames.iter().any(|f| f["error"]["code"] == -32600),
        "{null_frames:?}"
    );
    // The baseline's own frame cap and message.
    assert!(
        null_frames.iter().any(|f| f["error"]["code"] == -32700
            && f["error"]["message"] == "frame exceeds the 2097152-byte MCP cap"),
        "{null_frames:?}"
    );
    // Unknown method: the baseline's exact message.
    let unknown_frame = by_id(&frames, 2);
    assert_eq!(unknown_frame["error"]["code"], -32601);
    assert_eq!(
        unknown_frame["error"]["message"],
        "method not supported by theodolite: prompts/list"
    );
    // Server alive and serving after all of it.
    assert_eq!(
        by_id(&frames, 9)["result"]["tools"]
            .as_array()
            .map(Vec::len),
        Some(13)
    );
    bridge.shutdown();
}

#[test]
fn notification_shaped_call_is_not_executed() {
    let dir = temp_dir("notification");
    let config = write_config(&dir, "mutation = \"off\"");
    let mut bridge = RawBridge::spawn(&config);
    let notification = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"session_status","arguments":{"workspace":"/tmp","server":"fake"}}}"#.to_string();
    let list = format!(r#"{{"jsonrpc":"2.0","id":9,"method":"tools/list","params":{{{META}}}}}"#);
    // Only discover + list answer; the notification produces none.
    let frames = bridge.exchange(&[discover(0), notification, list], 2);
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|f| f.get("result").is_some()));
    bridge.shutdown();
}

#[test]
fn mutation_registration_follows_meta_identity() {
    let dir = temp_dir("posture");
    let config = write_config(
        &dir,
        "mutation = \"gated\"\nmutation_allowlist = [\"gate-probe\"]",
    );
    let mut bridge = RawBridge::spawn(&config);
    let list_as = |id: i64, name: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{{"name":"{name}","version":"0"}},"io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
        )
    };
    let frames = bridge.exchange(
        &[
            discover(0),
            list_as(1, "gate-probe"),
            list_as(2, "stranger"),
        ],
        3,
    );
    // Allowlisted identity (in _meta, the modern handshake's
    // successor): the mutation class registers — 13 + 2.
    assert_eq!(
        by_id(&frames, 1)["result"]["tools"]
            .as_array()
            .map(Vec::len),
        Some(15)
    );
    // Any other identity: absent, exactly as under the legacy
    // handshake.
    assert_eq!(
        by_id(&frames, 2)["result"]["tools"]
            .as_array()
            .map(Vec::len),
        Some(13)
    );
    bridge.shutdown();
}
