//! theodolite-fake-lsp — a scripted LSP test double (SPEC §9.4).
//!
//! Not a product binary: a deterministic fake language server the
//! test suite drives the bridge against. Behavior is selected with
//! `--mode`; `--version` answers the bridge's version probe.
//!
//! Modes: `normal` (correct answers in deliberately scrambled
//! order, two diagnostics waves), `bigrefs` (references past the
//! result budget), `crash` (dies on the first hover request),
//! `slow` (never answers after initialize), `oversized` (declares a
//! Content-Length past the frame cap), `garbage` (frames non-JSON),
//! `truncated` (short body then EOF), `unknown-id` (sends a response
//! with an id the bridge never minted), `applyedit` (pushes a
//! `workspace/applyEdit` request after didOpen and logs the bridge's
//! refusal to `$FAKE_LSP_STATE_FILE`), `pull` (advertises
//! `diagnosticProvider`; pushes a two-item set on didOpen and answers
//! `textDocument/diagnostic` with one duplicate of a pushed item and
//! one pull-only item, for the P3 merge gate), `rich` (advertises
//! the definition-family, workspace-symbol, and code-action
//! providers with scripted answers, for the P3 read-class gates).
//!
//! `$FAKE_LSP_PID_FILE`, when set, receives the fake's pid at start.

#![forbid(unsafe_code)]

use std::io::Write;

use theodolite::lsp::{LspMessage, read_message, write_message};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("theodolite-fake-lsp 1.0.0");
        return;
    }
    let mode = args
        .windows(2)
        .find(|pair| pair[0] == "--mode")
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| "normal".to_string());
    if let Ok(pid_file) = std::env::var("FAKE_LSP_PID_FILE") {
        let _ = std::fs::write(&pid_file, std::process::id().to_string());
    }
    serve(&mode);
}

struct Fake {
    mode: String,
    root_uri: String,
    opened_uri: Option<String>,
    diagnostics_sent: bool,
}

fn serve(mode: &str) {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin);
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut fake = Fake {
        mode: mode.to_string(),
        root_uri: String::new(),
        opened_uri: None,
        diagnostics_sent: false,
    };
    loop {
        let message = match read_message(&mut reader, "fake-lsp") {
            Ok(Some(message)) => message,
            _ => return, // EOF or unreadable client traffic: exit quietly
        };
        match message {
            LspMessage::Request { id, method, params } => {
                if !fake.handle_request(&mut writer, id, &method, &params) {
                    return;
                }
            }
            LspMessage::Notification { method, params } => {
                if method == "exit" {
                    return;
                }
                if method == "textDocument/didOpen" {
                    fake.handle_did_open(&mut writer, &params);
                }
            }
            LspMessage::Response { result, .. } => {
                // Only applyedit mode sends requests; the bridge's
                // answer to ours lands here and is logged.
                if fake.mode == "applyedit" {
                    let rendered = match &result {
                        Ok(value) => format!("ok:{value}"),
                        Err((code, message)) => format!("error:{code}:{message}"),
                    };
                    log_state(&format!("applyedit_response {rendered}"));
                }
            }
        }
    }
}

impl Fake {
    /// Returns false when the process should exit.
    fn handle_request(
        &mut self,
        writer: &mut impl Write,
        id: serde_json::Value,
        method: &str,
        params: &serde_json::Value,
    ) -> bool {
        if self.mode == "slow" && method != "initialize" {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        if self.mode == "crash" && method == "textDocument/hover" {
            std::process::exit(2);
        }
        match method {
            "initialize" => {
                self.root_uri = params
                    .get("rootUri")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let capabilities = match self.mode.as_str() {
                    "pull" => serde_json::json!({
                        "diagnosticProvider": {
                            "interFileDependencies": false,
                            "workspaceDiagnostics": false
                        }
                    }),
                    "rich" => serde_json::json!({
                        "declarationProvider": true,
                        "typeDefinitionProvider": true,
                        "implementationProvider": true,
                        "workspaceSymbolProvider": true,
                        "codeActionProvider": true
                    }),
                    _ => serde_json::json!({}),
                };
                respond(
                    writer,
                    id,
                    serde_json::json!({
                        "capabilities": capabilities,
                        "serverInfo": {"name": "theodolite-fake-lsp", "version": "1.0.0"}
                    }),
                );
                match self.mode.as_str() {
                    "oversized" => {
                        // A header declaring a body past the frame
                        // cap, then nothing: the bridge must reject
                        // it before allocating.
                        let _ = writer.write_all(b"Content-Length: 99999999\r\n\r\n");
                        let _ = writer.flush();
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        return false;
                    }
                    "garbage" => {
                        let body = b"this is not json at all";
                        let header = format!("Content-Length: {}\r\n\r\n", body.len());
                        let _ = writer.write_all(header.as_bytes());
                        let _ = writer.write_all(body);
                        let _ = writer.flush();
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        return false;
                    }
                    "truncated" => {
                        let _ = writer.write_all(b"Content-Length: 100\r\n\r\n{\"jsonrpc\"");
                        let _ = writer.flush();
                        return false;
                    }
                    "unknown-id" => {
                        let _ = write_message(
                            writer,
                            "fake-lsp",
                            &serde_json::json!({"jsonrpc": "2.0", "id": 9999, "result": null}),
                        );
                    }
                    _ => {}
                }
                true
            }
            "shutdown" => {
                respond(writer, id, serde_json::Value::Null);
                true
            }
            "textDocument/diagnostic" => {
                // Pull mode: a full report with one item identical to
                // a pushed item (same range/severity/source/code/
                // message) and one pull-only item.
                respond(
                    writer,
                    id,
                    serde_json::json!({
                        "kind": "full",
                        "items": [
                            {
                                "range": range(1, 0, 1, 5),
                                "severity": 1,
                                "source": "clippy",
                                "code": "shared_code",
                                "message": "in both channels"
                            },
                            {
                                "range": range(7, 0, 7, 3),
                                "severity": 1,
                                "source": "rustc",
                                "code": "pull_only",
                                "message": "pull only error"
                            }
                        ]
                    }),
                );
                true
            }
            "textDocument/hover" => {
                respond(
                    writer,
                    id,
                    serde_json::json!({
                        "contents": {"kind": "markdown", "value": "fake hover for `foo`"},
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 3}
                        }
                    }),
                );
                true
            }
            "textDocument/definition" => {
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {"uri": format!("{root}/src/other.rs"), "range": range(5, 0, 5, 3)},
                        {"uri": format!("{root}/src/lib.rs"), "range": range(2, 0, 2, 3)}
                    ]),
                );
                true
            }
            "textDocument/declaration" => {
                // Single Location object (not an array): the bridge
                // must normalize all three LSP result shapes.
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!({
                        "uri": format!("{root}/src/lib.rs"), "range": range(1, 0, 1, 3)
                    }),
                );
                true
            }
            "textDocument/typeDefinition" => {
                // LocationLink shape: the bridge must land on
                // targetSelectionRange, not targetRange.
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!([{
                        "originSelectionRange": range(6, 6, 6, 10),
                        "targetUri": format!("{root}/src/lib.rs"),
                        "targetRange": range(2, 0, 4, 1),
                        "targetSelectionRange": range(2, 4, 2, 9)
                    }]),
                );
                true
            }
            "textDocument/implementation" => {
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {"uri": format!("{root}/src/other.rs"), "range": range(5, 0, 5, 3)},
                        {"uri": format!("{root}/src/lib.rs"), "range": range(3, 0, 3, 3)}
                    ]),
                );
                true
            }
            "textDocument/references" => {
                if self.mode == "bigrefs" {
                    let root = self.root_uri.clone();
                    let many: Vec<serde_json::Value> = (0..1500u64)
                        .map(|line| {
                            serde_json::json!({
                                "uri": format!("{root}/src/lib.rs"),
                                "range": range(line, 0, line, 3)
                            })
                        })
                        .collect();
                    respond(writer, id, serde_json::Value::Array(many));
                    return true;
                }
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {"uri": format!("{root}/src/lib.rs"), "range": range(9, 1, 9, 4)},
                        {"uri": format!("{root}/src/lib.rs"), "range": range(3, 1, 3, 4)},
                        {"uri": format!("{root}/src/lib.rs"), "range": range(7, 1, 7, 4)}
                    ]),
                );
                true
            }
            "workspace/symbol" => {
                let query = params
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let root = self.root_uri.clone();
                if query == "many" {
                    // Budget stress: 1 500 symbols — well past the
                    // 64 KiB shaped-result budget.
                    let many: Vec<serde_json::Value> = (0..1500u64)
                        .map(|line| {
                            serde_json::json!({
                                "name": format!("symbol_{line}"),
                                "kind": 6,
                                "location": {
                                    "uri": format!("{root}/src/lib.rs"),
                                    "range": range(line, 0, line, 7)
                                },
                                "containerName": "stress"
                            })
                        })
                        .collect();
                    respond(writer, id, serde_json::Value::Array(many));
                    return true;
                }
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {
                            "name": "beta_fn", "kind": 12,
                            "location": {
                                "uri": format!("{root}/src/other.rs"),
                                "range": range(3, 0, 3, 7)
                            },
                            "containerName": ""
                        },
                        {
                            "name": "alpha_fn", "kind": 12,
                            "location": {
                                "uri": format!("{root}/src/lib.rs"),
                                "range": range(1, 0, 1, 8)
                            },
                            "containerName": "mod_a"
                        },
                        {
                            "name": "alpha_type", "kind": 5,
                            "location": {
                                "uri": format!("{root}/src/lib.rs"),
                                "range": range(9, 0, 9, 10)
                            },
                            "containerName": ""
                        }
                    ]),
                );
                true
            }
            "textDocument/codeAction" => {
                let start_line = params
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                if start_line == 99 {
                    // A region with no actions: a valid empty list.
                    respond(writer, id, serde_json::json!([]));
                    return true;
                }
                let root = self.root_uri.clone();
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {"title": "Zulu refactor", "kind": "refactor.extract"},
                        {
                            "title": "Fix the thing",
                            "kind": "quickfix",
                            "edit": {"changes": {format!("{root}/src/lib.rs"): [{
                                "range": range(1, 0, 1, 5),
                                "newText": "PAYLOAD-MARKER-EDIT"
                            }]}},
                            "command": {
                                "title": "apply",
                                "command": "secret.command.id",
                                "arguments": ["PAYLOAD-MARKER-COMMAND"]
                            }
                        },
                        {
                            "title": "Legacy command",
                            "command": "legacy.cmd",
                            "arguments": []
                        },
                        {
                            "title": "Alphabet fix",
                            "kind": "source.fixAll",
                            "command": {"title": "c", "command": "fix.all"}
                        }
                    ]),
                );
                true
            }
            "textDocument/documentSymbol" => {
                respond(
                    writer,
                    id,
                    serde_json::json!([
                        {
                            "name": "outer_fn", "kind": 12,
                            "range": range(0, 0, 10, 1),
                            "selectionRange": range(0, 3, 0, 11),
                            "children": [{
                                "name": "inner_value", "kind": 13,
                                "range": range(2, 4, 2, 15),
                                "selectionRange": range(2, 8, 2, 19)
                            }]
                        },
                        {
                            "name": "aaa_first", "kind": 12,
                            "range": range(0, 0, 0, 20),
                            "selectionRange": range(0, 3, 0, 12)
                        }
                    ]),
                );
                true
            }
            _ => {
                let _ = write_message(
                    writer,
                    "fake-lsp",
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": format!("fake has no {method}")}
                    }),
                );
                true
            }
        }
    }

    fn handle_did_open(&mut self, writer: &mut impl Write, params: &serde_json::Value) {
        let uri = params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.opened_uri = Some(uri.clone());
        if self.mode == "applyedit" {
            let _ = write_message(
                writer,
                "fake-lsp",
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": 42, "method": "workspace/applyEdit",
                    "params": {"edit": {"changes": {}}}
                }),
            );
            return;
        }
        if self.mode == "pull" {
            if self.diagnostics_sent {
                return;
            }
            self.diagnostics_sent = true;
            // The push half of the pull-merge fixture: one item the
            // pull report repeats verbatim, one push-only item.
            let diagnostics = vec![
                serde_json::json!({
                    "range": range(1, 0, 1, 5),
                    "severity": 1,
                    "source": "clippy",
                    "code": "shared_code",
                    "message": "in both channels"
                }),
                serde_json::json!({
                    "range": range(4, 2, 4, 9),
                    "severity": 2,
                    "source": "clippy",
                    "code": "push_only",
                    "message": "push only warning"
                }),
            ];
            let _ = write_message(
                writer,
                "fake-lsp",
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {"uri": uri, "diagnostics": diagnostics}
                }),
            );
            return;
        }
        if self.mode != "normal" || self.diagnostics_sent {
            return;
        }
        self.diagnostics_sent = true;
        // Wave 1, then wave 2 after a pause: the bridge's settle
        // predicate must wait for the second generation.
        notify_diagnostics(writer, &uri, false);
        std::thread::sleep(std::time::Duration::from_millis(350));
        notify_diagnostics(writer, &uri, true);
    }
}

fn notify_diagnostics(writer: &mut impl Write, uri: &str, second_wave: bool) {
    let mut diagnostics = vec![serde_json::json!({
        "range": range(4, 2, 4, 9),
        "severity": 2,
        "source": "clippy",
        "code": "clippy::all",
        "message": "first wave warning"
    })];
    if second_wave {
        diagnostics.push(serde_json::json!({
            "range": range(1, 0, 1, 5),
            "severity": 1,
            "source": "clippy",
            "code": "dead_code",
            "message": "second wave error"
        }));
    }
    let _ = write_message(
        writer,
        "fake-lsp",
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": diagnostics}
        }),
    );
}

fn respond(writer: &mut impl Write, id: serde_json::Value, result: serde_json::Value) {
    let _ = write_message(
        writer,
        "fake-lsp",
        &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn range(start_line: u64, start_char: u64, end_line: u64, end_char: u64) -> serde_json::Value {
    serde_json::json!({
        "start": {"line": start_line, "character": start_char},
        "end": {"line": end_line, "character": end_char}
    })
}

fn log_state(line: &str) {
    if let Ok(path) = std::env::var("FAKE_LSP_STATE_FILE")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
