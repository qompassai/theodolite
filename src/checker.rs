//! The checker manager (checker design §3): version pinning,
//! per-document single-flight runs with debounce and
//! supersession, a content-addressed result cache with depfile
//! revalidation, and the one-shot process runner. Checker children
//! are direct execs — never the session supervisor (design §10.1)
//! — and no checker state enters `Session`.
//!
//! Concurrency shape: the first caller for a (checker, workspace,
//! document, content hash) becomes the run's leader and executes
//! it on its own call stack; same-hash callers wait on the run's
//! outcome. A different hash supersedes: it cancels the in-flight
//! run, and the newest caller leads a replacement after a debounce
//! window. A superseded caller receives the typed
//! `CheckerSuperseded` — never stale diagnostics, never the newest
//! run's set. At most [`MAX_CONCURRENT_RUNS`] checker processes
//! run per bridge process.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checker_glslang::{self, KnownSource, ParseInput};
use crate::config::{CheckerEntry, sha256_hex};
use crate::errors::{BridgeError, BridgeResult, io_error};
use crate::session::{VersionProbeFailure, run_version_probe};
use crate::shaping::{Diagnostic, path_to_uri};

/// Global checker concurrency cap for v1 (design §3.2).
pub const MAX_CONCURRENT_RUNS: usize = 2;

/// Dependency bounds for cache revalidation (design §3.4): at
/// most this many depfile files, each hashed only within the
/// run's input bound.
const MAX_DEPENDENCY_FILES: usize = 256;

/// Poll interval for the runner and the coordination loops, in
/// the codebase's 10–50 ms poll discipline.
const POLL: Duration = Duration::from_millis(10);

/// Grace for reaping a killed checker child, matching the
/// bridge's shutdown-grace order of magnitude.
const KILL_REAP_GRACE: Duration = Duration::from_millis(2_000);

/// Upper bound for re-hashing a cached dependency: the largest
/// `max_input_bytes` a stanza may declare.
const MAX_DEPENDENCY_HASH_BYTES: u64 = 16_777_216;

/// One document's identity in the run registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DocKey {
    checker: String,
    workspace: PathBuf,
    uri: String,
}

/// The freshness key of a completed run (design §3.4). A cached
/// result is served only for an identical key, and only after
/// every dependency has been re-hashed to its recorded value.
#[derive(Debug, Clone)]
struct RunKey {
    config_hash: String,
    tool_version: String,
    input_sha256: String,
    stage: String,
    project_root: PathBuf,
    dependencies: Vec<(PathBuf, String)>,
    dependencies_complete: bool,
}

/// The run-record family `session_status` reports for a checker
/// (design §3.5): the transcript of what actually ran.
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// Bridge-unique run number.
    pub run_id: u64,
    /// `completed`, `failed`, `timeout`, or `superseded`.
    pub state: &'static str,
    /// The tool's exit code, when it exited (not on a kill).
    pub exit_code: Option<i32>,
    /// SHA-256 of the exact bytes fed to the tool on stdin.
    pub input_sha256: String,
    /// The tool version the probe actually read.
    pub tool_version_actual: String,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
    /// Workspace-relative dependency paths from the depfile.
    pub dependencies: Vec<String>,
    /// Whether the depfile evidence was complete (a run without
    /// it is never served from the cache).
    pub dependencies_complete: bool,
}

impl RunRecord {
    /// The record as it appears in tool envelopes.
    pub fn to_json(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "state": self.state,
            "exit_code": self.exit_code,
            "input_sha256": self.input_sha256,
            "tool_version_actual": self.tool_version_actual,
            "duration_ms": self.duration_ms,
            "dependencies": self.dependencies,
            "dependencies_complete": self.dependencies_complete,
        })
    }
}

/// A finished run: its key, its record, and its diagnostics.
#[derive(Debug, Clone)]
struct CompletedRun {
    key: RunKey,
    record: RunRecord,
    diagnostics: Vec<Diagnostic>,
}

/// An in-flight run: followers wait on `outcome`; supersession
/// sets `cancel`, which the leader's runner polls.
struct ActiveRun {
    hash: String,
    cancel: AtomicBool,
    outcome: Mutex<Option<BridgeResult<CompletedRun>>>,
    finished: Condvar,
}

impl ActiveRun {
    fn new(hash: String) -> Arc<Self> {
        Arc::new(Self {
            hash,
            cancel: AtomicBool::new(false),
            outcome: Mutex::new(None),
            finished: Condvar::new(),
        })
    }

    fn finish(&self, outcome: BridgeResult<CompletedRun>) {
        *self.outcome.lock().unwrap() = Some(outcome);
        self.finished.notify_all();
    }

    fn wait_outcome(&self) -> BridgeResult<CompletedRun> {
        let mut guard = self.outcome.lock().unwrap();
        loop {
            if let Some(outcome) = &*guard {
                return outcome.clone();
            }
            guard = self.finished.wait(guard).unwrap();
        }
    }
}

/// Per-document coordination state.
#[derive(Default)]
struct DocState {
    active: Option<Arc<ActiveRun>>,
    completed: Option<CompletedRun>,
    /// The newest content hash that superseded an in-flight run,
    /// and when it arrived (the debounce clock).
    newest_hash: Option<String>,
    newest_since: Option<Instant>,
}

/// The bridge-wide counting semaphore for checker runs.
struct RunSlots {
    in_use: Mutex<usize>,
    freed: Condvar,
}

impl RunSlots {
    /// Take a slot, polling the run's cancel flag while waiting:
    /// a superseded leader-to-be gives up its claim instead of
    /// queueing a run nobody wants anymore.
    fn acquire(&self, cancel: &AtomicBool) -> bool {
        let mut guard = self.in_use.lock().unwrap();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            if *guard < MAX_CONCURRENT_RUNS {
                *guard += 1;
                return true;
            }
            let (next, _) = self
                .freed
                .wait_timeout(guard, POLL)
                .unwrap();
            guard = next;
        }
    }

    fn release(&self) {
        let mut guard = self.in_use.lock().unwrap();
        *guard = guard.saturating_sub(1);
        self.freed.notify_all();
    }
}

/// Owns every checker concern for one bridge process: the
/// version-probe cache, the run registry, the result cache, and
/// the run-record transcript.
pub struct CheckerManager {
    config_hash: String,
    versions: Mutex<HashMap<String, String>>,
    docs: Mutex<HashMap<DocKey, Arc<Mutex<DocState>>>>,
    records: Mutex<HashMap<DocKey, RunRecord>>,
    next_run_id: AtomicU64,
    slots: RunSlots,
}

impl CheckerManager {
    /// Create the manager for one loaded configuration.
    pub fn new(config_hash: &str) -> Self {
        Self {
            config_hash: config_hash.to_string(),
            versions: Mutex::new(HashMap::new()),
            docs: Mutex::new(HashMap::new()),
            records: Mutex::new(HashMap::new()),
            next_run_id: AtomicU64::new(1),
            slots: RunSlots {
                in_use: Mutex::new(0),
                freed: Condvar::new(),
            },
        }
    }

    /// The status transcript for one checker in one workspace:
    /// the pinned tool version (once probed) and the last run
    /// record per document.
    pub fn status_snapshot(&self, checker_id: &str, workspace: &Path) -> (Option<String>, Vec<RunRecord>) {
        let version = self
            .versions
            .lock()
            .unwrap()
            .get(checker_id)
            .cloned();
        let mut runs: Vec<RunRecord> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.checker == checker_id && key.workspace == workspace)
            .map(|(_, record)| record.clone())
            .collect();
        runs.sort_by_key(|record| record.run_id);
        (version, runs)
    }

    /// Probe and pin the checker's tool version (design §6): the
    /// probe runs once per (checker, configuration); a mismatch
    /// is `CheckerVersionMismatch` before any document run. Probe
    /// failures are not cached — a fixed PATH or a finished
    /// install is picked up on the next call.
    fn tool_version(&self, entry: &CheckerEntry) -> BridgeResult<String> {
        if let Some(version) = self.versions.lock().unwrap().get(&entry.id) {
            return Ok(version.clone());
        }
        let actual = run_version_probe(&entry.binary, &entry.version_probe).map_err(|failure| {
            let detail = match failure {
                VersionProbeFailure::Io(detail) => detail,
                VersionProbeFailure::Timeout => {
                    "version probe exceeded its deadline".to_string()
                }
                VersionProbeFailure::NonZeroExit(status) => {
                    format!("version probe exited with {status}")
                }
                VersionProbeFailure::OutputTooLarge => {
                    "version probe output exceeds its byte cap".to_string()
                }
            };
            BridgeError::CheckerUnavailable {
                checker: entry.id.clone(),
                detail,
            }
        })?;
        if actual != entry.expect_version {
            return Err(BridgeError::CheckerVersionMismatch {
                checker: entry.id.clone(),
                expected: entry.expect_version.clone(),
                actual,
            });
        }
        self.versions
            .lock()
            .unwrap()
            .insert(entry.id.clone(), actual.clone());
        Ok(actual)
    }

    /// Run (or reuse) the checker for one document and return its
    /// diagnostics with the run record. `bytes` are the document's
    /// exact current bytes; their SHA-256 is the run's identity.
    pub fn diagnostics(
        &self,
        entry: &CheckerEntry,
        workspace: &Path,
        target: &Path,
        bytes: &[u8],
    ) -> BridgeResult<(Vec<Diagnostic>, RunRecord)> {
        if bytes.len() as u64 > entry.max_input_bytes {
            return Err(BridgeError::CheckerInputTooLarge {
                checker: entry.id.clone(),
                size: bytes.len() as u64,
                limit: entry.max_input_bytes,
            });
        }
        let version = self.tool_version(entry)?;
        let stage = checker_glslang::derive_stage(entry, target)?;
        let project_root = discover_project_root(entry, workspace, target);
        let input_sha256 = sha256_hex(bytes);
        let key = DocKey {
            checker: entry.id.clone(),
            workspace: workspace.to_path_buf(),
            uri: path_to_uri(target),
        };
        let doc = {
            let mut docs = self.docs.lock().unwrap();
            docs.entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(DocState::default())))
                .clone()
        };
        loop {
            enum Step {
                Serve(CompletedRun),
                Follow(Arc<ActiveRun>),
                Supersede(Arc<ActiveRun>),
                Debounce,
                Lead(Arc<ActiveRun>),
            }
            let step = {
                let mut state = doc.lock().unwrap();
                if let Some(completed) = &state.completed
                    && completed.key.config_hash == self.config_hash
                    && completed.key.tool_version == version
                    && completed.key.input_sha256 == input_sha256
                    && completed.key.stage == stage
                    && completed.key.project_root == project_root
                    && completed.key.dependencies_complete
                {
                    Step::Serve(completed.clone())
                } else if let Some(active) = state.active.clone() {
                    if active.hash == input_sha256 {
                        Step::Follow(active)
                    } else {
                        // A different hash supersedes the in-flight
                        // run; this caller becomes the newest claim.
                        active.cancel.store(true, Ordering::SeqCst);
                        state.newest_hash = Some(input_sha256.clone());
                        state.newest_since = Some(Instant::now());
                        Step::Supersede(active)
                    }
                } else if state.newest_hash.as_deref() == Some(input_sha256.as_str()) {
                    // A supersession is debouncing: wait out the
                    // window unless a newer hash replaces this one.
                    let since = state.newest_since.unwrap_or_else(Instant::now);
                    if since.elapsed() < Duration::from_millis(entry.debounce_ms) {
                        Step::Debounce
                    } else {
                        state.newest_hash = None;
                        state.newest_since = None;
                        let active = ActiveRun::new(input_sha256.clone());
                        state.active = Some(active.clone());
                        Step::Lead(active)
                    }
                } else if state.newest_hash.is_some() {
                    // A newer hash than this caller's is debouncing;
                    // this caller was itself superseded in line.
                    return Err(BridgeError::CheckerSuperseded {
                        checker: entry.id.clone(),
                        detail: "a newer document version superseded this call before \
                                 its run started"
                            .to_string(),
                    });
                } else {
                    let active = ActiveRun::new(input_sha256.clone());
                    state.active = Some(active.clone());
                    Step::Lead(active)
                }
            };
            match step {
                Step::Serve(completed) => {
                    if dependencies_fresh(&completed.key) {
                        return Ok((completed.diagnostics.clone(), completed.record.clone()));
                    }
                    // A dependency changed: the entry is void.
                    // Clear it if it is still the cached one, then
                    // fall through to a fresh run.
                    let mut state = doc.lock().unwrap();
                    if state
                        .completed
                        .as_ref()
                        .is_some_and(|c| c.record.run_id == completed.record.run_id)
                    {
                        state.completed = None;
                    }
                }
                Step::Follow(active) => {
                    let completed = active.wait_outcome()?;
                    return Ok((completed.diagnostics, completed.record));
                }
                Step::Supersede(active) => {
                    // The run's outcome is supersession for its own
                    // callers; this caller only needs it reaped
                    // before leading the replacement.
                    let _ = active.wait_outcome();
                    let mut state = doc.lock().unwrap();
                    if state
                        .active
                        .as_ref()
                        .is_some_and(|a| Arc::ptr_eq(a, &active))
                    {
                        state.active = None;
                    }
                }
                Step::Debounce => {
                    std::thread::sleep(POLL);
                }
                Step::Lead(active) => {
                    let outcome = self.execute(
                        entry, workspace, target, bytes, &stage, &project_root, &version, &active,
                    );
                    let record = match &outcome {
                        Ok(completed) => completed.record.clone(),
                        Err(error) => self.failure_record(error, &input_sha256, &version),
                    };
                    self.records
                        .lock()
                        .unwrap()
                        .insert(key.clone(), record);
                    {
                        let mut state = doc.lock().unwrap();
                        if let Ok(completed) = &outcome {
                            state.completed = Some(completed.clone());
                        }
                        if state
                            .active
                            .as_ref()
                            .is_some_and(|a| Arc::ptr_eq(a, &active))
                        {
                            state.active = None;
                        }
                    }
                    active.finish(outcome.clone());
                    let completed = outcome?;
                    return Ok((completed.diagnostics, completed.record));
                }
            }
        }
    }

    /// A run record for a failed run, so `session_status` shows
    /// the failure honestly instead of the last success.
    fn failure_record(
        &self,
        error: &BridgeError,
        input_sha256: &str,
        version: &str,
    ) -> RunRecord {
        let state = match error {
            BridgeError::CheckerTimeout { .. } => "timeout",
            BridgeError::CheckerSuperseded { .. } => "superseded",
            _ => "failed",
        };
        RunRecord {
            run_id: self.next_run_id.fetch_add(1, Ordering::SeqCst),
            state,
            exit_code: None,
            input_sha256: input_sha256.to_string(),
            tool_version_actual: version.to_string(),
            duration_ms: 0,
            dependencies: Vec::new(),
            dependencies_complete: false,
        }
    }

    /// Execute one run as its leader: take a global slot, spawn
    /// the tool, feed it the exact bytes, enforce the bounds, and
    /// parse the result through the stanza's profile.
    #[allow(clippy::too_many_arguments)]
    fn execute(
        &self,
        entry: &CheckerEntry,
        workspace: &Path,
        target: &Path,
        bytes: &[u8],
        stage: &str,
        project_root: &Path,
        version: &str,
        active: &Arc<ActiveRun>,
    ) -> BridgeResult<CompletedRun> {
        let run_id = self.next_run_id.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        if !self.slots.acquire(&active.cancel) {
            return Err(BridgeError::CheckerSuperseded {
                checker: entry.id.clone(),
                detail: "superseded while waiting for a checker run slot".to_string(),
            });
        }
        let result = self.execute_inner(
            entry,
            workspace,
            target,
            bytes,
            stage,
            project_root,
            version,
            run_id,
            started,
            active,
        );
        self.slots.release();
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_inner(
        &self,
        entry: &CheckerEntry,
        workspace: &Path,
        target: &Path,
        bytes: &[u8],
        stage: &str,
        project_root: &Path,
        version: &str,
        run_id: u64,
        started: Instant,
        active: &Arc<ActiveRun>,
    ) -> BridgeResult<CompletedRun> {
        let superseded = || BridgeError::CheckerSuperseded {
            checker: entry.id.clone(),
            detail: "a newer document version superseded this run".to_string(),
        };
        // Per-run temp paths for the tool's declared outputs; the
        // directory is deleted when the run record is built
        // (design §3.4).
        let temp_dir = std::env::temp_dir().join(format!(
            "theodolite-checker-run-{}-{run_id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| io_error("creating the checker run directory", e))?;
        let output_path = temp_dir.join("output.spv");
        let depfile_path = temp_dir.join("deps.d");
        let finish = |result: BridgeResult<CompletedRun>| {
            let _ = std::fs::remove_dir_all(&temp_dir);
            result
        };

        // Whole-element placeholder substitution (design §2.2).
        let relpath = match target.strip_prefix(project_root) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => {
                return finish(Err(BridgeError::CheckerProjectRoot {
                    checker: entry.id.clone(),
                    detail: format!(
                        "target {} is not under its project root {}",
                        target.display(),
                        project_root.display()
                    ),
                }));
            }
        };
        let file_dir = target
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let substitute = |element: &str| -> String {
            match element {
                "{path}" => target.to_string_lossy().into_owned(),
                "{relpath}" => relpath.clone(),
                "{stage}" => stage.to_string(),
                "{file_dir}" => file_dir.clone(),
                "{project_root}" => project_root.to_string_lossy().into_owned(),
                "{output}" => output_path.to_string_lossy().into_owned(),
                "{depfile}" => depfile_path.to_string_lossy().into_owned(),
                other => other.to_string(),
            }
        };
        let argv: Vec<String> = entry.command[1..].iter().map(|e| substitute(e)).collect();

        let mut command = Command::new(&entry.binary);
        command
            .args(&argv)
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        if let Some(home) = std::env::var_os("HOME") {
            command.env("HOME", home);
        }
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        command.env("LANG", "C.UTF-8");
        command.env("LC_ALL", "C.UTF-8");
        for (key, value) in &entry.env {
            command.env(key, value);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return finish(Err(BridgeError::CheckerUnavailable {
                    checker: entry.id.clone(),
                    detail: format!("spawning checker {}: {e}", entry.binary.display()),
                }));
            }
        };
        let child_pid = child.id();

        // Feed the exact hashed bytes on a writer thread: a tool
        // that fills its output pipes must never deadlock against
        // a bridge still writing its input.
        let mut stdin = child.stdin.take().expect("checker stdin is piped");
        let input = bytes.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
            let _ = stdin.flush();
        });
        // Drain both output streams under one combined byte cap.
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let captured_err = Arc::new(Mutex::new(Vec::<u8>::new()));
        let total = Arc::new(AtomicUsize::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let cap = entry.max_output_bytes as usize;
        let out_thread = drain_stream(
            child.stdout.take(),
            captured.clone(),
            total.clone(),
            overflow.clone(),
            cap,
        );
        let err_thread = drain_stream(
            child.stderr.take(),
            captured_err.clone(),
            total.clone(),
            overflow.clone(),
            cap,
        );

        // Wait with the run's three cancellation sources: its
        // deadline, supersession, and the output cap. Kills are by
        // the recorded PID, never by name (design §3.3).
        let deadline = started + Duration::from_millis(entry.timeout_ms);
        let status = loop {
            if active.cancel.load(Ordering::SeqCst) {
                kill_and_reap(&mut child, child_pid);
                let _ = writer.join();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return finish(Err(superseded()));
            }
            if overflow.load(Ordering::SeqCst) {
                kill_and_reap(&mut child, child_pid);
                let _ = writer.join();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return finish(Err(BridgeError::CheckerOutputTooLarge {
                    checker: entry.id.clone(),
                    detail: format!(
                        "checker output exceeded the max_output_bytes limit of {} \
                         (captured at the cap); the run was killed at the cap",
                        entry.max_output_bytes
                    ),
                }));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        kill_and_reap(&mut child, child_pid);
                        let _ = writer.join();
                        let _ = out_thread.join();
                        let _ = err_thread.join();
                        return finish(Err(BridgeError::CheckerTimeout {
                            checker: entry.id.clone(),
                            timeout_ms: entry.timeout_ms,
                        }));
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => {
                    let _ = writer.join();
                    let _ = out_thread.join();
                    let _ = err_thread.join();
                    return finish(Err(io_error("waiting on the checker process", e)));
                }
            }
        };
        let _ = writer.join();
        let _ = out_thread.join();
        let _ = err_thread.join();
        let duration_ms = started.elapsed().as_millis() as u64;
        let Some(exit_code) = status.code() else {
            return finish(Err(BridgeError::CheckerFailed {
                checker: entry.id.clone(),
                detail: format!("checker process died by signal ({status})"),
            }));
        };
        let stderr_bytes = captured_err.lock().unwrap().clone();
        let stderr = match String::from_utf8(stderr_bytes) {
            Ok(text) => text,
            Err(_) => {
                return finish(Err(BridgeError::CheckerOutputInvalid {
                    checker: entry.id.clone(),
                    detail: "checker stderr is not valid UTF-8".to_string(),
                }));
            }
        };

        // Depfile evidence: the files the tool actually read. A
        // failed compile may write none — the run's own answer
        // still stands, but the entry is marked incomplete and is
        // never served from the cache.
        let mut dependencies_complete = true;
        let mut known_sources: Vec<KnownSource> = Vec::new();
        let mut key_dependencies: Vec<(PathBuf, String)> = Vec::new();
        let mut record_dependencies: Vec<String> = Vec::new();
        if depfile_path.exists() {
            let depfile_text = std::fs::read_to_string(&depfile_path).unwrap_or_default();
            let dep_paths = checker_glslang::parse_depfile(&depfile_text);
            if dep_paths.len() > MAX_DEPENDENCY_FILES {
                dependencies_complete = false;
            }
            for dep in dep_paths {
                let canonical = std::fs::canonicalize(&dep).unwrap_or_else(|_| dep.clone());
                if !canonical.starts_with(workspace) {
                    return finish(Err(BridgeError::CheckerOutputInvalid {
                        checker: entry.id.clone(),
                        detail: format!(
                            "depfile names {}, outside the workspace {}; the run is \
                             invalid",
                            canonical.display(),
                            workspace.display()
                        ),
                    }));
                }
                let relative = canonical
                    .strip_prefix(workspace)
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| canonical.to_string_lossy().into_owned());
                record_dependencies.push(relative);
                let dep_bytes = read_bounded(&canonical, entry.max_input_bytes);
                match &dep_bytes {
                    Some(bytes) if key_dependencies.len() < MAX_DEPENDENCY_FILES => {
                        key_dependencies.push((canonical.clone(), sha256_hex(bytes)));
                    }
                    _ => {
                        dependencies_complete = false;
                    }
                }
                known_sources.push(checker_glslang::known_source(&canonical, dep_bytes));
            }
        } else {
            dependencies_complete = false;
        }

        let diagnostics = match checker_glslang::parse_run(&ParseInput {
            checker_id: &entry.id,
            target_uri: &path_to_uri(target),
            target_bytes: bytes,
            sources: &known_sources,
            exit_code,
            stderr: &stderr,
            max_diagnostics: entry.max_diagnostics,
        }) {
            Ok(diagnostics) => diagnostics,
            Err(error) => return finish(Err(error)),
        };
        let record = RunRecord {
            run_id,
            state: "completed",
            exit_code: Some(exit_code),
            input_sha256: sha256_hex(bytes),
            tool_version_actual: version.to_string(),
            duration_ms,
            dependencies: record_dependencies,
            dependencies_complete,
        };
        let completed = CompletedRun {
            key: RunKey {
                config_hash: self.config_hash.clone(),
                tool_version: version.to_string(),
                input_sha256: sha256_hex(bytes),
                stage: stage.to_string(),
                project_root: project_root.to_path_buf(),
                dependencies: key_dependencies,
                dependencies_complete,
            },
            record,
            diagnostics,
        };
        finish(Ok(completed))
    }
}

/// The project root for a run (design §2.3): walk up from the
/// file's directory to the workspace root; the first directory
/// containing any root marker wins; otherwise the workspace root.
/// Marker contents are never read in v1 — presence only.
pub fn discover_project_root(entry: &CheckerEntry, workspace: &Path, target: &Path) -> PathBuf {
    let mut dir = target.parent().map(Path::to_path_buf);
    while let Some(current) = dir {
        if entry
            .root_markers
            .iter()
            .any(|marker| current.join(marker).exists())
        {
            return current;
        }
        if current == workspace || !current.starts_with(workspace) {
            return workspace.to_path_buf();
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    workspace.to_path_buf()
}

/// Whether every dependency in a cached run's key still hashes to
/// its recorded value (design §3.4). A changed, missing, or
/// unreadable dependency invalidates the entry.
fn dependencies_fresh(key: &RunKey) -> bool {
    key.dependencies.iter().all(|(path, recorded)| {
        read_bounded(path, MAX_DEPENDENCY_HASH_BYTES)
            .is_some_and(|bytes| sha256_hex(&bytes) == *recorded)
    })
}

/// Drain one child stream into `sink`, counting bytes against the
/// run's combined output cap: past the cap the stream is still
/// drained (so the child never blocks on a full pipe) but nothing
/// more is kept, and `overflow` tells the waiter to kill the run.
fn drain_stream(
    stream: Option<impl Read + Send + 'static>,
    sink: Arc<Mutex<Vec<u8>>>,
    total: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
    cap: usize,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut stream) = stream else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let seen = total.fetch_add(n, Ordering::SeqCst) + n;
                    if seen > cap {
                        overflow.store(true, Ordering::SeqCst);
                    } else {
                        sink.lock().unwrap().extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
    })
}

/// Read a file for the parser/cache, bounded: anything past the
/// bound is `None` (unavailable), never a silent truncation.
fn read_bounded(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() as u64 > max_bytes {
        return None;
    }
    Some(buffer)
}

/// Kill a checker child by its recorded PID and reap it within
/// the bridge's shutdown grace; output gathered so far is the
/// caller's to discard.
fn kill_and_reap(child: &mut std::process::Child, pid: u32) {
    debug_assert_eq!(child.id(), pid);
    let _ = child.kill();
    let deadline = Instant::now() + KILL_REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.wait();
                    return;
                }
                std::thread::sleep(POLL);
            }
        }
    }
}
