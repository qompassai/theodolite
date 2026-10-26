//! WorkspaceEdit shaping and preview rendering (SPEC §6 mutation
//! class, P5 item 1; the applier half lands with item 3 in this
//! same module).
//!
//! A server-returned `WorkspaceEdit` is untrusted input: it is
//! shaped into one canonical form here (per-file edit lists sorted
//! by (uri, range, new_text), exact duplicates dropped) and
//! rendered for previews. One hard rule already applies at shaping
//! time: a `documentChanges` entry carrying a file create / rename
//! / delete operation is a typed refusal, never silently dropped
//! and never applied (the bridge edits file *contents* only).

use std::path::Path;

use crate::errors::{BridgeError, BridgeResult};
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
