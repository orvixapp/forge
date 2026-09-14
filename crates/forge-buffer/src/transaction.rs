//! Edits grouped into transactions, the unit of undo, journaling and agent
//! proposals (ARCHITECTURE.md §13.2).

use crate::{BufferError, selection::Selections};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Replace `range` (char indices) with `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Edit {
    #[must_use]
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: at..at,
            text: text.into(),
        }
    }

    #[must_use]
    pub fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }

    /// Chars inserted minus chars removed.
    #[must_use]
    pub fn delta(&self) -> isize {
        isize::try_from(self.text.chars().count()).unwrap_or(isize::MAX)
            - isize::try_from(self.range.len()).unwrap_or(isize::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    /// Sorted by start, non-overlapping, in the coordinates of the text
    /// before the transaction.
    pub edits: Vec<Edit>,
    pub selections_before: Selections,
    pub selections_after: Selections,
    pub version_before: u64,
}

impl Transaction {
    /// Sorts edits by position and validates them against a text of `len`
    /// chars. Adjacent edits are allowed; overlapping ones are not.
    ///
    /// # Errors
    ///
    /// Ranges out of bounds or overlapping.
    pub fn sorted(mut edits: Vec<Edit>, len: usize) -> Result<Vec<Edit>, BufferError> {
        for edit in &edits {
            if edit.range.start > edit.range.end || edit.range.end > len {
                return Err(BufferError::InvalidRange {
                    start: edit.range.start,
                    end: edit.range.end,
                    len,
                });
            }
        }
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        for pair in edits.windows(2) {
            if pair[1].range.start < pair[0].range.end
                || (pair[1].range.start == pair[0].range.start
                    && !pair[0].range.is_empty()
                    && !pair[1].range.is_empty())
            {
                return Err(BufferError::Overlap {
                    at: pair[1].range.start,
                });
            }
        }
        Ok(edits)
    }

    /// Maps a char index of the text before the transaction to the text
    /// after it. Positions inside a replaced range collapse to the end of
    /// the inserted text (`sticky`), or to its start.
    #[must_use]
    pub fn map_position(&self, position: usize, sticky: bool) -> usize {
        map_through(&self.edits, position, sticky)
    }

    /// `self` followed by `next`, as a single transaction whose edits are
    /// expressed against the text before `self`. Used to merge typing into
    /// one undo group; the merged edit list is not minimal, so it stays
    /// correct by composing conservatively: it is only used for replaying
    /// (`forward`) and never re-sorted against the original text.
    #[must_use]
    pub fn then(&self, next: &Self) -> Self {
        Self {
            edits: compose(&self.edits, &next.edits),
            selections_before: self.selections_before.clone(),
            selections_after: next.selections_after.clone(),
            version_before: self.version_before,
        }
    }
}

/// Maps `position` through `edits` (sorted, pre-transaction coordinates).
pub(crate) fn map_through(edits: &[Edit], position: usize, sticky: bool) -> usize {
    let mut mapped = position;
    for edit in edits {
        if edit.range.start > position {
            break;
        }
        let inserted = edit.text.chars().count();
        if position >= edit.range.end
            && !(edit.range.is_empty() && position == edit.range.start && !sticky)
        {
            mapped = mapped + inserted - edit.range.len();
        } else if position > edit.range.start || (position == edit.range.start && sticky) {
            // Inside the replaced range (or at an insertion point, sticky).
            mapped = mapped - (position - edit.range.start) + if sticky { inserted } else { 0 };
        }
    }
    mapped
}

/// One piece of the text after a transaction: a kept slice of the
/// original text, or inserted text. The last original piece is open-ended
/// because the original length is not known here.
#[derive(Debug, Clone)]
enum Piece {
    Original(Range<usize>),
    Inserted(Vec<char>),
}

impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Original(range) => range.len(),
            Self::Inserted(chars) => chars.len(),
        }
    }

    /// Splits at `at` chars into the piece.
    fn split(self, at: usize) -> (Self, Self) {
        match self {
            Self::Original(range) => (
                Self::Original(range.start..range.start + at),
                Self::Original(range.start + at..range.end),
            ),
            Self::Inserted(mut chars) => {
                let tail = chars.split_off(at);
                (Self::Inserted(chars), Self::Inserted(tail))
            }
        }
    }
}

/// Composes two sorted edit lists into one against the original text; the
/// second list is in post-`first` coordinates. The text after `first` is
/// modelled as pieces, the second edits cut and splice those pieces, and
/// the surviving original pieces determine the minimal combined edits.
fn compose(first: &[Edit], second: &[Edit]) -> Vec<Edit> {
    let mut pieces = Vec::new();
    let mut position = 0;
    for edit in first {
        if edit.range.start > position {
            pieces.push(Piece::Original(position..edit.range.start));
        }
        if !edit.text.is_empty() {
            pieces.push(Piece::Inserted(edit.text.chars().collect()));
        }
        position = edit.range.end;
    }
    pieces.push(Piece::Original(position..usize::MAX));
    let mut second = second.to_vec();
    second.sort_by_key(|edit| (edit.range.start, edit.range.end));
    for edit in second.iter().rev() {
        pieces = splice_pieces(pieces, edit);
    }
    let mut edits = Vec::new();
    let mut original_position = 0;
    let mut pending: Vec<char> = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Original(range) => {
                if range.start > original_position || !pending.is_empty() {
                    edits.push(Edit {
                        range: original_position..range.start,
                        text: std::mem::take(&mut pending).into_iter().collect(),
                    });
                }
                original_position = range.end;
            }
            Piece::Inserted(chars) => pending.extend(chars),
        }
    }
    edits
}

/// Applies one post-`first` edit to the piece list.
fn splice_pieces(pieces: Vec<Piece>, edit: &Edit) -> Vec<Piece> {
    let mut out = Vec::with_capacity(pieces.len() + 2);
    let mut cursor: usize = 0;
    let mut inserted = false;
    for piece in pieces {
        let len = piece.len();
        let (start, end) = (cursor, cursor.saturating_add(len));
        cursor = end;
        if end <= edit.range.start {
            out.push(piece);
            continue;
        }
        if start >= edit.range.end {
            if !inserted {
                out.push(Piece::Inserted(edit.text.chars().collect()));
                inserted = true;
            }
            out.push(piece);
            continue;
        }
        // The piece overlaps the edited range: keep its head and tail.
        let head_len = edit.range.start.saturating_sub(start);
        let tail_from = edit.range.end.saturating_sub(start).min(len);
        let (head, rest) = piece.split(head_len);
        if head_len > 0 {
            out.push(head);
        }
        if !inserted {
            out.push(Piece::Inserted(edit.text.chars().collect()));
            inserted = true;
        }
        let (_, tail) = rest.split(tail_from - head_len);
        if tail.len() > 0 {
            out.push(tail);
        }
    }
    if !inserted {
        out.push(Piece::Inserted(edit.text.chars().collect()));
    }
    out.retain(|piece| piece.len() > 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_map_through_insertions_and_deletions() {
        let edits = vec![Edit::insert(2, "xx"), Edit::delete(5..8)];
        assert_eq!(map_through(&edits, 1, true), 1);
        assert_eq!(
            map_through(&edits, 2, true),
            4,
            "sticky insertion point moves after"
        );
        assert_eq!(map_through(&edits, 2, false), 2);
        assert_eq!(map_through(&edits, 4, true), 6);
        assert_eq!(
            map_through(&edits, 6, true),
            7,
            "inside a deletion collapses"
        );
        assert_eq!(map_through(&edits, 6, false), 7);
        assert_eq!(map_through(&edits, 10, true), 9);
    }

    #[test]
    fn composition_of_typing_equals_sequential_application() {
        let apply = |text: &str, edits: &[Edit]| -> String {
            let mut chars: Vec<char> = text.chars().collect();
            let mut edits = edits.to_vec();
            edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
            for edit in edits.iter().rev() {
                chars.splice(edit.range.clone(), edit.text.chars());
            }
            chars.into_iter().collect()
        };
        let text = "hello world";
        let first = vec![Edit::insert(5, ",")];
        let after_first = apply(text, &first);
        let second = vec![Edit::insert(6, " there"), Edit::delete(0..1)];
        let expected = apply(&after_first, &second);
        let composed = compose(&first, &second);
        assert_eq!(apply(text, &composed), expected);
        // Editing inside the inserted text merges into it.
        let third = vec![Edit::delete(5..6)];
        let composed = compose(&first, &third);
        assert_eq!(apply(text, &composed), "hello world");
        // Adjacent deletions (undo of typing) collapse into one edit.
        let composed = compose(&[Edit::delete(1..2)], &[Edit::delete(0..1)]);
        assert_eq!(apply("abc", &composed), "c");
        assert_eq!(composed, [Edit::delete(0..2)]);
    }

    proptest::proptest! {
        #[test]
        fn composition_matches_sequential_application(
            text in "[a-c]{0,12}",
            a in (0usize..14, 0usize..14, "[x-z]{0,3}"),
            b in (0usize..16, 0usize..16, "[x-z]{0,3}"),
        ) {
            let clamp = |len: usize, (s, e, t): (usize, usize, String)| {
                let (s, e) = (s.min(len), e.min(len));
                vec![Edit { range: s.min(e)..s.max(e), text: t }]
            };
            let apply = |text: &str, edits: &[Edit]| -> String {
                let mut chars: Vec<char> = text.chars().collect();
                for edit in edits.iter().rev() {
                    chars.splice(edit.range.clone(), edit.text.chars());
                }
                chars.into_iter().collect()
            };
            let first = clamp(text.chars().count(), a);
            let after_first = apply(&text, &first);
            let second = clamp(after_first.chars().count(), b);
            let expected = apply(&after_first, &second);
            let composed = compose(&first, &second);
            let composed = Transaction::sorted(composed, text.chars().count()).unwrap();
            proptest::prop_assert_eq!(apply(&text, &composed), expected);
        }
    }
}
