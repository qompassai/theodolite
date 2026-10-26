//! WorkspaceEdit shaping, preview rendering, and the single
//! edit applier (SPEC §6 mutation class, P5 items 1 and 3).
//!
//! A server-returned `WorkspaceEdit` is untrusted input: it is
//! shaped into one canonical form here (per-file edit lists sorted
//! by (uri, range, new_text), exact duplicates dropped), rendered
//! for previews, and — only through the gated apply tools —
//! written by the applier in this module, which is the bridge's
//! ONLY write path. Two hard rules:
//!
//! - A `documentChanges` entry carrying a file create / rename /
//!   delete operation is a typed refusal, never silently dropped
//!   and never applied (the bridge edits file *contents* only).
//! - The applier validates everything (workspace boundary, file
//!   existence, range spans, edit overlap) for every target file
//!   before writing anything, then writes per file atomically
//!   (write-temp + rename, original permissions preserved). A
//!   refused apply writes zero bytes anywhere.

use std::path::{Path, PathBuf};

use crate::errors::{BridgeError, BridgeResult, io_error};
use crate::session::resolve_within;
use crate::shaping::{Position, Range, parse_range, relativize_uri, uri_to_path};

/// One text replacement inside one file, in LSP range terms
/// (0-based lines, UTF-16 code-unit columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// The range being replaced.
    pub range: Range,
    /// The replacement text.
    pub new_text: String,
}

/// All edits for one file. `uri` is the absolute `file://` URI the
/// server named; relativization happens only at result rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdits {
    /// Absolute `file://` URI of the target document.
    pub uri: String,
    /// Edits, canonically sorted by (range, new_text).
    pub edits: Vec<TextEdit>,
}

/// Shape a raw LSP `WorkspaceEdit` (either wire form) into the
/// canonical per-file list. `null` shapes to an empty list (a
/// server answering "no edits" is a real answer). Malformed entries
/// and file operations are typed errors.
pub fn shape_workspace_edit(raw: &serde_json::Value) -> BridgeResult<Vec<FileEdits>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let mut by_uri: Vec<FileEdits> = Vec::new();
    if let Some(changes) = raw.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| BridgeError::InvalidArgument {
                detail: "WorkspaceEdit.changes is not an object".to_string(),
            })?;
        for (uri, edits) in changes {
            let parsed = parse_text_edits(edits)?;
            push_file_edits(&mut by_uri, uri, parsed);
        }
    }
    if let Some(document_changes) = raw.get("documentChanges") {
        let entries = document_changes
            .as_array()
            .ok_or_else(|| BridgeError::InvalidArgument {
                detail: "WorkspaceEdit.documentChanges is not an array".to_string(),
            })?;
        for entry in entries {
            // A resource operation (create / rename / delete of a
            // FILE) carries a `kind` field. The bridge refuses the
            // whole edit set: applying the text edits and dropping
            // the file operation would silently change the meaning
            // of the server's answer.
            if let Some(kind) = entry.get("kind").and_then(serde_json::Value::as_str) {
                return Err(BridgeError::UnsupportedMethod {
                    detail: format!(
                        "WorkspaceEdit carries a file {kind} operation; the bridge edits \
                         file contents only and refuses the whole edit set"
                    ),
                });
            }
            let uri = entry
                .get("textDocument")
                .and_then(|doc| doc.get("uri"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| BridgeError::InvalidArgument {
                    detail: "documentChanges entry has no textDocument.uri".to_string(),
                })?;
            let edits = entry
                .get("edits")
                .ok_or_else(|| BridgeError::InvalidArgument {
                    detail: "documentChanges entry has no edits array".to_string(),
                })?;
            let parsed = parse_text_edits(edits)?;
            push_file_edits(&mut by_uri, uri, parsed);
        }
    }
    by_uri.sort_by(|a, b| a.uri.cmp(&b.uri));
    Ok(by_uri)
}

fn push_file_edits(accumulator: &mut Vec<FileEdits>, uri: &str, mut edits: Vec<TextEdit>) {
    edits.sort_by(|a, b| a.range.cmp(&b.range).then(a.new_text.cmp(&b.new_text)));
    edits.dedup();
    if edits.is_empty() {
        return;
    }
    match accumulator.iter_mut().find(|file| file.uri == uri) {
        Some(existing) => {
            existing.edits.extend(edits);
            existing
                .edits
                .sort_by(|a, b| a.range.cmp(&b.range).then(a.new_text.cmp(&b.new_text)));
            existing.edits.dedup();
        }
        None => accumulator.push(FileEdits {
            uri: uri.to_string(),
            edits,
        }),
    }
}

fn parse_text_edits(raw: &serde_json::Value) -> BridgeResult<Vec<TextEdit>> {
    let items = raw.as_array().ok_or_else(|| BridgeError::InvalidArgument {
        detail: "WorkspaceEdit edit list is not an array".to_string(),
    })?;
    let mut edits = Vec::with_capacity(items.len());
    for item in items {
        let range = parse_range(item.get("range").unwrap_or(&serde_json::Value::Null));
        if range.end < range.start {
            return Err(BridgeError::InvalidArgument {
                detail: "WorkspaceEdit contains an edit whose end precedes its start".to_string(),
            });
        }
        let new_text = item
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BridgeError::InvalidArgument {
                detail: "WorkspaceEdit edit has no string newText".to_string(),
            })?
            .to_string();
        edits.push(TextEdit { range, new_text });
    }
    Ok(edits)
}

/// Total edit count across a shaped edit set.
pub fn total_edits(files: &[FileEdits]) -> usize {
    files.iter().map(|file| file.edits.len()).sum()
}

/// The text a range covers in `content`, for the diff-shaped
/// preview rendering ("old text"). Columns are UTF-16 code units
/// (the LSP convention). `None` when the range falls outside the
/// document — the preview then renders `null` for that edit rather
/// than inventing text.
pub fn extract_range_text(content: &str, range: Range) -> Option<String> {
    let start = byte_offset(content, range.start)?;
    let end = byte_offset(content, range.end)?;
    if end < start {
        return None;
    }
    content.get(start..end).map(str::to_string)
}

/// Byte offset of an LSP position inside `content`, or `None` when
/// the position is past the document (or splits a surrogate pair,
/// which a byte offset cannot express).
fn byte_offset(content: &str, position: Position) -> Option<usize> {
    let line_index = usize::try_from(position.line).ok()?;
    let mut line_start = 0usize;
    for _ in 0..line_index {
        let newline = content[line_start..].find('\n')? + line_start;
        line_start = newline + 1;
        if line_start > content.len() {
            return None;
        }
    }
    let line_end = content[line_start..]
        .find('\n')
        .map(|index| line_start + index)
        .unwrap_or(content.len());
    let line_text = &content[line_start..line_end];
    // Columns count UTF-16 code units; walk the line's chars.
    let mut units: u64 = 0;
    for (index, ch) in line_text.char_indices() {
        if units == position.character {
            return Some(line_start + index);
        }
        units += u64::try_from(ch.len_utf16()).ok()?;
        if units > position.character {
            return None; // the column splits a surrogate pair
        }
    }
    if units == position.character {
        return Some(line_end);
    }
    None
}

/// One file's outcome from the applier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFile {
    /// Absolute `file://` URI that was written.
    pub uri: String,
    /// How many edits were written into it.
    pub edits_applied: usize,
}

/// Apply a shaped edit set — the bridge's ONLY write path (P5
/// items 3–4). Two phases: phase 1 resolves and validates every
/// target (workspace boundary, existence, UTF-8 content, span
/// bounds, overlap) and computes every new content in memory;
/// phase 2 writes each file atomically. Any refusal happens in
/// phase 1, so a refused apply writes nothing anywhere.
pub fn apply_workspace_edit(
    workspace_root: &Path,
    files: &[FileEdits],
) -> BridgeResult<Vec<AppliedFile>> {
    // Phase 1: compute, writing nothing.
    let mut planned: Vec<PlannedFile> = Vec::with_capacity(files.len());
    for file in files {
        let path = uri_to_path(&file.uri)?;
        let absolute = resolve_within(workspace_root, &path.to_string_lossy())?;
        let bytes = std::fs::read(&absolute).map_err(|e| io_error("reading edit target", e))?;
        let content = String::from_utf8(bytes).map_err(|_| BridgeError::InvalidArgument {
            detail: format!("edit target {} is not valid UTF-8", file.uri),
        })?;
        let permissions = std::fs::metadata(&absolute)
            .map_err(|e| io_error("stat-ing edit target", e))?
            .permissions();
        let new_content = apply_edits_to_content(&content, &file.edits, &file.uri)?;
        planned.push(PlannedFile {
            uri: file.uri.clone(),
            path: absolute,
            permissions,
            new_content,
            edits_applied: file.edits.len(),
        });
    }
    // Phase 2: write each file atomically (temp sibling + rename).
    let mut applied = Vec::with_capacity(planned.len());
    for plan in planned {
        write_atomically(&plan)?;
        applied.push(AppliedFile {
            uri: plan.uri,
            edits_applied: plan.edits_applied,
        });
    }
    Ok(applied)
}

struct PlannedFile {
    uri: String,
    path: PathBuf,
    permissions: std::fs::Permissions,
    new_content: String,
    edits_applied: usize,
}

fn write_atomically(plan: &PlannedFile) -> BridgeResult<()> {
    let directory = plan.path.parent().ok_or_else(|| BridgeError::Io {
        detail: format!("edit target {} has no parent directory", plan.uri),
    })?;
    let file_name = plan
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = directory.join(format!(
        ".{file_name}.theodolite-tmp-{}",
        std::process::id()
    ));
    let outcome = (|| -> BridgeResult<()> {
        std::fs::write(&temp, plan.new_content.as_bytes())
            .map_err(|e| io_error("writing edit temp file", e))?;
        std::fs::set_permissions(&temp, plan.permissions.clone())
            .map_err(|e| io_error("setting edit temp permissions", e))?;
        std::fs::rename(&temp, &plan.path).map_err(|e| io_error("renaming edit temp file", e))?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    outcome
}

/// Apply one file's edits to its content, validating spans and
/// overlap. Edits are applied last-to-first so earlier offsets stay
/// valid; insertions at one point land in canonical (sorted) order.
fn apply_edits_to_content(content: &str, edits: &[TextEdit], uri: &str) -> BridgeResult<String> {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| a.range.cmp(&b.range).then(a.new_text.cmp(&b.new_text)));
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.range.start < b.range.end && b.range.start < a.range.end {
            return Err(BridgeError::InvalidArgument {
                detail: format!("overlapping edits for {uri}; the edit set is refused whole"),
            });
        }
    }
    let mut result = content.to_string();
    for edit in sorted.iter().rev() {
        let start =
            byte_offset(&result, edit.range.start).ok_or_else(|| BridgeError::InvalidArgument {
                detail: format!("edit start is outside {uri}"),
            })?;
        let end =
            byte_offset(&result, edit.range.end).ok_or_else(|| BridgeError::InvalidArgument {
                detail: format!("edit end is outside {uri}"),
            })?;
        if end < start {
            return Err(BridgeError::InvalidArgument {
                detail: format!("edit end precedes its start in {uri}"),
            });
        }
        result.replace_range(start..end, &edit.new_text);
    }
    Ok(result)
}

/// Render a shaped edit set as the canonical preview/apply file
/// list: workspace-relative URIs, per-edit old text derived from the
/// current on-disk content (the documents are synced before any
/// rename is computed, so disk content is the server's view), and
/// per-file counts. Shared by `rename_preview` (applied = false)
/// and `rename_apply` (applied = true) so the two results can never
/// drift apart in shape.
pub fn render_file_list(
    workspace_root: &Path,
    files: &[FileEdits],
) -> BridgeResult<Vec<serde_json::Value>> {
    let mut rendered = Vec::with_capacity(files.len());
    for file in files {
        let relative_uri = relativize_uri(&file.uri, workspace_root)?;
        let content = uri_to_path(&file.uri)
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let edits: Vec<serde_json::Value> = file
            .edits
            .iter()
            .map(|edit| {
                let old_text = content
                    .as_deref()
                    .and_then(|text| extract_range_text(text, edit.range));
                serde_json::json!({
                    "range": crate::shaping::range_json(edit.range),
                    "old_text": old_text,
                    "new_text": edit.new_text,
                })
            })
            .collect();
        rendered.push(serde_json::json!({
            "uri": relative_uri,
            "edit_count": edits.len(),
            "edits": edits,
        }));
    }
    Ok(rendered)
}
