//! Session supervision (SPEC §2 `bridge-session`, §3, §4.2).
//!
//! One session per `(workspace_root, server_id)`, at most
//! `MAX_SESSIONS` per bridge. The bridge owns every child end to
//! end: absolute-path spawn through `setpriv --pdeathsig SIGKILL`
//! (the estate precedent from periscope `0ec568e`), a scrubbed
//! environment, a reaper thread, bounded stderr ring, state-based
//! readiness (fixed sleeps are banned as a readiness mechanism), and
//! a shutdown path that leaves no orphans behind.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::config::{Config, Readiness, ServerEntry, sha256_hex};
use crate::errors::{BridgeError, BridgeResult, io_error};
use crate::lsp::{LspMessage, read_message, write_message};
use crate::shaping::{Diagnostic, DiagnosticChannel, diagnostics_from_lsp};

/// Maximum concurrent sessions per bridge process (SPEC §3.2).
pub const MAX_SESSIONS: usize = 4;
/// Stderr ring capacity per child in bytes (SPEC §3.7).
pub const STDERR_RING_BYTES: usize = 256 * 1024;
/// Version-probe output cap and deadline.
pub const VERSION_PROBE_MAX_BYTES: usize = 64 * 1024;
/// Version-probe deadline in milliseconds.
pub const VERSION_PROBE_TIMEOUT_MS: u64 = 10_000;
/// Diagnostics settle poll interval (SPEC §6: two polls this far
/// apart with a stable diagnostics generation means settled).
pub const SETTLE_POLL_MS: u64 = 250;
/// Readiness poll interval in milliseconds.
pub const READY_POLL_MS: u64 = 100;
/// Absolute path of the pdeathsig wrapper (util-linux).
pub const SETPRIV_PATH: &str = "/usr/bin/setpriv";

/// Session lifecycle phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Spawned and initialized; readiness predicate not yet held.
    Starting,
    /// Readiness predicate holds; tools may proceed.
    Ready,
    /// Child died or traffic turned malformed; detail is preserved
    /// for every later caller (no silent respawn, SPEC §3.4).
    Dead(String),
}

/// One open document's sync state (SPEC §3.5).
#[derive(Debug, Clone)]
struct OpenDocument {
    version: i64,
    content_hash: String,
    size: u64,
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    diagnostics: BTreeMap<String, Vec<Diagnostic>>,
    diagnostics_generation: u64,
    progress_tokens: HashSet<String>,
    server_status_quiescent: Option<bool>,
    /// Whether any load activity has ever been observed. Quiescence
    /// before any activity is not quiescence — it is a server that
    /// has not started loading yet (the readiness race both studied
    /// bridges lose).
    saw_activity: bool,
    documents: HashMap<String, OpenDocument>,
    /// The server's advertised capabilities, captured verbatim from
    /// the `initialize` result (P3). Feature support is read from
    /// this, never probed by trial request: an unadvertised feature
    /// is a typed `UnsupportedMethod`, not a server error.
    server_capabilities: serde_json::Value,
    stderr_ring: VecDeque<u8>,
    exit_status: Option<String>,
    protocol_violations: u64,
    last_violation: Option<String>,
    started_ms: u128,
}

/// Shared mutable session state, touched by the reader thread, the
/// reaper thread, and tool calls.
#[derive(Debug)]
pub struct SessionState {
    inner: Mutex<Inner>,
    /// Set by the reaper thread once the child has been reaped.
    reaped: AtomicBool,
}

impl SessionState {
    fn new(started_ms: u128) -> SessionState {
        SessionState {
            inner: Mutex::new(Inner {
                phase: Phase::Starting,
                diagnostics: BTreeMap::new(),
                diagnostics_generation: 0,
                progress_tokens: HashSet::new(),
                server_status_quiescent: None,
                saw_activity: false,
                documents: HashMap::new(),
                server_capabilities: serde_json::Value::Null,
                stderr_ring: VecDeque::new(),
                exit_status: None,
                protocol_violations: 0,
                last_violation: None,
                started_ms,
            }),
            reaped: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mark the session dead; the first detail wins so callers see
    /// the root cause, not a downstream symptom.
    fn mark_dead(&self, detail: String) {
        let mut inner = self.lock();
        if !matches!(inner.phase, Phase::Dead(_)) {
            inner.phase = Phase::Dead(detail);
        }
    }

    fn record_violation(&self, what: String) {
        let mut inner = self.lock();
        inner.protocol_violations += 1;
        inner.last_violation = Some(what);
    }

    fn phase(&self) -> Phase {
        self.lock().phase.clone()
    }
}

/// A live (or dead-but-recorded) language-server session.
pub struct Session {
    /// Canonical workspace root this session is bound to.
    pub workspace_root: PathBuf,
    /// The config entry this session was built from.
    pub entry: ServerEntry,
    /// Actual probed version string (transcript, SPEC §4.2).
    pub actual_version: String,
    state: Arc<SessionState>,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    pending: Arc<PendingMap>,
    next_id: AtomicI64,
    start_instant: Instant,
    bridge_started: Instant,
}

impl Session {
    /// Probe the binary's version, spawn the child, run the
    /// initialize handshake, and return a Starting session.
    /// Readiness is *not* awaited here; see `ensure_ready`.
    fn start(
        bridge_started: Instant,
        entry: &ServerEntry,
        workspace_root: &Path,
        config: &Config,
    ) -> BridgeResult<Session> {
        let actual_version = probe_version(entry)?;
        if actual_version != entry.expect_version {
            return Err(BridgeError::ServerVersionMismatch {
                server: entry.id.clone(),
                expected: entry.expect_version.clone(),
                actual: actual_version,
            });
        }
        let (mut child, pid) = spawn_child(entry, workspace_root)?;
        let stdin = child.stdin.take().ok_or_else(|| BridgeError::Io {
            detail: "child stdin was not piped".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BridgeError::Io {
            detail: "child stdout was not piped".to_string(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| BridgeError::Io {
            detail: "child stderr was not piped".to_string(),
        })?;
        let _ = pid;
        let state = Arc::new(SessionState::new(bridge_started.elapsed().as_millis()));
        let stdin = Arc::new(Mutex::new(stdin));
        let child = Arc::new(Mutex::new(child));
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        spawn_stderr_drain(stderr, Arc::clone(&state));
        spawn_reaper(Arc::clone(&child), Arc::clone(&state), entry.id.clone());
        let session = Session {
            workspace_root: workspace_root.to_path_buf(),
            entry: entry.clone(),
            actual_version,
            state: Arc::clone(&state),
            stdin: Arc::clone(&stdin),
            child,
            pending: Arc::clone(&pending),
            next_id: AtomicI64::new(1),
            start_instant: Instant::now(),
            bridge_started,
        };
        spawn_reader(
            stdout,
            entry.clone(),
            Arc::clone(&state),
            Arc::clone(&stdin),
            Arc::clone(&pending),
        );
        session.initialize(config)?;
        Ok(session)
    }

    fn initialize(&self, config: &Config) -> BridgeResult<()> {
        let root_uri = crate::shaping::path_to_uri(&self.workspace_root);
        let name = self
            .workspace_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut params = serde_json::json!({
            "processId": null,
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": false},
                    "publishDiagnostics": {"relatedInformation": false},
                    "diagnostic": {"dynamicRegistration": false},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {},
                    "declaration": {},
                    "references": {},
                    "typeDefinition": {},
                    "implementation": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {"codeActionKind": {"valueSet": [
                            "", "quickfix", "refactor", "refactor.extract",
                            "refactor.inline", "refactor.rewrite", "source",
                            "source.organizeImports"
                        ]}}
                    }
                },
                "workspace": {"symbol": {}},
                "window": {"workDoneProgress": true},
                "experimental": {"serverStatusNotification": true}
            },
            "clientInfo": {"name": "theodolite", "version": env!("CARGO_PKG_VERSION")},
            "trace": "off"
        });
        if let Some(options) = &self.entry.initialization_options {
            params["initializationOptions"] = options.clone();
        }
        let initialize_result = self.request("initialize", params, config.request_timeout_ms)?;
        if let Some(capabilities) = initialize_result.get("capabilities") {
            self.state.lock().server_capabilities = capabilities.clone();
        }
        // The `initialized` notification is best-effort at start: if
        // the child died immediately after answering initialize
        // (truncated/garbage fixtures do exactly this), the write
        // fails — but the session must still be recorded, in its
        // Dead state, so callers get the typed death and the
        // transcript exists. The reader/reaper owns the death record;
        // swallowing this one write error loses nothing.
        let _ = self.notify("initialized", serde_json::json!({}));
        if self.entry.readiness == Readiness::Immediate {
            let mut inner = self.state.lock();
            if matches!(inner.phase, Phase::Starting) {
                inner.phase = Phase::Ready;
            }
        }
        Ok(())
    }

    /// Wait until the readiness predicate holds, the session dies, or
    /// the ready deadline passes (SPEC §3.3). State-based: polls held
    /// state, never sleeps *as* the readiness mechanism.
    pub fn ensure_ready(&self, config: &Config) -> BridgeResult<()> {
        let deadline = Instant::now() + Duration::from_millis(config.ready_timeout_ms);
        loop {
            match self.state.phase() {
                Phase::Ready => return Ok(()),
                Phase::Dead(detail) => {
                    return Err(BridgeError::ServerDied {
                        server: self.entry.id.clone(),
                        detail,
                    });
                }
                Phase::Starting => {}
            }
            if self.entry.readiness == Readiness::Quiescent && self.quiescence_holds() {
                // Probe: a successful workspace/symbol answer proves
                // the server computes, not merely idles.
                if self
                    .request("workspace/symbol", serde_json::json!({"query": ""}), 5_000)
                    .is_ok()
                {
                    self.state.lock().phase = Phase::Ready;
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(BridgeError::ServerNotReady {
                    server: self.entry.id.clone(),
                    detail: format!(
                        "readiness predicate {:?} did not hold within {} ms",
                        self.entry.readiness, config.ready_timeout_ms
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(READY_POLL_MS));
        }
    }

    fn quiescence_holds(&self) -> bool {
        let inner = self.state.lock();
        inner.progress_tokens.is_empty()
            && inner.server_status_quiescent != Some(false)
            && (inner.saw_activity || inner.server_status_quiescent == Some(true))
    }

    /// Send one request and wait for its response with a deadline.
    /// A server error is preserved verbatim as `ServerError`.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout_ms: u64,
    ) -> BridgeResult<serde_json::Value> {
        if let Phase::Dead(detail) = self.state.phase() {
            return Err(BridgeError::ServerDied {
                server: self.entry.id.clone(),
                detail,
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, sender);
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        if let Err(error) = self.send_frame(&frame) {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(error);
        }
        // Wait in slices, re-checking the phase: if the reader
        // thread records the death after this call registered (the
        // registration can race the reader's fail-all drain), the
        // call must still resolve as ServerDied instead of hanging
        // until the request deadline.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                return Err(BridgeError::ServerTimeout {
                    server: self.entry.id.clone(),
                    detail: format!("request {method} exceeded {timeout_ms} ms"),
                });
            }
            let slice = remaining.min(Duration::from_millis(100));
            match receiver.recv_timeout(slice) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Phase::Dead(detail) = self.state.phase() {
                        self.pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&id);
                        return Err(BridgeError::ServerDied {
                            server: self.entry.id.clone(),
                            detail,
                        });
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(BridgeError::ServerDied {
                        server: self.entry.id.clone(),
                        detail: match self.state.phase() {
                            Phase::Dead(detail) => detail,
                            _ => "reader thread ended without a response".to_string(),
                        },
                    });
                }
            }
        }
    }

    /// Send one notification; write failures surface as typed errors.
    pub fn notify(&self, method: &str, params: serde_json::Value) -> BridgeResult<()> {
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_frame(&frame)
    }

    /// Write one frame to the child's stdin. A write failure racing
    /// the child's death is reported as the death (the root cause),
    /// not as a bare pipe error: the reaper/reader gets a short,
    /// bounded window to record the death first.
    fn send_frame(&self, frame: &serde_json::Value) -> BridgeResult<()> {
        let outcome = {
            let mut guard = self.stdin.lock().unwrap_or_else(|p| p.into_inner());
            write_message(&mut *guard, &self.entry.id, frame)
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                for _ in 0..50 {
                    if let Phase::Dead(detail) = self.state.phase() {
                        return Err(BridgeError::ServerDied {
                            server: self.entry.id.clone(),
                            detail,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error)
            }
        }
    }

    /// Sync one file from disk into the session (SPEC §3.5): stat +
    /// hash check, `didOpen` on first touch, full-text `didChange`
    /// when the bytes changed, `didClose` when the file vanished.
    /// Returns the document URI and its current version.
    pub fn sync_file(&self, absolute_path: &Path) -> BridgeResult<(String, i64)> {
        let uri = crate::shaping::path_to_uri(absolute_path);
        let metadata = std::fs::metadata(absolute_path);
        let key = uri.clone();
        if metadata.is_err() {
            let was_open = self.state.lock().documents.remove(&key).is_some();
            if was_open {
                self.notify(
                    "textDocument/didClose",
                    serde_json::json!({"textDocument": {"uri": uri}}),
                )?;
            }
            return Err(BridgeError::InvalidArgument {
                detail: format!("file {} does not exist", absolute_path.display()),
            });
        }
        let content =
            std::fs::read(absolute_path).map_err(|e| io_error("reading document for sync", e))?;
        let hash = sha256_hex(&content);
        let size = content.len() as u64;
        let existing = self.state.lock().documents.get(&key).cloned();
        match existing {
            Some(doc) if doc.content_hash == hash && doc.size == size => Ok((uri, doc.version)),
            Some(doc) => {
                let version = doc.version + 1;
                let text = String::from_utf8_lossy(&content).into_owned();
                self.notify(
                    "textDocument/didChange",
                    serde_json::json!({
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": [{"text": text}]
                    }),
                )?;
                self.state.lock().documents.insert(
                    key,
                    OpenDocument {
                        version,
                        content_hash: hash,
                        size,
                    },
                );
                Ok((uri, version))
            }
            None => {
                let extension = absolute_path
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let language_id = self
                    .entry
                    .language_ids
                    .get(&extension)
                    .cloned()
                    .ok_or_else(|| BridgeError::InvalidArgument {
                        detail: format!(
                            "server {} has no language id for extension {extension:?}",
                            self.entry.id
                        ),
                    })?;
                let text = String::from_utf8_lossy(&content).into_owned();
                self.notify(
                    "textDocument/didOpen",
                    serde_json::json!({
                        "textDocument": {
                            "uri": uri, "languageId": language_id, "version": 1, "text": text
                        }
                    }),
                )?;
                self.state.lock().documents.insert(
                    key,
                    OpenDocument {
                        version: 1,
                        content_hash: hash,
                        size,
                    },
                );
                Ok((uri, 1))
            }
        }
    }

    /// Diagnostics for the whole workspace, or one file when
    /// `only_uri` is given, from the push accumulation. Waits for the
    /// settle predicate first (SPEC §6): no in-flight progress and a
    /// diagnostics generation stable across two polls.
    pub fn diagnostics(
        &self,
        config: &Config,
        only_uri: Option<&str>,
    ) -> BridgeResult<Vec<Diagnostic>> {
        self.wait_diagnostics_settled(config)?;
        let inner = self.state.lock();
        let mut collected: Vec<Diagnostic> = Vec::new();
        for (uri, diagnostics) in inner.diagnostics.iter() {
            if only_uri.is_some_and(|only| uri != only) {
                continue;
            }
            collected.extend(diagnostics.iter().cloned());
        }
        Ok(collected)
    }

    /// Whether the server advertised a capability in its
    /// `initialize` result. Boolean `false` and absence both mean
    /// unadvertised; an options object or `true` means advertised.
    pub fn capability_advertised(&self, capability: &str) -> bool {
        match self.state.lock().server_capabilities.get(capability) {
            Some(serde_json::Value::Bool(flag)) => *flag,
            Some(serde_json::Value::Null) | None => false,
            Some(_) => true,
        }
    }

    /// Whether the server offers pull diagnostics
    /// (`textDocument/diagnostic`, advertised as `diagnosticProvider`).
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.capability_advertised("diagnosticProvider")
    }

    /// URIs of the documents the bridge currently has open, sorted
    /// for deterministic iteration (pull scope, P3).
    pub fn open_document_uris(&self) -> Vec<String> {
        let inner = self.state.lock();
        let mut uris: Vec<String> = inner.documents.keys().cloned().collect();
        uris.sort();
        uris
    }

    /// Pull diagnostics for one document (`textDocument/diagnostic`).
    /// Only call when `supports_pull_diagnostics` holds. The bridge
    /// keeps no `resultId`, so every pull asks for a full report; an
    /// `unchanged` report (which a conforming server only sends
    /// against a `previousResultId` the bridge never supplied)
    /// normalizes to an empty set.
    pub fn pull_diagnostics(&self, config: &Config, uri: &str) -> BridgeResult<Vec<Diagnostic>> {
        let raw = self.request(
            "textDocument/diagnostic",
            serde_json::json!({"textDocument": {"uri": uri}}),
            config.request_timeout_ms,
        )?;
        if raw.get("kind").and_then(serde_json::Value::as_str) == Some("unchanged") {
            return Ok(Vec::new());
        }
        let items = raw
            .get("items")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(diagnostics_from_lsp(uri, &items, DiagnosticChannel::Pull))
    }

    fn wait_diagnostics_settled(&self, config: &Config) -> BridgeResult<()> {
        let deadline = Instant::now() + Duration::from_millis(config.ready_timeout_ms);
        let mut last_generation: Option<u64> = None;
        loop {
            if let Phase::Dead(detail) = self.state.phase() {
                return Err(BridgeError::ServerDied {
                    server: self.entry.id.clone(),
                    detail,
                });
            }
            let (generation, in_flight) = {
                let inner = self.state.lock();
                (inner.diagnostics_generation, inner.progress_tokens.len())
            };
            if in_flight == 0 && last_generation == Some(generation) {
                return Ok(());
            }
            last_generation = Some(generation);
            if Instant::now() >= deadline {
                return Err(BridgeError::ServerNotReady {
                    server: self.entry.id.clone(),
                    detail: "diagnostics did not settle within the ready deadline".to_string(),
                });
            }
            std::thread::sleep(Duration::from_millis(SETTLE_POLL_MS));
        }
    }

    /// The session transcript (SPEC §4.2): everything an agent needs
    /// to name exactly what produced a result.
    pub fn transcript(&self, config: &Config) -> serde_json::Value {
        let inner = self.state.lock();
        let phase = match &inner.phase {
            Phase::Starting => "starting",
            Phase::Ready => "ready",
            Phase::Dead(_) => "dead",
        };
        let mut value = serde_json::json!({
            "server": self.entry.id,
            "binary": self.entry.binary.to_string_lossy(),
            "actual_version": self.actual_version,
            "expected_version": self.entry.expect_version,
            "workspace": self.workspace_root.to_string_lossy(),
            "bridge_version": env!("CARGO_PKG_VERSION"),
            "toolchain_pin": crate::TOOLCHAIN_PIN,
            "config_hash": config.config_hash,
            "phase": phase,
            "session_started_ms": inner.started_ms,
            "session_age_ms": self.start_instant.elapsed().as_millis(),
            "open_documents": inner.documents.len(),
            "protocol_violations": inner.protocol_violations,
            "mutation": config.mutation,
        });
        if let Phase::Dead(detail) = &inner.phase {
            value["dead_detail"] = serde_json::Value::String(detail.clone());
        }
        if let Some(status) = &inner.exit_status {
            value["exit_status"] = serde_json::Value::String(status.clone());
        }
        if let Some(violation) = &inner.last_violation {
            value["last_violation"] = serde_json::Value::String(violation.clone());
        }
        if matches!(inner.phase, Phase::Dead(_)) && !inner.stderr_ring.is_empty() {
            let tail: Vec<u8> = inner.stderr_ring.iter().copied().collect();
            value["stderr_tail"] =
                serde_json::Value::String(String::from_utf8_lossy(&tail).into_owned());
        }
        value
    }

    /// Child pid and liveness for `session_status`.
    pub fn pid_and_liveness(&self) -> (u32, bool) {
        let guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
        (guard.id(), !self.state.reaped.load(Ordering::SeqCst))
    }

    /// Graceful shutdown (SPEC §3.6): LSP `shutdown` + `exit`, grace,
    /// then SIGKILL the child if it survives; the reaper reaps it.
    pub fn shutdown(&self, grace_ms: u64) {
        if !self.state.reaped.load(Ordering::SeqCst) {
            let _ = self.request("shutdown", serde_json::Value::Null, grace_ms);
            let _ = self.notify("exit", serde_json::Value::Null);
            let deadline = Instant::now() + Duration::from_millis(grace_ms);
            while !self.state.reaped.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if !self.state.reaped.load(Ordering::SeqCst) {
            let mut guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
            let _ = guard.kill();
        }
    }

    /// Session start instant on the bridge's monotonic clock.
    pub fn bridge_started(&self) -> Instant {
        self.bridge_started
    }
}

/// Owns all sessions for one bridge process.
pub struct SessionManager {
    config: Arc<Config>,
    sessions: Mutex<HashMap<(PathBuf, String), Arc<Session>>>,
    started: Instant,
}

impl SessionManager {
    /// Create an empty manager bound to a validated config.
    pub fn new(config: Arc<Config>) -> SessionManager {
        SessionManager {
            config,
            sessions: Mutex::new(HashMap::new()),
            started: Instant::now(),
        }
    }

    /// The bound config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get the live session for a key, starting it on first use. A
    /// recorded dead session is returned as-is (no silent respawn;
    /// `session_restart` is the only respawn path, SPEC §3.4).
    pub fn get_or_start(
        &self,
        workspace_root: &Path,
        server_id: &str,
    ) -> BridgeResult<Arc<Session>> {
        let entry = self
            .config
            .server_by_id(server_id)
            .ok_or_else(|| BridgeError::InvalidArgument {
                detail: format!("no configured server with id {server_id:?}"),
            })?
            .clone();
        let key = (workspace_root.to_path_buf(), server_id.to_string());
        {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(existing) = guard.get(&key) {
                return Ok(Arc::clone(existing));
            }
            if guard.len() >= MAX_SESSIONS {
                return Err(BridgeError::SessionLimit {
                    detail: format!(
                        "a fifth distinct (workspace, server) key exceeds MAX_SESSIONS = \
                         {MAX_SESSIONS}; no session is evicted silently"
                    ),
                });
            }
        }
        let session = Session::start(self.started, &entry, workspace_root, &self.config)?;
        let session = Arc::new(session);
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(key, Arc::clone(&session));
        Ok(session)
    }

    /// Look up a session without starting it (for `session_status`
    /// on a never-started key).
    pub fn peek(&self, workspace_root: &Path, server_id: &str) -> Option<Arc<Session>> {
        let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .get(&(workspace_root.to_path_buf(), server_id.to_string()))
            .cloned()
    }

    /// Tear down and restart one session (SPEC §6 `session_restart`).
    pub fn restart(&self, workspace_root: &Path, server_id: &str) -> BridgeResult<Arc<Session>> {
        let key = (workspace_root.to_path_buf(), server_id.to_string());
        let old = {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.remove(&key)
        };
        if let Some(old) = old {
            old.shutdown(self.config.shutdown_grace_ms);
        }
        self.get_or_start(workspace_root, server_id)
    }

    /// Shut down every session; called on transport EOF/signals.
    pub fn shutdown_all(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.drain().map(|(_, session)| session).collect()
        };
        for session in sessions {
            session.shutdown(self.config.shutdown_grace_ms);
        }
    }
}

/// Validate and canonicalize a workspace argument: absolute path,
/// existing directory. Relative paths are `InvalidArgument`, never
/// silently resolved against an ambient cwd (SPEC §6 conventions).
pub fn validate_workspace(raw: &str) -> BridgeResult<PathBuf> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(BridgeError::InvalidArgument {
            detail: format!("workspace {raw:?} must be an absolute path"),
        });
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| io_error("canonicalizing workspace", e))?;
    if !canonical.is_dir() {
        return Err(BridgeError::InvalidArgument {
            detail: format!("workspace {raw:?} is not a directory"),
        });
    }
    Ok(canonical)
}

/// Resolve a tool's file argument inside a workspace. Accepts an
/// absolute path or a workspace-relative path; the canonicalized
/// result must stay inside the canonical workspace root, so `../`,
/// absolute outsiders, and symlinks pointing out are all
/// `WorkspaceEscape` (SPEC §9.9).
pub fn resolve_within(workspace_root: &Path, raw: &str) -> BridgeResult<PathBuf> {
    if raw.is_empty() {
        return Err(BridgeError::InvalidArgument {
            detail: "path argument must not be empty".to_string(),
        });
    }
    let candidate = {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            // Reject textual traversal before canonicalization too, so
            // an encoded `..` can never be legitimized by a symlink.
            if raw.split('/').any(|part| part == "..") {
                return Err(BridgeError::WorkspaceEscape {
                    detail: format!("path {raw:?} contains a `..` component"),
                });
            }
            path
        } else {
            if PathBuf::from(raw)
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(BridgeError::WorkspaceEscape {
                    detail: format!("path {raw:?} contains a `..` component"),
                });
            }
            workspace_root.join(path)
        }
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|_| BridgeError::InvalidArgument {
            detail: format!("path {raw:?} does not exist inside the workspace"),
        })?;
    if !canonical.starts_with(workspace_root) {
        return Err(BridgeError::WorkspaceEscape {
            detail: format!("path {raw:?} resolves outside the workspace"),
        });
    }
    Ok(canonical)
}

/// Probe a server's version: run `<binary> <probe argv>` with a
/// scrubbed environment, a deadline, and a bounded output. The
/// trimmed stdout is the actual version string (SPEC §4.1).
fn probe_version(entry: &ServerEntry) -> BridgeResult<String> {
    let mut command = Command::new(&entry.binary);
    command.args(&entry.version_probe);
    command.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|e| io_error("spawning version probe", e))?;
    let deadline = Instant::now() + Duration::from_millis(VERSION_PROBE_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(BridgeError::ServerNotReady {
                        server: entry.id.clone(),
                        detail: "version probe exceeded its deadline".to_string(),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(io_error("waiting on version probe", e)),
        }
    };
    if !status.success() {
        return Err(BridgeError::ServerNotReady {
            server: entry.id.clone(),
            detail: format!("version probe exited with {status}"),
        });
    }
    let mut output = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut limited = Read::take(&mut stdout, (VERSION_PROBE_MAX_BYTES + 1) as u64);
        limited
            .read_to_end(&mut output)
            .map_err(|e| io_error("reading version probe output", e))?;
    }
    if output.len() > VERSION_PROBE_MAX_BYTES {
        return Err(BridgeError::ServerNotReady {
            server: entry.id.clone(),
            detail: "version probe output exceeds its byte cap".to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

/// Spawn the language-server child under the estate's orphan-proof
/// discipline (SPEC §3.1).
fn spawn_child(entry: &ServerEntry, workspace_root: &Path) -> BridgeResult<(Child, u32)> {
    let mut command = Command::new(SETPRIV_PATH);
    command
        .arg("--pdeathsig")
        .arg("SIGKILL")
        .arg("--")
        .arg(&entry.binary)
        .args(&entry.args);
    command.current_dir(workspace_root);
    command.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    // Fixed PATH; a config entry may override it explicitly through
    // its env map (data, recorded in the transcript's config hash).
    command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    command.env("LANG", "C.UTF-8");
    command.env("LC_ALL", "C.UTF-8");
    for (name, value) in &entry.env {
        command.env(name, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|e| io_error("spawning language server via setpriv", e))?;
    let pid = child.id();
    Ok((child, pid))
}

fn spawn_stderr_drain(mut stderr: impl Read + Send + 'static, state: Arc<SessionState>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let mut inner = state.lock();
                    for byte in &chunk[..count] {
                        if inner.stderr_ring.len() >= STDERR_RING_BYTES {
                            inner.stderr_ring.pop_front();
                        }
                        inner.stderr_ring.push_back(*byte);
                    }
                }
            }
        }
    });
}

fn spawn_reaper(child: Arc<Mutex<Child>>, state: Arc<SessionState>, server: String) {
    std::thread::spawn(move || {
        loop {
            let status = {
                let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
                guard.try_wait()
            };
            match status {
                Ok(Some(exit)) => {
                    state.lock().exit_status = Some(exit.to_string());
                    state.mark_dead(format!("child exited with {exit}"));
                    state.reaped.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    state.mark_dead(format!("reaper failed for {server}: {error}"));
                    state.reaped.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    });
}

/// The reader thread: sole consumer of the child's stdout. Routes
/// responses to pending callers, accumulates notifications, and
/// answers server->client requests under the bridge's rules.
#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    mut stdout: impl Read + Send + 'static,
    entry: ServerEntry,
    state: Arc<SessionState>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<PendingMap>,
) {
    std::thread::spawn(move || {
        loop {
            match read_message(&mut stdout, &entry.id) {
                Ok(Some(LspMessage::Response { id, result })) => {
                    let sender = pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    match sender {
                        Some(sender) => {
                            let mapped =
                                result.map_err(|(rpc_code, message)| BridgeError::ServerError {
                                    server: entry.id.clone(),
                                    rpc_code,
                                    message,
                                });
                            let _ = sender.send(mapped);
                        }
                        None => {
                            // A response id the bridge never minted:
                            // protocol state is unrecoverable (§9.6).
                            let error = BridgeError::MalformedServerTraffic {
                                server: entry.id.clone(),
                                detail: format!("response with unknown id {id}"),
                            };
                            fail_all_pending(&pending, &error);
                            state.mark_dead(error.to_string());
                            break;
                        }
                    }
                }
                Ok(Some(LspMessage::Notification { method, params })) => {
                    handle_notification(&state, &method, &params);
                }
                Ok(Some(LspMessage::Request { id, method, params })) => {
                    handle_server_request(&entry, &state, &stdin, id, &method, &params);
                }
                Ok(None) => {
                    let error = BridgeError::ServerDied {
                        server: entry.id.clone(),
                        detail: "server closed its stdout (EOF)".to_string(),
                    };
                    fail_all_pending(&pending, &error);
                    state.mark_dead(error.to_string());
                    break;
                }
                Err(error) => {
                    fail_all_pending(&pending, &error);
                    state.mark_dead(error.to_string());
                    break;
                }
            }
        }
    });
}

/// Pending-request table shared by a session and its reader thread.
pub type PendingMap = Mutex<HashMap<i64, mpsc::Sender<BridgeResult<serde_json::Value>>>>;

fn fail_all_pending(pending: &Arc<PendingMap>, error: &BridgeError) {
    let senders: Vec<mpsc::Sender<BridgeResult<serde_json::Value>>> = {
        let mut guard = pending.lock().unwrap_or_else(|p| p.into_inner());
        guard.drain().map(|(_, sender)| sender).collect()
    };
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

fn handle_notification(state: &Arc<SessionState>, method: &str, params: &serde_json::Value) {
    match method {
        "textDocument/publishDiagnostics" => {
            let uri = params
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let items = params
                .get("diagnostics")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let normalized = diagnostics_from_lsp(uri, &items, DiagnosticChannel::Push);
            let mut inner = state.lock();
            inner.diagnostics.insert(uri.to_string(), normalized);
            inner.diagnostics_generation += 1;
        }
        "$/progress" => {
            let token = params
                .get("token")
                .map(|t| match t {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let kind = params
                .get("value")
                .and_then(|v| v.get("kind"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut inner = state.lock();
            match kind.as_str() {
                "begin" => {
                    inner.saw_activity = true;
                    inner.progress_tokens.insert(token);
                }
                "end" => {
                    inner.progress_tokens.remove(&token);
                }
                _ => {}
            }
        }
        "experimental/serverStatus" => {
            if let Some(quiescent) = params.get("quiescent").and_then(serde_json::Value::as_bool) {
                let mut inner = state.lock();
                inner.server_status_quiescent = Some(quiescent);
                if !quiescent {
                    inner.saw_activity = true;
                }
            }
        }
        _ => {}
    }
}

fn handle_server_request(
    entry: &ServerEntry,
    state: &Arc<SessionState>,
    stdin: &Arc<Mutex<ChildStdin>>,
    id: serde_json::Value,
    method: &str,
    params: &serde_json::Value,
) {
    let reply: serde_json::Value = match method {
        // SPEC §8: always a typed refusal. A server that pushes edits
        // is logged as a protocol violation in the transcript.
        "workspace/applyEdit" => {
            state.record_violation(
                "server pushed workspace/applyEdit; refused (SPEC section 8)".to_string(),
            );
            serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32001, "message": "workspace/applyEdit is refused by the bridge"}
            })
        }
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(serde_json::Value::as_array)
                .map_or(1, Vec::len);
            let value = entry.settings.clone().unwrap_or_else(|| {
                state.record_violation(
                    "workspace/configuration requested but no settings are declared".to_string(),
                );
                serde_json::Value::Null
            });
            let results: Vec<serde_json::Value> = (0..count).map(|_| value.clone()).collect();
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": results})
        }
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create" => {
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})
        }
        other => {
            state.record_violation(format!("server requested undeclared method {other}"));
            serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not supported by the bridge: {other}")}
            })
        }
    };
    let mut guard = stdin.lock().unwrap_or_else(|p| p.into_inner());
    let _ = write_message(&mut *guard, &entry.id, &reply);
}
