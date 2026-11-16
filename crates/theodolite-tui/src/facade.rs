//! The narrow bridge surface the TUI programs against.
//!
//! [`BridgeFacade`] exposes exactly what the screens need:
//! `status`, `tools`, `servers`, and a bounded `call`. Two
//! implementations: [`LibFacade`] over the member's own library
//! crate (`theodolite::tools::call_tool` on a `SessionManager` —
//! DESIGN SS D2 prefers the lib where one exists), and
//! [`FakeFacade`], an in-memory scripted fake recording every call,
//! used by every test.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use theodolite::config::Config;
use theodolite::session::SessionManager;
use theodolite::tools;

use crate::RESULT_BYTES_MAX;

/// One configured language server, as the Overview and Workflows
/// screens present it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// Config routing key.
    pub id: String,
    /// Languages this server serves.
    pub languages: Vec<String>,
    /// File extensions this server serves (no leading dot).
    pub extensions: Vec<String>,
}

/// A typed facade failure. Expected failures are values, never
/// panics: a server dying mid-call, an oversized result, and bad
/// operator input are all ordinary outcomes here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FacadeError {
    /// The bridge or a language server could not serve the call
    /// (includes server death mid-call).
    Unavailable(String),
    /// The caller supplied invalid input.
    Invalid(String),
    /// The result exceeded [`RESULT_BYTES_MAX`] serialized bytes.
    Oversized(usize),
}

impl std::fmt::Display for FacadeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FacadeError::Unavailable(detail) => write!(f, "unavailable: {detail}"),
            FacadeError::Invalid(detail) => write!(f, "invalid: {detail}"),
            FacadeError::Oversized(bytes) => {
                write!(
                    f,
                    "oversized: result is {bytes} bytes, budget is {RESULT_BYTES_MAX}"
                )
            }
        }
    }
}

impl std::error::Error for FacadeError {}

/// The bridge operations the TUI drives.
pub trait BridgeFacade {
    /// Member status report as JSON (name, version, server count,
    /// session count where the member can know it).
    fn status(&mut self) -> Result<Value, FacadeError>;
    /// The member's own MCP tool list, as `tools/list` values.
    fn tools(&mut self) -> Result<Vec<Value>, FacadeError>;
    /// The configured language servers / routes.
    fn servers(&mut self) -> Vec<ServerInfo>;
    /// Call one tool by name with JSON arguments.
    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, FacadeError>;
}

/// Enforce the result byte budget on one call result.
fn check_budget(result: &Value) -> Result<(), FacadeError> {
    let bytes = result.to_string().len();
    if bytes > RESULT_BYTES_MAX {
        return Err(FacadeError::Oversized(bytes));
    }
    Ok(())
}

/// The real facade: the member's library crate in-process.
pub struct LibFacade {
    manager: Arc<SessionManager>,
    config_path: PathBuf,
}

impl LibFacade {
    /// Load the bridge config at `config_path` and build the
    /// session manager. A config violation is a typed refusal,
    /// exactly as the server binary refuses to start on one.
    pub fn from_config(config_path: PathBuf) -> Result<LibFacade, FacadeError> {
        let config = Config::load(&config_path)
            .map_err(|error| FacadeError::Invalid(error.to_json().to_string()))?;
        Ok(LibFacade {
            manager: Arc::new(SessionManager::new(Arc::new(config))),
            config_path,
        })
    }

    /// Shut down every live language-server session (owner cleanup
    /// on exit; the server binary does the same at transport EOF).
    pub fn shutdown(&self) {
        self.manager.shutdown_all();
    }
}

impl BridgeFacade for LibFacade {
    fn status(&mut self) -> Result<Value, FacadeError> {
        let config = self.manager.config();
        Ok(serde_json::json!({
            "name": crate::MEMBER_NAME,
            "version": crate::MEMBER_VERSION,
            "config": self.config_path.display().to_string(),
            "config_hash": config.config_hash,
            "server_count": config.servers.len(),
            "tool_count": tools::TOOL_NAMES.len(),
            "mutation": config.mutation.as_str(),
        }))
    }

    fn tools(&mut self) -> Result<Vec<Value>, FacadeError> {
        let definitions = tools::tool_definitions(
            self.manager.config(),
            self.manager.client_identity().as_deref(),
        );
        match definitions {
            Value::Array(items) => Ok(items),
            _ => Err(FacadeError::Unavailable(
                "tool definitions did not decode as an array".to_string(),
            )),
        }
    }

    fn servers(&mut self) -> Vec<ServerInfo> {
        self.manager
            .config()
            .servers
            .iter()
            .map(|entry| ServerInfo {
                id: entry.id.clone(),
                languages: entry.languages.clone(),
                extensions: entry.extensions.clone(),
            })
            .collect()
    }

    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, FacadeError> {
        let result = tools::call_tool(&self.manager, tool, args)
            .map_err(|error| FacadeError::Unavailable(error.to_json().to_string()))?;
        check_budget(&result)?;
        Ok(result)
    }
}

/// An in-memory [`BridgeFacade`] for tests: scripted results,
/// recorded calls, and scriptable failure (server death, oversized
/// payloads) for the adversarial cases.
#[derive(Debug)]
pub struct FakeFacade {
    /// Result returned by [`FakeFacade::status`].
    pub status_result: Result<Value, FacadeError>,
    /// Tools returned by [`FakeFacade::tools`].
    pub tools_result: Vec<Value>,
    /// Servers returned by [`FakeFacade::servers`].
    pub servers_result: Vec<ServerInfo>,
    /// Results returned by successive `call`s; when the queue is
    /// empty, `call` echoes its arguments as `{"echo": args}`.
    pub call_results: VecDeque<Result<Value, FacadeError>>,
    /// Every `(tool, args)` passed to `call`, in order. The plan-mode
    /// tests prove their point by this staying empty before approval.
    pub calls: Vec<(String, Value)>,
}

impl FakeFacade {
    /// A fake with a plausible two-server status until scripted.
    pub fn new() -> FakeFacade {
        FakeFacade {
            status_result: Ok(serde_json::json!({
                "name": crate::MEMBER_NAME,
                "version": crate::MEMBER_VERSION,
                "server_count": 2,
                "tool_count": 13,
            })),
            tools_result: vec![
                serde_json::json!({"name": "session_status"}),
                serde_json::json!({"name": "diagnostics"}),
            ],
            servers_result: vec![
                ServerInfo {
                    id: "bacon-ls".to_string(),
                    languages: vec!["rust".to_string()],
                    extensions: vec!["rs".to_string()],
                },
                ServerInfo {
                    id: "lua-language-server".to_string(),
                    languages: vec!["lua".to_string()],
                    extensions: vec!["lua".to_string()],
                },
            ],
            call_results: VecDeque::new(),
            calls: Vec::new(),
        }
    }

    /// Queue one result for the next `call`.
    pub fn queue_call_result(&mut self, result: Result<Value, FacadeError>) {
        self.call_results.push_back(result);
    }

    /// How many tool calls have reached the facade.
    pub fn call_count(&self) -> usize {
        self.calls.len()
    }
}

impl Default for FakeFacade {
    fn default() -> FakeFacade {
        FakeFacade::new()
    }
}

impl BridgeFacade for FakeFacade {
    fn status(&mut self) -> Result<Value, FacadeError> {
        self.status_result.clone()
    }

    fn tools(&mut self) -> Result<Vec<Value>, FacadeError> {
        Ok(self.tools_result.clone())
    }

    fn servers(&mut self) -> Vec<ServerInfo> {
        self.servers_result.clone()
    }

    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, FacadeError> {
        self.calls.push((tool.to_string(), args.clone()));
        let result = self
            .call_results
            .pop_front()
            .unwrap_or_else(|| Ok(serde_json::json!({"echo": args})));
        if let Ok(value) = &result {
            check_budget(value)?;
        }
        result
    }
}
