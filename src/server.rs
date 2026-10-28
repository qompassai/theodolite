//! The rmcp server: tool surface and handler dispatch.
//!
//! Domain logic does not move in the WS2 Wave 4 migration — the
//! tool definitions and dispatch below are [`crate::tools`]',
//! unchanged: same schemas (the baseline's own hand-written
//! `inputSchema` values, served raw), same canonical text payloads
//! (sorted keys, no `structuredContent`), same failure taxonomy
//! (typed [`BridgeError`]s rendered as `isError` text carrying
//! `{"error": {code, message, …}}`). Only the framing changes
//! sides of the seam: rmcp owns the envelope, the era guard (see
//! [`crate::guard`]) owns the stdio front door, and this type owns
//! the surface.
//!
//! Modern-only posture lives in two places: the era guard refuses
//! `initialize` before the SDK sees it, and
//! [`ServerHandler::supported_protocol_versions`] pins this handler
//! to 2026-07-28, so `server/discover` advertises exactly one
//! version and the SDK validates every request's `_meta` against
//! it.
//!
//! Client identity — the mutation gate's input — moves with the
//! era: the legacy handshake carried it in `initialize`'s
//! `clientInfo`; the modern era carries it in every request's
//! `_meta` (`io.modelcontextprotocol/clientInfo`). Each handler
//! records the calling request's identity on the session manager
//! before computing anything (list or dispatch), which reproduces
//! the baseline's semantics for a stdio connection: the connected
//! client's declared name decides the registered surface, and an
//! absent identity fails closed in the gate, exactly as a
//! handshake without `clientInfo` did. Identity remains a routing
//! label, not authentication (CHARTER: mutation posture).
//!
//! The baseline served sequentially; dispatch here runs on
//! `spawn_blocking` behind a mutex, so pipelined modern requests
//! observe the same one-at-a-time session semantics.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

use crate::session::SessionManager;
use crate::tools;

/// The server handler: one session manager (the bridge's whole
/// domain state) plus the dispatch mutex.
pub struct TheodoliteServer {
    manager: Arc<SessionManager>,
    dispatch: Arc<Mutex<()>>,
}

impl TheodoliteServer {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        TheodoliteServer {
            manager,
            dispatch: Arc::new(Mutex::new(())),
        }
    }

    /// Record the calling request's declared client identity for
    /// the mutation gate (see the module docs). An identity that
    /// does not decode, or is absent, records NO identity — fail
    /// closed, as the baseline's handshake handling did.
    fn record_identity(&self, context: &RequestContext<RoleServer>) {
        let name = context
            .meta
            .client_info()
            .map(|info| info.name)
            .filter(|name| !name.is_empty());
        self.manager.set_client_identity(name);
    }
}

/// Build rmcp `Tool`s from the baseline's own definitions: the
/// name, description, and `inputSchema` values are the bridge's,
/// byte-for-byte — no schemars translation stands between the
/// baseline and the wire.
fn tool_definitions(manager: &SessionManager) -> Vec<Tool> {
    let definitions =
        tools::tool_definitions(manager.config(), manager.client_identity().as_deref());
    let Value::Array(items) = definitions else {
        unreachable!("tool_definitions returns an array");
    };
    items
        .into_iter()
        .map(|definition| {
            let name = definition["name"]
                .as_str()
                .expect("tool definition carries a name")
                .to_string();
            let description = definition["description"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let Value::Object(schema) = definition["inputSchema"].clone() else {
                unreachable!("tool definition carries an object inputSchema");
            };
            Tool::new(name, description, schema)
        })
        .collect()
}

/// Serialize a result canonically: object keys sorted recursively,
/// so identical results render byte-identical text (SPEC §4.5) —
/// the baseline MCP layer's shaping, unchanged.
fn canonical_text(value: &Value) -> String {
    sort_keys(value).to_string()
}

fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), sort_keys(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

fn text_result(text: String, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// The baseline's dispatch, unchanged: one tools call against the
/// session manager, success and typed failure both rendered as
/// canonical text content.
fn call_tool(manager: &SessionManager, name: &str, arguments: &Value) -> CallToolResult {
    match tools::call_tool(manager, name, arguments) {
        Ok(result) => text_result(canonical_text(&result), false),
        Err(error) => text_result(canonical_text(&json!({"error": error.to_json()})), true),
    }
}

impl ServerHandler for TheodoliteServer {
    fn get_info(&self) -> InitializeResult {
        // Both model types are non-exhaustive: start from the
        // default and set exactly the fields this server reports.
        let mut server_info = Implementation::default();
        server_info.name = "theodolite".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.record_identity(&context);
        Ok(ListToolsResult {
            tools: tool_definitions(&self.manager),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.record_identity(&context);
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let manager = Arc::clone(&self.manager);
        let dispatch = Arc::clone(&self.dispatch);
        let outcome = tokio::task::spawn_blocking(move || {
            // The baseline served one call at a time; a poisoned
            // dispatch lock means an earlier call panicked while
            // holding it — recover the guard and keep serving,
            // since the session state lives behind its own locks.
            let _held = match dispatch.lock() {
                Ok(held) => held,
                Err(poisoned) => poisoned.into_inner(),
            };
            call_tool(&manager, &name, &arguments)
        })
        .await;
        match outcome {
            Ok(result) => Ok(result.into()),
            Err(join) => Err(ErrorData::internal_error(
                format!("tool task failed: {join}"),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_text_sorts_keys_recursively() {
        let value = json!({"b": {"d": 1, "c": 2}, "a": [{"z": 1, "y": 2}]});
        assert_eq!(
            canonical_text(&value),
            "{\"a\":[{\"y\":2,\"z\":1}],\"b\":{\"c\":2,\"d\":1}}"
        );
    }
}
