//! Library-level gates: config validation, bounded framing, and
//! canonical shaping. Split between validation and adversarial
//! cases; the classification of every test is listed in
//! BUILD-REPORT.md.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use theodolite::config::{Config, MutationPosture, Readiness};
use theodolite::errors::BridgeError;
use theodolite::gate::{GateDecision, HandleState, VersionState, decide};
use theodolite::lsp::{LspMessage, read_message, write_message};
use theodolite::session::resolve_within;
use theodolite::shaping::{
    Diagnostic, DiagnosticChannel, Location, Position, Range, enforce_budget, merge_diagnostics,
    relativize_uri, sort_diagnostics, sort_locations, uri_to_path,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("theodolite-unit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("bridge.toml");
    std::fs::write(&path, body).expect("write config");
    path
}

const VALID_SERVER: &str = r#"
[[server]]
id = "fake"
binary = "/bin/true"
version_probe = ["--version"]
expect_version = "true 1.0"
languages = ["rust"]
extensions = ["rs"]
readiness = "immediate"
"#;

/// The serve-d registry entry from `bridge.toml.example`, verbatim
/// in shape: both D extensions share the one LSP language id, and
/// readiness is the first-response predicate (serve-d's workspace
/// indexing never goes quiescent inside the readiness window;
/// the first real answered request is its readiness evidence).
const SERVED_SERVER: &str = r#"
[[server]]
id = "serve-d"
binary = "/usr/bin/serve-d"
version_probe = ["--version"]
expect_version = "serve-d 0.7.6"
languages = ["d"]
extensions = ["d", "di"]
language_ids = { d = "d", di = "d" }
readiness = "first-response"
env = { PATH = "/usr/local/bin:/usr/bin:/bin" }
"#;

// --- Validation ----------------------------------------------------

#[test]
fn config_valid_loads_with_positional_language_pairing() {
    let dir = temp_dir("cfg-ok");
    let path = write_config(&dir, VALID_SERVER);
    let config = Config::load(&path).expect("valid config loads");
    assert_eq!(config.servers.len(), 1);
    assert_eq!(
        config.servers[0].language_ids.get("rs").map(String::as_str),
        Some("rust")
    );
    assert_eq!(config.config_hash.len(), 64);
    assert_eq!(config.mutation, MutationPosture::Off);
    assert!(config.mutation_allowlist.is_empty());
}

#[test]
fn config_serve_d_entry_loads() {
    let dir = temp_dir("cfg-served");
    let path = write_config(&dir, SERVED_SERVER);
    let config = Config::load(&path).expect("serve-d entry loads");
    assert_eq!(config.servers.len(), 1);
    let server = &config.servers[0];
    assert_eq!(server.id, "serve-d");
    assert_eq!(server.args, Vec::<String>::new());
    assert_eq!(server.language_ids.get("d").map(String::as_str), Some("d"));
    assert_eq!(server.language_ids.get("di").map(String::as_str), Some("d"));
    assert_eq!(server.readiness, Readiness::FirstResponse);
    assert!(!server.diagnostics_owner);
}

#[test]
fn config_first_response_readiness_loads() {
    let dir = temp_dir("cfg-first-response");
    let body = VALID_SERVER.replace("immediate", "first-response");
    let config = Config::load(&write_config(&dir, &body)).expect("first-response config loads");
    assert_eq!(config.servers[0].readiness, Readiness::FirstResponse);
}

#[test]
fn config_no_publish_means_empty_loads_and_defaults_to_none() {
    let dir = temp_dir("cfg-silence");
    // The key lands inside the server stanza (it is the file's
    // last table), opting that one server in to silence
    // semantics with a 5 s window.
    let body = format!("{VALID_SERVER}no_publish_means_empty_ms = 5000\n");
    let config = Config::load(&write_config(&dir, &body)).expect("silence opt-in loads");
    assert_eq!(config.servers[0].no_publish_means_empty_ms, Some(5000));
    // Absent the key the opt-in is off: silence stays an error,
    // never a default.
    let config = Config::load(&write_config(&dir, VALID_SERVER)).expect("default loads");
    assert_eq!(config.servers[0].no_publish_means_empty_ms, None);
}

#[test]
fn config_send_root_path_loads_and_defaults_to_false() {
    let dir = temp_dir("cfg-rootpath");
    // The key lands inside the server stanza (it is the file's
    // last table), opting that one server in to the legacy
    // rootPath initialize field.
    let body = format!("{VALID_SERVER}send_root_path = true\n");
    let config = Config::load(&write_config(&dir, &body)).expect("root-path opt-in loads");
    assert!(config.servers[0].send_root_path);
    // Absent the key the opt-in is off: the bridge sends the
    // modern initialize shape only, never a deprecated field
    // by default.
    let config = Config::load(&write_config(&dir, VALID_SERVER)).expect("default loads");
    assert!(!config.servers[0].send_root_path);
}

#[test]
fn config_gated_with_allowlist_loads() {
    let dir = temp_dir("cfg-gated");
    let body =
        format!("mutation = \"gated\"\nmutation_allowlist = [\"alice\", \"bob\"]\n{VALID_SERVER}");
    let config = Config::load(&write_config(&dir, &body)).expect("gated config loads");
    assert_eq!(config.mutation, MutationPosture::Gated);
    assert_eq!(config.mutation_allowlist, vec!["alice", "bob"]);
}

#[test]
fn gate_decide_allows_the_full_pass() {
    let allowlist = vec!["alice".to_string()];
    // The apply shape: valid handle, current versions.
    assert_eq!(
        decide(
            MutationPosture::Gated,
            Some("alice"),
            &allowlist,
            HandleState::Valid,
            VersionState::Current
        ),
        GateDecision::Allow
    );
    // The registration shape: no handle, no version state.
    assert_eq!(
        decide(
            MutationPosture::Gated,
            Some("alice"),
            &allowlist,
            HandleState::NotApplicable,
            VersionState::NotApplicable
        ),
        GateDecision::Allow
    );
}

#[test]
fn framing_roundtrip_classifies_all_three_shapes() {
    let mut buffer: Vec<u8> = Vec::new();
    write_message(
        &mut buffer,
        "srv",
        &serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}),
    )
    .expect("write response");
    write_message(
        &mut buffer,
        "srv",
        &serde_json::json!({"jsonrpc": "2.0", "method": "m/n", "params": {}}),
    )
    .expect("write notification");
    write_message(
        &mut buffer,
        "srv",
        &serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "m/r", "params": {}}),
    )
    .expect("write request");
    let mut cursor = Cursor::new(buffer);
    match read_message(&mut cursor, "srv")
        .expect("read response")
        .expect("frame")
    {
        LspMessage::Response { id, result } => {
            assert_eq!(id, 7);
            assert!(result.is_ok());
        }
        other => panic!("expected response, got {other:?}"),
    }
    match read_message(&mut cursor, "srv")
        .expect("read notification")
        .expect("frame")
    {
        LspMessage::Notification { method, .. } => assert_eq!(method, "m/n"),
        other => panic!("expected notification, got {other:?}"),
    }
    match read_message(&mut cursor, "srv")
        .expect("read request")
        .expect("frame")
    {
        LspMessage::Request { method, .. } => assert_eq!(method, "m/r"),
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn shaping_sorts_canonically_and_dedups_locations() {
    let range = |line: u64| Range {
        start: Position { line, character: 0 },
        end: Position { line, character: 3 },
    };
    let mut diagnostics = vec![
        Diagnostic {
            uri: "file:///b.rs".into(),
            range: range(1),
            severity: 1,
            source: "clippy".into(),
            code: "x".into(),
            message: "m".into(),
            channel: DiagnosticChannel::Push,
        },
        Diagnostic {
            uri: "file:///a.rs".into(),
            range: range(9),
            severity: 2,
            source: "clippy".into(),
            code: "x".into(),
            message: "m".into(),
            channel: DiagnosticChannel::Push,
        },
        Diagnostic {
            uri: "file:///a.rs".into(),
            range: range(2),
            severity: 2,
            source: "clippy".into(),
            code: "x".into(),
            message: "m".into(),
            channel: DiagnosticChannel::Push,
        },
    ];
    sort_diagnostics(&mut diagnostics);
    assert_eq!(diagnostics[0].uri, "file:///a.rs");
    assert_eq!(diagnostics[0].range.start.line, 2);
    assert_eq!(diagnostics[2].uri, "file:///b.rs");

    let mut locations = vec![
        Location {
            uri: "file:///a.rs".into(),
            range: range(5),
        },
        Location {
            uri: "file:///a.rs".into(),
            range: range(5),
        },
        Location {
            uri: "file:///a.rs".into(),
            range: range(1),
        },
    ];
    sort_locations(&mut locations);
    assert_eq!(locations.len(), 2);
    assert_eq!(locations[0].range.start.line, 1);
}

#[test]
fn merge_diagnostics_dedups_and_marks_provenance() {
    let diagnostic = |line: u64, message: &str, channel: DiagnosticChannel| Diagnostic {
        uri: "file:///a.rs".into(),
        range: Range {
            start: Position { line, character: 0 },
            end: Position { line, character: 3 },
        },
        severity: 1,
        source: "clippy".into(),
        code: "x".into(),
        message: message.into(),
        channel,
    };
    let push = vec![
        diagnostic(1, "shared", DiagnosticChannel::Push),
        diagnostic(4, "push only", DiagnosticChannel::Push),
    ];
    let pull = vec![
        diagnostic(1, "shared", DiagnosticChannel::Pull),
        diagnostic(7, "pull only", DiagnosticChannel::Pull),
    ];
    let merged = merge_diagnostics(push, pull);
    assert_eq!(merged.len(), 3);
    let channels: Vec<&str> = merged.iter().map(|d| d.channel.as_str()).collect();
    assert_eq!(channels, vec!["push+pull", "push", "pull"]);
    // Same-channel duplicates also collapse.
    let again = merge_diagnostics(
        vec![diagnostic(1, "shared", DiagnosticChannel::Push)],
        vec![diagnostic(1, "shared", DiagnosticChannel::Push)],
    );
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].channel, DiagnosticChannel::Push);
}

#[test]
fn uri_codec_roundtrips_and_relativizes() {
    let path = uri_to_path("file:///tmp/some%20dir/a.rs").expect("decode");
    assert_eq!(path, PathBuf::from("/tmp/some dir/a.rs"));
    let root = PathBuf::from("/tmp/some dir");
    let relative = relativize_uri("file:///tmp/some%20dir/a.rs", &root).expect("relativize");
    assert_eq!(relative, "file:///a.rs");
}

// --- Adversarial ---------------------------------------------------

#[test]
fn config_violations_are_typed_config_invalid() {
    let dir = temp_dir("cfg-bad");
    // Relative binary path.
    let bad = VALID_SERVER.replace("/bin/true", "relative/true");
    let error = Config::load(&write_config(&dir, &bad)).expect_err("relative binary refused");
    assert_eq!(error.code(), "ConfigInvalid");
    // Duplicate server id.
    let dup = format!("{VALID_SERVER}\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &dup)).expect_err("duplicate id refused");
    assert_eq!(error.code(), "ConfigInvalid");
    // Gated posture without an allowlist: an ambiguous posture,
    // refused at startup, never silently defaulted.
    let gated = format!("mutation = \"gated\"\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &gated)).expect_err("gated without allowlist");
    assert_eq!(error.code(), "ConfigInvalid");
    // Unknown readiness predicate.
    let readiness = VALID_SERVER.replace("immediate", "whenever");
    let error = Config::load(&write_config(&dir, &readiness)).expect_err("bad readiness");
    assert_eq!(error.code(), "ConfigInvalid");
}

#[test]
fn config_no_publish_means_empty_out_of_bounds_is_refused() {
    let dir = temp_dir("cfg-silence-bad");
    // Below the floor: indistinguishable from "no publish yet".
    let body = format!("{VALID_SERVER}no_publish_means_empty_ms = 100\n");
    let error = Config::load(&write_config(&dir, &body)).expect_err("sub-floor window refused");
    assert_eq!(error.code(), "ConfigInvalid");
    // Above the ceiling: a second deadline, not a bounded wait.
    let body = format!("{VALID_SERVER}no_publish_means_empty_ms = 120001\n");
    let error = Config::load(&write_config(&dir, &body)).expect_err("over-ceiling refused");
    assert_eq!(error.code(), "ConfigInvalid");
    // At or past the ready deadline: the ready deadline would
    // expire first, so the opt-in could never fire — a config
    // that silently does nothing is refused, not carried.
    let body = format!("ready_timeout_ms = 3000\n{VALID_SERVER}no_publish_means_empty_ms = 5000\n");
    let error = Config::load(&write_config(&dir, &body)).expect_err("window past deadline");
    assert_eq!(error.code(), "ConfigInvalid");
}

#[test]
fn config_send_root_path_non_bool_is_refused() {
    let dir = temp_dir("cfg-rootpath-bad");
    // A string is not a boolean opt-in: the parse refusal
    // surfaces as the typed ConfigInvalid, never a silently
    // defaulted false.
    let body = format!("{VALID_SERVER}send_root_path = \"yes\"\n");
    let error = Config::load(&write_config(&dir, &body)).expect_err("non-bool opt-in refused");
    assert_eq!(error.code(), "ConfigInvalid");
}

#[test]
fn config_serve_d_entry_missing_extension_language_id_is_refused() {
    let dir = temp_dir("cfg-served-bad");
    // The `.di` extension declared without a language id: the
    // bridge would have no didOpen authority for interface files.
    let bad = SERVED_SERVER.replace(
        "language_ids = { d = \"d\", di = \"d\" }",
        "language_ids = { d = \"d\" }",
    );
    let error =
        Config::load(&write_config(&dir, &bad)).expect_err("missing di language id refused");
    assert_eq!(error.code(), "ConfigInvalid");
    // The positional-pairing fallback cannot rescue two extensions
    // against one language either.
    let bad = SERVED_SERVER.replace("language_ids = { d = \"d\", di = \"d\" }\n", "");
    let error = Config::load(&write_config(&dir, &bad)).expect_err("unpaired extensions refused");
    assert_eq!(error.code(), "ConfigInvalid");
}

#[test]
fn config_mutation_posture_ambiguity_is_refused() {
    let dir = temp_dir("cfg-posture");
    // An allowlist while the posture is off.
    let body = format!("mutation_allowlist = [\"alice\"]\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &body)).expect_err("allowlist while off");
    assert_eq!(error.code(), "ConfigInvalid");
    // Gated with an explicitly empty allowlist.
    let body = format!("mutation = \"gated\"\nmutation_allowlist = []\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &body)).expect_err("gated with empty list");
    assert_eq!(error.code(), "ConfigInvalid");
    // Gated with a blank identity name.
    let body = format!("mutation = \"gated\"\nmutation_allowlist = [\"  \"]\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &body)).expect_err("gated with blank name");
    assert_eq!(error.code(), "ConfigInvalid");
    // An unknown posture string.
    let body = format!("mutation = \"sometimes\"\n{VALID_SERVER}");
    let error = Config::load(&write_config(&dir, &body)).expect_err("unknown posture");
    assert_eq!(error.code(), "ConfigInvalid");
}

#[test]
fn gate_decide_denies_each_check_independently() {
    let allowlist = vec!["alice".to_string()];
    let cases: Vec<(GateDecision, &str)> = vec![
        (
            decide(
                MutationPosture::Off,
                Some("alice"),
                &allowlist,
                HandleState::Valid,
                VersionState::Current,
            ),
            "posture is off",
        ),
        (
            decide(
                MutationPosture::Gated,
                None,
                &allowlist,
                HandleState::Valid,
                VersionState::Current,
            ),
            "no client identity",
        ),
        (
            decide(
                MutationPosture::Gated,
                Some("mallory"),
                &allowlist,
                HandleState::Valid,
                VersionState::Current,
            ),
            "not on the mutation allowlist",
        ),
        (
            decide(
                MutationPosture::Gated,
                Some("alice"),
                &allowlist,
                HandleState::Missing,
                VersionState::Current,
            ),
            "unknown action handle",
        ),
        (
            decide(
                MutationPosture::Gated,
                Some("alice"),
                &allowlist,
                HandleState::Used,
                VersionState::Current,
            ),
            "single-use",
        ),
        (
            decide(
                MutationPosture::Gated,
                Some("alice"),
                &allowlist,
                HandleState::WrongSession,
                VersionState::Current,
            ),
            "different session",
        ),
        (
            decide(
                MutationPosture::Gated,
                Some("alice"),
                &allowlist,
                HandleState::Valid,
                VersionState::Drifted,
            ),
            "changed since",
        ),
    ];
    for (decision, needle) in cases {
        match decision {
            GateDecision::Deny(reason) => {
                assert!(
                    reason.contains(needle),
                    "reason {reason:?} lacks {needle:?}"
                )
            }
            GateDecision::Allow => panic!("expected a denial mentioning {needle:?}"),
        }
    }
}

#[test]
fn framing_rejects_oversized_negative_and_garbage_before_allocation() {
    // Oversized: only the header exists in the stream. If the reader
    // tried to allocate or read the body first, the error would be a
    // truncation error instead of the cap error.
    let mut cursor = Cursor::new(b"Content-Length: 99999999\r\n\r\n".to_vec());
    let error = read_message(&mut cursor, "srv").expect_err("oversized refused");
    assert_eq!(error.code(), "MalformedServerTraffic");
    assert!(error.to_string().contains("MAX_FRAME_BYTES"), "{error}");

    let mut cursor = Cursor::new(b"Content-Length: -5\r\n\r\n".to_vec());
    let error = read_message(&mut cursor, "srv").expect_err("negative refused");
    assert_eq!(error.code(), "MalformedServerTraffic");

    let body = b"not json";
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    let mut cursor = Cursor::new(frame);
    let error = read_message(&mut cursor, "srv").expect_err("garbage refused");
    assert_eq!(error.code(), "MalformedServerTraffic");
}

#[test]
fn budget_overflow_is_typed_with_true_item_count() {
    let big = serde_json::json!({"items": "x".repeat(5000)});
    let error = enforce_budget(&big, 42, 1024).expect_err("over budget");
    match error {
        BridgeError::ResultTooLarge {
            items,
            budget_bytes,
        } => {
            assert_eq!(items, 42);
            assert_eq!(budget_bytes, 1024);
        }
        other => panic!("expected ResultTooLarge, got {other:?}"),
    }
}

#[test]
fn path_escapes_are_refused_in_all_forms() {
    let dir = temp_dir("escape");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).expect("mkdir ws");
    std::fs::write(workspace.join("inside.rs"), "fn main() {}\n").expect("write inside");
    let outside = dir.join("outside.rs");
    std::fs::write(&outside, "fn secret() {}\n").expect("write outside");
    let root = workspace.canonicalize().expect("canonical ws");

    // Textual traversal.
    let error = resolve_within(&root, "../outside.rs").expect_err("dotdot refused");
    assert_eq!(error.code(), "WorkspaceEscape");
    // Absolute path outside the workspace.
    let error =
        resolve_within(&root, outside.to_str().expect("utf8")).expect_err("absolute refused");
    assert_eq!(error.code(), "WorkspaceEscape");
    // Symlink inside the workspace pointing out.
    std::os::unix::fs::symlink(&outside, workspace.join("link.rs")).expect("symlink");
    let error = resolve_within(&root, "link.rs").expect_err("symlink escape refused");
    assert_eq!(error.code(), "WorkspaceEscape");
    // Non-file URI scheme.
    let error = uri_to_path("https://example.com/a.rs").expect_err("scheme refused");
    assert_eq!(error.code(), "WorkspaceEscape");
    // A URI outside the workspace cannot be relativized into a result.
    let error = relativize_uri("file:///etc/passwd", &root).expect_err("outside uri refused");
    assert_eq!(error.code(), "WorkspaceEscape");
    // The legitimate inside path resolves.
    let ok = resolve_within(&root, "inside.rs").expect("inside resolves");
    assert!(ok.ends_with("inside.rs"));
}

#[test]
fn supervisor_stat_parse_reads_pgrp_and_starttime() {
    // Field layout after the comm: state ppid pgrp session tty_nr
    // tpgid flags ... starttime is overall field 22 (index 19 of
    // the post-comm tail). Fixtures counted token by token.
    let line = "1659452 (head) R 1441586 1659452 1659452 0 -1 4194304 426 140 0 0 0 0 0 0 20 0 1 0 119799232 2785280";
    assert_eq!(
        theodolite::supervisor::parse_stat(line),
        Some((1659452, 119799232))
    );
    // A comm containing spaces and parentheses must not shift the
    // fields: parsing starts after the LAST closing paren.
    let weird = "42 (weird ) name (x) S 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 777 0";
    assert_eq!(theodolite::supervisor::parse_stat(weird), Some((42, 777)));
    // Adversarial: truncated and garbage lines parse to None, never
    // a panic or a wrong group.
    assert_eq!(theodolite::supervisor::parse_stat("42 (x) S 1"), None);
    assert_eq!(theodolite::supervisor::parse_stat(""), None);
    assert_eq!(theodolite::supervisor::parse_stat("no parens at all"), None);
    assert_eq!(
        theodolite::supervisor::parse_stat("1 (x) S a b c 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"),
        None
    );
}

// --- Checker stanzas: config validation (design §9.1.1, §9.2.8) ---

use theodolite::checker_glslang::{
    ParseInput, derive_stage, output_named_sources, parse_depfile, parse_run,
};

/// A GLSL-owning server stanza (glsld's shape): the diagnostics
/// owner for the GLSL extensions, against which checker ownership
/// is validated.
const GLSLD_SERVER: &str = r#"
[[server]]
id = "glsld"
binary = "/bin/true"
version_probe = ["--version"]
expect_version = "true 1.0"
languages = ["glsl"]
extensions = ["frag", "vert", "geom", "comp", "tesc", "tese", "glsl"]
language_ids = { frag = "glsl", vert = "glsl", geom = "glsl", comp = "glsl", tesc = "glsl", tese = "glsl", glsl = "glsl" }
readiness = "immediate"
diagnostics_owner = true
"#;

/// The design §2.1 cross-check stanza, with /bin/true standing in
/// for the tool binary (config load validates shape, not tools).
const VALID_CHECKER: &str = r#"
[[checker]]
id = "glslang-vulkan-crosscheck"
parser = "glslang-validator-1"
binary = "/bin/true"
version_probe = ["--version"]
expect_version = "true 1.0"
languages = ["glsl"]
extensions = ["frag", "vert", "geom", "comp", "tesc", "tese", "glsl"]
language_ids = { frag = "glsl", vert = "glsl", geom = "glsl", comp = "glsl", tesc = "glsl", tese = "glsl", glsl = "glsl" }
stage_by_extension = { frag = "frag", vert = "vert", geom = "geom", comp = "comp", tesc = "tesc", tese = "tese" }
authority = "cross-check"
trigger = "on-sync"
root_markers = [".git", "shaders.toml"]
command = ["/bin/true", "-V", "--target-env", "vulkan1.3", "--error-column", "--stdin", "-S", "{stage}", "-I{file_dir}", "-I{project_root}", "--depfile", "{depfile}", "-o", "{output}"]
env = { PATH = "/usr/local/bin:/usr/bin:/bin" }
"#;

fn load_config(tag: &str, body: &str) -> Result<Config, BridgeError> {
    let dir = temp_dir(tag);
    let path = write_config(&dir, body);
    Config::load(&path)
}

fn assert_config_invalid(tag: &str, body: &str) {
    match load_config(tag, body) {
        Err(BridgeError::ConfigInvalid { .. }) => {}
        Err(other) => panic!("{tag}: expected ConfigInvalid, got {other:?}"),
        Ok(_) => panic!("{tag}: expected ConfigInvalid, config loaded"),
    }
}

#[test]
fn checker_crosscheck_stanza_loads_and_server_routing_is_unchanged() {
    let body = format!("{GLSLD_SERVER}{VALID_CHECKER}");
    let config = load_config("cfg-checker-ok", &body).expect("cross-check stanza loads");
    assert_eq!(config.checkers.len(), 1);
    let entry = config
        .checker_by_id("glslang-vulkan-crosscheck")
        .expect("checker resolves by id");
    assert_eq!(entry.authority.as_str(), "cross-check");
    // Extension routing still considers servers only (§4.1): the
    // checker is reachable solely by explicit id.
    assert_eq!(
        config.server_for_extension("frag").expect("frag routes").id,
        "glsld"
    );
    assert!(config.server_by_id("glslang-vulkan-crosscheck").is_none());
}

#[test]
fn checker_owner_overlapping_an_lsp_owner_is_rejected() {
    let body = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("cross-check", "owner")
    );
    assert_config_invalid("cfg-checker-owner-overlap", &body);
}

#[test]
fn checker_owner_on_an_unclaimed_extension_loads() {
    let owner = r#"
[[checker]]
id = "mesh-owner"
parser = "glslang-validator-1"
binary = "/bin/true"
version_probe = ["--version"]
expect_version = "true 1.0"
languages = ["glsl"]
extensions = ["mesh"]
language_ids = { mesh = "glsl" }
stage_by_extension = { mesh = "mesh" }
authority = "owner"
trigger = "on-sync"
root_markers = [".git"]
command = ["/bin/true", "-V", "--error-column", "--stdin", "-S", "{stage}", "-I{file_dir}", "-I{project_root}", "--depfile", "{depfile}", "-o", "{output}"]
"#;
    let body = format!("{GLSLD_SERVER}{owner}");
    let config = load_config("cfg-checker-owner-free", &body).expect("owner on free extension");
    assert_eq!(config.checkers.len(), 1);
}

#[test]
fn checker_two_owners_on_one_extension_are_rejected() {
    let one = VALID_CHECKER
        .replace("glslang-vulkan-crosscheck", "owner-one")
        .replace("cross-check", "owner")
        .replace(
            "extensions = [\"frag\", \"vert\", \"geom\", \"comp\", \"tesc\", \"tese\", \"glsl\"]",
            "extensions = [\"mesh\"]",
        )
        .replace(
            "language_ids = { frag = \"glsl\", vert = \"glsl\", geom = \"glsl\", comp = \"glsl\", tesc = \"glsl\", tese = \"glsl\", glsl = \"glsl\" }",
            "language_ids = { mesh = \"glsl\" }",
        )
        .replace(
            "stage_by_extension = { frag = \"frag\", vert = \"vert\", geom = \"geom\", comp = \"comp\", tesc = \"tesc\", tese = \"tese\" }",
            "stage_by_extension = { mesh = \"mesh\" }",
        );
    let two = one.replace("owner-one", "owner-two");
    let body = format!("{GLSLD_SERVER}{one}{two}");
    assert_config_invalid("cfg-checker-two-owners", &body);
}

#[test]
fn checker_id_colliding_with_a_server_id_is_rejected() {
    let body = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("glslang-vulkan-crosscheck", "glsld")
    );
    assert_config_invalid("cfg-checker-id-collision", &body);
}

#[test]
fn checker_missing_authority_is_rejected() {
    let body = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("authority = \"cross-check\"\n", "")
    );
    assert_config_invalid("cfg-checker-no-authority", &body);
}

#[test]
fn checker_unknown_parser_and_trigger_are_rejected() {
    let bad_parser = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("glslang-validator-1", "regex-1")
    );
    assert_config_invalid("cfg-checker-bad-parser", &bad_parser);
    let bad_trigger = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("on-sync", "on-save")
    );
    assert_config_invalid("cfg-checker-bad-trigger", &bad_trigger);
}

#[test]
fn checker_command_contract_violations_are_rejected() {
    // command[0] must be the probed binary.
    let mismatch = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("command = [\"/bin/true\"", "command = [\"/bin/false\"")
    );
    assert_config_invalid("cfg-checker-cmd-mismatch", &mismatch);
    // Placeholders substitute whole elements only.
    let embedded = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("\"{stage}\"", "\"--stage={stage}\"")
    );
    assert_config_invalid("cfg-checker-cmd-embedded", &embedded);
    // Unknown placeholders are refused.
    let unknown = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("\"{stage}\"", "\"{bogus}\"")
    );
    assert_config_invalid("cfg-checker-cmd-unknown", &unknown);
    // The glslang profile requires --error-column.
    let no_column = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("\"--error-column\", ", "")
    );
    assert_config_invalid("cfg-checker-cmd-nocolumn", &no_column);
    // Suppression flags are refused by the profile.
    let quiet = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("\"-V\", ", "\"-V\", \"--quiet\", ")
    );
    assert_config_invalid("cfg-checker-cmd-quiet", &quiet);
    // The per-run output placeholder appears at most once.
    let dup_output = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace(
            "\"-o\", \"{output}\"",
            "\"-o\", \"{output}\", \"-o\", \"{output}\""
        )
    );
    assert_config_invalid("cfg-checker-cmd-dupout", &dup_output);
    // Every claimed extension but bare glsl needs a stage.
    let no_stage = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace(", tese = \"tese\" }", " }")
    );
    assert_config_invalid("cfg-checker-cmd-nostage", &no_stage);
    // A split `-I` is refused at load: this glslang build takes
    // the include directory glued only, and a split form would
    // otherwise fail every run with a command-line error.
    let split_include = format!(
        "{GLSLD_SERVER}{}",
        VALID_CHECKER.replace("\"-I{file_dir}\"", "\"-I\", \"{file_dir}\"")
    );
    assert_config_invalid("cfg-checker-cmd-spliti", &split_include);
}

#[test]
fn checker_bound_and_marker_violations_are_rejected() {
    let cases: Vec<(String, String)> = vec![
        (
            "debounce".to_string(),
            VALID_CHECKER.replace(
                "trigger = \"on-sync\"",
                "trigger = \"on-sync\"\ndebounce_ms = 5001",
            ),
        ),
        (
            "timeout".to_string(),
            VALID_CHECKER.replace(
                "trigger = \"on-sync\"",
                "trigger = \"on-sync\"\ntimeout_ms = 99",
            ),
        ),
        (
            "input".to_string(),
            VALID_CHECKER.replace(
                "trigger = \"on-sync\"",
                "trigger = \"on-sync\"\nmax_input_bytes = 65535",
            ),
        ),
        (
            "diagnostics".to_string(),
            VALID_CHECKER.replace(
                "trigger = \"on-sync\"",
                "trigger = \"on-sync\"\nmax_diagnostics = 0",
            ),
        ),
        (
            "markers-empty".to_string(),
            VALID_CHECKER.replace(
                "root_markers = [\".git\", \"shaders.toml\"]",
                "root_markers = []",
            ),
        ),
        (
            "markers-escape".to_string(),
            VALID_CHECKER.replace(
                "root_markers = [\".git\", \"shaders.toml\"]",
                "root_markers = [\"../up\"]",
            ),
        ),
        (
            "binary-relative".to_string(),
            VALID_CHECKER.replace("binary = \"/bin/true\"", "binary = \"bin/true\""),
        ),
    ];
    for (tag, stanza) in cases {
        let body = format!("{GLSLD_SERVER}{stanza}");
        assert_config_invalid(&format!("cfg-checker-bad-{tag}"), &body);
    }
}

/// The acceptance gate from the gdshader coordination update: the
/// CURRENT deployed bridge.toml shape — 41 server stanzas and the
/// 10 checker stanzas, with `send_root_path = true` on gdshader-lsp
/// and apex-language-server — must load. (Shape re-pinned at the
/// batch-5 repo sync, 2026-11-28; re-pinned again at next-10 set 1
/// the same day: +nixd server, +statix-check/+deadnix-check
/// checkers, nil binary path repaired; at set 2:
/// +luacheck-check checker; and at set 3: +basedpyright
/// server, +ruff-check checker; and at set 5:
/// +cmakelint-check/+checkmake-check checkers;
/// and at set 6: +actionlint-check checker;
/// and at set 7: +vnu-check checker;
/// and at set 8: +clangd-cuda server.)
#[test]
fn deployed_bridge_toml_shape_loads() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/deployed-bridge.toml");
    let config = Config::load(&path).expect("deployed bridge.toml shape loads");
    assert_eq!(config.servers.len(), 41);
    let checker_ids: Vec<&str> = config.checkers.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        checker_ids,
        [
            "glslang-vulkan-crosscheck",
            "shaderlab-hlsl-check",
            "statix-check",
            "deadnix-check",
            "luacheck-check",
            "ruff-check",
            "cmakelint-check",
            "checkmake-check",
            "actionlint-check",
            "vnu-check",
        ]
    );
    let gdshader = config
        .server_by_id("gdshader-lsp")
        .expect("gdshader stanza present");
    assert!(gdshader.send_root_path);
    assert_eq!(config.servers.iter().filter(|s| s.send_root_path).count(), 2);
}

// --- Checker parser goldens (design §9.1.2/9.1.4, §9.2.4, §9.2.6) ---

const BAD_FRAG: &[u8] = include_bytes!("fixtures/checker/vulkan-bad.frag");
const WARN_FRAG: &[u8] = include_bytes!("fixtures/checker/warn_ext.frag");
const UNICODE_FRAG: &[u8] = include_bytes!("fixtures/checker/unicode-bad.frag");
const NOMAIN_FRAG: &[u8] = include_bytes!("fixtures/checker/no-main.frag");

/// The stdin-mode info log for the defective seed, captured on
/// primo (checker design Appendix A.1, the `--error-column` run):
/// framing line, two positioned errors, the aggregate line.
fn bad_stderr() -> String {
    concat!(
        "stdin\n",
        "ERROR: stdin:7:28: 'missing_alpha' : undeclared identifier",
        " ",
        "\n",
        "ERROR: stdin:7:41: '' : compilation terminated",
        " ",
        "\n",
        "ERROR: 2 compilation errors.  No code generated.\n",
    )
    .to_string()
}

fn parse(
    target_bytes: &[u8],
    exit_code: i32,
    stderr: &str,
) -> Result<Vec<Diagnostic>, BridgeError> {
    parse_run(&ParseInput {
        checker_id: "test-checker",
        target_uri: "file:///ws/target.frag",
        target_bytes,
        sources: &[],
        exit_code,
        stderr,
        max_diagnostics: 512,
    })
}

#[test]
fn parser_golden_defective_seed_yields_exactly_two_diagnostics() {
    let diagnostics = parse(BAD_FRAG, 2, &bad_stderr()).expect("golden parse");
    assert_eq!(diagnostics.len(), 2);
    let first = &diagnostics[0];
    assert_eq!(first.uri, "file:///ws/target.frag");
    assert_eq!(first.range.start.line, 6);
    assert_eq!(first.range.start.character, 27);
    assert_eq!(first.range.end.line, 6);
    assert_eq!(first.range.end.character, 40);
    assert_eq!(first.severity, 1);
    assert_eq!(first.source, "glslangValidator");
    assert_eq!(first.channel, DiagnosticChannel::Checker);
    assert_eq!(
        first.message,
        concat!("'missing_alpha' : undeclared identifier", " ")
    );
    let second = &diagnostics[1];
    assert_eq!(second.range.start.line, 6);
    assert_eq!(second.range.start.character, 40);
    assert_eq!(second.range.end.character, 40);
    assert_eq!(second.severity, 1);
}

#[test]
fn parser_warning_seed_maps_both_warnings_at_severity_2() {
    let stderr = concat!(
        "stdin\n",
        "WARNING: stdin:3:8: extension GL_ARB_separate_shader_objects is being used for location\n",
        "WARNING: stdin:3:31: extension GL_ARB_separate_shader_objects is being used for location qualifier on output\n",
    );
    let diagnostics = parse(WARN_FRAG, 0, stderr).expect("warning parse");
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics.iter().all(|d| d.severity == 2));
    assert_eq!(diagnostics[0].range.start.line, 2);
    assert_eq!(diagnostics[0].range.start.character, 7);
    assert_eq!(diagnostics[1].range.start.character, 30);
}

#[test]
fn parser_link_failure_is_one_file_level_diagnostic() {
    let stderr = "stdin\nERROR: Linking fragment stage: Missing entry point: Each stage requires one entry point\n";
    let diagnostics = parse(NOMAIN_FRAG, 3, stderr).expect("link parse");
    assert_eq!(diagnostics.len(), 1);
    let only = &diagnostics[0];
    assert_eq!(only.range.start.line, 0);
    assert_eq!(only.range.start.character, 0);
    assert_eq!(only.range.end.character, 0);
    assert_eq!(only.severity, 1);
    assert_eq!(
        only.message,
        "Linking fragment stage: Missing entry point: Each stage requires one entry point"
    );
}

#[test]
fn parser_unicode_columns_convert_bytes_to_utf16() {
    // Captured primo run (Appendix A.5): byte column 35 is the
    // token start; the line's é (2 bytes) and 😀 (4 bytes, 2
    // UTF-16 units) put the UTF-16 column at 31.
    let stderr = concat!(
        "stdin\n",
        "ERROR: stdin:4:35: 'missing_value' : undeclared identifier",
        " ",
        "\n",
        "ERROR: stdin:4:48: '' : compilation terminated",
        " ",
        "\n",
        "ERROR: 2 compilation errors.  No code generated.\n",
    );
    let diagnostics = parse(UNICODE_FRAG, 2, stderr).expect("unicode parse");
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].range.start.line, 3);
    assert_eq!(diagnostics[0].range.start.character, 31);
    assert_eq!(diagnostics[0].range.end.character, 44);
    assert_eq!(diagnostics[1].range.start.character, 44);
    assert_eq!(diagnostics[1].range.end.character, 44);
}

#[test]
fn parser_mid_codepoint_column_is_invalid() {
    // Byte column 11 lands inside the 😀 (bytes 10..13 of line 4).
    let stderr = "stdin\nERROR: stdin:4:11: 'x' : undeclared identifier \n";
    match parse(UNICODE_FRAG, 2, stderr) {
        Err(BridgeError::CheckerOutputInvalid { .. }) => {}
        other => panic!("expected CheckerOutputInvalid, got {other:?}"),
    }
}

#[test]
fn parser_malformed_output_family_is_invalid() {
    // A positioned line without a column.
    let no_column = concat!(
        "stdin\n",
        "ERROR: stdin:7: 'missing_alpha' : undeclared identifier",
        " ",
        "\n",
        "ERROR: 2 compilation errors.  No code generated.\n",
    );
    assert!(matches!(
        parse(BAD_FRAG, 2, no_column),
        Err(BridgeError::CheckerOutputInvalid { .. })
    ));
    // An unknown severity token.
    let unknown_severity = "stdin\nNOTICE: stdin:1:1: 'x' : hi\n";
    assert!(matches!(
        parse(BAD_FRAG, 0, unknown_severity),
        Err(BridgeError::CheckerOutputInvalid { .. })
    ));
    // An aggregate count that disagrees with the positioned errors.
    let mismatch = bad_stderr().replace("2 compilation errors", "3 compilation errors");
    assert!(matches!(
        parse(BAD_FRAG, 2, &mismatch),
        Err(BridgeError::CheckerOutputInvalid { .. })
    ));
    // Exit 0 together with an ERROR line.
    assert!(matches!(
        parse(BAD_FRAG, 0, &bad_stderr()),
        Err(BridgeError::CheckerOutputInvalid { .. })
    ));
    // A line of no known class at all.
    let junk = "stdin\nsomething else entirely\n";
    assert!(matches!(
        parse(BAD_FRAG, 0, junk),
        Err(BridgeError::CheckerOutputInvalid { .. })
    ));
    // Exit 2 with no mappable diagnostic is a failure, not a set.
    assert!(matches!(
        parse(BAD_FRAG, 2, "stdin\n"),
        Err(BridgeError::CheckerFailed { .. })
    ));
}

#[test]
fn stage_derivation_compound_suffix_map_and_refusal() {
    let body = format!("{GLSLD_SERVER}{VALID_CHECKER}");
    let config = load_config("cfg-stage", &body).expect("config loads");
    let entry = config.checker_by_id("glslang-vulkan-crosscheck").unwrap();
    assert_eq!(
        derive_stage(entry, Path::new("/ws/x.frag")).unwrap(),
        "frag"
    );
    assert_eq!(
        derive_stage(entry, Path::new("/ws/x.comp")).unwrap(),
        "comp"
    );
    assert_eq!(
        derive_stage(entry, Path::new("/ws/x.frag.glsl")).unwrap(),
        "frag"
    );
    assert!(matches!(
        derive_stage(entry, Path::new("/ws/x.glsl")),
        Err(BridgeError::CheckerStageUnknown { .. })
    ));
}

#[test]
fn output_named_sources_finds_absolute_positioned_tokens() {
    // The failed-include shape measured on primo: no depfile is
    // written, and the header is named only in the log itself.
    let stderr = concat!(
        "stdin\n",
        "ERROR: /ws/inc/common.glsl:1:35: 'nope_undeclared' : undeclared identifier",
        " ",
        "\n",
        "ERROR: stdin:4:6: '' : compilation terminated",
        " ",
        "\n",
    );
    assert_eq!(
        output_named_sources(stderr),
        vec![PathBuf::from("/ws/inc/common.glsl")]
    );
    assert!(output_named_sources("stdin\n").is_empty());
}

#[test]
fn depfile_parsing_drops_stdin_and_keeps_absolute_paths() {
    let deps = parse_depfile("/tmp/out.spv: stdin /ws/inc/common.glsl /ws/inc/other.glsl\n");
    assert_eq!(
        deps,
        vec![
            PathBuf::from("/ws/inc/common.glsl"),
            PathBuf::from("/ws/inc/other.glsl")
        ]
    );
}
