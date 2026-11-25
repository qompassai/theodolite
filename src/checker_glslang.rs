//! The `glslang-validator-1` checker parser profile (checker
//! design §5): glslangValidator's stdin info-log grammar, stage
//! derivation, depfile parsing, and the byte→UTF-16 column
//! conversion. This module is a pure function over (bytes, exit
//! status, depfile) plus the config contract for the profile's
//! argv — it spawns nothing.
//!
//! The grammar is strict on purpose: a line the profile does not
//! understand makes the whole run `CheckerOutputInvalid`, never a
//! partial diagnostic set. A format the bridge cannot represent is
//! a bridge/tool mismatch to fix, not a set to approximate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{CheckerEntry, CheckerParser};
use crate::errors::{BridgeError, BridgeResult};
use crate::shaping::{Diagnostic, DiagnosticChannel, Position, Range, path_to_uri};

/// The `source` string stamped on every diagnostic this profile
/// produces (design §5.3).
pub const DIAGNOSTIC_SOURCE: &str = "glslangValidator";

/// The source token glslang prints for the stdin-fed target file.
pub const TARGET_TOKEN: &str = "stdin";

/// glslang's own stage list (design §2.4). Stage strings come only
/// from this list or the config's stage map — never from raw
/// filename text — so a hostile filename cannot smuggle an argv
/// element or flag.
pub const ADMITTED_STAGES: [&str; 14] = [
    "vert", "tesc", "tese", "geom", "frag", "comp", "mesh", "task", "rgen", "rint", "rahit",
    "rchit", "rmiss", "rcall",
];

/// Compiler flags the glslang profile refuses in a stanza command
/// (design §2.2): each either suppresses the evidence the gate
/// exists to read or papers over missing layout data.
const REFUSED_FLAGS: [&str; 9] = [
    "-s",
    "--quiet",
    "-w",
    "--suppress-warnings",
    "--relaxed-errors",
    "--auto-map-bindings",
    "--amb",
    "--auto-map-locations",
    "--aml",
];

/// The profile's config contract (design §2.2): exactly one
/// `{stage}`, `--stdin`, `-V`, `--error-column`, and one each of
/// the per-run temp placeholders (the depfile is the cache's
/// freshness evidence, and the SPIR-V output must land in the
/// bridge-owned temp path, never the workspace); none of the
/// refused flags; a stage mapping for every claimed extension
/// except bare `glsl`, whose stage comes from a compound suffix.
pub fn validate_config(entry: &CheckerEntry) -> BridgeResult<()> {
    let invalid = |detail: String| BridgeError::ConfigInvalid { detail };
    debug_assert_eq!(entry.parser, CheckerParser::GlslangValidator1);
    let count = |needle: &str| entry.command.iter().filter(|e| e.as_str() == needle).count();
    if count("{stage}") != 1 {
        return Err(invalid(format!(
            "checker {:?}: the glslang profile requires exactly one {{stage}} in command",
            entry.id
        )));
    }
    for required in ["--stdin", "-V", "--error-column", "{output}", "{depfile}"] {
        if count(required) != 1 {
            return Err(invalid(format!(
                "checker {:?}: the glslang profile requires exactly one {required:?} \
                 element in command",
                entry.id
            )));
        }
    }
    for element in &entry.command {
        if REFUSED_FLAGS.contains(&element.as_str()) {
            return Err(invalid(format!(
                "checker {:?}: the glslang profile refuses flag {element:?} in command; \
                 it suppresses diagnostics or papers over missing layout data",
                entry.id
            )));
        }
    }
    // The include search path (design §2.3) reaches the tool only
    // through argv, and this glslang build takes `-I<dir>` glued
    // only — a split `-I` is a runtime command-line failure (exit
    // 1), so the profile refuses it at load and requires the two
    // glued forms, the file's own directory before the project
    // root.
    if entry.command.iter().any(|e| e == "-I") {
        return Err(invalid(format!(
            "checker {:?}: a split \"-I\" element is refused; glslangValidator requires \
             the glued forms \"-I{{file_dir}}\" and \"-I{{project_root}}\"",
            entry.id
        )));
    }
    let position = |needle: &str| entry.command.iter().position(|e| e == needle);
    match (position("-I{file_dir}"), position("-I{project_root}")) {
        (Some(dir), Some(root)) if dir < root => {}
        _ => {
            return Err(invalid(format!(
                "checker {:?}: the glslang profile requires \"-I{{file_dir}}\" before \
                 \"-I{{project_root}}\" in command (design §2.3 include order)",
                entry.id
            )));
        }
    }
    for (extension, stage) in &entry.stage_by_extension {
        if !ADMITTED_STAGES.contains(&stage.as_str()) {
            return Err(invalid(format!(
                "checker {:?}: stage_by_extension[{extension:?}] = {stage:?} is not one of \
                 glslang's stages ({})",
                entry.id,
                ADMITTED_STAGES.join(", ")
            )));
        }
    }
    for extension in &entry.extensions {
        if extension != "glsl" && !entry.stage_by_extension.contains_key(extension) {
            return Err(invalid(format!(
                "checker {:?}: stage_by_extension has no stage for extension {extension:?}; \
                 only bare \"glsl\" may go without one (its stage comes from a compound \
                 suffix)",
                entry.id
            )));
        }
    }
    Ok(())
}

/// Derive the shader stage for a target file (design §2.4), in
/// order: a compound suffix (`name.<stage>.glsl`) wins; then the
/// config's stage map for the simple extension; otherwise the run
/// refuses — the bridge never guesses `frag` for a bare `.glsl`.
pub fn derive_stage(entry: &CheckerEntry, target: &Path) -> BridgeResult<String> {
    let unknown = || BridgeError::CheckerStageUnknown {
        checker: entry.id.clone(),
        detail: format!(
            "no stage derivable for {}; name it name.<stage>.glsl or map its extension \
             in stage_by_extension",
            target.display()
        ),
    };
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for wrapper in ["glsl", "hlsl"] {
        if let Some(stem) = file_name.strip_suffix(&format!(".{wrapper}"))
            && let Some((_, stage)) = stem.rsplit_once('.')
            && ADMITTED_STAGES.contains(&stage)
        {
            return Ok(stage.to_string());
        }
    }
    let extension = target
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    match entry.stage_by_extension.get(&extension) {
        Some(stage) => Ok(stage.clone()),
        None => Err(unknown()),
    }
}

/// Parse a make-style depfile (design §3.4) into the dependency
/// paths it names. The stdin token (the target itself, fed on
/// stdin) is dropped; only absolute paths are returned. Backslash
/// line continuations are joined and `\ ` escapes honoured, as
/// make depfiles write them.
pub fn parse_depfile(text: &str) -> Vec<PathBuf> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut deps = Vec::new();
    for line in joined.lines() {
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let mut token = String::new();
        let mut chars = rest.chars().peekable();
        let flush = |token: &mut String, deps: &mut Vec<PathBuf>| {
            if !token.is_empty() {
                if token != TARGET_TOKEN {
                    let path = PathBuf::from(token.as_str());
                    if path.is_absolute() && !deps.contains(&path) {
                        deps.push(path);
                    }
                }
                token.clear();
            }
        };
        while let Some(ch) = chars.next() {
            match ch {
                '\\' if chars.peek() == Some(&' ') => {
                    chars.next();
                    token.push(' ');
                }
                ' ' | '\t' => flush(&mut token, &mut deps),
                _ => token.push(ch),
            }
        }
        flush(&mut token, &mut deps);
    }
    deps
}

/// One source the tool may name in a positioned diagnostic: the
/// target (token `stdin`) or a depfile dependency (token = the
/// path as printed). `bytes` is the file's content for column
/// conversion; `None` when the file could not be read within the
/// run's bounds — a diagnostic naming such a source cannot be
/// mapped honestly and invalidates the run.
#[derive(Debug, Clone)]
pub struct KnownSource {
    /// The token as the tool prints it.
    pub token: String,
    /// Absolute `file://` URI for diagnostics against this source.
    pub uri: String,
    /// File bytes for byte→UTF-16 conversion, when readable.
    pub bytes: Option<Vec<u8>>,
}

/// Inputs to one parse: everything the profile needs, nothing it
/// must fetch. Pure over these values (design §10.2 rung 2).
#[derive(Debug)]
pub struct ParseInput<'a> {
    /// Checker id, for typed errors.
    pub checker_id: &'a str,
    /// The target file's absolute URI.
    pub target_uri: &'a str,
    /// The target file's exact bytes (the hashed snapshot).
    pub target_bytes: &'a [u8],
    /// Depfile-derived known sources (excluding the target).
    pub sources: &'a [KnownSource],
    /// The tool's exit code (signal death never reaches the
    /// parser; the runner maps it to `CheckerFailed`).
    pub exit_code: i32,
    /// The tool's stderr, verbatim.
    pub stderr: &'a str,
    /// Diagnostic-count bound; crossing it is
    /// `CheckerOutputTooLarge`, never a truncated set.
    pub max_diagnostics: usize,
}

/// Parse one completed glslang run into bridge diagnostics
/// (design §5.2–§5.4). Diagnostics carry absolute URIs; the tool
/// layer relativizes them into the result form.
pub fn parse_run(input: &ParseInput) -> BridgeResult<Vec<Diagnostic>> {
    let checker = input.checker_id;
    // Exit codes outside the mappable set are CheckerFailed and
    // the output is not parsed at all (design §5.3).
    if !matches!(input.exit_code, 0 | 2 | 3) {
        return Err(BridgeError::CheckerFailed {
            checker: checker.to_string(),
            detail: format!(
                "glslangValidator exited with code {}; only 0 (success), 2 (compile \
                 failure), and 3 (link failure) carry mappable diagnostics",
                input.exit_code
            ),
        });
    }
    // Known source tokens, longest first so a path that prefixes
    // another wins the match (design §5.2 class 2).
    let mut tokens: Vec<&str> = vec![TARGET_TOKEN];
    for source in input.sources {
        tokens.push(source.token.as_str());
    }
    tokens.sort_by_key(|token| std::cmp::Reverse(token.len()));

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut aggregate: Option<u64> = None;
    let mut positioned_errors: usize = 0;
    let mut link_diagnostics: usize = 0;
    for (index, line) in input.stderr.lines().enumerate() {
        let line_number = index + 1;
        if line.is_empty() || line == TARGET_TOKEN {
            continue; // blank line or the framing line (class 5 / 1)
        }
        let invalid = |detail: String| BridgeError::CheckerOutputInvalid {
            checker: checker.to_string(),
            detail,
        };
        let (severity, rest) = if let Some(rest) = line.strip_prefix("ERROR: ") {
            (1u64, rest)
        } else if let Some(rest) = line.strip_prefix("WARNING: ") {
            (2u64, rest)
        } else {
            return Err(invalid(format!(
                "line {line_number} of the info log matches no grammar class: {}",
                excerpt(line)
            )));
        };
        // Aggregate line (class 3): parser control, not a
        // diagnostic; its count must agree with the positioned
        // compile errors, checked after the loop.
        if severity == 1
            && let Some(count) = parse_aggregate(rest)
        {
            if aggregate.is_some() {
                return Err(invalid("duplicate aggregate line in the info log".to_string()));
            }
            aggregate = Some(count);
            continue;
        }
        // Link line (class 4): a genuine defect with no position.
        // It maps to one file-level diagnostic on the target; the
        // range claims no precision.
        if severity == 1 && is_link_line(rest) {
            link_diagnostics += 1;
            diagnostics.push(Diagnostic {
                uri: input.target_uri.to_string(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
                severity,
                source: DIAGNOSTIC_SOURCE.to_string(),
                code: String::new(),
                message: rest.to_string(),
                channel: DiagnosticChannel::Checker,
            });
            continue;
        }
        // Positioned diagnostic (class 2).
        let Some((token, line_no, byte_col, message)) = match_positioned(rest, &tokens) else {
            return Err(invalid(format!(
                "line {line_number} of the info log is not a positioned diagnostic \
                 against a known source: {}",
                excerpt(line)
            )));
        };
        let (uri, bytes) = if token == TARGET_TOKEN {
            (input.target_uri, Some(input.target_bytes))
        } else {
            match input.sources.iter().find(|s| s.token == token) {
                Some(source) => (source.uri.as_str(), source.bytes.as_deref()),
                None => {
                    return Err(invalid(format!(
                        "line {line_number} names unknown source {token:?}"
                    )));
                }
            }
        };
        let Some(bytes) = bytes else {
            return Err(invalid(format!(
                "line {line_number} names source {token:?}, whose bytes are unavailable \
                 for column conversion"
            )));
        };
        let range = convert_range(checker, token, bytes, line_no, byte_col, message)?;
        if severity == 1 {
            positioned_errors += 1;
        }
        diagnostics.push(Diagnostic {
            uri: uri.to_string(),
            range,
            severity,
            source: DIAGNOSTIC_SOURCE.to_string(),
            code: String::new(),
            message: message.to_string(),
            channel: DiagnosticChannel::Checker,
        });
    }
    if let Some(expected) = aggregate
        && expected != positioned_errors as u64
    {
        return Err(BridgeError::CheckerOutputInvalid {
            checker: checker.to_string(),
            detail: format!(
                "aggregate line claims {expected} compilation errors but {positioned_errors} \
                 positioned compile errors were parsed"
            ),
        });
    }
    // Consistency rules tying output to exit status (design §5.3).
    let error_diagnostics = positioned_errors + link_diagnostics;
    match input.exit_code {
        0 if error_diagnostics > 0 => {
            return Err(BridgeError::CheckerOutputInvalid {
                checker: checker.to_string(),
                detail: format!(
                    "exit 0 (success) together with {error_diagnostics} error diagnostic(s)"
                ),
            });
        }
        2 if error_diagnostics == 0 => {
            return Err(BridgeError::CheckerFailed {
                checker: checker.to_string(),
                detail: "exit 2 (compile failure) with no mappable diagnostic".to_string(),
            });
        }
        3 if link_diagnostics == 0 => {
            return Err(BridgeError::CheckerFailed {
                checker: checker.to_string(),
                detail: "exit 3 (link failure) with no link diagnostic".to_string(),
            });
        }
        _ => {}
    }
    if diagnostics.len() > input.max_diagnostics {
        return Err(BridgeError::CheckerOutputTooLarge {
            checker: checker.to_string(),
            detail: format!(
                "{} diagnostics exceeds the max_diagnostics limit of {}; the set is \
                 refused whole, never truncated",
                diagnostics.len(),
                input.max_diagnostics
            ),
        });
    }
    Ok(diagnostics)
}

/// The aggregate line's count, when `rest` (the text after
/// `ERROR: `) is exactly `<n> compilation errors.  No code
/// generated.` (singular `error` admitted; internal spacing is
/// glslang's own and tolerated around the fixed phrases).
fn parse_aggregate(rest: &str) -> Option<u64> {
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let tail = &rest[digits.len()..];
    let tail = tail.strip_prefix(" compilation error")?;
    let tail = tail.strip_prefix('s').unwrap_or(tail);
    let tail = tail.strip_prefix('.')?;
    if tail.trim_start() != "No code generated." {
        return None;
    }
    digits.parse().ok()
}

/// Stage display names as glslang spells them in link lines —
/// NOT the `-S` tokens of [`ADMITTED_STAGES`]: the captured
/// evidence (design Appendix A.3) reads `Linking fragment stage:`,
/// and glslang's own test goldens use these six names. A link
/// line naming anything else is not silently reinterpreted; it
/// falls through and invalidates the run like any unparseable
/// line.
const LINK_STAGE_DISPLAY_NAMES: [&str; 6] = [
    "vertex",
    "tessellation control",
    "tessellation evaluation",
    "geometry",
    "fragment",
    "compute",
];

/// Whether `rest` is a link line: `Linking <stage> stage: <text>`
/// with one of glslang's stage display names.
fn is_link_line(rest: &str) -> bool {
    let Some(after) = rest.strip_prefix("Linking ") else {
        return false;
    };
    let Some((stage, _)) = after.split_once(" stage: ") else {
        return false;
    };
    LINK_STAGE_DISPLAY_NAMES.contains(&stage)
}

/// Match a positioned diagnostic body against the known source
/// tokens (longest first): `<token>:<line>:<column>: <message>`.
/// Returns the token, the 1-based line and byte column, and the
/// verbatim message.
fn match_positioned<'a>(
    rest: &'a str,
    tokens: &[&'a str],
) -> Option<(&'a str, u64, u64, &'a str)> {
    for token in tokens {
        let Some(after) = rest.strip_prefix(token) else {
            continue;
        };
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let mut parts = after.splitn(3, ':');
        let line_no: u64 = parts.next()?.parse().ok()?;
        let byte_col: u64 = parts.next()?.parse().ok()?;
        let message = parts.next()?;
        let message = message.strip_prefix(' ')?;
        if line_no == 0 || byte_col == 0 {
            return None;
        }
        return Some((token, line_no, byte_col, message));
    }
    None
}

/// Convert a reported (line, byte column) into an LSP range
/// (design §5.4): glslang's column is a 1-based UTF-8 byte offset;
/// the LSP character is a UTF-16 code-unit count over the same
/// line's bytes. A column past end-of-line or mid-codepoint is
/// `CheckerOutputInvalid`. The end is the token end when the
/// message opens with a quoted token matching the source bytes at
/// the column; otherwise the range is a point.
fn convert_range(
    checker: &str,
    token: &str,
    bytes: &[u8],
    line_no: u64,
    byte_col: u64,
    message: &str,
) -> BridgeResult<Range> {
    let invalid = |detail: String| BridgeError::CheckerOutputInvalid {
        checker: checker.to_string(),
        detail,
    };
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    let line_index = (line_no - 1) as usize;
    let Some(line_bytes) = lines.get(line_index) else {
        return Err(invalid(format!(
            "diagnostic names line {line_no} of {token:?}, which has {} lines",
            lines.len()
        )));
    };
    let byte_index = (byte_col - 1) as usize;
    if byte_index > line_bytes.len() {
        return Err(invalid(format!(
            "diagnostic column {byte_col} is past the end of line {line_no} of {token:?} \
             ({} bytes)",
            line_bytes.len()
        )));
    }
    let prefix = std::str::from_utf8(&line_bytes[..byte_index]).map_err(|_| {
        invalid(format!(
            "diagnostic column {byte_col} on line {line_no} of {token:?} falls \
             mid-codepoint; glslang columns are UTF-8 byte offsets and must land on a \
             boundary"
        ))
    })?;
    let start_character = prefix.chars().map(char::len_utf16).sum::<usize>() as u64;
    let start = Position {
        line: line_no - 1,
        character: start_character,
    };
    let mut end = start;
    if let Some(token_text) = leading_quoted_token(message)
        && line_bytes[byte_index..].starts_with(token_text.as_bytes())
    {
        let token_utf16 = token_text.chars().map(char::len_utf16).sum::<usize>() as u64;
        end = Position {
            line: line_no - 1,
            character: start_character + token_utf16,
        };
    }
    Ok(Range { start, end })
}

/// The token inside the message's leading `'...'` quoting, when
/// the message opens with one (glslang's `'name' : ...` shape).
fn leading_quoted_token(message: &str) -> Option<&str> {
    let inner = message.strip_prefix('\'')?;
    let close = inner.find('\'')?;
    Some(&inner[..close])
}

/// A bounded excerpt of an offending output line for a typed
/// error: the first 200 bytes, on a char boundary (design §7).
fn excerpt(line: &str) -> String {
    if line.len() <= 200 {
        return format!("{line:?}");
    }
    let mut end = 200;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:?}...", &line[..end])
}

/// The depfile-derived known sources for one run, in depfile
/// order. A small constructor so the runner and the tests build
/// them identically.
pub fn known_source(path: &Path, bytes: Option<Vec<u8>>) -> KnownSource {
    KnownSource {
        token: path.to_string_lossy().into_owned(),
        uri: path_to_uri(path),
        bytes,
    }
}

/// Absolute paths the info log itself names as positioned
/// diagnostic sources. This exists because of a measured tool
/// behaviour (primo, 2026-11-24): glslang writes NO depfile on a
/// failed compile, so for exactly the runs that carry diagnostics
/// about included files, the depfile cannot name their sources —
/// yet design §9.1.6 requires those diagnostics reported against
/// the included file's URI. The runner completes the source set
/// from the log's own tokens (each candidate still has to
/// canonicalize inside the workspace and be readable before the
/// parser will map against it); the depfile remains the cache's
/// freshness evidence, and a run without one is never cached.
/// Pure scan: for each `ERROR: `/`WARNING: ` line, the longest
/// prefix that is an absolute path followed by `:<line>:<col>: `.
pub fn output_named_sources(stderr: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for line in stderr.lines() {
        let Some(rest) = line
            .strip_prefix("ERROR: ")
            .or_else(|| line.strip_prefix("WARNING: "))
        else {
            continue;
        };
        let bytes = rest.as_bytes();
        let mut best: Option<&str> = None;
        for i in 0..bytes.len() {
            if bytes[i] != b':' {
                continue;
            }
            // Parse ":<line>:<column>: " starting at i.
            let mut j = i + 1;
            let digits = |j: &mut usize| {
                let start = *j;
                while *j < bytes.len() && bytes[*j].is_ascii_digit() {
                    *j += 1;
                }
                *j > start
            };
            if !digits(&mut j) || j >= bytes.len() || bytes[j] != b':' {
                continue;
            }
            j += 1;
            if !digits(&mut j) || j + 1 >= bytes.len() || bytes[j] != b':' || bytes[j + 1] != b' '
            {
                continue;
            }
            let token = &rest[..i];
            if token.starts_with('/') && best.is_none_or(|b| token.len() > b.len()) {
                best = Some(token);
            }
        }
        if let Some(token) = best {
            let path = PathBuf::from(token);
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Stage map lookup used by tests and the runner alike: the
/// extensions a checker claims, for fixture construction.
pub fn stage_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
