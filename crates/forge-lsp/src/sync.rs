//! LSP document synchronization: UTF-16 coordinate mapping, version tracking,
//! and incremental transaction-to-didChange translation.

use forge_buffer::Edit;
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, Position, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
};
use ropey::Rope;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Converts a local filesystem path to an `lsp_types::Uri`.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let url = url::Url::from_file_path(path).ok()?;
    Uri::from_str(url.as_str()).ok()
}

/// Converts an `lsp_types::Uri` to a local filesystem `PathBuf`.
#[must_use]
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    url.to_file_path().ok()
}

/// Converts a 0-indexed char offset in `rope` to an `lsp_types::Position` (UTF-16 line/col).
#[must_use]
pub fn offset_to_position(rope: &Rope, char_offset: usize) -> Position {
    let char_offset = char_offset.min(rope.len_chars());
    let line = rope.char_to_line(char_offset);
    let line_start_char = rope.line_to_char(line);

    let line_slice = rope.slice(line_start_char..char_offset);
    let utf16_col: usize = line_slice.chars().map(char::len_utf16).sum();

    Position {
        line: u32::try_from(line).unwrap_or(u32::MAX),
        character: u32::try_from(utf16_col).unwrap_or(u32::MAX),
    }
}

/// Converts an `lsp_types::Position` (UTF-16 line/col) to a 0-indexed char offset in `rope`.
#[must_use]
pub fn position_to_offset(rope: &Rope, pos: Position) -> usize {
    let line_idx = pos.line as usize;
    if line_idx >= rope.len_lines() {
        return rope.len_chars();
    }

    let line_start_char = rope.line_to_char(line_idx);
    let line_slice = rope.line(line_idx);

    let target_utf16 = pos.character as usize;
    let mut current_utf16 = 0;
    let mut chars_count = 0;

    for c in line_slice.chars() {
        if c == '\n' || c == '\r' || current_utf16 + c.len_utf16() > target_utf16 {
            break;
        }
        current_utf16 += c.len_utf16();
        chars_count += 1;
    }

    line_start_char + chars_count
}

/// Converts a char range in `rope` to an `lsp_types::Range`.
#[must_use]
pub fn range_to_lsp_range(rope: &Rope, range: &Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: offset_to_position(rope, range.start),
        end: offset_to_position(rope, range.end),
    }
}

/// Converts an `lsp_types::Range` to a char range in `rope`.
#[must_use]
pub fn lsp_range_to_range(rope: &Rope, lsp_range: &lsp_types::Range) -> Range<usize> {
    let start = position_to_offset(rope, lsp_range.start);
    let end = position_to_offset(rope, lsp_range.end);
    start..end.max(start)
}

/// Translates non-overlapping buffer `edits` (relative to `old_rope`) into
/// an array of `TextDocumentContentChangeEvent` applied in reverse order.
#[must_use]
pub fn transaction_to_changes(
    old_rope: &Rope,
    edits: &[Edit],
) -> Vec<TextDocumentContentChangeEvent> {
    // Applying edits in descending order of position ensures that each edit's
    // range remains valid in the coordinate space of the prior document state.
    let mut sorted_indices: Vec<usize> = (0..edits.len()).collect();
    sorted_indices.sort_by_key(|&idx| std::cmp::Reverse(edits[idx].range.start));

    sorted_indices
        .into_iter()
        .map(|idx| {
            let edit = &edits[idx];
            let lsp_range = range_to_lsp_range(old_rope, &edit.range);
            let utf16_len: usize = old_rope
                .slice(edit.range.clone())
                .chars()
                .map(char::len_utf16)
                .sum();

            #[allow(deprecated)]
            TextDocumentContentChangeEvent {
                range: Some(lsp_range),
                range_length: u32::try_from(utf16_len).ok(),
                text: edit.text.clone(),
            }
        })
        .collect()
}

/// State of an open document tracked by the LSP manager.
#[derive(Debug, Clone)]
pub struct TrackedDocument {
    pub uri: Uri,
    pub language_id: String,
    pub version: i32,
    pub text: String,
}

/// Tracks open documents, their versions, and generates sync params.
#[derive(Debug, Default)]
pub struct DocumentTracker {
    documents: HashMap<Uri, TrackedDocument>,
}

impl DocumentTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a newly opened document and returns the `didOpen` params.
    pub fn did_open(
        &mut self,
        uri: Uri,
        language_id: String,
        text: String,
    ) -> DidOpenTextDocumentParams {
        self.did_open_versioned(uri, language_id, text, 1)
    }

    /// Registers the actual live buffer revision, including unsaved files.
    pub fn did_open_versioned(
        &mut self,
        uri: Uri,
        language_id: String,
        text: String,
        version: i32,
    ) -> DidOpenTextDocumentParams {
        let item = TextDocumentItem {
            uri: uri.clone(),
            language_id: language_id.clone(),
            version,
            text: text.clone(),
        };

        self.documents.insert(
            uri,
            TrackedDocument {
                uri: item.uri.clone(),
                language_id,
                version,
                text,
            },
        );

        DidOpenTextDocumentParams {
            text_document: item,
        }
    }

    /// Increments the document version and generates `didChange` params with incremental edits.
    pub fn did_change_incremental(
        &mut self,
        uri: &Uri,
        edits: &[Edit],
        old_rope: &Rope,
        new_text: String,
    ) -> Option<DidChangeTextDocumentParams> {
        let doc = self.documents.get_mut(uri)?;
        doc.version += 1;
        let version = doc.version;
        doc.text = new_text;

        let content_changes = transaction_to_changes(old_rope, edits);

        Some(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes,
        })
    }

    /// Increments the document version and generates `didChange` params with full document text.
    pub fn did_change_full(
        &mut self,
        uri: &Uri,
        new_text: String,
    ) -> Option<DidChangeTextDocumentParams> {
        let doc = self.documents.get_mut(uri)?;
        doc.version += 1;
        let version = doc.version;
        doc.text.clone_from(&new_text);

        Some(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: new_text,
            }],
        })
    }

    /// Generates `didSave` params for an open document.
    pub fn did_save(&self, uri: &Uri, include_text: bool) -> Option<DidSaveTextDocumentParams> {
        let doc = self.documents.get(uri)?;
        Some(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            text: include_text.then(|| doc.text.clone()),
        })
    }

    /// Unregisters an open document and returns `didClose` params.
    pub fn did_close(&mut self, uri: &Uri) -> Option<DidCloseTextDocumentParams> {
        self.documents.remove(uri)?;
        Some(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
        })
    }

    #[must_use]
    pub fn get(&self, uri: &Uri) -> Option<&TrackedDocument> {
        self.documents.get(uri)
    }

    #[must_use]
    pub fn version(&self, uri: &Uri) -> Option<i32> {
        self.documents.get(uri).map(|doc| doc.version)
    }

    /// Sets a real buffer revision after coalescing multiple transactions.
    pub fn set_version(&mut self, uri: &Uri, version: i32) {
        if let Some(doc) = self.documents.get_mut(uri) {
            doc.version = version;
        }
    }

    #[must_use]
    pub fn is_open(&self, uri: &Uri) -> bool {
        self.documents.contains_key(uri)
    }

    /// Returns a list of all currently tracked documents (for re-didOpen after server restarts).
    #[must_use]
    pub fn all_documents(&self) -> Vec<TrackedDocument> {
        self.documents.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf16_position_ascii() {
        let text = "hello\nworld";
        let rope = Rope::from_str(text);

        let pos = offset_to_position(&rope, 8); // 'r' in "world" -> line 1, col 2
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 2);

        let offset = position_to_offset(&rope, pos);
        assert_eq!(offset, 8);
    }

    #[test]
    fn test_utf16_position_multibyte_and_emoji() {
        // "añó 🦀!\nend"
        // 0: 'a'  (utf16: 0..1)
        // 1: 'ñ'  (utf16: 1..2)
        // 2: 'ó'  (utf16: 2..3)
        // 3: ' '  (utf16: 3..4)
        // 4: '🦀' (utf16: 4..6)
        // 5: '!'  (utf16: 6..7)
        // 6: '\n'
        // 7: 'e'  (line 1, col 0)
        // 8: 'n'  (line 1, col 1)
        // 9: 'd'  (line 1, col 2)
        let text = "añó 🦀!\nend";
        let rope = Rope::from_str(text);

        let pos_exclamation = offset_to_position(&rope, 5);
        assert_eq!(pos_exclamation.line, 0);
        assert_eq!(pos_exclamation.character, 6);

        let back_offset = position_to_offset(&rope, pos_exclamation);
        assert_eq!(back_offset, 5);

        let pos_end = offset_to_position(&rope, 8);
        assert_eq!(pos_end.line, 1);
        assert_eq!(pos_end.character, 1);

        let back_end = position_to_offset(&rope, pos_end);
        assert_eq!(back_end, 8);
    }

    #[test]
    fn test_document_tracker_lifecycle() {
        let mut tracker = DocumentTracker::new();
        let uri = Uri::from_str("file:///src/main.rs").unwrap();

        // 1. Open
        let open_params =
            tracker.did_open(uri.clone(), "rust".to_string(), "fn main() {}".to_string());
        assert_eq!(open_params.text_document.version, 1);
        assert_eq!(tracker.version(&uri), Some(1));

        // 2. Incremental change
        let old_rope = Rope::from_str("fn main() {}");
        let edit = Edit::insert(11, "\n    println!(\"hello\");\n");
        let change_params = tracker
            .did_change_incremental(
                &uri,
                &[edit],
                &old_rope,
                "fn main() {\n    println!(\"hello\");\n}".to_string(),
            )
            .unwrap();
        assert_eq!(change_params.text_document.version, 2);
        assert_eq!(change_params.content_changes.len(), 1);

        // 3. Save
        let save_params = tracker.did_save(&uri, true).unwrap();
        assert!(save_params.text.unwrap().contains("println"));

        // 4. Close
        let close_params = tracker.did_close(&uri).unwrap();
        assert_eq!(close_params.text_document.uri, uri);
        assert!(!tracker.is_open(&uri));
    }
}
