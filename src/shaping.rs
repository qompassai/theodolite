//! Result shaping: canonical ordering and byte budgets (SPEC §4.3,
//! §4.4, §4.5). Server return order never reaches the client, and an
//! oversized result is a typed `ResultTooLarge` error carrying the
//! true item count — never a silent truncation.

use std::path::{Path, PathBuf};

use crate::errors::{BridgeError, BridgeResult};

/// Default per-tool result budget in bytes (SPEC §4.4).
pub const MAX_RESULT_BYTES: usize = 65_536;

/// A position in a document, LSP convention (0-based line, UTF-16
/// code-unit character).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// 0-based line.
    pub line: u64,
    /// 0-based character in UTF-16 code units.
    pub character: u64,
}

/// A text range; ordering is by start, then end (SPEC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    /// Range start.
    pub start: Position,
    /// Range end.
    pub end: Position,
}

/// One source location, the unit of definition/references results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Workspace-relative `file://` URI, canonicalized when shaped.
    pub uri: String,
    /// The location's range.
    pub range: Range,
}

/// One diagnostic, normalized from either push or pull sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Workspace-relative `file://` URI.
    pub uri: String,
    /// Diagnostic range.
    pub range: Range,
    /// LSP severity (1 error .. 4 hint); 0 when the server omits it.
    pub severity: u64,
    /// Producing tool, verbatim (clippy for bacon-ls; parity gate).
    pub source: String,
    /// Diagnostic code, stringified ("" when absent).
    pub code: String,
    /// Human message, verbatim.
    pub message: String,
}

/// Parse an LSP range object; missing fields default to zero so a
/// partial server range still orders deterministically.
pub fn parse_range(value: &serde_json::Value) -> Range {
    let position = |key: &str| -> Position {
        let point = value.get(key);
        Position {
            line: point
                .and_then(|p| p.get("line"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            character: point
                .and_then(|p| p.get("character"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        }
    };
    Range {
        start: position("start"),
        end: position("end"),
    }
}

/// Render a range as canonical JSON.
pub fn range_json(range: Range) -> serde_json::Value {
    serde_json::json!({
        "start": {"line": range.start.line, "character": range.start.character},
        "end": {"line": range.end.line, "character": range.end.character},
    })
}

/// Render a location as canonical JSON.
pub fn location_json(location: &Location) -> serde_json::Value {
    serde_json::json!({"uri": location.uri, "range": range_json(location.range)})
}

/// Sort key for diagnostics (SPEC §4.3): uri, range, severity,
/// source, code, message.
pub fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        a.uri
            .cmp(&b.uri)
            .then(a.range.cmp(&b.range))
            .then(a.severity.cmp(&b.severity))
            .then(a.source.cmp(&b.source))
            .then(a.code.cmp(&b.code))
            .then(a.message.cmp(&b.message))
    });
}

/// Render one diagnostic as canonical JSON.
pub fn diagnostic_json(diagnostic: &Diagnostic) -> serde_json::Value {
    serde_json::json!({
        "uri": diagnostic.uri,
        "range": range_json(diagnostic.range),
        "severity": diagnostic.severity,
        "source": diagnostic.source,
        "code": diagnostic.code,
        "message": diagnostic.message,
    })
}

/// Sort locations by their SPEC §4.3 key and drop exact duplicates.
pub fn sort_locations(locations: &mut Vec<Location>) {
    locations.sort_by(|a, b| a.uri.cmp(&b.uri).then(a.range.cmp(&b.range)));
    locations.dedup();
}

/// Enforce a byte budget on a shaped result. `items` is the true
/// total item count, reported in the typed error (SPEC §4.4).
pub fn enforce_budget(
    rendered: &serde_json::Value,
    items: usize,
    budget_bytes: usize,
) -> BridgeResult<()> {
    let size = serde_json::to_vec(rendered)
        .map(|body| body.len())
        .unwrap_or(0);
    if size > budget_bytes {
        return Err(BridgeError::ResultTooLarge {
            items,
            budget_bytes,
        });
    }
    Ok(())
}

/// Build a `file://` URI for an absolute path (percent-encoding the
/// few characters that cannot appear raw in this bridge's URIs).
pub fn path_to_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut uri = String::from("file://");
    for ch in raw.chars() {
        match ch {
            ' ' => uri.push_str("%20"),
            '#' => uri.push_str("%23"),
            '?' => uri.push_str("%3F"),
            '%' => uri.push_str("%25"),
            _ => uri.push(ch),
        }
    }
    uri
}

/// Decode a `file://` URI back to a path. Non-file schemes, hosts,
/// and undecodable escapes are `WorkspaceEscape` errors: a hostile
/// server must not steer the bridge outside the workspace (§9.9).
pub fn uri_to_path(uri: &str) -> BridgeResult<PathBuf> {
    let Some(rest) = uri.strip_prefix("file://") else {
        return Err(BridgeError::WorkspaceEscape {
            detail: format!("URI {uri:?} is not a file:// URI"),
        });
    };
    let rest = match rest.strip_prefix("localhost") {
        Some(stripped) => stripped,
        None => rest,
    };
    if !rest.starts_with('/') {
        return Err(BridgeError::WorkspaceEscape {
            detail: format!("URI {uri:?} does not name an absolute path"),
        });
    }
    let bytes = rest.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex =
                bytes
                    .get(index + 1..index + 3)
                    .ok_or_else(|| BridgeError::WorkspaceEscape {
                        detail: format!("URI {uri:?} has a truncated percent escape"),
                    })?;
            let hex_text = std::str::from_utf8(hex).unwrap_or("");
            let value =
                u8::from_str_radix(hex_text, 16).map_err(|_| BridgeError::WorkspaceEscape {
                    detail: format!("URI {uri:?} has an invalid percent escape"),
                })?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(decoded).map_err(|_| BridgeError::WorkspaceEscape {
        detail: format!("URI {uri:?} does not decode as UTF-8"),
    })?;
    if text.contains('\0') {
        return Err(BridgeError::WorkspaceEscape {
            detail: format!("URI {uri:?} contains a NUL byte"),
        });
    }
    Ok(PathBuf::from(text))
}

/// Convert a server-returned absolute URI into the bridge's result
/// form: a workspace-relative URI anchored at the root, rendered as
/// `file:///<relative path>` (SPEC §6: "paths inside results are
/// workspace-relative URIs"). A URI outside the workspace is a
/// `WorkspaceEscape` — server output is untrusted input (§9.9).
pub fn relativize_uri(uri: &str, workspace_root: &Path) -> BridgeResult<String> {
    let path = uri_to_path(uri)?;
    let root_text = workspace_root.to_string_lossy();
    let path_text = path.to_string_lossy();
    let relative = path_text
        .strip_prefix(root_text.as_ref())
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(|| BridgeError::WorkspaceEscape {
            detail: format!("URI {uri:?} is outside the workspace"),
        })?;
    let anchored = PathBuf::from("/").join(relative);
    Ok(path_to_uri(&anchored))
}
