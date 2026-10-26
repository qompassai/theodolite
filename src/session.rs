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
use crate::edits::FileEdits;
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
/// Per-attempt timeout for the first-response readiness probe:
/// one real request gets at most this long to answer before the
/// attempt counts as not-ready and the poll loop tries again
/// (the ready deadline still governs the whole wait).
pub const FIRST_RESPONSE_PROBE_TIMEOUT_MS: u64 = 5_000;
/// Bounds on the first-response probe-file search: directory
/// depth and total entries scanned under the workspace root.
/// The search is bounded work like everything else in the
/// bridge; a workspace whose probe file hides past the bounds
/// fails readiness loudly rather than scanning forever.
pub const PROBE_SCAN_MAX_DEPTH: usize = 8;
/// Entry bound companion to `PROBE_SCAN_MAX_DEPTH`.
pub const PROBE_SCAN_MAX_ENTRIES: usize = 4096;
/// Extra wait, beyond the supervisor's own grace, for the EOF
/// escalation in `Session::shutdown` to take the tree down before
/// the bridge falls back to killing the supervisor alone.
pub const EOF_ESCALATION_SLACK_MS: u64 = 1_000;
/// Cap on the advisory drain in `Session::diagnostics_context`:
/// long enough for a publish provoked by the sync that precedes it
/// to be consumed by the reader thread, short enough to never park
/// a code-action call on a silent server.
pub const CONTEXT_DRAIN_MAX_MS: u64 = 2_000;
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

/// Gate-log capacity per session (P5 item 2): decisions are
/// metadata, but the log is still bounded like everything else in
/// the bridge; the oldest entries drop first.
pub const GATE_LOG_MAX: usize = 256;

/// One recorded gate decision (transcript metadata only).
#[derive(Debug, Clone)]
pub struct GateLogEntry {
    /// The mutation tool the decision concerned.
    pub tool: String,
    /// `"allow"` or `"deny"`.
    pub decision: String,
    /// The decide() reason, verbatim.
    pub reason: String,
}

/// The lookup state of one action handle (P5 item 3).
#[derive(Debug, Clone)]
pub enum ActionHandleLookup {
    /// Exists, unused, applicable.
    Valid(StoredAction),
    /// Exists, but its single use is spent.
    Used,
    /// No such handle in this session.
    Missing,
}

/// One minted action handle's stored state (P5 item 3): the shaped
/// edit set a `code_actions` listing offered, the listing-time
/// SHA-256 of every target file (`None` where the target did not
/// exist at listing), and the single-use flag. Handles live and
/// die with their session; nothing here is ever rendered into a
/// listing result beyond the opaque id.
#[derive(Debug, Clone)]
pub struct StoredAction {
    /// The shaped edit set to apply.
    pub edits: Vec<FileEdits>,
    /// (absolute uri, listing-time hash) per target file.
    pub file_hashes: Vec<(String, Option<String>)>,
    /// Set when the handle's one use is spent (spent even when the
    /// applier then refuses: a consumed handle never retries).
    pub used: bool,
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
    /// Settle evidence (P4 item 2): one session-wide event sequence,
    /// stamped when the bridge actually sends a document sync and
    /// when an evidence-bearing notification arrives, so "a publish
    /// exists that postdates the last sync" is a comparison instead
    /// of a guess about server timing.
    event_seq: u64,
    doc_sync_seq: HashMap<String, u64>,
    doc_publish_seq: HashMap<String, u64>,
    last_publish_seq: u64,
    last_sync_seq: u64,
    last_progress_end_seq: u64,
    last_quiescent_seq: u64,
    progress_completed: u64,
    /// The server's advertised capabilities, captured verbatim from
    /// the `initialize` result (P3). Feature support is read from
    /// this, never probed by trial request: an unadvertised feature
    /// is a typed `UnsupportedMethod`, not a server error.
    server_capabilities: serde_json::Value,
    /// The most recent first-response probe outcome, in words: what
    /// the probe tried and what the server did with it. Recorded on
    /// every attempt so a `ServerNotReady` failure names the
    /// evidence instead of only the elapsed window.
    probe_outcome: Option<String>,
    /// Action handles minted by `code_actions` for the gated apply
    /// (P5 item 3), keyed by their opaque id.
    action_handles: HashMap<String, StoredAction>,
    /// Handle id sequence (per session; ids are opaque counters,
    /// never encodings of the edit they name).
    next_handle_seq: u64,
    /// Gate decisions recorded for the transcript (P5 item 2):
    /// metadata only — tool, outcome, reason. No edit bodies, no
    /// file contents, ever.
    gate_log: Vec<GateLogEntry>,
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
                event_seq: 0,
                doc_sync_seq: HashMap::new(),
                doc_publish_seq: HashMap::new(),
                last_publish_seq: 0,
                last_sync_seq: 0,
                last_progress_end_seq: 0,
                last_quiescent_seq: 0,
                progress_completed: 0,
                server_capabilities: serde_json::Value::Null,
                probe_outcome: None,
                action_handles: HashMap::new(),
                next_handle_seq: 0,
                gate_log: Vec::new(),
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

    /// Stamp a document-sync event: the bridge is about to send a
    /// didOpen/didChange/didClose for `uri` (P4 item 2). Only actual
    /// sends are stamped; an unchanged-content re-sync sends nothing
    /// and stamps nothing.
    fn note_sync(&self, uri: &str) {
        let mut inner = self.lock();
        inner.event_seq += 1;
        let seq = inner.event_seq;
        inner.doc_sync_seq.insert(uri.to_string(), seq);
        inner.last_sync_seq = seq;
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
    /// The supervisor's stdin. `None` after `close_stdin`: closing
    /// it is the supervisor's EOF shutdown trigger (P4 item 1).
    stdin: Arc<Mutex<Option<ChildStdin>>>,
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
        let (mut child, pid) = spawn_child(entry, workspace_root, config.shutdown_grace_ms)?;
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
        let stdin = Arc::new(Mutex::new(Some(stdin)));
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
                    "rename": {"prepareSupport": true},
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
            if self.entry.readiness == Readiness::FirstResponse {
                // One real answered request proves the server
                // computes; background churn is not consulted at
                // all, so a server that never goes quiescent is
                // not held hostage by its own indexing.
                if Instant::now() < deadline && self.first_response_probe() {
                    self.state.lock().phase = Phase::Ready;
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                let mut detail = format!(
                    "readiness predicate {:?} did not hold within {} ms",
                    self.entry.readiness, config.ready_timeout_ms
                );
                if self.entry.readiness == Readiness::FirstResponse
                    && let Some(outcome) = self.state.lock().probe_outcome.clone()
                {
                    detail.push_str(&format!("; last probe: {outcome}"));
                }
                return Err(BridgeError::ServerNotReady {
                    server: self.entry.id.clone(),
                    detail,
                });
            }
            std::thread::sleep(Duration::from_millis(READY_POLL_MS));
        }
    }

    /// One first-response probe attempt: pick the probe file, sync
    /// it, and ask the server one real question about it. Returns
    /// true only on a successful (non-error) answer — an error
    /// response, a timeout, and a missing probe surface all count
    /// as not-ready, never as ready. Every outcome is recorded so
    /// the eventual `ServerNotReady` (if any) names the evidence.
    ///
    /// The probe method is read from the advertised capabilities,
    /// never probed by trial: `documentSymbol` where advertised,
    /// else `hover`. A server advertising neither cannot satisfy
    /// this predicate, and the recorded outcome says exactly that.
    fn first_response_probe(&self) -> bool {
        let (method, with_position) = if self.capability_advertised("documentSymbolProvider") {
            ("textDocument/documentSymbol", false)
        } else if self.capability_advertised("hoverProvider") {
            ("textDocument/hover", true)
        } else {
            self.record_probe_outcome(
                "server advertises neither documentSymbolProvider nor hoverProvider; \
                     no probe request exists to try",
            );
            return false;
        };
        let Some(path) = self.find_probe_file() else {
            self.record_probe_outcome(
                "no file with a served extension exists under the workspace root",
            );
            return false;
        };
        let (uri, _version) = match self.sync_file(&path) {
            Ok(pair) => pair,
            Err(error) => {
                self.record_probe_outcome(&format!(
                    "probe file {} could not be synced: {error}",
                    path.display()
                ));
                return false;
            }
        };
        let mut params = serde_json::json!({"textDocument": {"uri": uri}});
        if with_position {
            params["position"] = serde_json::json!({"line": 0, "character": 0});
        }
        // The attempt timeout is the fixed probe cap, NOT the time
        // remaining to the ready deadline (the quiescent probe
        // above makes the same choice). Clamping an attempt to a
        // shrinking remainder manufactures timed-out requests
        // whose late answers then land as unknown-id traffic and
        // kill the session; the deadline governs the wait between
        // attempts, one attempt may run to its own cap.
        match self.request(method, params, FIRST_RESPONSE_PROBE_TIMEOUT_MS) {
            Ok(_) => {
                self.record_probe_outcome(&format!("{method} on {} answered", path.display()));
                true
            }
            Err(error) => {
                self.record_probe_outcome(&format!("{method} on {}: {error}", path.display()));
                false
            }
        }
    }

    fn record_probe_outcome(&self, outcome: &str) {
        self.state.lock().probe_outcome = Some(outcome.to_string());
    }

    /// Find the probe file for first-response readiness: the first
    /// file under the workspace root whose extension this entry
    /// serves. The walk is deterministic (directory entries are
    /// sorted before use, so a fixed tree always yields the same
    /// file), skips dot-named entries, and is bounded by
    /// `PROBE_SCAN_MAX_DEPTH` / `PROBE_SCAN_MAX_ENTRIES`.
    fn find_probe_file(&self) -> Option<PathBuf> {
        let mut stack: Vec<(PathBuf, usize)> = vec![(self.workspace_root.clone(), 0)];
        let mut scanned: usize = 0;
        while let Some((dir, depth)) = stack.pop() {
            let mut entries: Vec<PathBuf> = Vec::new();
            let read = std::fs::read_dir(&dir).ok()?;
            for entry in read {
                let Ok(entry) = entry else { continue };
                entries.push(entry.path());
            }
            entries.sort();
            let mut subdirs: Vec<PathBuf> = Vec::new();
            for path in entries {
                scanned += 1;
                if scanned > PROBE_SCAN_MAX_ENTRIES {
                    return None;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    if depth < PROBE_SCAN_MAX_DEPTH {
                        subdirs.push(path);
                    }
                } else if path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| self.entry.extensions.iter().any(|e| e == ext))
                {
                    return Some(path);
                }
            }
            // Push reversed so the lexicographically first subdir
            // is popped first: a depth-first walk in name order.
            for subdir in subdirs.into_iter().rev() {
                stack.push((subdir, depth + 1));
            }
        }
        None
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
            match guard.as_mut() {
                Some(writer) => write_message(writer, &self.entry.id, frame),
                None => Err(BridgeError::Io {
                    detail: "session stdin is closed (shutdown in progress)".to_string(),
                }),
            }
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
                self.state.note_sync(&uri);
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
                self.state.note_sync(&uri);
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
                self.state.note_sync(&uri);
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
    /// settle predicate first (SPEC section 6 as revisited by P4
    /// item 2): stability AND post-sync evidence — see
    /// `wait_diagnostics_settled`.
    pub fn diagnostics(
        &self,
        config: &Config,
        only_uri: Option<&str>,
    ) -> BridgeResult<Vec<Diagnostic>> {
        self.wait_diagnostics_settled(config, only_uri)?;
        Ok(self.collect_diagnostics(only_uri))
    }

    /// The push accumulation for an advisory consumer that feeds it
    /// back to the server itself (the code-action request context).
    /// NOT the settling read: the full evidence predicate would
    /// park this for a whole ready timeout — and fail the call — on
    /// any server whose evidence arrives late or never, where v1
    /// served the call fine (P4 item 2 judgment call). Instead this
    /// waits only for reader-thread catch-up: the diagnostics
    /// generation stable across two polls, bounded by
    /// CONTEXT_DRAIN_MAX_MS, and it never fails — a server that
    /// stays silent yields the (empty) accumulation as it stands,
    /// which the server itself can only improve on.
    pub fn diagnostics_context(&self, only_uri: Option<&str>) -> Vec<Diagnostic> {
        let deadline = Instant::now() + Duration::from_millis(CONTEXT_DRAIN_MAX_MS);
        let mut last_generation: Option<u64> = None;
        loop {
            if matches!(self.state.phase(), Phase::Dead(_)) {
                break;
            }
            let generation = self.state.lock().diagnostics_generation;
            if last_generation == Some(generation) {
                break;
            }
            last_generation = Some(generation);
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(SETTLE_POLL_MS));
        }
        self.collect_diagnostics(only_uri)
    }

    fn collect_diagnostics(&self, only_uri: Option<&str>) -> Vec<Diagnostic> {
        let inner = self.state.lock();
        let mut collected: Vec<Diagnostic> = Vec::new();
        for (uri, diagnostics) in inner.diagnostics.iter() {
            if only_uri.is_some_and(|only| uri != only) {
                continue;
            }
            collected.extend(diagnostics.iter().cloned());
        }
        collected
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

    /// Mint an action handle for one listed code action's shaped
    /// edit set (P5 item 3). The id is an opaque per-session
    /// counter; the edit set and its listing-time file hashes stay
    /// server-side and are never rendered into the listing.
    pub fn mint_action_handle(
        &self,
        edits: Vec<FileEdits>,
        file_hashes: Vec<(String, Option<String>)>,
    ) -> String {
        let mut inner = self.state.lock();
        inner.next_handle_seq += 1;
        let id = format!("act-{}", inner.next_handle_seq);
        inner.action_handles.insert(
            id.clone(),
            StoredAction {
                edits,
                file_hashes,
                used: false,
            },
        );
        id
    }

    /// Look up a handle without consuming it.
    pub fn action_handle_lookup(&self, id: &str) -> ActionHandleLookup {
        let inner = self.state.lock();
        match inner.action_handles.get(id) {
            None => ActionHandleLookup::Missing,
            Some(action) if action.used => ActionHandleLookup::Used,
            Some(action) => ActionHandleLookup::Valid(action.clone()),
        }
    }

    /// Spend a handle's single use. Called only after the gate
    /// allows the apply; the use stays spent even if the applier
    /// then refuses (a consumed handle never retries blindly).
    pub fn consume_action_handle(&self, id: &str) {
        let mut inner = self.state.lock();
        if let Some(action) = inner.action_handles.get_mut(id) {
            action.used = true;
        }
    }

    /// Record one gate decision in the session transcript's gate
    /// log (P5 item 2). Metadata only: tool, outcome, reason.
    pub fn record_gate_decision(&self, tool: &str, decision: &str, reason: &str) {
        let mut inner = self.state.lock();
        if inner.gate_log.len() >= GATE_LOG_MAX {
            inner.gate_log.remove(0);
        }
        inner.gate_log.push(GateLogEntry {
            tool: tool.to_string(),
            decision: decision.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Whether the server's advertised rename provider declares
    /// `prepareSupport` (it answers `textDocument/prepareRename`).
    /// Read from the captured capabilities, never probed.
    pub fn rename_prepare_supported(&self) -> bool {
        self.state
            .lock()
            .server_capabilities
            .get("renameProvider")
            .and_then(|provider| provider.get("prepareSupport"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
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

    /// The settle predicate (SPEC section 6, revisited in P4
    /// item 2). The v1 predicate — no in-flight progress and a
    /// diagnostics generation stable across two polls — fired at
    /// generation 0 for lua-language-server, whose first real
    /// publish lands ~1.6 s after didOpen (P2-REPORT Finding 3;
    /// timings measured again for P4). Stability alone cannot
    /// distinguish "the server has answered" from "the server has
    /// not answered yet", so the predicate is now stability AND
    /// evidence, where evidence is protocol-shaped only (no server
    /// names anywhere in this file's logic):
    ///
    /// - a `publishDiagnostics` for the scoped document (or any
    ///   document, unscoped) that postdates that document's last
    ///   sync, or
    /// - a `$/progress` completion that postdates the last sync, or
    /// - a `serverStatus` quiescent=true report that postdates the
    ///   last sync and follows observed activity, or
    /// - an advertised pull channel (`diagnosticProvider`): the
    ///   caller's synchronous pull immediately after this wait IS
    ///   the evidence channel for such servers (rust-analyzer
    ///   publishes nothing at all for a clean file), and the wait
    ///   then only drains in-flight push traffic.
    ///
    /// The vacuous case: no document was ever synced in this
    /// session, so no publish is owed and stability alone settles.
    /// A server offering none of these surfaces never settles and
    /// the call fails with the typed `ServerNotReady` at the ready
    /// deadline — a loud error in place of a confident empty set.
    fn wait_diagnostics_settled(
        &self,
        config: &Config,
        only_uri: Option<&str>,
    ) -> BridgeResult<()> {
        let deadline = Instant::now() + Duration::from_millis(config.ready_timeout_ms);
        let pull_advertised = self.supports_pull_diagnostics();
        let mut last_generation: Option<u64> = None;
        loop {
            if let Phase::Dead(detail) = self.state.phase() {
                return Err(BridgeError::ServerDied {
                    server: self.entry.id.clone(),
                    detail,
                });
            }
            let (generation, in_flight, evidence) = {
                let inner = self.state.lock();
                (
                    inner.diagnostics_generation,
                    inner.progress_tokens.len(),
                    settle_evidence(&inner, only_uri, pull_advertised),
                )
            };
            if in_flight == 0 && last_generation == Some(generation) && evidence {
                return Ok(());
            }
            last_generation = Some(generation);
            if Instant::now() >= deadline {
                let detail = if evidence {
                    "diagnostics generation did not stabilize within the ready deadline".to_string()
                } else {
                    "no diagnostics evidence arrived after the last document sync \
                     within the ready deadline"
                        .to_string()
                };
                return Err(BridgeError::ServerNotReady {
                    server: self.entry.id.clone(),
                    detail,
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
            "progress_cycles_completed": inner.progress_completed,
            "protocol_violations": inner.protocol_violations,
            "mutation": config.mutation.as_str(),
            "mutation_allowlist_size": config.mutation_allowlist.len(),
            "gate_decisions": inner.gate_log.iter().map(|entry| serde_json::json!({
                "tool": entry.tool, "decision": entry.decision, "reason": entry.reason
            })).collect::<Vec<_>>(),
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

    /// Tree-root pid and liveness for `session_status`. Since P4
    /// the pid is the session SUPERVISOR's — the owned root of the
    /// server tree — not the language server's own pid.
    pub fn pid_and_liveness(&self) -> (u32, bool) {
        let guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
        (guard.id(), !self.state.reaped.load(Ordering::SeqCst))
    }

    /// Close the supervisor's stdin. Dropping the handle sends EOF
    /// down the pipe; the supervisor treats EOF as a shutdown
    /// trigger and kills the whole tree once its own grace expires
    /// (P4 item 1).
    fn close_stdin(&self) {
        let mut guard = self.stdin.lock().unwrap_or_else(|p| p.into_inner());
        *guard = None;
    }

    /// Graceful shutdown (SPEC §3.6): LSP `shutdown` + `exit` with a
    /// grace window; if the tree survives that, close stdin so the
    /// supervisor escalates to a whole-tree kill after its grace;
    /// `Child::kill` on the supervisor remains only as the last
    /// resort for a wedged supervisor (documented residue class in
    /// the README's section 3.6 statement).
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
            self.close_stdin();
            let deadline =
                Instant::now() + Duration::from_millis(grace_ms + EOF_ESCALATION_SLACK_MS);
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

/// The evidence half of the settle predicate (P4 item 2), computed
/// under the state lock. See `Session::wait_diagnostics_settled` for
/// the reasoning per branch.
fn settle_evidence(inner: &Inner, only_uri: Option<&str>, pull_advertised: bool) -> bool {
    if pull_advertised {
        return true;
    }
    if inner.last_sync_seq == 0 {
        // Nothing was ever presented to the server, so no publish
        // is owed and the evidence requirement is vacuous.
        return true;
    }
    if inner.last_progress_end_seq > inner.last_sync_seq {
        return true;
    }
    if inner.saw_activity && inner.last_quiescent_seq > inner.last_sync_seq {
        return true;
    }
    match only_uri {
        Some(uri) => {
            let synced = inner.doc_sync_seq.get(uri).copied().unwrap_or(0);
            let published = inner.doc_publish_seq.get(uri).copied().unwrap_or(0);
            synced > 0 && published > synced
        }
        None => inner.last_publish_seq > inner.last_sync_seq,
    }
}

/// Owns all sessions for one bridge process.
pub struct SessionManager {
    config: Arc<Config>,
    sessions: Mutex<HashMap<(PathBuf, String), Arc<Session>>>,
    started: Instant,
    /// The connected client's identity: the `clientInfo.name` from
    /// the MCP `initialize` handshake (P5 item 2). The bridge
    /// serves one client per process; `None` until a handshake
    /// names one, and a handshake without `clientInfo` leaves it
    /// `None` — under the gated mutation posture that means no
    /// mutation tools, fail closed. This is a routing label the
    /// client asserts about itself inside the MCP host's process
    /// boundary (SPEC §8's auth model), not an authentication.
    client_identity: Mutex<Option<String>>,
}

impl SessionManager {
    /// Create an empty manager bound to a validated config.
    pub fn new(config: Arc<Config>) -> SessionManager {
        SessionManager {
            config,
            sessions: Mutex::new(HashMap::new()),
            started: Instant::now(),
            client_identity: Mutex::new(None),
        }
    }

    /// The bound config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Record the client identity from the MCP handshake. An empty
    /// or whitespace name is no identity at all (fail closed).
    pub fn set_client_identity(&self, name: Option<String>) {
        let cleaned = name.filter(|value| !value.trim().is_empty());
        *self
            .client_identity
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = cleaned;
    }

    /// The connected client's asserted identity, if any.
    pub fn client_identity(&self) -> Option<String> {
        self.client_identity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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
/// actual version string is the trimmed stdout, falling back to
/// the trimmed stderr when stdout is empty: several real servers
/// (serve-d is one) print their version text on stderr only
/// (SPEC §4.1).
fn probe_version(entry: &ServerEntry) -> BridgeResult<String> {
    let mut command = Command::new(&entry.binary);
    command.args(&entry.version_probe);
    command.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    if String::from_utf8_lossy(&output).trim().is_empty() {
        // The serve-d case: the version text exists, but on stderr.
        // The probe's contract is the version text the binary
        // prints, not the stream it prints it on; stdout stays
        // authoritative whenever it carries anything.
        let mut err_output = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            let mut limited = Read::take(&mut stderr, (VERSION_PROBE_MAX_BYTES + 1) as u64);
            limited
                .read_to_end(&mut err_output)
                .map_err(|e| io_error("reading version probe stderr", e))?;
        }
        if err_output.len() > VERSION_PROBE_MAX_BYTES {
            return Err(BridgeError::ServerNotReady {
                server: entry.id.clone(),
                detail: "version probe output exceeds its byte cap".to_string(),
            });
        }
        if !String::from_utf8_lossy(&err_output).trim().is_empty() {
            output = err_output;
        }
    }
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

/// Spawn one session's process tree under the estate's
/// orphan-proof discipline (SPEC §3.1 as extended by §3.6/P4): the
/// bridge re-executes itself in supervisor mode
/// (`crate::supervisor`) as the direct child, in a fresh process
/// group led by the supervisor, and the supervisor spawns the server
/// — still wrapped in `setpriv --pdeathsig SIGKILL`, now binding the
/// server to the supervisor — inside that group. The returned pid is
/// the SUPERVISOR's: it is the owned root of the tree, and
/// session_status reports it as such. The server inherits the
/// scrubbed environment unchanged through the supervisor.
fn spawn_child(
    entry: &ServerEntry,
    workspace_root: &Path,
    shutdown_grace_ms: u64,
) -> BridgeResult<(Child, u32)> {
    let exe = std::env::current_exe()
        .map_err(|e| io_error("resolving the bridge executable for supervision", e))?;
    let bridge_start = crate::supervisor::self_starttime_ticks().unwrap_or(0);
    let mut command = Command::new(&exe);
    command
        .arg(crate::supervisor::SUPERVISE_FLAG)
        .arg("--bridge-pid")
        .arg(std::process::id().to_string())
        .arg("--bridge-start")
        .arg(bridge_start.to_string())
        .arg("--grace-ms")
        .arg(shutdown_grace_ms.to_string())
        .arg("--")
        .arg(SETPRIV_PATH)
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
        .map_err(|e| io_error("spawning session supervisor", e))?;
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
    stdin: Arc<Mutex<Option<ChildStdin>>>,
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
            inner.event_seq += 1;
            let seq = inner.event_seq;
            inner.doc_publish_seq.insert(uri.to_string(), seq);
            inner.last_publish_seq = seq;
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
                    inner.event_seq += 1;
                    inner.last_progress_end_seq = inner.event_seq;
                    inner.progress_completed += 1;
                }
                _ => {}
            }
        }
        "experimental/serverStatus" => {
            if let Some(quiescent) = params.get("quiescent").and_then(serde_json::Value::as_bool) {
                let mut inner = state.lock();
                inner.server_status_quiescent = Some(quiescent);
                if quiescent {
                    inner.event_seq += 1;
                    inner.last_quiescent_seq = inner.event_seq;
                } else {
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
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
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
    if let Some(writer) = guard.as_mut() {
        let _ = write_message(writer, &entry.id, &reply);
    }
}
