//! Library-level gates: config validation, bounded framing, and
//! canonical shaping. Split between validation and adversarial
//! cases; the classification of every test is listed in
//! BUILD-REPORT.md.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use theodolite::config::{Config, MutationPosture};
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
