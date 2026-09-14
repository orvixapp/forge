//! Proposed edits overlay and hunk diff review for text buffers (ARCHITECTURE.md §17.5).
//!
//! When an agent or extension proposes writing to a file (`fs/write_text_file`),
//! the change is not written to disk. Instead, it is diffed against the buffer's
//! live content into individual hunks. Each hunk can be reviewed, accepted or
//! rejected. Accepted hunks are applied to the buffer as normal
//! [`crate::Transaction`]s. If the user edits the buffer concurrently, pending
//! hunks are rebased or marked as conflicts if their ranges overlap.

use crate::{Buffer, BufferError, Edit, Transaction};
use imara_diff::{Algorithm, Diff, InternedInput, sources::lines};
use serde::{Deserialize, Serialize};
use std::{ops::Range, path::PathBuf};

/// Status of an individual proposed change hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HunkStatus {
    /// Pending user review.
    Pending,
    /// Applied to the live buffer as a transaction.
    Accepted,
    /// Rejected; live buffer was untouched.
    Rejected,
    /// Cannot be cleanly applied due to concurrent user edits in the same range.
    Conflict(String),
}

/// A single diff hunk inside a proposed edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedHunk {
    /// 0-indexed identifier within the parent [`ProposedEdit`].
    pub id: usize,
    /// Line range in buffer coordinates when the proposal was made or rebased.
    pub buffer_lines: Range<usize>,
    /// Char range in buffer coordinates when the proposal was made or rebased.
    pub buffer_chars: Range<usize>,
    /// Text currently or previously in the buffer at `buffer_chars`.
    pub old_text: String,
    /// Replacement text proposed for this hunk.
    pub new_text: String,
    /// Current review state of this hunk.
    pub status: HunkStatus,
}

/// A collection of proposed changes for a file, computed against a live buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedEdit {
    /// Absolute or workspace-relative path of the target file.
    pub path: PathBuf,
    /// Name or identifier of the agent/extension proposing this edit.
    pub author: String,
    /// File content on disk / at HEAD (empty for newly created files).
    pub original_text: String,
    /// The live buffer text at the moment the proposal was generated.
    pub base_text: String,
    /// Full proposed text content.
    pub proposed_text: String,
    /// Version of the buffer against which `hunks` were generated or rebased.
    pub base_version: u64,
    /// Individual change hunks.
    pub hunks: Vec<ProposedHunk>,
}

impl ProposedEdit {
    /// Computes diff hunks between `buffer` and `proposed_text` using Myers diff.
    #[must_use]
    pub fn from_proposal(
        path: PathBuf,
        author: impl Into<String>,
        original_text: String,
        buffer: &Buffer,
        proposed_text: String,
    ) -> Self {
        let base_text = buffer.text();
        let base_version = buffer.version();
        let hunks = compute_hunks(buffer, &base_text, &proposed_text);
        Self {
            path,
            author: author.into(),
            original_text,
            base_text,
            proposed_text,
            base_version,
            hunks,
        }
    }

    /// Number of hunks still pending review.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| matches!(h.status, HunkStatus::Pending))
            .count()
    }

    /// Number of hunks marked as conflicted.
    #[must_use]
    pub fn conflict_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| matches!(h.status, HunkStatus::Conflict(_)))
            .count()
    }

    /// Number of hunks accepted.
    #[must_use]
    pub fn accepted_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| matches!(h.status, HunkStatus::Accepted))
            .count()
    }

    /// Number of hunks rejected.
    #[must_use]
    pub fn rejected_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| matches!(h.status, HunkStatus::Rejected))
            .count()
    }

    /// Returns `true` if all hunks have been either accepted or rejected.
    #[must_use]
    pub fn is_all_resolved(&self) -> bool {
        self.hunks
            .iter()
            .all(|h| matches!(h.status, HunkStatus::Accepted | HunkStatus::Rejected))
    }

    /// Rebases pending hunks against concurrent changes made to `buffer`.
    ///
    /// If user edits overlap with a pending hunk's range, the hunk is marked
    /// with [`HunkStatus::Conflict`]. Non-overlapping edits shift the hunk's
    /// range by their net character delta.
    pub fn rebase(&mut self, buffer: &Buffer) -> bool {
        if self.base_version == buffer.version() {
            return false;
        }

        let transactions = buffer.transactions_since(self.base_version);
        if let Some(txs) = transactions {
            for tx in txs {
                // All edits of a transaction are in the coordinates of the
                // text *before* it, so overlap checks use the hunk's range
                // as it was before the transaction and the shift is applied
                // once, after every edit has been considered.
                for hunk in &mut self.hunks {
                    if !matches!(hunk.status, HunkStatus::Pending) {
                        continue;
                    }
                    let hunk_start = hunk.buffer_chars.start;
                    let hunk_end = hunk.buffer_chars.end;
                    let mut shift: isize = 0;
                    for edit in &tx.edits {
                        let edit_start = edit.range.start;
                        let edit_end = edit.range.end;
                        let touches = if edit.range.is_empty() {
                            // An insertion strictly inside the hunk; at its
                            // edges it only shifts.
                            edit_start > hunk_start && edit_start < hunk_end
                        } else {
                            edit_start < hunk_end && edit_end > hunk_start
                        };
                        if touches {
                            hunk.status = HunkStatus::Conflict(
                                "Modificación concurrente en la misma región del archivo".into(),
                            );
                            break;
                        }
                        if edit_end <= hunk_start {
                            shift += edit.delta();
                        }
                    }
                    if matches!(hunk.status, HunkStatus::Pending) && shift != 0 {
                        hunk.buffer_chars = hunk_start.saturating_add_signed(shift)
                            ..hunk_end.saturating_add_signed(shift);
                    }
                }
            }
        } else {
            // History was pruned; check text directly
            for hunk in &mut self.hunks {
                if matches!(hunk.status, HunkStatus::Pending)
                    && (hunk.buffer_chars.end > buffer.len_chars()
                        || buffer.slice(hunk.buffer_chars.clone()) != hunk.old_text)
                {
                    hunk.status = HunkStatus::Conflict(
                        "El contenido del buffer cambió concurrentemente".into(),
                    );
                }
            }
        }

        // Verify remaining pending hunks still match the live buffer slice
        for hunk in &mut self.hunks {
            if matches!(hunk.status, HunkStatus::Pending) {
                if hunk.buffer_chars.end > buffer.len_chars()
                    || buffer.slice(hunk.buffer_chars.clone()) != hunk.old_text
                {
                    hunk.status = HunkStatus::Conflict(
                        "El contenido del buffer no coincide con la versión base".into(),
                    );
                } else {
                    let start_line = buffer.position_of(hunk.buffer_chars.start).line;
                    let end_line = buffer.position_of(hunk.buffer_chars.end).line;
                    hunk.buffer_lines = start_line..end_line.max(start_line + 1);
                }
            }
        }

        self.base_version = buffer.version();
        true
    }

    /// Applies a single pending hunk to `buffer` as a [`Transaction`].
    ///
    /// # Errors
    ///
    /// Returns an error if the hunk is not found, not pending, in conflict,
    /// or if the buffer transaction fails.
    pub fn apply_hunk(
        &mut self,
        hunk_id: usize,
        buffer: &mut Buffer,
    ) -> Result<Transaction, BufferError> {
        self.rebase(buffer);
        let hunk = self
            .hunks
            .iter_mut()
            .find(|h| h.id == hunk_id)
            .ok_or_else(|| BufferError::Journal("hunk no encontrado".into()))?;

        match &hunk.status {
            HunkStatus::Pending => {}
            HunkStatus::Conflict(msg) => {
                return Err(BufferError::Journal(format!("hunk en conflicto: {msg}")));
            }
            HunkStatus::Accepted => {
                return Err(BufferError::Journal("el hunk ya fue aceptado".into()));
            }
            HunkStatus::Rejected => {
                return Err(BufferError::Journal("el hunk fue rechazado".into()));
            }
        }

        let edit = Edit {
            range: hunk.buffer_chars.clone(),
            text: hunk.new_text.clone(),
        };
        let delta = edit.delta();
        let applied_range = hunk.buffer_chars.clone();

        let tx = buffer.edit(vec![edit], false)?;
        hunk.status = HunkStatus::Accepted;
        self.base_version = buffer.version();

        // Rebase the remaining pending hunks following this applied edit
        for other in &mut self.hunks {
            if other.id != hunk_id
                && matches!(other.status, HunkStatus::Pending)
                && applied_range.end <= other.buffer_chars.start
            {
                let new_start = other.buffer_chars.start.saturating_add_signed(delta);
                let new_end = other.buffer_chars.end.saturating_add_signed(delta);
                other.buffer_chars = new_start..new_end;
                let start_line = buffer.position_of(other.buffer_chars.start).line;
                let end_line = buffer.position_of(other.buffer_chars.end).line;
                other.buffer_lines = start_line..end_line.max(start_line + 1);
            }
        }

        Ok(tx)
    }

    /// Rejects an individual hunk.
    pub fn reject_hunk(&mut self, hunk_id: usize) -> bool {
        if let Some(hunk) = self.hunks.iter_mut().find(|h| h.id == hunk_id) {
            hunk.status = HunkStatus::Rejected;
            true
        } else {
            false
        }
    }

    /// Applies all currently pending, non-conflicting hunks to `buffer` as a
    /// single transactional edit.
    ///
    /// # Errors
    ///
    /// Propagates buffer transaction failures.
    pub fn apply_all_pending(
        &mut self,
        buffer: &mut Buffer,
    ) -> Result<Option<Transaction>, BufferError> {
        self.rebase(buffer);
        let mut edits = Vec::new();
        let mut pending_indices = Vec::new();
        for (index, hunk) in self.hunks.iter().enumerate() {
            if matches!(hunk.status, HunkStatus::Pending) {
                edits.push(Edit {
                    range: hunk.buffer_chars.clone(),
                    text: hunk.new_text.clone(),
                });
                pending_indices.push(index);
            }
        }

        if edits.is_empty() {
            return Ok(None);
        }

        // Sort edits strictly by range start
        edits.sort_by_key(|e| e.range.start);

        let tx = buffer.edit(edits, false)?;
        for index in pending_indices {
            self.hunks[index].status = HunkStatus::Accepted;
        }
        self.base_version = buffer.version();
        Ok(Some(tx))
    }

    /// Rejects all pending or conflicted hunks in this proposed edit.
    pub fn reject_all_pending(&mut self) {
        for hunk in &mut self.hunks {
            if matches!(hunk.status, HunkStatus::Pending | HunkStatus::Conflict(_)) {
                hunk.status = HunkStatus::Rejected;
            }
        }
    }
}

fn compute_hunks(buffer: &Buffer, base_text: &str, proposed_text: &str) -> Vec<ProposedHunk> {
    let input = InternedInput::new(lines(base_text), lines(proposed_text));
    let diff = Diff::compute(Algorithm::Myers, &input);

    let proposed_lines: Vec<&str> = proposed_text.lines().collect();
    let base_lines_count = buffer.len_lines();

    let mut hunks = Vec::new();

    for (hunk_id, diff_hunk) in diff.hunks().enumerate() {
        let before_start = diff_hunk.before.start as usize;
        let before_end = diff_hunk.before.end as usize;
        let after_start = diff_hunk.after.start as usize;
        let after_end = diff_hunk.after.end as usize;

        let char_start = if before_start >= base_lines_count {
            buffer.len_chars()
        } else {
            buffer.char_at(crate::Position {
                line: before_start,
                column: 0,
            })
        };

        let char_end = if before_end >= base_lines_count {
            buffer.len_chars()
        } else {
            buffer.char_at(crate::Position {
                line: before_end,
                column: 0,
            })
        };

        let old_text = buffer.slice(char_start..char_end);

        let new_text = if after_start < after_end {
            let lines_slice = &proposed_lines
                [after_start.min(proposed_lines.len())..after_end.min(proposed_lines.len())];
            let mut joined = lines_slice.join("\n");
            // Add trailing newline if the replaced hunk ended with newline or proposal has trailing newline
            if (old_text.ends_with('\n')
                || after_end < proposed_lines.len()
                || proposed_text.ends_with('\n'))
                && !joined.ends_with('\n')
            {
                joined.push('\n');
            }
            joined
        } else {
            String::new()
        };

        let buffer_lines = before_start..before_end.max(before_start + 1);

        hunks.push(ProposedHunk {
            id: hunk_id,
            buffer_lines,
            buffer_chars: char_start..char_end,
            old_text,
            new_text,
            status: HunkStatus::Pending,
        });
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_hunks_and_applies_single_hunk() {
        let mut buffer = Buffer::new("line 1\nline 2\nline 3\n");
        let proposed = "line 1\nline 2 modified\nline 3\n".to_string();

        let mut proposed_edit = ProposedEdit::from_proposal(
            PathBuf::from("/workspace/test.txt"),
            "test-agent",
            "line 1\nline 2\nline 3\n".into(),
            &buffer,
            proposed,
        );

        assert_eq!(proposed_edit.hunks.len(), 1);
        assert_eq!(proposed_edit.pending_count(), 1);
        assert_eq!(proposed_edit.hunks[0].new_text, "line 2 modified\n");

        let _tx = proposed_edit.apply_hunk(0, &mut buffer).unwrap();
        assert_eq!(buffer.text(), "line 1\nline 2 modified\nline 3\n");
        assert_eq!(proposed_edit.accepted_count(), 1);
        assert_eq!(proposed_edit.pending_count(), 0);

        // Undo works because it was applied as a Transaction!
        buffer.undo();
        assert_eq!(buffer.text(), "line 1\nline 2\nline 3\n");
    }

    #[test]
    fn reject_hunk_leaves_buffer_untouched() {
        let buffer = Buffer::new("line 1\nline 2\n");
        let proposed = "line 1\nline 2 modified\n".to_string();

        let mut proposed_edit = ProposedEdit::from_proposal(
            PathBuf::from("/workspace/test.txt"),
            "test-agent",
            buffer.text(),
            &buffer,
            proposed,
        );

        assert!(proposed_edit.reject_hunk(0));
        assert_eq!(proposed_edit.rejected_count(), 1);
        assert_eq!(proposed_edit.pending_count(), 0);
        assert_eq!(buffer.text(), "line 1\nline 2\n");
    }

    #[test]
    fn rebases_hunk_when_user_edits_before_it() {
        let mut buffer = Buffer::new("line 1\nline 2\nline 3\n");
        let proposed = "line 1\nline 2\nline 3 modified\n".to_string();

        let mut proposed_edit = ProposedEdit::from_proposal(
            PathBuf::from("/workspace/test.txt"),
            "test-agent",
            buffer.text(),
            &buffer,
            proposed,
        );

        // User inserts a new line at the very top (before hunk)
        let _ = buffer.insert("header\n", false).unwrap();
        assert_eq!(buffer.version(), 1);

        // Rebase should shift the hunk
        assert!(proposed_edit.rebase(&buffer));
        assert_eq!(proposed_edit.conflict_count(), 0);
        assert_eq!(proposed_edit.pending_count(), 1);

        // Applying hunk now applies cleanly at the shifted location
        proposed_edit.apply_hunk(0, &mut buffer).unwrap();
        assert_eq!(buffer.text(), "header\nline 1\nline 2\nline 3 modified\n");
    }

    #[test]
    fn rebase_survives_multi_cursor_edits_and_undo() {
        let mut buffer = Buffer::new("aaa\nbbb\nccc\nddd\n");
        let mut proposal = ProposedEdit::from_proposal(
            PathBuf::from("x"),
            "agent",
            buffer.text(),
            &buffer,
            "aaa\nbbb\nccc\nDDD\n".into(),
        );
        assert_eq!(proposal.hunks.len(), 1);
        // Two insertions before the hunk in one transaction (multi-cursor).
        buffer
            .edit(vec![Edit::insert(0, "1"), Edit::insert(4, "22")], false)
            .unwrap();
        assert!(proposal.rebase(&buffer));
        assert!(matches!(proposal.hunks[0].status, HunkStatus::Pending));
        assert_eq!(
            buffer.slice(proposal.hunks[0].buffer_chars.clone()),
            "ddd\n"
        );
        // Undo is a text change too: the hunk follows it back.
        assert!(buffer.undo());
        assert!(proposal.rebase(&buffer));
        assert!(matches!(proposal.hunks[0].status, HunkStatus::Pending));
        assert_eq!(
            buffer.slice(proposal.hunks[0].buffer_chars.clone()),
            "ddd\n"
        );
        proposal
            .apply_hunk(proposal.hunks[0].id, &mut buffer)
            .unwrap();
        assert_eq!(buffer.text(), "aaa\nbbb\nccc\nDDD\n");
    }

    #[test]
    fn detects_conflict_when_user_edits_inside_hunk() {
        let mut buffer = Buffer::new("line 1\nline 2\nline 3\n");
        let proposed = "line 1\nline 2 modified\nline 3\n".to_string();

        let mut proposed_edit = ProposedEdit::from_proposal(
            PathBuf::from("/workspace/test.txt"),
            "test-agent",
            buffer.text(),
            &buffer,
            proposed,
        );

        // User edits line 2 concurrently!
        let hunk_chars = proposed_edit.hunks[0].buffer_chars.clone();
        let _ = buffer
            .edit(
                vec![Edit {
                    range: hunk_chars.start..hunk_chars.start + 4,
                    text: "LINE".into(),
                }],
                false,
            )
            .unwrap();

        // Rebase detects conflict
        assert!(proposed_edit.rebase(&buffer));
        assert_eq!(proposed_edit.conflict_count(), 1);
        assert_eq!(proposed_edit.pending_count(), 0);

        // Applying conflicted hunk fails
        assert!(proposed_edit.apply_hunk(0, &mut buffer).is_err());
    }

    #[test]
    fn apply_all_pending_applies_multiple_hunks() {
        let mut buffer = Buffer::new("alpha\nbeta\ngamma\n");
        let proposed = "alpha modified\nbeta\ngamma modified\n".to_string();

        let mut proposed_edit = ProposedEdit::from_proposal(
            PathBuf::from("/workspace/test.txt"),
            "test-agent",
            buffer.text(),
            &buffer,
            proposed,
        );

        assert_eq!(proposed_edit.hunks.len(), 2);
        let _ = proposed_edit.apply_all_pending(&mut buffer).unwrap();
        assert_eq!(buffer.text(), "alpha modified\nbeta\ngamma modified\n");
        assert_eq!(proposed_edit.accepted_count(), 2);
    }
}
