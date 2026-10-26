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

/// Readiness predicate named by a server entry (SPEC §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ready as soon as the initialize handshake completes.
    Immediate,
    /// Ready when no `$/progress` work is in flight and a probe
    /// request succeeds (rust-analyzer's shape).
    Quiescent,
}

impl Readiness {
    fn parse(raw: &str) -> BridgeResult<Readiness> {
        match raw {
            "immediate" => Ok(Readiness::Immediate),
            "quiescent" => Ok(Readiness::Quiescent),
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

/// The whole validated bridge configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Declared servers, in file order (routing scans all entries).
    pub servers: Vec<ServerEntry>,
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
        Ok(Config {
            servers,
            request_timeout_ms: raw.request_timeout_ms.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
            ready_timeout_ms: raw.ready_timeout_ms.unwrap_or(DEFAULT_READY_TIMEOUT_MS),
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
    })
}

fn resolve_language_ids(raw: &RawServer) -> BridgeResult<BTreeMap<String, String>> {
    if !raw.language_ids.is_empty() {
        for extension in &raw.extensions {
            if !raw.language_ids.contains_key(extension) {
                return Err(BridgeError::ConfigInvalid {
                    detail: format!(
                        "server {:?}: language_ids has no entry for extension {extension:?}",
                        raw.id
                    ),
                });
            }
        }
        return Ok(raw.language_ids.clone());
    }
    if raw.languages.len() == raw.extensions.len() && !raw.languages.is_empty() {
        let mut map = BTreeMap::new();
        for (extension, language) in raw.extensions.iter().zip(raw.languages.iter()) {
            map.insert(extension.clone(), language.clone());
        }
        return Ok(map);
    }
    Err(BridgeError::ConfigInvalid {
        detail: format!(
            "server {:?}: declare language_ids (extension -> language id); positional pairing \
             needs languages and extensions of equal length",
            raw.id
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
