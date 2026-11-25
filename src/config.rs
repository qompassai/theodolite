//! Bridge configuration: data, not code (SPEC §5).
//!
//! Servers are declared in one TOML file. The bridge refuses to start
//! on an invalid file with a typed `ConfigInvalid` error. Adding a
//! server is adding an entry; the binary contains no per-server code
//! beyond the named readiness predicates.
//!
//! Judgment call (BUILD-REPORT): the SPEC's illustrative shape lists
//! parallel `languages`/`extensions` arrays, which cannot say which
//! language id belongs to which extension. This implementation keeps
//! those fields and adds one explicit map, `language_ids`
//! (extension -> language id), which is the routing authority. When a
//! server entry omits `language_ids`, the bridge derives it only when
//! the two arrays have equal length (positional pairing); otherwise
//! the config is invalid. Nothing is ever hardcoded in the binary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::errors::{BridgeError, BridgeResult};

/// Default per-request timeout when the config omits it (SPEC §3.7).
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
/// Default readiness timeout when the config omits it (SPEC §3.3).
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
/// Default shutdown grace when the config omits it (SPEC §3.6).
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 2_000;

/// Lower bound for a server's `no_publish_means_empty_ms` opt-in:
/// below one settle poll (250 ms) the window cannot be told apart
/// from "no publish yet", so the config refuses it instead of
/// letting it silently mean something else.
pub const NO_PUBLISH_MEANS_EMPTY_MS_MIN: u64 = 250;
/// Upper bound for the same opt-in: the window is a bounded wait
/// inside the ready deadline, not a second deadline. Two minutes
/// is far past the measured publish latency of every server whose
/// contract is "publishes promptly or never".
pub const NO_PUBLISH_MEANS_EMPTY_MS_MAX: u64 = 120_000;

/// Readiness predicate named by a server entry (SPEC §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ready as soon as the initialize handshake completes.
    Immediate,
    /// Ready when no `$/progress` work is in flight and a probe
    /// request succeeds (rust-analyzer's shape).
    Quiescent,
    /// Ready when the initialize handshake has completed and one
    /// real request against a probe file from the workspace —
    /// `textDocument/documentSymbol`, or `textDocument/hover`
    /// where symbols are not advertised — succeeds. For servers
    /// that compute immediately but never go quiescent, because
    /// their startup indexing churn outlives every readiness
    /// window (serve-d's shape).
    FirstResponse,
}

impl Readiness {
    fn parse(raw: &str) -> BridgeResult<Readiness> {
        match raw {
            "immediate" => Ok(Readiness::Immediate),
            "quiescent" => Ok(Readiness::Quiescent),
            "first-response" => Ok(Readiness::FirstResponse),
            other => Err(BridgeError::ConfigInvalid {
                detail: format!("unknown readiness predicate {other:?}"),
            }),
        }
    }
}

/// One declared language server, fully resolved and validated.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    /// Routing key used by tools (SPEC §5).
    pub id: String,
    /// Absolute path to the server binary; never PATH-resolved.
    pub binary: PathBuf,
    /// Extra argv for the server process (config data, may be empty).
    pub args: Vec<String>,
    /// argv appended to the binary to probe its version.
    pub version_probe: Vec<String>,
    /// Exact version string the probe must return (after trimming).
    pub expect_version: String,
    /// Languages this entry serves (declarative; SPEC §5 shape).
    pub languages: Vec<String>,
    /// File extensions this entry serves (no leading dot).
    pub extensions: Vec<String>,
    /// Extension -> language id, the didOpen authority (SPEC §3.5).
    pub language_ids: BTreeMap<String, String>,
    /// Named readiness predicate.
    pub readiness: Readiness,
    /// Explicit environment allowlist additions for the child.
    pub env: BTreeMap<String, String>,
    /// `initializationOptions` passed verbatim in `initialize`.
    pub initialization_options: Option<serde_json::Value>,
    /// Settings answered to `workspace/configuration` requests.
    pub settings: Option<serde_json::Value>,
    /// Whether this entry owns workspace diagnostics for parity
    /// purposes (bacon-ls in the v1 set).
    pub diagnostics_owner: bool,
    /// Opt-in legacy initialize shape: when true, the session's
    /// `initialize` params also carry `rootPath` beside `rootUri`
    /// and `workspaceFolders`, for servers that read their project
    /// root only from that deprecated field (gdshader-lsp; the
    /// rootPath lane's field, mirrored here so the two branches'
    /// config shapes compose). Default off.
    pub send_root_path: bool,
    /// Opt-in silence semantics for this server's diagnostics
    /// channel: the server publishes `publishDiagnostics` only
    /// for non-empty sets, so once this many milliseconds have
    /// passed since the last content sync with no diagnostics
    /// evidence, an absent publish IS the empty set (the nil
    /// shape, measured 2026-11-15/16: a clean file gets no
    /// notification at all, ever). `None` (the default) keeps the
    /// evidence-only settle predicate: silence is a typed
    /// `ServerNotReady` at the ready deadline, never a zero.
    pub no_publish_means_empty_ms: Option<u64>,
}

/// Mutation posture (SPEC §6): a configuration state, never a
/// runtime prompt. `Off` registers no mutation tools at all;
/// `Gated` registers them for allowlisted client identities only,
/// behind the decide()-class gate (`crate::gate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationPosture {
    /// No mutation tools, for any client.
    Off,
    /// Mutation tools for allowlisted identities, gate-decided
    /// again at every apply.
    Gated,
}

impl MutationPosture {
    /// The config-file / transcript string for this posture.
    pub fn as_str(&self) -> &'static str {
        match self {
            MutationPosture::Off => "off",
            MutationPosture::Gated => "gated",
        }
    }
}

/// Checker debounce bound (checker design §2.2): the coalescing
/// window for repeated changed-hash requests for one document.
pub const CHECKER_DEBOUNCE_MS_MAX: u64 = 5_000;
/// Checker per-run deadline bounds (design §2.2).
pub const CHECKER_TIMEOUT_MS_MIN: u64 = 100;
/// Checker per-run deadline bounds (design §2.2).
pub const CHECKER_TIMEOUT_MS_MAX: u64 = 120_000;
/// Checker input refusal bounds (design §2.2).
pub const CHECKER_MAX_INPUT_BYTES_MIN: u64 = 65_536;
/// Checker input refusal bounds (design §2.2).
pub const CHECKER_MAX_INPUT_BYTES_MAX: u64 = 16_777_216;
/// Checker combined capture bounds (design §2.2).
pub const CHECKER_MAX_OUTPUT_BYTES_MIN: u64 = 65_536;
/// Checker combined capture bounds (design §2.2).
pub const CHECKER_MAX_OUTPUT_BYTES_MAX: u64 = 4_194_304;
/// Checker diagnostic-count bound ceiling (design §2.2).
pub const CHECKER_MAX_DIAGNOSTICS_MAX: usize = 512;

/// Defaults for a checker entry's bounded fields, matching the
/// design §2.1 example stanza. The bounds are refusal limits, not
/// tuning knobs; an entry may narrow them within their ranges.
pub const DEFAULT_CHECKER_DEBOUNCE_MS: u64 = 250;
/// See `DEFAULT_CHECKER_DEBOUNCE_MS`.
pub const DEFAULT_CHECKER_TIMEOUT_MS: u64 = 15_000;
/// See `DEFAULT_CHECKER_DEBOUNCE_MS`.
pub const DEFAULT_CHECKER_MAX_INPUT_BYTES: u64 = 4_194_304;
/// See `DEFAULT_CHECKER_DEBOUNCE_MS`.
pub const DEFAULT_CHECKER_MAX_OUTPUT_BYTES: u64 = 1_048_576;
/// See `DEFAULT_CHECKER_DEBOUNCE_MS`.
pub const DEFAULT_CHECKER_MAX_DIAGNOSTICS: usize = 512;

/// A checker parser profile implemented in the bridge (design
/// §2.2). A closed, versioned set: config names a profile, it never
/// supplies a parser or a regex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckerParser {
    /// The `glslang-validator-1` profile (design §5): glslang's
    /// stdin info-log grammar, depfile handling, byte→UTF-16
    /// column conversion.
    GlslangValidator1,
}

impl CheckerParser {
    fn parse(raw: &str) -> BridgeResult<CheckerParser> {
        match raw {
            "glslang-validator-1" => Ok(CheckerParser::GlslangValidator1),
            other => Err(BridgeError::ConfigInvalid {
                detail: format!("unknown checker parser {other:?}"),
            }),
        }
    }

    /// The config-file string for this profile.
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckerParser::GlslangValidator1 => "glslang-validator-1",
        }
    }
}

/// A checker's diagnostics authority (design §4.1). There is no
/// default: an entry that omits it does not parse, because the
/// ownership posture must never be ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckerAuthority {
    /// The gate diagnostics source for its extensions; refused
    /// when an owner server or another owner checker claims one.
    Owner,
    /// May overlap an LSP owner; runs only when explicitly named,
    /// never merged with the owner's set.
    CrossCheck,
}

impl CheckerAuthority {
    fn parse(raw: &str) -> BridgeResult<CheckerAuthority> {
        match raw {
            "owner" => Ok(CheckerAuthority::Owner),
            "cross-check" => Ok(CheckerAuthority::CrossCheck),
            other => Err(BridgeError::ConfigInvalid {
                detail: format!(
                    "unknown checker authority {other:?} (expected \"owner\" or \"cross-check\")"
                ),
            }),
        }
    }

    /// The config-file / result-envelope string for this authority.
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckerAuthority::Owner => "owner",
            CheckerAuthority::CrossCheck => "cross-check",
        }
    }
}

/// When a checker runs (design §3.1). v1 admits exactly one value:
/// the bridge learns about a document when a tool call syncs it,
/// so "on open" and "on change" are the same observable event and
/// there is no distinct on-save trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckerTrigger {
    /// Run when a `diagnostics` call presents a new content hash.
    OnSync,
}

impl CheckerTrigger {
    fn parse(raw: &str) -> BridgeResult<CheckerTrigger> {
        match raw {
            "on-sync" => Ok(CheckerTrigger::OnSync),
            other => Err(BridgeError::ConfigInvalid {
                detail: format!(
                    "unknown checker trigger {other:?}; v1 admits exactly \"on-sync\""
                ),
            }),
        }
    }

    /// The config-file string for this trigger.
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckerTrigger::OnSync => "on-sync",
        }
    }
}

/// One declared checker, fully resolved and validated (design §2).
/// A checker is not an LSP server: no initialize, capabilities,
/// readiness predicate, or document sync — the bridge runs the
/// tool synchronously against the exact bytes it just hashed.
#[derive(Debug, Clone)]
pub struct CheckerEntry {
    /// Routing key; shares one namespace with server ids, and is
    /// the explicit `server` argument value that selects this
    /// checker in the `diagnostics` tool.
    pub id: String,
    /// The parser profile implemented in the bridge.
    pub parser: CheckerParser,
    /// Absolute path to the checker binary; never PATH-resolved.
    /// Must be the file `command[0]` names.
    pub binary: PathBuf,
    /// argv appended to the binary to probe its version.
    pub version_probe: Vec<String>,
    /// Exact version string the probe must return (after trimming).
    pub expect_version: String,
    /// Languages this entry serves (declarative, as for servers).
    pub languages: Vec<String>,
    /// File extensions this entry claims (no leading dot).
    pub extensions: Vec<String>,
    /// Extension -> language id, resolved as for servers.
    pub language_ids: BTreeMap<String, String>,
    /// Extension -> shader stage (parser-profile data; the glslang
    /// profile requires it for every claimed extension except bare
    /// `glsl`, whose stage comes from a compound suffix).
    pub stage_by_extension: BTreeMap<String, String>,
    /// Diagnostics authority: owner or cross-check.
    pub authority: CheckerAuthority,
    /// Run trigger; v1 admits `on-sync` only.
    pub trigger: CheckerTrigger,
    /// Coalescing window for changed-hash requests, milliseconds.
    pub debounce_ms: u64,
    /// Hard per-run deadline, milliseconds; expiry kills the child
    /// by PID and discards its output.
    pub timeout_ms: u64,
    /// Input refusal bound, checked before spawn.
    pub max_input_bytes: u64,
    /// Combined stdout+stderr capture bound.
    pub max_output_bytes: u64,
    /// Diagnostic-count bound; crossing it is a typed failure,
    /// never a truncated set.
    pub max_diagnostics: usize,
    /// Relative names marking a project root (design §2.3). Marker
    /// contents are never read in v1.
    pub root_markers: Vec<String>,
    /// The argv vector, executed directly — never through a shell.
    /// Placeholders are substituted as whole argv elements.
    pub command: Vec<String>,
    /// Explicit environment allowlist additions for the child.
    pub env: BTreeMap<String, String>,
}

/// The whole validated bridge configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Declared servers, in file order (routing scans all entries).
    pub servers: Vec<ServerEntry>,
    /// Declared checkers, in file order. Checkers never participate
    /// in extension-only routing; they answer only when named.
    pub checkers: Vec<CheckerEntry>,
    /// Per-request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// Readiness timeout in milliseconds.
    pub ready_timeout_ms: u64,
    /// Shutdown grace in milliseconds.
    pub shutdown_grace_ms: u64,
    /// SHA-256 of the config file bytes, for session transcripts.
    pub config_hash: String,
    /// Mutation posture, validated as one unambiguous state: an
    /// allowlist without the gated posture, or the gated posture
    /// without a non-empty allowlist, refuses to start (P5 item 2).
    pub mutation: MutationPosture,
    /// Client identities (`clientInfo.name` values) allowed the
    /// mutation class under the gated posture. Empty when off.
    pub mutation_allowlist: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    request_timeout_ms: Option<u64>,
    ready_timeout_ms: Option<u64>,
    shutdown_grace_ms: Option<u64>,
    mutation: Option<String>,
    mutation_allowlist: Option<Vec<String>>,
    server: Vec<RawServer>,
    #[serde(default)]
    checker: Vec<RawChecker>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChecker {
    id: String,
    parser: String,
    binary: String,
    version_probe: Vec<String>,
    expect_version: String,
    #[serde(default)]
    languages: Vec<String>,
    extensions: Vec<String>,
    #[serde(default)]
    language_ids: BTreeMap<String, String>,
    #[serde(default)]
    stage_by_extension: BTreeMap<String, String>,
    // No default on purpose (design §2.2): an omitted authority
    // fails the TOML parse, which `Config::load` surfaces as
    // ConfigInvalid — the ownership posture is never ambiguous.
    authority: String,
    trigger: String,
    debounce_ms: Option<u64>,
    timeout_ms: Option<u64>,
    max_input_bytes: Option<u64>,
    max_output_bytes: Option<u64>,
    max_diagnostics: Option<usize>,
    root_markers: Vec<String>,
    command: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    id: String,
    binary: String,
    #[serde(default)]
    args: Vec<String>,
    version_probe: Vec<String>,
    expect_version: String,
    #[serde(default)]
    languages: Vec<String>,
    extensions: Vec<String>,
    #[serde(default)]
    language_ids: BTreeMap<String, String>,
    readiness: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    initialization_options: Option<serde_json::Value>,
    settings: Option<serde_json::Value>,
    #[serde(default)]
    diagnostics_owner: bool,
    #[serde(default)]
    send_root_path: bool,
    no_publish_means_empty_ms: Option<u64>,
}

impl Config {
    /// Load and validate a config file. Every violation is a loud,
    /// typed `ConfigInvalid` error (SPEC §5, §9 gate 5).
    pub fn load(path: &Path) -> BridgeResult<Config> {
        let bytes =
            std::fs::read(path).map_err(|e| crate::errors::io_error("reading config file", e))?;
        let text = String::from_utf8(bytes.clone()).map_err(|_| BridgeError::ConfigInvalid {
            detail: "config file is not valid UTF-8".to_string(),
        })?;
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hex_encode(&hasher.finalize())
        };
        let raw: RawConfig = toml::from_str(&text).map_err(|e| BridgeError::ConfigInvalid {
            detail: format!("TOML parse failure: {e}"),
        })?;
        Config::validate(raw, hash)
    }

    fn validate(raw: RawConfig, config_hash: String) -> BridgeResult<Config> {
        // The mutation posture is validated as ONE state, never as
        // two knobs that can half-agree (P5 item 2): unknown values,
        // gated-without-an-allowlist, and an-allowlist-while-off
        // are all startup refusals with typed reasons.
        let posture_raw = raw.mutation.as_deref().unwrap_or("off");
        let mutation = match posture_raw {
            "off" => MutationPosture::Off,
            "gated" => MutationPosture::Gated,
            other => {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "unknown mutation posture {other:?} (expected \"off\" or \"gated\")"
                    ),
                });
            }
        };
        let mutation_allowlist = match (mutation, &raw.mutation_allowlist) {
            (MutationPosture::Off, Some(_)) => {
                return Err(BridgeError::ConfigInvalid {
                    detail: "mutation_allowlist is present while mutation posture is \"off\"; \
                             an allowlist without the gated posture is an ambiguous posture"
                        .to_string(),
                });
            }
            (MutationPosture::Off, None) => Vec::new(),
            (MutationPosture::Gated, Some(list))
                if !list.is_empty() && list.iter().all(|name| !name.trim().is_empty()) =>
            {
                list.clone()
            }
            (MutationPosture::Gated, _) => {
                return Err(BridgeError::ConfigInvalid {
                    detail: "mutation posture \"gated\" requires a non-empty \
                             mutation_allowlist of client identity names"
                        .to_string(),
                });
            }
        };
        if raw.server.is_empty() {
            return Err(BridgeError::ConfigInvalid {
                detail: "config declares no servers".to_string(),
            });
        }
        let mut seen_ids: Vec<&str> = Vec::new();
        let mut servers = Vec::new();
        for entry in &raw.server {
            if seen_ids.contains(&entry.id.as_str()) {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!("duplicate server id {:?}", entry.id),
                });
            }
            seen_ids.push(entry.id.as_str());
            servers.push(validate_server(entry)?);
        }
        // Server and checker ids share one namespace (design §4.1):
        // a caller naming an id must never face an ambiguity about
        // which kind of route answers.
        let mut checkers = Vec::new();
        for entry in &raw.checker {
            if seen_ids.contains(&entry.id.as_str()) {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "duplicate id {:?} across the server/checker namespace",
                        entry.id
                    ),
                });
            }
            seen_ids.push(entry.id.as_str());
            checkers.push(validate_checker(entry)?);
        }
        validate_checker_ownership(&servers, &checkers)?;
        let ready_timeout_ms = raw.ready_timeout_ms.unwrap_or(DEFAULT_READY_TIMEOUT_MS);
        for entry in &servers {
            if let Some(window) = entry.no_publish_means_empty_ms
                && window >= ready_timeout_ms
            {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "server {:?}: no_publish_means_empty_ms {window} must be below \
                         ready_timeout_ms {ready_timeout_ms}; the ready deadline would \
                         expire first and the opt-in could never fire",
                        entry.id
                    ),
                });
            }
        }
        Ok(Config {
            servers,
            checkers,
            request_timeout_ms: raw.request_timeout_ms.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
            ready_timeout_ms,
            shutdown_grace_ms: raw.shutdown_grace_ms.unwrap_or(DEFAULT_SHUTDOWN_GRACE_MS),
            config_hash,
            mutation,
            mutation_allowlist,
        })
    }

    /// Find a server entry by its routing id.
    pub fn server_by_id(&self, id: &str) -> Option<&ServerEntry> {
        self.servers.iter().find(|entry| entry.id == id)
    }

    /// Find a checker entry by its routing id. Checkers share the
    /// server id namespace and are reachable only by this explicit
    /// name — never through extension routing (design §4.1).
    pub fn checker_by_id(&self, id: &str) -> Option<&CheckerEntry> {
        self.checkers.iter().find(|entry| entry.id == id)
    }

    /// Resolve the server for a file extension. Ambiguity and absence
    /// are both errors — there is no default server (SPEC §5).
    pub fn server_for_extension(&self, extension: &str) -> BridgeResult<&ServerEntry> {
        let mut matches = self
            .servers
            .iter()
            .filter(|entry| entry.extensions.iter().any(|ext| ext == extension));
        match (matches.next(), matches.next()) {
            (Some(entry), None) => Ok(entry),
            (None, _) => Err(BridgeError::InvalidArgument {
                detail: format!("no configured server serves extension {extension:?}"),
            }),
            (Some(_), Some(_)) => Err(BridgeError::InvalidArgument {
                detail: format!(
                    "extension {extension:?} is claimed by more than one server; name a server \
                     explicitly"
                ),
            }),
        }
    }
}

fn validate_server(raw: &RawServer) -> BridgeResult<ServerEntry> {
    let binary = PathBuf::from(&raw.binary);
    if !binary.is_absolute() {
        return Err(BridgeError::ConfigInvalid {
            detail: format!("server {:?}: binary path must be absolute", raw.id),
        });
    }
    if raw.version_probe.is_empty() {
        return Err(BridgeError::ConfigInvalid {
            detail: format!("server {:?}: version_probe must not be empty", raw.id),
        });
    }
    if raw.expect_version.trim().is_empty() {
        return Err(BridgeError::ConfigInvalid {
            detail: format!("server {:?}: expect_version must not be empty", raw.id),
        });
    }
    if raw.extensions.is_empty() {
        return Err(BridgeError::ConfigInvalid {
            detail: format!("server {:?}: extensions must not be empty", raw.id),
        });
    }
    if let Some(window) = raw.no_publish_means_empty_ms
        && !(NO_PUBLISH_MEANS_EMPTY_MS_MIN..=NO_PUBLISH_MEANS_EMPTY_MS_MAX).contains(&window)
    {
        return Err(BridgeError::ConfigInvalid {
            detail: format!(
                "server {:?}: no_publish_means_empty_ms {window} is outside the supported \
                 range {NO_PUBLISH_MEANS_EMPTY_MS_MIN}..={NO_PUBLISH_MEANS_EMPTY_MS_MAX}",
                raw.id
            ),
        });
    }
    let language_ids = resolve_language_ids(raw)?;
    Ok(ServerEntry {
        id: raw.id.clone(),
        binary,
        args: raw.args.clone(),
        version_probe: raw.version_probe.clone(),
        expect_version: raw.expect_version.clone(),
        languages: raw.languages.clone(),
        extensions: raw.extensions.clone(),
        language_ids,
        readiness: Readiness::parse(&raw.readiness)?,
        env: raw.env.clone(),
        initialization_options: raw.initialization_options.clone(),
        settings: raw.settings.clone(),
        diagnostics_owner: raw.diagnostics_owner,
        send_root_path: raw.send_root_path,
        no_publish_means_empty_ms: raw.no_publish_means_empty_ms,
    })
}

/// Placeholders admitted in a checker `command` (design §2.2).
/// Substitution is whole-element only: an element containing a
/// brace must be exactly one of these strings, so a filename with
/// spaces or shell metacharacters stays one inert argument.
pub const CHECKER_PLACEHOLDERS: [&str; 7] = [
    "{path}",
    "{relpath}",
    "{stage}",
    "{file_dir}",
    "{project_root}",
    "{output}",
    "{depfile}",
];

fn validate_checker(raw: &RawChecker) -> BridgeResult<CheckerEntry> {
    let invalid = |detail: String| BridgeError::ConfigInvalid { detail };
    let binary = PathBuf::from(&raw.binary);
    if !binary.is_absolute() {
        return Err(invalid(format!(
            "checker {:?}: binary path must be absolute",
            raw.id
        )));
    }
    if raw.version_probe.is_empty() {
        return Err(invalid(format!(
            "checker {:?}: version_probe must not be empty",
            raw.id
        )));
    }
    if raw.expect_version.trim().is_empty() {
        return Err(invalid(format!(
            "checker {:?}: expect_version must not be empty",
            raw.id
        )));
    }
    if raw.extensions.is_empty() {
        return Err(invalid(format!(
            "checker {:?}: extensions must not be empty",
            raw.id
        )));
    }
    let parser = CheckerParser::parse(&raw.parser)?;
    let authority = CheckerAuthority::parse(&raw.authority)?;
    let trigger = CheckerTrigger::parse(&raw.trigger)?;
    let debounce_ms = raw.debounce_ms.unwrap_or(DEFAULT_CHECKER_DEBOUNCE_MS);
    if debounce_ms > CHECKER_DEBOUNCE_MS_MAX {
        return Err(invalid(format!(
            "checker {:?}: debounce_ms {debounce_ms} exceeds the maximum {CHECKER_DEBOUNCE_MS_MAX}",
            raw.id
        )));
    }
    let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_CHECKER_TIMEOUT_MS);
    if !(CHECKER_TIMEOUT_MS_MIN..=CHECKER_TIMEOUT_MS_MAX).contains(&timeout_ms) {
        return Err(invalid(format!(
            "checker {:?}: timeout_ms {timeout_ms} is outside the supported range \
             {CHECKER_TIMEOUT_MS_MIN}..={CHECKER_TIMEOUT_MS_MAX}",
            raw.id
        )));
    }
    let max_input_bytes = raw
        .max_input_bytes
        .unwrap_or(DEFAULT_CHECKER_MAX_INPUT_BYTES);
    if !(CHECKER_MAX_INPUT_BYTES_MIN..=CHECKER_MAX_INPUT_BYTES_MAX).contains(&max_input_bytes) {
        return Err(invalid(format!(
            "checker {:?}: max_input_bytes {max_input_bytes} is outside the supported range \
             {CHECKER_MAX_INPUT_BYTES_MIN}..={CHECKER_MAX_INPUT_BYTES_MAX}",
            raw.id
        )));
    }
    let max_output_bytes = raw
        .max_output_bytes
        .unwrap_or(DEFAULT_CHECKER_MAX_OUTPUT_BYTES);
    if !(CHECKER_MAX_OUTPUT_BYTES_MIN..=CHECKER_MAX_OUTPUT_BYTES_MAX).contains(&max_output_bytes) {
        return Err(invalid(format!(
            "checker {:?}: max_output_bytes {max_output_bytes} is outside the supported range \
             {CHECKER_MAX_OUTPUT_BYTES_MIN}..={CHECKER_MAX_OUTPUT_BYTES_MAX}",
            raw.id
        )));
    }
    let max_diagnostics = raw
        .max_diagnostics
        .unwrap_or(DEFAULT_CHECKER_MAX_DIAGNOSTICS);
    if max_diagnostics == 0 || max_diagnostics > CHECKER_MAX_DIAGNOSTICS_MAX {
        return Err(invalid(format!(
            "checker {:?}: max_diagnostics {max_diagnostics} is outside the supported range \
             1..={CHECKER_MAX_DIAGNOSTICS_MAX}",
            raw.id
        )));
    }
    if raw.root_markers.is_empty() {
        return Err(invalid(format!(
            "checker {:?}: root_markers must not be empty",
            raw.id
        )));
    }
    for marker in &raw.root_markers {
        let marker_path = Path::new(marker);
        if marker.is_empty()
            || marker_path.is_absolute()
            || marker_path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(invalid(format!(
                "checker {:?}: root marker {marker:?} must be a non-empty relative name \
                 without `..`",
                raw.id
            )));
        }
    }
    if raw.command.is_empty() {
        return Err(invalid(format!(
            "checker {:?}: command must not be empty",
            raw.id
        )));
    }
    if raw.command[0] != raw.binary {
        return Err(invalid(format!(
            "checker {:?}: command[0] {:?} must name the same file as binary {:?}; the \
             probed binary and the run binary are one tool",
            raw.id, raw.command[0], raw.binary
        )));
    }
    // Placeholder discipline: whole-element substitution only, no
    // unknown placeholders, and the per-run temp placeholders at
    // most once each (two runs' outputs must never share a path
    // inside one invocation). One profile exception, driven by
    // the tool's own argv grammar: glslangValidator rejects a
    // split `-I` (`-I<dir>` must be one element — its own error
    // text says so), so the glslang profile admits exactly the
    // two glued include forms. The substituted value still lands
    // as exactly one argv element; the discipline's purpose —
    // no value is ever split or smeared across elements — holds.
    let mut seen_output = false;
    let mut seen_depfile = false;
    for element in &raw.command {
        if !element.contains('{') && !element.contains('}') {
            continue;
        }
        let glued_include = parser == CheckerParser::GlslangValidator1
            && (element == "-I{file_dir}" || element == "-I{project_root}");
        if !glued_include && !CHECKER_PLACEHOLDERS.contains(&element.as_str()) {
            return Err(invalid(format!(
                "checker {:?}: command element {element:?} is not a whole-element known \
                 placeholder (one of {})",
                raw.id,
                CHECKER_PLACEHOLDERS.join(", ")
            )));
        }
        match element.as_str() {
            "{output}" => {
                if seen_output {
                    return Err(invalid(format!(
                        "checker {:?}: duplicate {{output}} in command",
                        raw.id
                    )));
                }
                seen_output = true;
            }
            "{depfile}" => {
                if seen_depfile {
                    return Err(invalid(format!(
                        "checker {:?}: duplicate {{depfile}} in command",
                        raw.id
                    )));
                }
                seen_depfile = true;
            }
            _ => {}
        }
    }
    let language_ids = resolve_language_ids_for(
        "checker",
        &raw.id,
        &raw.languages,
        &raw.extensions,
        &raw.language_ids,
    )?;
    let entry = CheckerEntry {
        id: raw.id.clone(),
        parser,
        binary,
        version_probe: raw.version_probe.clone(),
        expect_version: raw.expect_version.clone(),
        languages: raw.languages.clone(),
        extensions: raw.extensions.clone(),
        language_ids,
        stage_by_extension: raw.stage_by_extension.clone(),
        authority,
        trigger,
        debounce_ms,
        timeout_ms,
        max_input_bytes,
        max_output_bytes,
        max_diagnostics,
        root_markers: raw.root_markers.clone(),
        command: raw.command.clone(),
        env: raw.env.clone(),
    };
    // Parser-profile command/stage contracts (design §2.2): the
    // profile module owns what its tool's argv must and must not
    // contain.
    match entry.parser {
        CheckerParser::GlslangValidator1 => crate::checker_glslang::validate_config(&entry)?,
    }
    Ok(entry)
}

/// Ownership is per extension and is settled here, at load time —
/// never negotiated at call time (design §4.1). An owner checker
/// may not claim an extension a diagnostics-owner server claims,
/// and two owner checkers may not claim the same extension. A
/// cross-check checker may overlap anything; it is reachable only
/// by explicit id and is never a gate source.
fn validate_checker_ownership(
    servers: &[ServerEntry],
    checkers: &[CheckerEntry],
) -> BridgeResult<()> {
    let mut owner_claims: Vec<(&str, &str)> = Vec::new(); // (extension, claimant id)
    for checker in checkers {
        if checker.authority != CheckerAuthority::Owner {
            continue;
        }
        for extension in &checker.extensions {
            if let Some(server) = servers
                .iter()
                .find(|s| s.diagnostics_owner && s.extensions.iter().any(|e| e == extension))
            {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "checker {:?}: authority \"owner\" claims extension {extension:?}, \
                         which server {:?} already owns for gate diagnostics",
                        checker.id, server.id
                    ),
                });
            }
            if let Some((_, claimant)) = owner_claims
                .iter()
                .find(|(claimed, _)| claimed == &extension.as_str())
            {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "checker {:?}: authority \"owner\" claims extension {extension:?}, \
                         already claimed by owner checker {claimant:?}",
                        checker.id
                    ),
                });
            }
            owner_claims.push((extension.as_str(), checker.id.as_str()));
        }
    }
    Ok(())
}

fn resolve_language_ids(raw: &RawServer) -> BridgeResult<BTreeMap<String, String>> {
    resolve_language_ids_for(
        "server",
        &raw.id,
        &raw.languages,
        &raw.extensions,
        &raw.language_ids,
    )
}

/// The one language-id resolution rule, shared by server and
/// checker entries: an explicit `language_ids` map covering every
/// declared extension wins; otherwise positional pairing of equal-
/// length `languages`/`extensions` arrays; otherwise invalid.
fn resolve_language_ids_for(
    kind: &str,
    id: &str,
    languages: &[String],
    extensions: &[String],
    language_ids: &BTreeMap<String, String>,
) -> BridgeResult<BTreeMap<String, String>> {
    if !language_ids.is_empty() {
        for extension in extensions {
            if !language_ids.contains_key(extension) {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "{kind} {id:?}: language_ids has no entry for extension {extension:?}"
                    ),
                });
            }
        }
        return Ok(language_ids.clone());
    }
    if languages.len() == extensions.len() && !languages.is_empty() {
        let mut map = BTreeMap::new();
        for (extension, language) in extensions.iter().zip(languages.iter()) {
            map.insert(extension.clone(), language.clone());
        }
        return Ok(map);
    }
    Err(BridgeError::ConfigInvalid {
        detail: format!(
            "{kind} {id:?}: declare language_ids (extension -> language id); positional pairing \
             needs languages and extensions of equal length"
        ),
    })
}

/// Lowercase hex encoding for digests (transcripts, document hashes).
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256 hex digest of an in-memory byte string.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}
