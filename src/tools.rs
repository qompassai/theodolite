//! MCP tool definitions and result shaping (SPEC §2 `bridge-tools`,
//! §6). The v1 surface is read-class only, per the Owner-approved
//! staged scope: `session_status`, `diagnostics`, `hover`,
//! `definition`, `references`, `document_symbols`, and the session
//! class's `session_restart`. The mutation class is not registered
//! at all (SPEC §6 gate posture) and there is no generic
//! any-LSP-method tool (SPEC §8).

use std::sync::Arc;

use crate::config::Config;
use crate::errors::{BridgeError, BridgeResult};
use crate::session::{Session, SessionManager, resolve_within, validate_workspace};
use crate::shaping::{
    Diagnostic, Location, MAX_RESULT_BYTES, diagnostic_json, enforce_budget, location_json,
    merge_diagnostics, parse_range, path_to_uri, range_json, relativize_uri, sort_diagnostics,
    sort_locations, uri_to_path,
};

/// Tool names registered in v1, in registration order.
pub const TOOL_NAMES: [&str; 7] = [
    "session_status",
    "diagnostics",
    "hover",
    "definition",
    "references",
    "document_symbols",
    "session_restart",
];

/// JSON schemas for `tools/list`. Every tool names its workspace
/// explicitly and (except `session_status` on unstarted sessions)
/// syncs the file it names before asking the server anything.
pub fn tool_definitions() -> serde_json::Value {
    let workspace = serde_json::json!({
        "type": "string", "description": "Absolute path of the workspace root."
    });
    let server = serde_json::json!({
        "type": "string", "description": "Configured server id (config routing key)."
    });
    let path = serde_json::json!({
        "type": "string",
        "description": "File path, absolute or workspace-relative; must stay inside the workspace."
    });
    let position_props = |with_include: bool| {
        let mut props = serde_json::json!({
            "workspace": workspace, "server": server, "path": path,
            "line": {"type": "integer", "minimum": 0},
            "character": {"type": "integer", "minimum": 0}
        });
        if with_include {
            props["include_declaration"] = serde_json::json!({
                "type": "boolean", "default": true,
                "description": "Include the declaration in reference results."
            });
        }
        props
    };
    let tool = |name: &str, description: &str, properties: serde_json::Value, required: &[&str]| {
        serde_json::json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            }
        })
    };
    serde_json::json!([
        tool(
            "session_status",
            "Session transcript: pinned binary, actual version, workspace, readiness, open documents.",
            serde_json::json!({"workspace": workspace, "server": server}),
            &["workspace", "server"],
        ),
        tool(
            "diagnostics",
            "Push diagnostics for the workspace, or one file when `path` is given. Sorted, byte-bounded.",
            serde_json::json!({
                "workspace": workspace, "server": server,
                "path": {"type": "string", "description": "Optional single file to scope to."}
            }),
            &["workspace", "server"],
        ),
        tool(
            "hover",
            "Hover contents at a position (0-based line, UTF-16 character).",
            position_props(false),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "definition",
            "Definitions of the symbol at a position, as a sorted location list.",
            position_props(false),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "references",
            "References to the symbol at a position, as a sorted location list.",
            position_props(true),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "document_symbols",
            "Flattened document outline: name, kind, container, ranges, sorted by range.",
            serde_json::json!({"workspace": workspace, "server": server, "path": path}),
            &["workspace", "server", "path"],
        ),
        tool(
            "session_restart",
            "The only respawn path: tears down the (workspace, server) session and starts a fresh one.",
            serde_json::json!({"workspace": workspace, "server": server}),
            &["workspace", "server"],
        ),
    ])
}

/// Dispatch one `tools/call`. The return value is the canonical
/// result JSON; all failures are typed `BridgeError`s.
pub fn call_tool(
    manager: &SessionManager,
    name: &str,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    match name {
        "session_status" => tool_session_status(manager, arguments),
        "diagnostics" => tool_diagnostics(manager, arguments),
        "hover" => tool_hover(manager, arguments),
        "definition" => tool_definition(manager, arguments),
        "references" => tool_references(manager, arguments),
        "document_symbols" => tool_document_symbols(manager, arguments),
        "session_restart" => tool_session_restart(manager, arguments),
        other => Err(BridgeError::UnsupportedMethod {
            detail: format!("no tool named {other:?} is registered in v1"),
        }),
    }
}

fn require_string(arguments: &serde_json::Value, field: &str) -> BridgeResult<String> {
    arguments
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| BridgeError::InvalidArgument {
            detail: format!("argument {field:?} must be a string"),
        })
}

fn require_u64(arguments: &serde_json::Value, field: &str) -> BridgeResult<u64> {
    arguments
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| BridgeError::InvalidArgument {
            detail: format!("argument {field:?} must be a non-negative integer"),
        })
}

fn session_for(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<(Arc<Session>, String)> {
    let workspace_raw = require_string(arguments, "workspace")?;
    let server_id = require_string(arguments, "server")?;
    let workspace = validate_workspace(&workspace_raw)?;
    let session = manager.get_or_start(&workspace, &server_id)?;
    Ok((session, server_id))
}

/// Resolve the position-bearing arguments shared by hover /
/// definition / references: validate, sync the file, and return the
/// session plus the LSP position params.
fn position_params(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<(Arc<Session>, serde_json::Value)> {
    let (session, _) = session_for(manager, arguments)?;
    let raw_path = require_string(arguments, "path")?;
    let absolute = resolve_within(&session.workspace_root, &raw_path)?;
    session.ensure_ready(manager.config())?;
    let (uri, _version) = session.sync_file(&absolute)?;
    let line = require_u64(arguments, "line")?;
    let character = require_u64(arguments, "character")?;
    Ok((
        session,
        serde_json::json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
    ))
}

fn tool_session_status(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let workspace_raw = require_string(arguments, "workspace")?;
    let server_id = require_string(arguments, "server")?;
    let workspace = validate_workspace(&workspace_raw)?;
    if manager.config().server_by_id(&server_id).is_none() {
        return Err(BridgeError::InvalidArgument {
            detail: format!("no configured server with id {server_id:?}"),
        });
    }
    match manager.peek(&workspace, &server_id) {
        Some(session) => {
            let (pid, alive) = session.pid_and_liveness();
            let mut transcript = session.transcript(manager.config());
            transcript["pid"] = serde_json::Value::Number(pid.into());
            transcript["alive"] = serde_json::Value::Bool(alive);
            Ok(transcript)
        }
        None => Ok(serde_json::json!({
            "server": server_id,
            "workspace": workspace.to_string_lossy(),
            "phase": "not_started",
            "bridge_version": env!("CARGO_PKG_VERSION"),
            "config_hash": manager.config().config_hash,
        })),
    }
}

fn tool_diagnostics(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, server_id) = session_for(manager, arguments)?;
    session.ensure_ready(manager.config())?;
    let only_uri = match arguments.get("path") {
        Some(path_value) => {
            let raw = path_value
                .as_str()
                .ok_or_else(|| BridgeError::InvalidArgument {
                    detail: "argument \"path\" must be a string".to_string(),
                })?;
            let absolute = resolve_within(&session.workspace_root, raw)?;
            let (uri, _) = session.sync_file(&absolute)?;
            Some(uri)
        }
        None => None,
    };
    let mut diagnostics = session.diagnostics(manager.config(), only_uri.as_deref())?;
    // P3 item 1: when the server advertises pull diagnostics, merge
    // the pull set with the accumulated push set. The merge is
    // protocol-shaped (advertisement-driven), never server-shaped:
    // servers without `diagnosticProvider` behave exactly as v1.
    // The pull scope is the `path`-scoped document, or — unscoped —
    // every document the push set names plus every document the
    // bridge has open.
    let mut channels: Vec<&str> = vec!["push"];
    if session.supports_pull_diagnostics() {
        let scope_uris: Vec<String> = match &only_uri {
            Some(uri) => vec![uri.clone()],
            None => {
                let mut uris: Vec<String> = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.uri.clone())
                    .chain(session.open_document_uris())
                    .collect();
                uris.sort();
                uris.dedup();
                uris
            }
        };
        let mut pulled: Vec<Diagnostic> = Vec::new();
        for uri in &scope_uris {
            // A pull needs the document open. Sync it in when it is a
            // real file inside this workspace; a push URI that names
            // no syncable file keeps its push entries and is simply
            // not pulled (its push set above is unaffected).
            let synced = match uri_to_path(uri) {
                Ok(path) if path.starts_with(&session.workspace_root) => {
                    session.sync_file(&path).is_ok()
                }
                _ => false,
            };
            if !synced {
                continue;
            }
            pulled.extend(session.pull_diagnostics(manager.config(), uri)?);
        }
        diagnostics = merge_diagnostics(diagnostics, pulled);
        channels.push("pull");
        channels.sort_unstable();
    }
    // Relativize URIs into the workspace-relative result form; a URI
    // outside the workspace in a result is a WorkspaceEscape (§9.9).
    let mut shaped: Vec<Diagnostic> = Vec::with_capacity(diagnostics.len());
    for mut diagnostic in diagnostics.drain(..) {
        diagnostic.uri = relativize_uri(&diagnostic.uri, &session.workspace_root)?;
        shaped.push(diagnostic);
    }
    sort_diagnostics(&mut shaped);
    let items: Vec<serde_json::Value> = shaped.iter().map(diagnostic_json).collect();
    // Per SPEC §7.2: when the asked server is not the workspace's
    // diagnostics owner and an owner is configured, say so in
    // metadata instead of silently substituting the owner's set.
    let mut result = serde_json::json!({
        "server": server_id,
        "diagnostics_source": server_id,
        "channels": channels,
        "count": items.len(),
        "diagnostics": items,
    });
    if !session.entry.diagnostics_owner
        && let Some(owner) = manager
            .config()
            .servers
            .iter()
            .find(|entry| entry.diagnostics_owner && entry.id != server_id)
    {
        result["diagnostics_owner"] = serde_json::Value::String(owner.id.clone());
        result["note"] = serde_json::Value::String(format!(
            "diagnostics shown are {}'s own push set; {} owns gate diagnostics for this estate",
            server_id, owner.id
        ));
    }
    enforce_budget(&result, shaped.len(), MAX_RESULT_BYTES)?;
    Ok(result)
}

fn tool_hover(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, params) = position_params(manager, arguments)?;
    let config: &Config = manager.config();
    let raw = session.request("textDocument/hover", params, config.request_timeout_ms)?;
    if raw.is_null() {
        return Ok(serde_json::json!({"contents": null, "range": null}));
    }
    // Hover markup is preserved verbatim (SPEC §6); only the range
    // is normalized into the canonical range shape.
    let range = raw.get("range").map(|r| range_json(parse_range(r)));
    Ok(serde_json::json!({
        "contents": raw.get("contents").cloned().unwrap_or(serde_json::Value::Null),
        "range": range.unwrap_or(serde_json::Value::Null),
    }))
}

/// Normalize an LSP location-ish result (`Location | Location[] |
/// LocationLink[] | null`) into sorted bridge locations.
fn shape_locations(session: &Session, raw: &serde_json::Value) -> BridgeResult<Vec<Location>> {
    let mut entries: Vec<&serde_json::Value> = Vec::new();
    match raw {
        serde_json::Value::Null => {}
        serde_json::Value::Array(items) => entries.extend(items.iter()),
        single => entries.push(single),
    }
    let mut locations: Vec<Location> = Vec::with_capacity(entries.len());
    for entry in entries {
        // LocationLink carries targetUri/targetSelectionRange.
        let (uri_raw, range_value) = if let Some(target) = entry.get("targetUri") {
            (
                target.as_str().unwrap_or_default(),
                entry
                    .get("targetSelectionRange")
                    .or_else(|| entry.get("targetRange"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
        } else {
            (
                entry
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                entry
                    .get("range")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
        };
        if uri_raw.is_empty() {
            continue;
        }
        let uri = relativize_uri(uri_raw, &session.workspace_root)?;
        locations.push(Location {
            uri,
            range: parse_range(&range_value),
        });
    }
    sort_locations(&mut locations);
    Ok(locations)
}

fn shape_location_result(
    session: &Session,
    raw: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let locations = shape_locations(session, raw)?;
    let items: Vec<serde_json::Value> = locations.iter().map(location_json).collect();
    let result = serde_json::json!({"count": items.len(), "locations": items});
    enforce_budget(&result, locations.len(), MAX_RESULT_BYTES)?;
    Ok(result)
}

fn tool_definition(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, params) = position_params(manager, arguments)?;
    let raw = session.request(
        "textDocument/definition",
        params,
        manager.config().request_timeout_ms,
    )?;
    shape_location_result(&session, &raw)
}

fn tool_references(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, mut params) = position_params(manager, arguments)?;
    let include_declaration = arguments
        .get("include_declaration")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    params["context"] = serde_json::json!({"includeDeclaration": include_declaration});
    let raw = session.request(
        "textDocument/references",
        params,
        manager.config().request_timeout_ms,
    )?;
    shape_location_result(&session, &raw)
}

/// One flattened document symbol (SPEC §6: name, kind, container,
/// range, selection range).
#[derive(Debug)]
struct FlatSymbol {
    name: String,
    kind: u64,
    container: String,
    range: crate::shaping::Range,
    selection_range: crate::shaping::Range,
}

fn flatten_symbols(value: &serde_json::Value, container: &str, out: &mut Vec<FlatSymbol>) {
    let Some(items) = value.as_array() else {
        return;
    };
    for item in items {
        // Hierarchical DocumentSymbol shape.
        if item.get("name").is_some() && item.get("range").is_some() {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.push(FlatSymbol {
                name: name.clone(),
                kind: item
                    .get("kind")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                container: container.to_string(),
                range: parse_range(item.get("range").unwrap_or(&serde_json::Value::Null)),
                selection_range: parse_range(
                    item.get("selectionRange")
                        .unwrap_or(&serde_json::Value::Null),
                ),
            });
            if let Some(children) = item.get("children") {
                flatten_symbols(children, &name, out);
            }
            continue;
        }
        // Flat SymbolInformation shape.
        if let Some(location) = item.get("location") {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let range = parse_range(location.get("range").unwrap_or(&serde_json::Value::Null));
            out.push(FlatSymbol {
                name,
                kind: item
                    .get("kind")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                container: item
                    .get("containerName")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                range,
                selection_range: range,
            });
        }
    }
}

fn tool_document_symbols(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, _) = session_for(manager, arguments)?;
    let raw_path = require_string(arguments, "path")?;
    let absolute = resolve_within(&session.workspace_root, &raw_path)?;
    session.ensure_ready(manager.config())?;
    let (uri, _) = session.sync_file(&absolute)?;
    let raw = session.request(
        "textDocument/documentSymbol",
        serde_json::json!({"textDocument": {"uri": uri}}),
        manager.config().request_timeout_ms,
    )?;
    let mut flat: Vec<FlatSymbol> = Vec::new();
    flatten_symbols(&raw, "", &mut flat);
    flat.sort_by(|a, b| {
        a.range
            .cmp(&b.range)
            .then(a.name.cmp(&b.name))
            .then(a.kind.cmp(&b.kind))
    });
    let items: Vec<serde_json::Value> = flat
        .iter()
        .map(|symbol| {
            serde_json::json!({
                "name": symbol.name,
                "kind": symbol.kind,
                "container": symbol.container,
                "range": range_json(symbol.range),
                "selection_range": range_json(symbol.selection_range),
            })
        })
        .collect();
    let relative_uri = relativize_uri(&path_to_uri(&absolute), &session.workspace_root)?;
    let result = serde_json::json!({
        "uri": relative_uri,
        "count": items.len(),
        "symbols": items,
    });
    enforce_budget(&result, flat.len(), MAX_RESULT_BYTES)?;
    Ok(result)
}

fn tool_session_restart(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let workspace_raw = require_string(arguments, "workspace")?;
    let server_id = require_string(arguments, "server")?;
    let workspace = validate_workspace(&workspace_raw)?;
    let session = manager.restart(&workspace, &server_id)?;
    let (pid, alive) = session.pid_and_liveness();
    let mut transcript = session.transcript(manager.config());
    transcript["pid"] = serde_json::Value::Number(pid.into());
    transcript["alive"] = serde_json::Value::Bool(alive);
    transcript["restarted"] = serde_json::Value::Bool(true);
    Ok(transcript)
}
