//! Checker integration gates (checker design §9): the runner
//! against fake checker scripts (half the family adversarial —
//! missing binary, version drift, timeout kill, oversize in both
//! directions, argv integrity, supersession) and against the real
//! glslangValidator on primo (the §9.1 corpus; skipping loudly
//! where the binary is absent, the parity test's pattern).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use theodolite::config::{CheckerEntry, Config};
use theodolite::errors::BridgeError;
use theodolite::session::SessionManager;
use theodolite::shaping::Diagnostic;
use theodolite::tools::call_tool;

const FAKE_VERSION: &str = "fake-checker 1.0";
const GLSLANG: &str = "/usr/bin/glslangValidator";
/// The design §6 pin, verified live on primo 2026-11-24.
const GLSLANG_PIN: &str = "Glslang Version: 11:16.6.0
ESSL Version: OpenGL ES GLSL 3.20 glslang Khronos. 16.6.0
GLSL Version: 4.60 glslang Khronos. 16.6.0
SPIR-V Version 0x00010600, Revision 1
GLSL.std.450 Version 100, Revision 1
Khronos Tool ID 8
SPIR-V Generator Version 11
GL_KHR_vulkan_glsl version 100
ARB_GL_gl_spirv version 100";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("theodolite-checker-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.canonicalize().expect("canonical temp dir")
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(body.as_bytes()).expect("write script");
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
    path
}

/// A config with one inert server (the loader requires at least
/// one) plus one checker stanza built from `stanza` lines.
fn load_manager(dir: &Path, stanza: &str) -> (SessionManager, CheckerEntry) {
    let body = format!(
        r#"
[[server]]
id = "inert"
binary = "/bin/true"
version_probe = ["--version"]
expect_version = "true 1.0"
languages = ["rust"]
extensions = ["rs"]
readiness = "immediate"

{stanza}
"#
    );
    let path = dir.join("bridge.toml");
    std::fs::write(&path, body).expect("write config");
    let config = Config::load(&path).expect("config loads");
    let entry = config.checkers[0].clone();
    (SessionManager::new(Arc::new(config)), entry)
}

/// A checker stanza around `binary`, with the fake's version
/// contract; `extra` carries stanza fields (bounds, debounce…).
fn fake_stanza(binary: &Path, extra: &str) -> String {
    format!(
        r#"
[[checker]]
id = "fake-checker"
parser = "glslang-validator-1"
binary = "{binary}"
version_probe = ["--version"]
expect_version = "{FAKE_VERSION}"
languages = ["glsl"]
extensions = ["frag", "glsl"]
language_ids = {{ frag = "glsl", glsl = "glsl" }}
stage_by_extension = {{ frag = "frag" }}
authority = "cross-check"
trigger = "on-sync"
root_markers = [".git"]
command = ["{binary}", "-V", "--error-column", "--stdin", "-S", "{{stage}}", "-I{{file_dir}}", "-I{{project_root}}", "--depfile", "{{depfile}}", "-o", "{{output}}"]
{extra}
"#,
        binary = binary.display()
    )
}

fn workspace_with(dir: &Path, name: &str, content: &[u8]) -> (PathBuf, PathBuf) {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let target = ws.join(name);
    std::fs::write(&target, content).expect("write target");
    (ws, target)
}

fn run(
    manager: &SessionManager,
    entry: &CheckerEntry,
    ws: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(Vec<Diagnostic>, theodolite::checker::RunRecord), BridgeError> {
    manager.checkers().diagnostics(entry, ws, target, bytes)
}

// --- Fake-checker runner gates ------------------------------------

#[test]
fn fake_ok_run_completes_and_second_call_is_cached() {
    let dir = temp_dir("fake-ok");
    let counter = dir.join("runs.txt");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             echo run >> \"{counter}\"\ncat > /dev/null\n\
             dep=\"\"; prev=\"\"; for a in \"$@\"; do if [ \"$prev\" = \"--depfile\" ]; then dep=\"$a\"; fi; prev=\"$a\"; done\n\
             printf 'out: stdin\\n' > \"$dep\"\nexit 0\n",
            counter = counter.display()
        ),
    );
    let (manager, entry) = load_manager(&dir, &fake_stanza(&script, ""));
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    let (diagnostics, record) = run(&manager, &entry, &ws, &target, &bytes).expect("first run");
    assert!(diagnostics.is_empty());
    assert_eq!(record.state, "completed");
    assert_eq!(record.exit_code, Some(0));
    let (_, record2) = run(&manager, &entry, &ws, &target, &bytes).expect("cached call");
    assert_eq!(
        record2.run_id, record.run_id,
        "second call served the same run"
    );
    let runs = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "the tool ran exactly once");
    // A changed document runs again.
    let changed = b"void main() { /* v2 */ }\n";
    std::fs::write(&target, changed).unwrap();
    run(&manager, &entry, &ws, &target, changed).expect("changed run");
    let runs = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 2);
}

#[test]
fn missing_binary_is_checker_unavailable() {
    let dir = temp_dir("fake-missing");
    let stanza = fake_stanza(Path::new("/nonexistent/fake-checker"), "");
    let (manager, entry) = load_manager(&dir, &stanza);
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    match run(&manager, &entry, &ws, &target, &bytes) {
        Err(BridgeError::CheckerUnavailable { .. }) => {}
        other => panic!("expected CheckerUnavailable, got {other:?}"),
    }
}

#[test]
fn version_drift_is_a_mismatch_before_any_run() {
    let dir = temp_dir("fake-drift");
    let marker = dir.join("ran.txt");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"fake-checker 9.9\"; exit 0; fi\n\
             echo ran >> \"{marker}\"\nexit 0\n",
            marker = marker.display()
        ),
    );
    let (manager, entry) = load_manager(&dir, &fake_stanza(&script, ""));
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    match run(&manager, &entry, &ws, &target, &bytes) {
        Err(BridgeError::CheckerVersionMismatch {
            expected, actual, ..
        }) => {
            assert_eq!(expected, FAKE_VERSION);
            assert_eq!(actual, "fake-checker 9.9");
        }
        other => panic!("expected CheckerVersionMismatch, got {other:?}"),
    }
    assert!(!marker.exists(), "no document run happened");
}

#[test]
fn timeout_kills_the_child_by_pid() {
    let dir = temp_dir("fake-timeout");
    let pidfile = dir.join("child.pid");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             echo $$ > \"{pidfile}\"\nexec sleep 30\n",
            pidfile = pidfile.display()
        ),
    );
    let stanza = fake_stanza(&script, "timeout_ms = 500");
    let (manager, entry) = load_manager(&dir, &stanza);
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    match run(&manager, &entry, &ws, &target, &bytes) {
        Err(BridgeError::CheckerTimeout { timeout_ms, .. }) => assert_eq!(timeout_ms, 500),
        other => panic!("expected CheckerTimeout, got {other:?}"),
    }
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The killed child is gone (checked by PID, never by name).
    let proc_path = format!("/proc/{pid}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while Path::new(&proc_path).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(!Path::new(&proc_path).exists(), "child {pid} still exists");
}

#[test]
fn oversize_input_is_refused_before_spawn() {
    let dir = temp_dir("fake-big-input");
    let marker = dir.join("ran.txt");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             echo ran >> \"{marker}\"\nexit 0\n",
            marker = marker.display()
        ),
    );
    let stanza = fake_stanza(&script, "max_input_bytes = 65536");
    let (manager, entry) = load_manager(&dir, &stanza);
    let (ws, target) = workspace_with(&dir, "a.frag", b"x");
    let big = vec![b'x'; 65_537];
    match run(&manager, &entry, &ws, &target, &big) {
        Err(BridgeError::CheckerInputTooLarge { size, limit, .. }) => {
            assert_eq!(size, 65_537);
            assert_eq!(limit, 65_536);
        }
        other => panic!("expected CheckerInputTooLarge, got {other:?}"),
    }
    assert!(!marker.exists(), "the tool never ran");
}

#[test]
fn oversize_output_is_killed_at_the_cap() {
    let dir = temp_dir("fake-big-output");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             head -c 200000 /dev/zero | tr '\\0' 'e' >&2\nexit 0\n"
        ),
    );
    let stanza = fake_stanza(&script, "max_output_bytes = 65536");
    let (manager, entry) = load_manager(&dir, &stanza);
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    match run(&manager, &entry, &ws, &target, &bytes) {
        Err(BridgeError::CheckerOutputTooLarge { .. }) => {}
        other => panic!("expected CheckerOutputTooLarge, got {other:?}"),
    }
}

#[test]
fn filename_with_spaces_and_semicolons_is_one_argv_element() {
    let dir = temp_dir("fake-argv");
    let argv_file = dir.join("argv.txt");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             for a in \"$@\"; do echo \"$a\"; done > \"{argv_file}\"\ncat > /dev/null\nexit 0\n",
            argv_file = argv_file.display()
        ),
    );
    let stanza = format!(
        r#"
[[checker]]
id = "fake-checker"
parser = "glslang-validator-1"
binary = "{binary}"
version_probe = ["--version"]
expect_version = "{FAKE_VERSION}"
languages = ["glsl"]
extensions = ["frag", "glsl"]
language_ids = {{ frag = "glsl", glsl = "glsl" }}
stage_by_extension = {{ frag = "frag" }}
authority = "cross-check"
trigger = "on-sync"
root_markers = [".git"]
command = ["{binary}", "run", "{{path}}", "--stdin", "-V", "--error-column", "-S", "{{stage}}", "-I{{file_dir}}", "-I{{project_root}}", "--depfile", "{{depfile}}", "-o", "{{output}}"]
"#,
        binary = script.display()
    );
    let (manager, entry) = load_manager(&dir, &stanza);
    let (ws, target) = workspace_with(&dir, "weird name;rm -rf.frag", b"void main() {}\n");
    let bytes = std::fs::read(&target).unwrap();
    run(&manager, &entry, &ws, &target, &bytes).expect("run completes");
    let argv_text = std::fs::read_to_string(&argv_file).unwrap();
    let lines: Vec<&str> = argv_text.lines().collect();
    assert_eq!(lines.len(), 13, "argv element count: {lines:?}");
    assert_eq!(
        lines[1],
        target.to_string_lossy().as_ref(),
        "the hostile filename arrives as exactly one element"
    );
}

#[test]
fn supersession_mid_run_types_the_stale_caller() {
    let dir = temp_dir("fake-supersede");
    let started = dir.join("started.txt");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             touch \"{started}\"\ncat > /dev/null\nsleep 2\n\
             dep=\"\"; prev=\"\"; for a in \"$@\"; do if [ \"$prev\" = \"--depfile\" ]; then dep=\"$a\"; fi; prev=\"$a\"; done\n\
             printf 'out: stdin\\n' > \"$dep\"\nexit 0\n",
            started = started.display()
        ),
    );
    let stanza = fake_stanza(&script, "debounce_ms = 50");
    let (manager, entry) = load_manager(&dir, &stanza);
    let manager = Arc::new(manager);
    let entry = Arc::new(entry);
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() { /* v1 */ }\n");
    let v1 = std::fs::read(&target).unwrap();
    let v2 = b"void main() { /* v2 */ }\n".to_vec();
    let stale = {
        let manager = manager.clone();
        let entry = entry.clone();
        let ws = ws.clone();
        let target = target.clone();
        std::thread::spawn(move || run(&manager, &entry, &ws, &target, &v1))
    };
    // Wait until the first run is actually in flight, then change
    // the document under it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(started.exists(), "first run started");
    std::fs::write(&target, &v2).unwrap();
    let fresh = run(&manager, &entry, &ws, &target, &v2).expect("newest run completes");
    assert!(fresh.0.is_empty());
    match stale.join().expect("stale thread") {
        Err(BridgeError::CheckerSuperseded { .. }) => {}
        other => panic!("expected CheckerSuperseded for the stale caller, got {other:?}"),
    }
}

#[test]
fn tools_layer_envelope_and_refusals() {
    let dir = temp_dir("fake-tools");
    let script = write_script(
        &dir,
        "fake.sh",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{FAKE_VERSION}\"; exit 0; fi\n\
             cat > /dev/null\nexit 0\n"
        ),
    );
    let (manager, entry) = load_manager(&dir, &fake_stanza(&script, ""));
    let _ = entry;
    let (ws, target) = workspace_with(&dir, "a.frag", b"void main() {}\n");
    let result = call_tool(
        &manager,
        "diagnostics",
        &serde_json::json!({
            "server": "fake-checker",
            "workspace": ws.to_string_lossy(),
            "path": "a.frag",
        }),
    )
    .expect("checker diagnostics through the tool layer");
    assert_eq!(result["channels"], serde_json::json!(["checker"]));
    assert_eq!(result["authority"], "cross-check");
    assert_eq!(result["count"], 0);
    assert_eq!(result["checker_run"]["state"], "completed");
    let _ = target;
    // session_status reports the run-record family.
    let status = call_tool(
        &manager,
        "session_status",
        &serde_json::json!({
            "server": "fake-checker",
            "workspace": ws.to_string_lossy(),
        }),
    )
    .expect("checker session_status");
    assert_eq!(status["kind"], "checker");
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    // Every other tool refuses a checker id.
    let refused = call_tool(
        &manager,
        "hover",
        &serde_json::json!({
            "server": "fake-checker",
            "workspace": ws.to_string_lossy(),
            "path": "a.frag",
            "line": 0,
            "character": 0,
        }),
    );
    assert!(
        matches!(refused, Err(BridgeError::UnsupportedMethod { .. })),
        "hover with a checker id: {refused:?}"
    );
}

// --- Real glslangValidator gates (§9.1) ----------------------------

fn real_manager(dir: &Path) -> Option<(SessionManager, CheckerEntry)> {
    if !Path::new(GLSLANG).exists() {
        eprintln!("SKIP: {GLSLANG} absent on this machine");
        return None;
    }
    // The pin is nine lines; in TOML it rides as one basic string
    // with escaped newlines.
    let pin_escaped = GLSLANG_PIN.replace('\n', "\\n");
    let stanza = format!(
        r#"
[[checker]]
id = "glslang-vulkan-crosscheck"
parser = "glslang-validator-1"
binary = "{GLSLANG}"
version_probe = ["--version"]
expect_version = "{pin_escaped}"
languages = ["glsl"]
extensions = ["frag", "vert", "geom", "comp", "tesc", "tese", "glsl"]
language_ids = {{ frag = "glsl", vert = "glsl", geom = "glsl", comp = "glsl", tesc = "glsl", tese = "glsl", glsl = "glsl" }}
stage_by_extension = {{ frag = "frag", vert = "vert", geom = "geom", comp = "comp", tesc = "tesc", tese = "tese" }}
authority = "cross-check"
trigger = "on-sync"
root_markers = [".git", "shaders.toml"]
command = ["{GLSLANG}", "-V", "--target-env", "vulkan1.3", "--error-column", "--stdin", "-S", "{{stage}}", "-I{{file_dir}}", "-I{{project_root}}", "--depfile", "{{depfile}}", "-o", "{{output}}"]
env = {{ PATH = "/usr/local/bin:/usr/bin:/bin" }}
"#
    );
    Some(load_manager(dir, &stanza))
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checker")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[test]
fn real_bad_seed_golden_and_clean_control_in_one_session() {
    let dir = temp_dir("real-pair");
    let Some((manager, entry)) = real_manager(&dir) else {
        return;
    };
    let bad = fixture("vulkan-bad.frag");
    let (ws, target) = workspace_with(&dir, "vulkan-bad.frag", &bad);
    let (diagnostics, record) = run(&manager, &entry, &ws, &target, &bad).expect("bad run");
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].range.start.line, 6);
    assert_eq!(diagnostics[0].range.start.character, 27);
    assert_eq!(diagnostics[0].range.end.character, 40);
    assert_eq!(diagnostics[0].severity, 1);
    assert_eq!(record.state, "completed");
    assert_eq!(record.exit_code, Some(2));
    assert_eq!(
        record.input_sha256,
        theodolite::config::sha256_hex(&bad),
        "the run record carries the seed's SHA-256"
    );
    // The clean control in the same session: a verified zero.
    let clean = fixture("vulkan-clean.frag");
    let target2 = ws.join("vulkan-clean.frag");
    std::fs::write(&target2, &clean).unwrap();
    let (diagnostics, record) = run(&manager, &entry, &ws, &target2, &clean).expect("clean run");
    assert!(diagnostics.is_empty());
    assert_eq!(record.exit_code, Some(0));
}

#[test]
fn real_warning_seed_maps_at_severity_2_with_exit_0() {
    let dir = temp_dir("real-warn");
    let Some((manager, entry)) = real_manager(&dir) else {
        return;
    };
    let warn = fixture("warn_ext.frag");
    let (ws, target) = workspace_with(&dir, "warn_ext.frag", &warn);
    let (diagnostics, record) = run(&manager, &entry, &ws, &target, &warn).expect("warn run");
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics.iter().all(|d| d.severity == 2));
    assert_eq!(record.exit_code, Some(0));
}

#[test]
fn real_include_clean_then_broken_header_reports_the_header() {
    let dir = temp_dir("real-include");
    let Some((manager, entry)) = real_manager(&dir) else {
        return;
    };
    let ws = dir.join("ws");
    let inc = ws.join("inc");
    std::fs::create_dir_all(&inc).unwrap();
    let header = inc.join("common.glsl");
    std::fs::write(&header, fixture("inc/common.glsl")).unwrap();
    let main = fixture("inc/with-include.frag");
    let target = inc.join("with-include.frag");
    std::fs::write(&target, &main).unwrap();
    let (diagnostics, record) = run(&manager, &entry, &ws, &target, &main).expect("include run");
    assert!(diagnostics.is_empty(), "clean include: {diagnostics:?}");
    assert_eq!(record.dependencies, vec!["inc/common.glsl".to_string()]);
    assert!(record.dependencies_complete);
    // Break the header: the same target bytes must re-run (the
    // dependency hash is part of the run key) and the diagnostic
    // must land on the header's URI.
    std::fs::write(
        &header,
        b"vec4 estate_color() { return vec4(nope_undeclared); }\n",
    )
    .unwrap();
    let (diagnostics, _) = run(&manager, &entry, &ws, &target, &main).expect("broken include run");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.uri.ends_with("inc/common.glsl") && d.severity == 1),
        "header diagnostic present: {diagnostics:?}"
    );
}

#[test]
fn real_corpus_verdicts() {
    // The checker side of §9.1.8 (verdict agreement with glsld is
    // gated live in the rung-5 session; here the verdicts
    // themselves are pinned).
    let dir = temp_dir("real-corpus");
    let Some((manager, entry)) = real_manager(&dir) else {
        return;
    };
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for (name, expect_errors) in [
        ("bad.vert", true),
        ("good.vert", false),
        ("bad3.frag", true),
        ("vk_bad.frag", true),
        ("vk_good.frag", false),
    ] {
        let bytes = fixture(&format!("corpus/{name}"));
        let target = ws.join(name);
        std::fs::write(&target, &bytes).unwrap();
        let (diagnostics, _) = run(&manager, &entry, &ws, &target, &bytes)
            .unwrap_or_else(|e| panic!("{name}: run failed: {e:?}"));
        assert_eq!(
            diagnostics.iter().any(|d| d.severity == 1),
            expect_errors,
            "{name} verdict"
        );
    }
}
