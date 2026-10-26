//! MCP stdio transport: hand-rolled NDJSON JSON-RPC following the
//! estate's existing server conventions (SPEC §2; pratique's MCP
//! layer is the local reference shape). Frames are capped before
//! allocation, results are canonical JSON text, and typed bridge
//! errors render as stable `{code, message}` objects.

use std::io::{BufRead, Write};
use std::sync::Arc;

use crate::errors::BridgeError;
use crate::session::SessionManager;
use crate::tools;

/// Maximum MCP frame size in bytes (one NDJSON line).
pub const MAX_MCP_FRAME_BYTES: usize = 2 * 1024 * 1024;
/// MCP protocol version this bridge speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve MCP over stdio until EOF. On EOF every session is shut
/// down under SPEC §3.6 discipline before returning.
pub fn serve_stdio(manager: Arc<SessionManager>) -> i32 {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let read_result = read_bounded_line(&mut reader, &mut line);
        match read_result {
            Ok(0) => break, // transport EOF: shut down sessions below
            Ok(_) => {}
            Err(error) => {
                let _ = writeln!(writer, "{}", error_frame(None, -32700, &error));
                let _ = writer.flush();
                continue;
            }
        }
        let Some(response) = handle_frame(&manager, &line) else {
            continue; // a notification: no response is owed
        };
        if writeln!(writer, "{response}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
    manager.shutdown_all();
    0
}

/// Read one NDJSON line with a hard cap. Reading past the cap is a
/// parse-class error, and the rest of the line is drained so the
/// next frame starts clean.
fn read_bounded_line(reader: &mut impl BufRead, out: &mut Vec<u8>) -> Result<usize, String> {
    let mut total = 0usize;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| format!("stdin read failed: {e}"))?;
        if available.is_empty() {
            return Ok(total);
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let take = newline.map_or(available.len(), |pos| pos + 1);
        total += take;
        if total > MAX_MCP_FRAME_BYTES {
            reader.consume(take);
            // Drain to the end of the oversized line.
            if newline.is_none() {
                let mut scratch: Vec<u8> = Vec::new();
                loop {
                    scratch.clear();
                    let drained = reader
                        .read_until(b'\n', &mut scratch)
                        .map_err(|e| format!("stdin drain failed: {e}"))?;
                    if drained == 0 || scratch.ends_with(b"\n") {
                        break;
                    }
                }
            }
            return Err(format!(
                "frame exceeds the {MAX_MCP_FRAME_BYTES}-byte MCP cap"
            ));
        }
        out.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(total);
        }
    }
}

/// Handle one frame; `None` means the frame was a notification.
fn handle_frame(manager: &SessionManager, frame: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(frame);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return Some(error_frame(None, -32700, &format!("parse error: {error}")));
        }
    };
    let id = value.get("id").cloned();
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    // A frame without an id is a notification: no response is owed
    // (this includes notifications/initialized).
    let id = id?;
    let response = match method {
        "initialize" => {
            // The handshake's clientInfo.name is the client identity
            // the mutation gate decides on (P5 item 2). Absent
            // clientInfo records NO identity — fail closed.
            manager.set_client_identity(
                value
                    .get("params")
                    .and_then(|p| p.get("clientInfo"))
                    .and_then(|info| info.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            );
            let client_version = value
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            serde_json::json!({
                "protocolVersion": client_version,
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "theodolite",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        "ping" => serde_json::json!({}),
        "tools/list" => serde_json::json!({
            "tools": tools::tool_definitions(
                manager.config(),
                manager.client_identity().as_deref(),
            )
        }),
        "tools/call" => return Some(handle_tools_call(manager, &id, &value)),
        other => {
            return Some(error_frame(
                Some(&id),
                -32601,
                &format!("method not supported by theodolite: {other}"),
            ));
        }
    };
    Some(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": response}).to_string())
}

fn handle_tools_call(
    manager: &SessionManager,
    id: &serde_json::Value,
    frame: &serde_json::Value,
) -> String {
    let params = frame
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let (text, is_error) = match tools::call_tool(manager, name, &arguments) {
        Ok(result) => (canonical_text(&result), false),
        Err(error) => (canonical_text(&error_payload(&error)), true),
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": is_error
        }
    })
    .to_string()
}

fn error_payload(error: &BridgeError) -> serde_json::Value {
    serde_json::json!({"error": error.to_json()})
}

/// Serialize a result canonically: object keys sorted recursively,
/// so identical results render byte-identical text (SPEC §4.5).
fn canonical_text(value: &serde_json::Value) -> String {
    sort_keys(value).to_string()
}

fn sort_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), sort_keys(&map[key]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_keys).collect())
        }
        other => other.clone(),
    }
}

fn error_frame(id: Option<&serde_json::Value>, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(serde_json::Value::Null),
        "error": {"code": code, "message": message}
    })
    .to_string()
}
