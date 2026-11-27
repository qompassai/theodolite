//! Hot-path benchmarks for theodolite: cold start to discover, tools/list latency, and the representative tool call (session_status). Hand-rolled on std::time; run with `cargo bench --bench hot_paths`.
#![allow(dead_code)]
const BIN_NAME: &str = "theodolite";
const BIN_ENV: &str = "THEODOLITE_BIN";
const REPO_TAG: &str = "theodolite";
const TOOL_NAME: &str = "session_status";

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2026-07-28";
const DEADLINE: Duration = Duration::from_secs(30);

fn temp_root(tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "bench-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .map_err(|e| format!("system time: {e}"))?
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn binary_path() -> PathBuf {
    if let Ok(p) = std::env::var(BIN_ENV)
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join(BIN_NAME)
}

fn meta() -> Value {
    let mut m = json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientInfo": {"name": "bench-example", "version": "0.1.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    if let Some((k, v)) = extra_meta() {
        m[k] = v;
    }
    m
}

fn frame(id: u64, method: &str, mut params: Value) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"] = meta();
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Session {
    fn spawn(command: &mut Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn server: {e}"))?;
        let stdin = child.stdin.take().ok_or("server stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("server stdout unavailable")?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines: rx,
            next_id: 0,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let req = frame(id, method, params);
        let text = serde_json::to_string(&req).map_err(|e| format!("encode request: {e}"))?;
        writeln!(self.stdin, "{text}").map_err(|e| format!("write request: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush: {e}"))?;
        loop {
            let line = self
                .lines
                .recv_timeout(DEADLINE)
                .map_err(|e| format!("no response for id {id} within {DEADLINE:?}: {e}"))?;
            let value: Value =
                serde_json::from_str(&line).map_err(|e| format!("parse response: {e}"))?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(value);
            }
        }
    }

    fn discover(&mut self) -> Result<Value, String> {
        self.request("server/discover", json!({}))
    }

    fn tools_list(&mut self) -> Result<Value, String> {
        self.request("tools/list", json!({}))
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    fn finish(mut self) -> Result<(), String> {
        drop(self.stdin);
        let started = Instant::now();
        loop {
            match self
                .child
                .try_wait()
                .map_err(|e| format!("poll server: {e}"))?
            {
                Some(status) => {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("server exited with {status}"));
                }
                None => {
                    if started.elapsed() > DEADLINE {
                        let _ = self.child.kill();
                        return Err("server did not exit before deadline".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn result_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        format!("{}...", &text[..max])
    }
}

fn extra_meta() -> Option<(&'static str, Value)> {
    None
}

/// Build the server command: theodolite needs `--config <bridge.toml>`.
/// The config pins the crate's own fake LSP (built beside the server
/// binary) in `normal` mode, exactly as the integration tests do.
fn server_command(root: &Path) -> Result<Command, String> {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).map_err(|e| format!("mkdir ws: {e}"))?;
    std::fs::write(ws.join("main.rs"), "fn main() {}\n")
        .map_err(|e| format!("write fixture: {e}"))?;
    let fake = binary_path().with_file_name("theodolite-fake-lsp");
    let body = format!(
        "[[server]]\nid = \"fake\"\nbinary = \"{}\"\nargs = [\"--mode\", \"normal\"]\nversion_probe = [\"--version\"]\nexpect_version = \"theodolite-fake-lsp 1.0.0\"\nlanguages = [\"rust\"]\nextensions = [\"rs\"]\nlanguage_ids = {{ rs = \"rust\" }}\nreadiness = \"immediate\"\nenv = {{}}\n",
        fake.display()
    );
    let config = root.join("bridge.toml");
    std::fs::write(&config, body).map_err(|e| format!("write config: {e}"))?;
    let mut cmd = Command::new(binary_path());
    cmd.arg("--config").arg(&config);
    Ok(cmd)
}

fn representative_args(root: &Path) -> Value {
    json!({"workspace": root.join("ws").to_string_lossy(), "server": "fake"})
}

const COLD_START_RUNS: usize = 5;
const WARMUP_CALLS: usize = 20;
const MEASURED_CALLS: usize = 200;

fn report(label: &str, iterations: usize, elapsed: Duration) {
    let total_ns = elapsed.as_nanos() as f64;
    let per = total_ns / iterations as f64;
    let ops = if per > 0.0 {
        1_000_000_000.0 / per
    } else {
        0.0
    };
    println!(
        "workload={label} iterations={iterations} total_ms={:.3} ns_per_op={per:.1} ops_per_sec={ops:.1}",
        elapsed.as_secs_f64() * 1000.0
    );
}

fn run() -> Result<(), String> {
    // Cold start: spawn -> server/discover response, averaged.
    let root = temp_root(REPO_TAG)?;
    let mut cold_total = Duration::ZERO;
    for _ in 0..COLD_START_RUNS {
        let started = Instant::now();
        let mut cmd = server_command(&root)?;
        let mut session = Session::spawn(&mut cmd)?;
        let _ = session.discover()?;
        cold_total += started.elapsed();
        session.finish()?;
    }
    report("cold_start_discover", COLD_START_RUNS, cold_total);

    // Warm session for the steady-state workloads.
    let mut cmd = server_command(&root)?;
    let mut session = Session::spawn(&mut cmd)?;
    let _ = session.discover()?;
    for _ in 0..WARMUP_CALLS {
        let _ = session.tools_list()?;
    }
    let started = Instant::now();
    for _ in 0..MEASURED_CALLS {
        let _ = session.tools_list()?;
    }
    report("tools_list", MEASURED_CALLS, started.elapsed());

    let args = representative_args(&root);
    for _ in 0..WARMUP_CALLS {
        let _ = session.call_tool(TOOL_NAME, args.clone())?;
    }
    let started = Instant::now();
    for _ in 0..MEASURED_CALLS {
        let _ = session.call_tool(TOOL_NAME, args.clone())?;
    }
    report("tool_call", MEASURED_CALLS, started.elapsed());
    // Label the tool-call workload with its tool for the report.
    println!("tool_call_tool={TOOL_NAME}");
    session.finish()?;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("hot_paths: error: {e}");
        std::process::exit(1);
    }
}
