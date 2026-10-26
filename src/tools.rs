//! MCP tool definitions and result shaping (SPEC §2 `bridge-tools`,
//! §6). The surface is the v1 set (`session_status`, `diagnostics`,
//! `hover`, `definition`, `references`, `document_symbols`,
//! `session_restart`) plus the P3 read-class completion (pull
//! diagnostics merged into `diagnostics`; the definition family
//! `declaration` / `type_definition` / `implementation`;
//! `workspace_symbols`; `code_actions` as a listing only) plus the
//! P5 staged mutation class:
//!
//! - `rename_preview` — preview-class: it computes a rename's full
//!   edit set and renders it as data, and applies NOTHING. SPEC §6
//!   classifies the preview as the read-side rename tool ("this is
//!   the only rename tool in v1" meant the *unapplied* one), so it
//!   is registered by default and the mutation gate below does NOT
//!   govern it. The distinction is enforced by construction: the
//!   preview path holds no applier reference.
//! - `code_action_apply` / `rename_apply` — the mutation class
//!   proper: registered only under the decide()-class gate
//!   (`crate::gate`), absent from `tools/list` otherwise.
//!
//! There is no generic any-LSP-method tool (SPEC §8) and no
//! `edit_file`, ever.

use std::sync::Arc;

use crate::config::Config;
use crate::edits::{FileEdits, render_file_list, shape_workspace_edit, total_edits};
use crate::errors::{BridgeError, BridgeResult};
use crate::session::{Session, SessionManager, resolve_within, validate_workspace};
use crate::shaping::{
    Diagnostic, Location, MAX_RESULT_BYTES, Position, Range, diagnostic_json, diagnostic_to_lsp,
    enforce_budget, location_json, merge_diagnostics, parse_range, path_to_uri, range_json,
    relativize_uri, sort_diagnostics, sort_locations, uri_to_path,
};

/// Tool names registered unconditionally (read class plus the
/// preview-class `rename_preview`), in registration order. The
/// mutation-class names are separate: they join this list only
/// when the gate registers them for the connected client (P5
/// item 2, `registered_tool_names`).
pub const TOOL_NAMES: [&str; 13] = [
    "session_status",
    "diagnostics",
    "hover",
    "definition",
    "declaration",
    "type_definition",
    "implementation",
    "references",
    "document_symbols",
    "workspace_symbols",
    "code_actions",
    "rename_preview",
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
            "declaration",
            "Declaration of the symbol at a position, as a sorted location list. \
             Typed UnsupportedMethod when the server does not advertise declaration support.",
            position_props(false),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "type_definition",
            "Type definition of the symbol at a position, as a sorted location list. \
             Typed UnsupportedMethod when the server does not advertise type-definition support.",
            position_props(false),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "implementation",
            "Implementations of the symbol at a position, as a sorted location list. \
             Typed UnsupportedMethod when the server does not advertise implementation support.",
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
            "workspace_symbols",
            "Workspace-wide symbol search. Results are the server's own (no client-side \
             name filtering), sorted per the canonical symbol key, byte-bounded: overflow \
             is a typed ResultTooLarge carrying the true count. Typed UnsupportedMethod \
             when the server does not advertise workspace-symbol support.",
            serde_json::json!({
                "workspace": workspace, "server": server,
                "query": {"type": "string", "description": "Query string passed to the server verbatim; may be empty."}
            }),
            &["workspace", "server", "query"],
        ),
        tool(
            "code_actions",
            "LIST the code actions a server offers for a range: title, kind, and whether \
             each carries an edit or a command. The edit/command payloads are never \
             returned and never executed — this listing is the read half the gated \
             mutation class will one day consume. Typed UnsupportedMethod when the \
             server does not advertise code-action support; an empty list is a valid, \
             distinct answer.",
            serde_json::json!({
                "workspace": workspace, "server": server, "path": path,
                "line": {"type": "integer", "minimum": 0},
                "character": {"type": "integer", "minimum": 0},
                "end_line": {"type": "integer", "minimum": 0,
                    "description": "Range end line; defaults to `line` (a point range)."},
                "end_character": {"type": "integer", "minimum": 0,
                    "description": "Range end character; defaults to `character`."}
            }),
            &["workspace", "server", "path", "line", "character"],
        ),
        tool(
            "rename_preview",
            "Compute a rename's full edit set and render it as data: per-file sorted edit \
             lists with old text, and totals. Applies NOTHING (applied is always false); \
             this preview class is registered by default and is not governed by the \
             mutation gate. When the server advertises prepareRename support its answer \
             is honored: a position it reports as not renameable returns renameable=false \
             instead of an edit set. Typed UnsupportedMethod when the server does not \
             advertise rename support.",
            serde_json::json!({
                "workspace": workspace, "server": server, "path": path,
                "line": {"type": "integer", "minimum": 0},
                "character": {"type": "integer", "minimum": 0},
                "new_name": {"type": "string", "description": "The symbol's new name."}
            }),
            &[
                "workspace",
                "server",
                "path",
                "line",
                "character",
                "new_name"
            ],
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
        "declaration" => definition_family_tool(manager, arguments, FamilyMember::Declaration),
        "type_definition" => {
            definition_family_tool(manager, arguments, FamilyMember::TypeDefinition)
        }
        "implementation" => {
            definition_family_tool(manager, arguments, FamilyMember::Implementation)
        }
        "references" => tool_references(manager, arguments),
        "document_symbols" => tool_document_symbols(manager, arguments),
        "workspace_symbols" => tool_workspace_symbols(manager, arguments),
        "code_actions" => tool_code_actions(manager, arguments),
        "rename_preview" => tool_rename_preview(manager, arguments),
        "session_restart" => tool_session_restart(manager, arguments),
        other => Err(BridgeError::UnsupportedMethod {
            detail: format!("no tool named {other:?} is registered"),
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

/// The definition family beyond `definition` itself (SPEC §6:
/// separate tools, not merged). Each member shares `definition`'s
/// shaping exactly and is gated on the server's advertised provider:
/// an unadvertised member is a typed `UnsupportedMethod`, never a
/// silent empty list (an empty list from an advertising server is a
/// real answer and stays one).
#[derive(Debug, Clone, Copy)]
enum FamilyMember {
    Declaration,
    TypeDefinition,
    Implementation,
}

impl FamilyMember {
    fn method(&self) -> &'static str {
        match self {
            FamilyMember::Declaration => "textDocument/declaration",
            FamilyMember::TypeDefinition => "textDocument/typeDefinition",
            FamilyMember::Implementation => "textDocument/implementation",
        }
    }

    fn capability(&self) -> &'static str {
        match self {
            FamilyMember::Declaration => "declarationProvider",
            FamilyMember::TypeDefinition => "typeDefinitionProvider",
            FamilyMember::Implementation => "implementationProvider",
        }
    }
}

fn definition_family_tool(
    manager: &SessionManager,
    arguments: &serde_json::Value,
    member: FamilyMember,
) -> BridgeResult<serde_json::Value> {
    let (session, params) = position_params(manager, arguments)?;
    if !session.capability_advertised(member.capability()) {
        return Err(BridgeError::UnsupportedMethod {
            detail: format!(
                "server {} does not advertise {}; {} is unavailable, not empty",
                session.entry.id,
                member.capability(),
                member.method()
            ),
        });
    }
    let raw = session.request(member.method(), params, manager.config().request_timeout_ms)?;
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

/// One workspace symbol, flattened from either the
/// `SymbolInformation` or the `WorkspaceSymbol` wire shape.
#[derive(Debug)]
struct WorkspaceSymbol {
    name: String,
    kind: u64,
    container: String,
    uri: String,
    range: crate::shaping::Range,
}

fn tool_workspace_symbols(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, _) = session_for(manager, arguments)?;
    let query = require_string(arguments, "query")?;
    session.ensure_ready(manager.config())?;
    if !session.capability_advertised("workspaceSymbolProvider") {
        return Err(BridgeError::UnsupportedMethod {
            detail: format!(
                "server {} does not advertise workspaceSymbolProvider; workspace/symbol is \
                 unavailable, not empty",
                session.entry.id
            ),
        });
    }
    let raw = session.request(
        "workspace/symbol",
        serde_json::json!({"query": query}),
        manager.config().request_timeout_ms,
    )?;
    let mut symbols: Vec<WorkspaceSymbol> = Vec::new();
    if let Some(items) = raw.as_array() {
        for item in items {
            let Some(location) = item.get("location") else {
                continue;
            };
            let uri_raw = location
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if uri_raw.is_empty() {
                continue;
            }
            symbols.push(WorkspaceSymbol {
                name: item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                kind: item
                    .get("kind")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                container: item
                    .get("containerName")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                uri: relativize_uri(uri_raw, &session.workspace_root)?,
                range: parse_range(location.get("range").unwrap_or(&serde_json::Value::Null)),
            });
        }
    }
    // SPEC §4.3 symbol key: (uri, range, name, kind).
    symbols.sort_by(|a, b| {
        a.uri
            .cmp(&b.uri)
            .then(a.range.cmp(&b.range))
            .then(a.name.cmp(&b.name))
            .then(a.kind.cmp(&b.kind))
            .then(a.container.cmp(&b.container))
    });
    symbols.dedup_by(|next, kept| {
        next.uri == kept.uri
            && next.range == kept.range
            && next.name == kept.name
            && next.kind == kept.kind
            && next.container == kept.container
    });
    let items: Vec<serde_json::Value> = symbols
        .iter()
        .map(|symbol| {
            serde_json::json!({
                "name": symbol.name,
                "kind": symbol.kind,
                "container": symbol.container,
                "uri": symbol.uri,
                "range": range_json(symbol.range),
            })
        })
        .collect();
    let result = serde_json::json!({"count": items.len(), "symbols": items});
    enforce_budget(&result, symbols.len(), MAX_RESULT_BYTES)?;
    Ok(result)
}

/// One listed code action (P3 item 4): the offer's identity and
/// payload *presence* — never the payload. Presence is what P5's
/// gated apply will need to mint handles against; content stays in
/// the server until then.
#[derive(Debug, PartialEq, Eq)]
struct ActionSummary {
    title: String,
    kind: String,
    has_edit: bool,
    has_command: bool,
}

fn tool_code_actions(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let (session, _) = session_for(manager, arguments)?;
    let raw_path = require_string(arguments, "path")?;
    let absolute = resolve_within(&session.workspace_root, &raw_path)?;
    session.ensure_ready(manager.config())?;
    if !session.capability_advertised("codeActionProvider") {
        return Err(BridgeError::UnsupportedMethod {
            detail: format!(
                "server {} does not advertise codeActionProvider; code actions are \
                 unavailable, not empty",
                session.entry.id
            ),
        });
    }
    let (uri, _) = session.sync_file(&absolute)?;
    let line = require_u64(arguments, "line")?;
    let character = require_u64(arguments, "character")?;
    let end_line = arguments
        .get("end_line")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(line);
    let end_character = arguments
        .get("end_character")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(character);
    let range = Range {
        start: Position { line, character },
        end: Position {
            line: end_line,
            character: end_character,
        },
    };
    if range.end < range.start {
        return Err(BridgeError::InvalidArgument {
            detail: "range end precedes range start".to_string(),
        });
    }
    // The protocol's context: the file's diagnostics overlapping the
    // range, in wire shape. Servers key quickfix offers off these
    // (a type error's fix exists only relative to its diagnostic),
    // so they are part of the request, not an embellishment. This is
    // the bounded-drain context read, not the settling read (P4
    // item 2): the server already holds its own newest diagnostics
    // state, and parking this request behind the strict evidence
    // predicate would stall — and at the deadline, fail — code
    // actions on any server whose evidence arrives late or never.
    let context_diagnostics: Vec<serde_json::Value> = session
        .diagnostics_context(Some(&uri))
        .iter()
        .filter(|diagnostic| ranges_overlap(diagnostic.range, range))
        .map(diagnostic_to_lsp)
        .collect();
    let raw = session.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": {"uri": uri},
            "range": range_json(range),
            "context": {"diagnostics": context_diagnostics}
        }),
        manager.config().request_timeout_ms,
    )?;
    let mut actions: Vec<ActionSummary> = Vec::new();
    if let Some(entries) = raw.as_array() {
        for entry in entries {
            actions.push(summarize_action(entry));
        }
    }
    actions.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then(a.kind.cmp(&b.kind))
            .then(a.has_edit.cmp(&b.has_edit))
            .then(a.has_command.cmp(&b.has_command))
    });
    actions.dedup();
    let items: Vec<serde_json::Value> = actions
        .iter()
        .map(|action| {
            serde_json::json!({
                "title": action.title,
                "kind": action.kind,
                "has_edit": action.has_edit,
                "has_command": action.has_command,
            })
        })
        .collect();
    let result = serde_json::json!({"count": items.len(), "actions": items});
    enforce_budget(&result, actions.len(), MAX_RESULT_BYTES)?;
    Ok(result)
}

/// Whether two ranges overlap; a point range overlaps a diagnostic
/// that touches the point (inclusive on both ends, since a caret
/// sits *between* characters and servers expect the diagnostics at
/// the caret to be in context).
fn ranges_overlap(a: Range, b: Range) -> bool {
    if a.start == a.end || b.start == b.end {
        let (point, span) = if a.start == a.end { (a, b) } else { (b, a) };
        return span.start <= point.start && point.start <= span.end;
    }
    a.start < b.end && b.start < a.end
}

/// Reduce one wire code-action entry (a `CodeAction` or a bare
/// `Command`) to its listing summary. The edit and command values
/// are inspected for presence only and dropped here — this is the
/// single place payloads could leak, and they do not leave it.
fn summarize_action(entry: &serde_json::Value) -> ActionSummary {
    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    // A bare Command carries `command` as a string and has no kind
    // or edit of its own.
    if entry
        .get("command")
        .is_some_and(serde_json::Value::is_string)
    {
        return ActionSummary {
            title,
            kind: String::new(),
            has_edit: false,
            has_command: true,
        };
    }
    ActionSummary {
        title,
        kind: entry
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        has_edit: entry.get("edit").is_some_and(|edit| !edit.is_null()),
        has_command: entry.get("command").is_some_and(|cmd| !cmd.is_null()),
    }
}

/// The outcome of computing a rename against a live session:
/// either the server's prepare step refused the position (a real
/// answer, carried as data), or the shaped edit set.
enum RenameOutcome {
    NotRenameable,
    Edits(Vec<FileEdits>),
}

/// One computed rename, shared verbatim by `rename_preview`
/// (item 1) and `rename_apply` (item 4) so the applied set is
/// always the set the preview path would have rendered.
struct RenameComputation {
    session: Arc<Session>,
    new_name: String,
    outcome: RenameOutcome,
}

/// Compute a rename: sync the document, honor `prepareRename`
/// when the server advertises prepare support, then shape the
/// `textDocument/rename` WorkspaceEdit. Support is read from the
/// captured initialize capabilities (P3 discipline), never probed
/// by trial request. This function performs no writes.
fn compute_rename(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<RenameComputation> {
    let (session, _) = session_for(manager, arguments)?;
    let raw_path = require_string(arguments, "path")?;
    let absolute = resolve_within(&session.workspace_root, &raw_path)?;
    let new_name = require_string(arguments, "new_name")?;
    if new_name.is_empty() {
        return Err(BridgeError::InvalidArgument {
            detail: "argument \"new_name\" must not be empty".to_string(),
        });
    }
    session.ensure_ready(manager.config())?;
    if !session.capability_advertised("renameProvider") {
        return Err(BridgeError::UnsupportedMethod {
            detail: format!(
                "server {} does not advertise renameProvider; rename is unavailable, \
                 not empty",
                session.entry.id
            ),
        });
    }
    let (uri, _version) = session.sync_file(&absolute)?;
    let line = require_u64(arguments, "line")?;
    let character = require_u64(arguments, "character")?;
    let position = serde_json::json!({
        "textDocument": {"uri": uri},
        "position": {"line": line, "character": character}
    });
    if session.rename_prepare_supported() {
        let prepared = session.request(
            "textDocument/prepareRename",
            position.clone(),
            manager.config().request_timeout_ms,
        )?;
        if prepared.is_null() {
            return Ok(RenameComputation {
                session,
                new_name,
                outcome: RenameOutcome::NotRenameable,
            });
        }
    }
    let mut rename_params = position;
    rename_params["newName"] = serde_json::Value::String(new_name.clone());
    let raw = session.request(
        "textDocument/rename",
        rename_params,
        manager.config().request_timeout_ms,
    )?;
    let files = shape_workspace_edit(&raw)?;
    Ok(RenameComputation {
        session,
        new_name,
        outcome: RenameOutcome::Edits(files),
    })
}

fn tool_rename_preview(
    manager: &SessionManager,
    arguments: &serde_json::Value,
) -> BridgeResult<serde_json::Value> {
    let computation = compute_rename(manager, arguments)?;
    match computation.outcome {
        RenameOutcome::NotRenameable => Ok(serde_json::json!({
            "applied": false,
            "renameable": false,
            "reason": "the server reported this position is not renameable \
                       (prepareRename returned null)",
            "new_name": computation.new_name,
            "files": [],
            "total_files": 0,
            "total_edits": 0,
        })),
        RenameOutcome::Edits(files) => {
            let rendered = render_file_list(&computation.session.workspace_root, &files)?;
            let result = serde_json::json!({
                "applied": false,
                "renameable": true,
                "new_name": computation.new_name,
                "files": rendered,
                "total_files": files.len(),
                "total_edits": total_edits(&files),
            });
            enforce_budget(&result, total_edits(&files), MAX_RESULT_BYTES)?;
            Ok(result)
        }
    }
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
