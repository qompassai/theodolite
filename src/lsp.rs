//! Bounded LSP JSON-RPC framing (SPEC §2 `bridge-protocol`, §3.7).
//!
//! The anti-patterns named in the STUDY are designed out here: the
//! `Content-Length` header is validated *before* any allocation, a
//! hard frame cap applies in both directions, truncated frames are
//! typed errors, and there is no code path that waits without a
//! deadline (deadlines live in the session layer, which owns the
//! channels this layer feeds).

use std::io::{Read, Write};

use crate::errors::{BridgeError, BridgeResult};

/// Maximum LSP frame body in bytes, either direction (SPEC §3.7).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum size of the header block preceding a frame body.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

/// One decoded JSON-RPC message from a language server.
#[derive(Debug, Clone, PartialEq)]
pub enum LspMessage {
    /// A response to a request the bridge sent (`id` is always one
    /// the bridge minted; unknown ids are rejected by the reader
    /// loop's caller contract — see `Session`).
    Response {
        id: i64,
        result: Result<serde_json::Value, (i64, String)>,
    },
    /// A notification (no id).
    Notification {
        method: String,
        params: serde_json::Value,
    },
    /// A request from the server to the client (has an id).
    Request {
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
}

/// Read exactly one framed message from `reader`.
///
/// Returns `Ok(None)` on a clean EOF at a frame boundary. Every
/// malformed condition (oversized or negative length, truncated
/// body, garbage JSON, missing fields) is a typed
/// `MalformedServerTraffic` error carrying the server id.
pub fn read_message<R: Read>(reader: &mut R, server: &str) -> BridgeResult<Option<LspMessage>> {
    let mut header_bytes: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                if header_bytes.is_empty() {
                    return Ok(None);
                }
                return Err(malformed(server, "EOF inside frame header"));
            }
            Ok(_) => {
                header_bytes.push(byte[0]);
                if header_bytes.len() > MAX_HEADER_BYTES {
                    return Err(malformed(server, "frame header exceeds the header cap"));
                }
                if header_bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) => return Err(crate::errors::io_error("reading LSP header", e)),
        }
    }
    let header_text = String::from_utf8_lossy(&header_bytes);
    let mut content_length: Option<i64> = None;
    for line in header_text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<i64>().ok();
            if content_length.is_none() {
                return Err(malformed(server, "Content-Length is not an integer"));
            }
        }
    }
    let Some(length) = content_length else {
        return Err(malformed(server, "frame has no Content-Length header"));
    };
    if length < 0 {
        return Err(malformed(server, "Content-Length is negative"));
    }
    let length_usize = usize::try_from(length)
        .map_err(|_| malformed(server, "Content-Length does not fit usize"))?;
    if length_usize > MAX_FRAME_BYTES {
        // Rejected before allocation: the STUDY §1.5 unbounded
        // `make([]byte, contentLength)` is the named anti-pattern.
        return Err(malformed(server, "Content-Length exceeds MAX_FRAME_BYTES"));
    }
    if length_usize == 0 {
        return Err(malformed(server, "Content-Length is zero"));
    }
    let mut body = vec![0u8; length_usize];
    reader
        .read_exact(&mut body)
        .map_err(|e| malformed(server, &format!("truncated frame body: {e}")))?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| malformed(server, &format!("frame body is not valid JSON: {e}")))?;
    classify(value, server).map(Some)
}

fn classify(value: serde_json::Value, server: &str) -> BridgeResult<LspMessage> {
    let object = value
        .as_object()
        .ok_or_else(|| malformed(server, "frame body is not a JSON object"))?;
    let has_method = object.contains_key("method");
    let id_value = object.get("id").cloned();
    match (has_method, id_value) {
        (true, None) => Ok(LspMessage::Notification {
            method: object
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            params: object
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        (true, Some(id)) => Ok(LspMessage::Request {
            id,
            method: object
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            params: object
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        (false, Some(id)) => {
            let id = id
                .as_i64()
                .ok_or_else(|| malformed(server, "response id is not an integer"))?;
            if let Some(error) = object.get("error") {
                let rpc_code = error
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(LspMessage::Response {
                    id,
                    result: Err((rpc_code, message)),
                })
            } else {
                let result = object
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok(LspMessage::Response {
                    id,
                    result: Ok(result),
                })
            }
        }
        (false, None) => Err(malformed(server, "frame has neither method nor id")),
    }
}

/// Write one framed JSON-RPC message. Bodies above the frame cap are
/// refused before anything is written.
pub fn write_message<W: Write>(
    writer: &mut W,
    server: &str,
    value: &serde_json::Value,
) -> BridgeResult<()> {
    let body = serde_json::to_vec(value).map_err(|e| BridgeError::Io {
        detail: format!("serializing LSP message: {e}"),
    })?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(malformed(server, "outgoing frame exceeds MAX_FRAME_BYTES"));
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .and_then(|()| writer.write_all(&body))
        .and_then(|()| writer.flush())
        .map_err(|e| crate::errors::io_error("writing LSP frame", e))
}

fn malformed(server: &str, detail: &str) -> BridgeError {
    BridgeError::MalformedServerTraffic {
        server: server.to_string(),
        detail: detail.to_string(),
    }
}
