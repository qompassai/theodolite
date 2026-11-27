//! Typed bridge errors with stable codes (SPEC §4.6).
//!
//! Every failure the bridge reports is one of these variants. The
//! `code()` string is part of the wire contract: identical conditions
//! render identical codes, and a server-side JSON-RPC error surfaces as
//! `ServerError` with the server's code and message preserved verbatim.

use std::fmt;

/// Stable machine-readable error code (SPEC §4.6).
pub type ErrorCode = &'static str;

/// One typed bridge failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// Configuration is invalid; the bridge refuses to start.
    ConfigInvalid { detail: String },
    /// Session exists but its readiness predicate never held in time.
    ServerNotReady { server: String, detail: String },
    /// The language-server child died; carries the observed exit status.
    ServerDied { server: String, detail: String },
    /// A request exceeded its named timeout.
    ServerTimeout { server: String, detail: String },
    /// The probed binary version differs from the pinned config string.
    ServerVersionMismatch {
        server: String,
        expected: String,
        actual: String,
    },
    /// A shaped result would exceed its byte budget; carries the true
    /// item count so the caller can narrow the query (SPEC §4.4).
    ResultTooLarge { items: usize, budget_bytes: usize },
    /// A path or URI escapes the session workspace.
    WorkspaceEscape { detail: String },
    /// A gated (mutation-class) operation was refused by the
    /// decide()-class gate (P5): posture, identity, handle validity,
    /// or version state denied the apply. The decision and its
    /// reason are recorded in the session transcript.
    GateDenied { detail: String },
    /// The closed method/tool set was asked for something outside it.
    UnsupportedMethod { detail: String },
    /// The server sent traffic that violates bounded framing or the
    /// protocol state (oversized, truncated, unparseable, unknown id).
    MalformedServerTraffic { server: String, detail: String },
    /// The server answered a request with a JSON-RPC error; its code
    /// and message are preserved verbatim (SPEC §4.6).
    ServerError {
        server: String,
        rpc_code: i64,
        message: String,
    },
    /// Local I/O failure (spawn, pipe, filesystem) with bounded context.
    Io { detail: String },
    /// Session-pool bound reached (SPEC §3.2, `MAX_SESSIONS`).
    SessionLimit { detail: String },
    /// A tool argument failed validation before any server traffic.
    InvalidArgument { detail: String },
    /// A checker binary is missing, not executable, or its version
    /// probe could not run at all (checker design §7).
    CheckerUnavailable { checker: String, detail: String },
    /// A checker's probed version differs from the pinned config
    /// string; modelled on `ServerVersionMismatch` (design §6).
    CheckerVersionMismatch {
        checker: String,
        expected: String,
        actual: String,
    },
    /// A checker run exceeded its configured deadline; the child
    /// was killed by PID and its output discarded (design §7).
    CheckerTimeout { checker: String, timeout_ms: u64 },
    /// The caller's content hash was superseded by a newer hash
    /// while a run was in flight; no stale diagnostics are served
    /// (design §3.2).
    CheckerSuperseded { checker: String, detail: String },
    /// A checker run's output did not match the parser profile's
    /// grammar; the whole run is discarded, never approximated
    /// (design §5.2, §7).
    CheckerOutputInvalid { checker: String, detail: String },
    /// A checker run's captured output or diagnostic count crossed
    /// its configured bound; the run was killed or refused whole,
    /// never truncated into a plausible subset (design §7).
    CheckerOutputTooLarge { checker: String, detail: String },
    /// The input file exceeds the checker's `max_input_bytes`;
    /// refused before spawn, the tool never ran (design §7).
    CheckerInputTooLarge {
        checker: String,
        size: u64,
        limit: u64,
    },
    /// The input file is not valid UTF-8; a checker must not
    /// validate different bytes than the ones it hashed (design §7).
    CheckerInputInvalid { checker: String, detail: String },
    /// No shader stage can be derived for the target file; the
    /// bridge never guesses one (design §2.4).
    CheckerStageUnknown { checker: String, detail: String },
    /// The target is not under the discovered project root, so a
    /// `{relpath}` substitution would hand the tool an outside
    /// path (design §2.2).
    CheckerProjectRoot { checker: String, detail: String },
    /// A checker tool exited in a way the profile cannot map to
    /// diagnostics (unexpected status, signal death) (design §7).
    CheckerFailed { checker: String, detail: String },
}

impl BridgeError {
    /// The stable wire code for this error (SPEC §4.6).
    pub fn code(&self) -> ErrorCode {
        match self {
            BridgeError::ConfigInvalid { .. } => "ConfigInvalid",
            BridgeError::ServerNotReady { .. } => "ServerNotReady",
            BridgeError::ServerDied { .. } => "ServerDied",
            BridgeError::ServerTimeout { .. } => "ServerTimeout",
            BridgeError::ServerVersionMismatch { .. } => "ServerVersionMismatch",
            BridgeError::ResultTooLarge { .. } => "ResultTooLarge",
            BridgeError::WorkspaceEscape { .. } => "WorkspaceEscape",
            BridgeError::GateDenied { .. } => "GateDenied",
            BridgeError::UnsupportedMethod { .. } => "UnsupportedMethod",
            BridgeError::MalformedServerTraffic { .. } => "MalformedServerTraffic",
            BridgeError::ServerError { .. } => "ServerError",
            BridgeError::Io { .. } => "Io",
            BridgeError::SessionLimit { .. } => "SessionLimit",
            BridgeError::InvalidArgument { .. } => "InvalidArgument",
            BridgeError::CheckerUnavailable { .. } => "CheckerUnavailable",
            BridgeError::CheckerVersionMismatch { .. } => "CheckerVersionMismatch",
            BridgeError::CheckerTimeout { .. } => "CheckerTimeout",
            BridgeError::CheckerSuperseded { .. } => "CheckerSuperseded",
            BridgeError::CheckerOutputInvalid { .. } => "CheckerOutputInvalid",
            BridgeError::CheckerOutputTooLarge { .. } => "CheckerOutputTooLarge",
            BridgeError::CheckerInputTooLarge { .. } => "CheckerInputTooLarge",
            BridgeError::CheckerInputInvalid { .. } => "CheckerInputInvalid",
            BridgeError::CheckerStageUnknown { .. } => "CheckerStageUnknown",
            BridgeError::CheckerProjectRoot { .. } => "CheckerProjectRoot",
            BridgeError::CheckerFailed { .. } => "CheckerFailed",
        }
    }

    /// Render as the canonical JSON error object placed in MCP results.
    pub fn to_json(&self) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "code".to_string(),
            serde_json::Value::String(self.code().to_string()),
        );
        fields.insert(
            "message".to_string(),
            serde_json::Value::String(self.to_string()),
        );
        if let BridgeError::ResultTooLarge {
            items,
            budget_bytes,
        } = self
        {
            fields.insert(
                "items".to_string(),
                serde_json::Value::Number((*items as u64).into()),
            );
            fields.insert(
                "budget_bytes".to_string(),
                serde_json::Value::Number((*budget_bytes as u64).into()),
            );
        }
        if let BridgeError::ServerError { rpc_code, .. } = self {
            fields.insert(
                "server_rpc_code".to_string(),
                serde_json::Value::Number((*rpc_code).into()),
            );
        }
        if let BridgeError::CheckerInputTooLarge {
            checker,
            size,
            limit,
        } = self
        {
            fields.insert(
                "checker".to_string(),
                serde_json::Value::String(checker.clone()),
            );
            fields.insert(
                "size".to_string(),
                serde_json::Value::Number((*size).into()),
            );
            fields.insert(
                "limit".to_string(),
                serde_json::Value::Number((*limit).into()),
            );
        }
        if let BridgeError::CheckerTimeout {
            checker,
            timeout_ms,
        } = self
        {
            fields.insert(
                "checker".to_string(),
                serde_json::Value::String(checker.clone()),
            );
            fields.insert(
                "timeout_ms".to_string(),
                serde_json::Value::Number((*timeout_ms).into()),
            );
        }
        serde_json::Value::Object(fields)
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::ConfigInvalid { detail } => write!(f, "invalid config: {detail}"),
            BridgeError::ServerNotReady { server, detail } => {
                write!(f, "server {server} not ready: {detail}")
            }
            BridgeError::ServerDied { server, detail } => {
                write!(f, "server {server} died: {detail}")
            }
            BridgeError::ServerTimeout { server, detail } => {
                write!(f, "server {server} timed out: {detail}")
            }
            BridgeError::ServerVersionMismatch {
                server,
                expected,
                actual,
            } => write!(
                f,
                "server {server} version mismatch: expected {expected:?}, probed {actual:?}"
            ),
            BridgeError::ResultTooLarge {
                items,
                budget_bytes,
            } => write!(
                f,
                "result has {items} items and exceeds the {budget_bytes}-byte budget"
            ),
            BridgeError::WorkspaceEscape { detail } => write!(f, "workspace escape: {detail}"),
            BridgeError::GateDenied { detail } => write!(f, "gate denied: {detail}"),
            BridgeError::UnsupportedMethod { detail } => {
                write!(f, "unsupported method: {detail}")
            }
            BridgeError::MalformedServerTraffic { server, detail } => {
                write!(f, "malformed traffic from {server}: {detail}")
            }
            BridgeError::ServerError {
                server,
                rpc_code,
                message,
            } => {
                write!(f, "server {server} error {rpc_code}: {message}")
            }
            BridgeError::Io { detail } => write!(f, "io failure: {detail}"),
            BridgeError::SessionLimit { detail } => write!(f, "session limit: {detail}"),
            BridgeError::InvalidArgument { detail } => write!(f, "invalid argument: {detail}"),
            BridgeError::CheckerUnavailable { checker, detail } => {
                write!(f, "checker {checker} unavailable: {detail}")
            }
            BridgeError::CheckerVersionMismatch {
                checker,
                expected,
                actual,
            } => write!(
                f,
                "checker {checker} version mismatch: expected {expected:?}, probed {actual:?}"
            ),
            BridgeError::CheckerTimeout {
                checker,
                timeout_ms,
            } => write!(
                f,
                "checker {checker} timed out after {timeout_ms} ms; output discarded"
            ),
            BridgeError::CheckerSuperseded { checker, detail } => {
                write!(f, "checker {checker} run superseded: {detail}")
            }
            BridgeError::CheckerOutputInvalid { checker, detail } => {
                write!(f, "checker {checker} output invalid: {detail}")
            }
            BridgeError::CheckerOutputTooLarge { checker, detail } => {
                write!(f, "checker {checker} output too large: {detail}")
            }
            BridgeError::CheckerInputTooLarge {
                checker,
                size,
                limit,
            } => write!(
                f,
                "checker {checker} input too large: {size} bytes exceeds the {limit}-byte limit"
            ),
            BridgeError::CheckerInputInvalid { checker, detail } => {
                write!(f, "checker {checker} input invalid: {detail}")
            }
            BridgeError::CheckerStageUnknown { checker, detail } => {
                write!(f, "checker {checker} stage unknown: {detail}")
            }
            BridgeError::CheckerProjectRoot { checker, detail } => {
                write!(f, "checker {checker} project root: {detail}")
            }
            BridgeError::CheckerFailed { checker, detail } => {
                write!(f, "checker {checker} failed: {detail}")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

/// Convenience alias used across the crate.
pub type BridgeResult<T> = Result<T, BridgeError>;

/// Build an `Io` error from a `std::io::Error` with a short context label.
pub fn io_error(context: &str, source: std::io::Error) -> BridgeError {
    BridgeError::Io {
        detail: format!("{context}: {source}"),
    }
}
