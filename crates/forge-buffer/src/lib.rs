//! Text buffer of the Forge editor (ARCHITECTURE.md §13).
//!
//! A [`Buffer`] is a persistent rope plus the state that turns it into an
//! editable document: a version counter, a transactional undo history,
//! multiple selections (Helix model: the cursor is a selection) and an
//! optional append-only [`Journal`] for crash recovery. Positions are
//! **char indices** into the rope; byte offsets for tree-sitter and LSP
//! come from [`Buffer::byte_of`] / [`Buffer::char_of`].

pub mod file;
pub mod journal;
pub mod large;
pub mod motion;
pub mod proposed;
pub mod selection;
pub mod transaction;

pub use file::{LineEnding, LoadedFile};
pub use journal::Journal;
pub use large::LargeFile;
pub use motion::{Cursor, Motion};
pub use proposed::{HunkStatus, ProposedEdit, ProposedHunk};
pub use selection::{Position, Selection, Selections};
pub use transaction::{Edit, Transaction};

use ropey::Rope;
use std::{
    ops::Range,
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BufferError {
    #[error("edit range {start}..{end} is invalid for {len} characters")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("edits overlap at char {at}")]
    Overlap { at: usize },
    #[error("journal: {0}")]
    Journal(String),
}

/// Consecutive edits closer than this are undone together.
pub const UNDO_GROUP_WINDOW: Duration = Duration::from_millis(300);

/// One undo step: the applied transaction and the one that reverts it.
#[derive(Debug, Clone)]
struct HistoryEntry {
    forward: Transaction,
    inverse: Transaction,
    at: Instant,
    /// Typing-like transactions merge into the previous entry when close in
    /// time; explicit edits (paste, replace-all) never do.
    mergeable: bool,
}

/// One edit as it landed in the text, for consumers that track the buffer
/// incrementally (tree-sitter, LSP): where the new text starts, what went
/// in and what came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub new_start_char: usize,
    pub inserted: String,
    pub removed: String,
}

/// Not `Clone`: the journal is a file handle. Background work takes
/// [`Buffer::rope`] snapshots instead.
#[derive(Debug)]
pub struct Buffer {
    text: Rope,
    version: u64,
    /// Edits of the transaction that produced `version`, in text order.
    last_change: Vec<AppliedEdit>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    selections: Selections,
    /// Version at the last save/load; `dirty` compares against it.
    saved_version: u64,
    journal: Option<Journal>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new("")
    }
}

impl Buffer {
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
            version: 0,
            last_change: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            selections: Selections::default(),
            saved_version: 0,
            journal: None,
        }
    }

    // ----- reading ----------------------------------------------------------

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Returns transactions applied between `version` and current version,
    /// or `None` if `version` is not in history.
    #[must_use]
    pub fn transactions_since(&self, version: u64) -> Option<Vec<&Transaction>> {
        if version > self.version {
            return None;
        }
        if version == self.version {
            return Some(Vec::new());
        }
        let mut result = Vec::new();
        for entry in &self.undo {
            if entry.forward.version_before >= version {
                result.push(&entry.forward);
            }
        }
        Some(result)
    }

    /// The edits that took the buffer from `version() - 1` to `version()`,
    /// in text order with positions in the current text.
    #[must_use]
    pub fn last_change(&self) -> &[AppliedEdit] {
        &self.last_change
    }

    /// Snapshot of the text; O(1), for background work (parsing, search).
    #[must_use]
    pub fn rope(&self) -> Rope {
        self.text.clone()
    }

    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.text.len_bytes()
    }

    #[must_use]
    pub fn len_lines(&self) -> usize {
        self.text.len_lines()
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Text of line `line` without its terminator; `None` past the end.
    #[must_use]
    pub fn line(&self, line: usize) -> Option<String> {
        (line < self.len_lines()).then(|| {
            let slice = self.text.line(line);
            let text = slice.to_string();
            text.trim_end_matches(['\n', '\r']).to_owned()
        })
    }

    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.len_chars());
        let start = range.start.min(end);
        self.text.slice(start..end).to_string()
    }

    #[must_use]
    pub fn byte_of(&self, char_idx: usize) -> usize {
        self.text.char_to_byte(char_idx.min(self.len_chars()))
    }

    #[must_use]
    pub fn char_of(&self, byte_idx: usize) -> usize {
        self.text.byte_to_char(byte_idx.min(self.len_bytes()))
    }

    /// Line/column (chars) of a char index, clamped to the text.
    #[must_use]
    pub fn position_of(&self, char_idx: usize) -> Position {
        let char_idx = char_idx.min(self.len_chars());
        let line = self.text.char_to_line(char_idx);
        Position {
            line,
            column: char_idx - self.text.line_to_char(line),
        }
    }

    /// Char index of a position; the column is clamped to the line.
    #[must_use]
    pub fn char_at(&self, position: Position) -> usize {
        let line = position.line.min(self.len_lines().saturating_sub(1));
        let start = self.text.line_to_char(line);
        let length = self.line_len_chars(line);
        start + position.column.min(length)
    }

    /// Chars in `line` excluding its terminator.
    #[must_use]
    pub fn line_len_chars(&self, line: usize) -> usize {
        if line >= self.len_lines() {
            return 0;
        }
        let slice = self.text.line(line);
        let mut length = slice.len_chars();
        let mut chars = slice.chars_at(length);
        while let Some(c) = chars.prev() {
            if c == '\n' || c == '\r' {
                length -= 1;
            } else {
                break;
            }
        }
        length
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    /// Marks the current content as what is on disk.
    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
        if let Some(journal) = &mut self.journal {
            journal.truncate();
        }
    }

    #[must_use]
    pub fn selections(&self) -> &Selections {
        &self.selections
    }

    pub fn set_selections(&mut self, selections: Selections) {
        let len = self.len_chars();
        self.selections = selections.clamped(len).normalized();
    }

    // ----- editing ----------------------------------------------------------

    /// Applies `edits` as one transaction, mapping selections through it.
    /// Returns the transaction actually applied (edits sorted).
    ///
    /// # Errors
    ///
    /// Out-of-range or overlapping edits; nothing is applied then.
    pub fn edit(&mut self, edits: Vec<Edit>, mergeable: bool) -> Result<Transaction, BufferError> {
        let selections_after = self
            .selections
            .clone()
            .map_through(&edits)
            .clamped(Self::len_after(self.len_chars(), &edits)?);
        self.edit_with_selections(edits, selections_after, mergeable)
    }

    /// Like [`Self::edit`] with explicit selections for after the edit.
    ///
    /// # Errors
    ///
    /// Out-of-range or overlapping edits; nothing is applied then.
    pub fn edit_with_selections(
        &mut self,
        edits: Vec<Edit>,
        selections_after: Selections,
        mergeable: bool,
    ) -> Result<Transaction, BufferError> {
        let transaction = Transaction {
            edits: Transaction::sorted(edits, self.len_chars())?,
            selections_before: self.selections.clone(),
            selections_after,
            version_before: self.version,
        };
        let inverse = self.apply(&transaction);
        self.redo.clear();
        let now = Instant::now();
        let merged = mergeable
            && self.undo.last().is_some_and(|last| {
                last.mergeable && now.duration_since(last.at) <= UNDO_GROUP_WINDOW
            });
        if let Some(last) = self.undo.last_mut().filter(|_| merged) {
            last.forward = last.forward.then(&transaction);
            last.inverse = inverse.then(&last.inverse);
            last.at = now;
        } else {
            self.undo.push(HistoryEntry {
                forward: transaction.clone(),
                inverse,
                at: now,
                mergeable,
            });
        }
        if let Some(journal) = &mut self.journal
            && let Err(error) = journal.append(&transaction)
        {
            return Err(BufferError::Journal(error.to_string()));
        }
        Ok(transaction)
    }

    /// Replaces every selection with `text` (typing, paste); the cursors end
    /// after the inserted text.
    ///
    /// # Errors
    ///
    /// Never for valid selections; propagates journal failures.
    pub fn insert(&mut self, text: &str, mergeable: bool) -> Result<Transaction, BufferError> {
        let edits: Vec<Edit> = self
            .selections
            .iter()
            .map(|selection| Edit {
                range: selection.range(),
                text: text.to_owned(),
            })
            .collect();
        self.edit(edits, mergeable)
    }

    /// Deletes every selection; empty selections delete `before` chars to the
    /// left (Backspace) or `after` chars to the right (Delete).
    ///
    /// # Errors
    ///
    /// Propagates journal failures.
    pub fn delete(&mut self, before: usize, after: usize) -> Result<Transaction, BufferError> {
        let len = self.len_chars();
        let edits: Vec<Edit> = self
            .selections
            .iter()
            .map(|selection| {
                let range = selection.range();
                let range = if range.is_empty() {
                    range.start.saturating_sub(before)..(range.end + after).min(len)
                } else {
                    range
                };
                Edit {
                    range,
                    text: String::new(),
                }
            })
            .collect();
        self.edit(edits, true)
    }

    /// Reverts the last undo group. Returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        let Some(entry) = self.undo.pop() else {
            return false;
        };
        self.apply(&entry.inverse);
        self.selections = entry.forward.selections_before.clone();
        self.redo.push(entry);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(entry) = self.redo.pop() else {
            return false;
        };
        self.apply(&entry.forward);
        self.selections = entry.forward.selections_after.clone();
        self.undo.push(entry);
        true
    }

    /// Applies a transaction from a journal or an agent proposal exactly as
    /// recorded, bypassing history grouping. Versions must line up.
    ///
    /// # Errors
    ///
    /// Version mismatch or invalid ranges.
    pub fn replay(&mut self, transaction: &Transaction) -> Result<(), BufferError> {
        if transaction.version_before != self.version {
            return Err(BufferError::Journal(format!(
                "transaction for version {} but buffer is at {}",
                transaction.version_before, self.version
            )));
        }
        Transaction::sorted(transaction.edits.clone(), self.len_chars())?;
        let inverse = self.apply(transaction);
        self.undo.push(HistoryEntry {
            forward: transaction.clone(),
            inverse,
            at: Instant::now(),
            mergeable: false,
        });
        Ok(())
    }

    /// Applies the edits (last first, so earlier ranges stay valid), bumps
    /// the version and returns the transaction that undoes it.
    fn apply(&mut self, transaction: &Transaction) -> Transaction {
        let mut inverse_edits = Vec::with_capacity(transaction.edits.len());
        let mut applied = Vec::with_capacity(transaction.edits.len());
        // Positions in the inverse are in the *new* text; track the drift of
        // earlier edits so each inverse range lands where the text ended up.
        let mut drift: isize = 0;
        for edit in &transaction.edits {
            let start = edit.range.start;
            let removed = self.slice(edit.range.clone());
            let new_start =
                usize::try_from(isize::try_from(start).unwrap_or(0) + drift).unwrap_or(0);
            let inserted_len = edit.text.chars().count();
            applied.push(AppliedEdit {
                new_start_char: new_start,
                inserted: edit.text.clone(),
                removed: removed.clone(),
            });
            inverse_edits.push(Edit {
                range: new_start..new_start + inserted_len,
                text: removed,
            });
            drift += isize::try_from(inserted_len).unwrap_or(0)
                - isize::try_from(edit.range.len()).unwrap_or(0);
        }
        for edit in transaction.edits.iter().rev() {
            self.text.remove(edit.range.clone());
            if !edit.text.is_empty() {
                self.text.insert(edit.range.start, &edit.text);
            }
        }
        self.version += 1;
        self.last_change = applied;
        self.selections = transaction.selections_after.clone();
        Transaction {
            edits: inverse_edits,
            selections_before: transaction.selections_after.clone(),
            selections_after: transaction.selections_before.clone(),
            version_before: self.version,
        }
    }

    fn len_after(len: usize, edits: &[Edit]) -> Result<usize, BufferError> {
        let sorted = Transaction::sorted(edits.to_vec(), len)?;
        Ok(sorted.iter().fold(len, |acc, edit| {
            acc + edit.text.chars().count() - edit.range.len()
        }))
    }

    // ----- journal ------------------------------------------------------------

    /// Records every transaction from now on into `journal`.
    pub fn attach_journal(&mut self, journal: Journal) {
        self.journal = Some(journal);
    }

    #[must_use]
    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn buffer_with_cursor(text: &str, at: usize) -> Buffer {
        let mut buffer = Buffer::new(text);
        buffer.set_selections(Selections::single(Selection::point(at)));
        buffer
    }

    #[test]
    fn typing_inserts_at_every_cursor_and_moves_them() {
        let mut buffer = Buffer::new("ab\ncd");
        buffer.set_selections(Selections::new(
            vec![Selection::point(1), Selection::point(4)],
            0,
        ));
        buffer.insert("XY", true).unwrap();
        assert_eq!(buffer.text(), "aXYb\ncXYd");
        let heads: Vec<usize> = buffer.selections().iter().map(|s| s.head).collect();
        assert_eq!(heads, [3, 8]);
        assert_eq!(buffer.version(), 1);
        assert!(buffer.is_dirty());
    }

    #[test]
    fn backspace_and_delete_work_on_empty_selections() {
        let mut buffer = buffer_with_cursor("hello", 3);
        buffer.delete(1, 0).unwrap();
        assert_eq!(buffer.text(), "helo");
        assert_eq!(buffer.selections().primary().head, 2);
        buffer.delete(0, 1).unwrap();
        assert_eq!(buffer.text(), "heo");
        let mut start = buffer_with_cursor("x", 0);
        start.delete(1, 0).unwrap();
        assert_eq!(start.text(), "x", "backspace at the start is a no-op");
    }

    #[test]
    fn undo_groups_typing_and_restores_selections() {
        let mut buffer = buffer_with_cursor("", 0);
        for c in ["a", "b", "c"] {
            buffer.insert(c, true).unwrap();
        }
        assert_eq!(buffer.text(), "abc");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "", "three keystrokes are one undo group");
        assert_eq!(buffer.selections().primary().head, 0);
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "abc");
        assert_eq!(buffer.selections().primary().head, 3);
        assert!(!buffer.redo());
        buffer.insert("!", false).unwrap();
        buffer.insert("?", false).unwrap();
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "abc!", "non-mergeable edits undo one by one");
    }

    #[test]
    fn multi_edit_transactions_apply_in_reverse_and_invert_exactly() {
        let mut buffer = Buffer::new("0123456789");
        buffer
            .edit(
                vec![
                    Edit {
                        range: 8..9,
                        text: String::new(),
                    },
                    Edit {
                        range: 2..4,
                        text: "ab".into(),
                    },
                    Edit {
                        range: 0..0,
                        text: "<<".into(),
                    },
                ],
                false,
            )
            .unwrap();
        assert_eq!(buffer.text(), "<<01ab4567 9".replace(' ', ""));
        buffer.undo();
        assert_eq!(buffer.text(), "0123456789");
        assert!(matches!(
            buffer.edit(
                vec![
                    Edit {
                        range: 0..3,
                        text: String::new()
                    },
                    Edit {
                        range: 2..5,
                        text: String::new()
                    }
                ],
                false
            ),
            Err(BufferError::Overlap { at: 2 })
        ));
        assert!(matches!(
            buffer.edit(
                vec![Edit {
                    range: 5..50,
                    text: String::new()
                }],
                false
            ),
            Err(BufferError::InvalidRange { .. })
        ));
    }

    #[test]
    fn positions_and_bytes_round_trip_with_multibyte_text() {
        let buffer = Buffer::new("héllo\nwörld\r\nend");
        assert_eq!(buffer.len_lines(), 3);
        assert_eq!(buffer.line(1).as_deref(), Some("wörld"));
        assert_eq!(buffer.line_len_chars(1), 5);
        assert_eq!(buffer.position_of(8), Position { line: 1, column: 2 });
        assert_eq!(
            buffer.char_at(Position {
                line: 1,
                column: 99
            }),
            11
        );
        assert_eq!(buffer.byte_of(2), 3, "é is two bytes");
        assert_eq!(buffer.char_of(3), 2);
        assert_eq!(buffer.line(5), None);
    }

    #[test]
    fn the_last_change_describes_what_landed_in_the_text() {
        let mut buffer = Buffer::new("hello world");
        buffer
            .edit(vec![Edit::insert(0, ">> "), Edit::delete(6..11)], false)
            .unwrap();
        assert_eq!(buffer.text(), ">> hello ");
        assert_eq!(
            buffer.last_change(),
            [
                AppliedEdit {
                    new_start_char: 0,
                    inserted: ">> ".into(),
                    removed: String::new()
                },
                AppliedEdit {
                    new_start_char: 9,
                    inserted: String::new(),
                    removed: "world".into()
                }
            ]
        );
        buffer.undo();
        assert_eq!(buffer.last_change()[1].inserted, "world");
    }

    #[test]
    fn replay_requires_matching_versions() {
        let mut source = buffer_with_cursor("", 0);
        let first = source.insert("a", false).unwrap();
        let second = source.insert("b", false).unwrap();
        let mut target = Buffer::new("");
        target.replay(&first).unwrap();
        target.replay(&second).unwrap();
        assert_eq!(target.text(), "ab");
        assert!(target.replay(&first).is_err());
    }

    proptest! {
        /// Any sequence of random edits undone in order restores the text.
        #[test]
        fn undo_restores_every_previous_state(
            initial in "[a-zé\n]{0,20}",
            ops in prop::collection::vec((0usize..30, 0usize..30, "[a-zé\n]{0,5}"), 1..12),
        ) {
            let mut buffer = Buffer::new(&initial);
            let mut states = vec![buffer.text()];
            for (a, b, text) in ops {
                let len = buffer.len_chars();
                let (start, end) = (a.min(len), b.min(len));
                let range = start.min(end)..start.max(end);
                buffer.edit(vec![Edit { range, text }], false).unwrap();
                states.push(buffer.text());
            }
            while let Some(expected) = states.pop() {
                prop_assert_eq!(buffer.text(), expected);
                if !states.is_empty() {
                    prop_assert!(buffer.undo());
                }
            }
            prop_assert!(!buffer.undo());
        }
    }
}
