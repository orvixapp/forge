//! Selections in the Helix model: every cursor is a selection, operations
//! apply to all of them, and the list stays sorted and merged.

use crate::transaction::{Edit, map_through};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Line and column in chars, both 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

/// `anchor` is where the selection started, `head` where the cursor is;
/// equal means a bare cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    #[must_use]
    pub const fn point(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    #[must_use]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    #[must_use]
    pub fn clamped(self, len: usize) -> Self {
        Self {
            anchor: self.anchor.min(len),
            head: self.head.min(len),
        }
    }

    /// Union of two overlapping/touching selections, keeping `self`'s
    /// direction.
    fn merge(self, other: Self) -> Self {
        let start = self.range().start.min(other.range().start);
        let end = self.range().end.max(other.range().end);
        if self.head >= self.anchor {
            Self::new(start, end)
        } else {
            Self::new(end, start)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selections {
    list: Vec<Selection>,
    primary: usize,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(Selection::point(0))
    }
}

impl Selections {
    #[must_use]
    pub fn single(selection: Selection) -> Self {
        Self {
            list: vec![selection],
            primary: 0,
        }
    }

    /// Builds from a list (any order) and the index of the primary one.
    #[must_use]
    pub fn new(list: Vec<Selection>, primary: usize) -> Self {
        Self { list, primary }.normalized()
    }

    #[must_use]
    pub fn primary(&self) -> Selection {
        self.list[self.primary]
    }

    #[must_use]
    pub fn primary_index(&self) -> usize {
        self.primary
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.list.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.list.iter()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Selection] {
        &self.list
    }

    /// Applies `f` to every selection and re-normalises.
    #[must_use]
    pub fn map(mut self, f: impl FnMut(Selection) -> Selection) -> Self {
        self.list = self.list.into_iter().map(f).collect();
        self.normalized()
    }

    /// Adds a selection; it becomes primary.
    pub fn push(&mut self, selection: Selection) {
        self.list.push(selection);
        self.primary = self.list.len() - 1;
        *self = std::mem::take(self).normalized();
    }

    /// Keeps only the primary selection.
    #[must_use]
    pub fn collapse_to_primary(self) -> Self {
        Self::single(self.primary())
    }

    #[must_use]
    pub fn clamped(self, len: usize) -> Self {
        self.map(|selection| selection.clamped(len))
    }

    /// Selections after `edits` (sorted, pre-edit coordinates): a replaced
    /// selection becomes a cursor after the inserted text.
    #[must_use]
    pub fn map_through(self, edits: &[Edit]) -> Self {
        let mut sorted = edits.to_vec();
        sorted.sort_by_key(|edit| (edit.range.start, edit.range.end));
        self.map(|selection| {
            let range = selection.range();
            let replaced = sorted
                .iter()
                .any(|edit| edit.range == range || (!range.is_empty() && edit.range.start <= range.start && edit.range.end >= range.end && edit.range.start == range.start));
            if replaced {
                let end = map_through(&sorted, range.end, true);
                return Selection::point(end);
            }
            Selection {
                anchor: map_through(&sorted, selection.anchor, true),
                head: map_through(&sorted, selection.head, true),
            }
        })
    }

    /// Sorts by start, merges overlapping selections and keeps the primary
    /// pointing at the selection that absorbed it.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.list.is_empty() {
            return Self::default();
        }
        let primary = self.list[self.primary.min(self.list.len() - 1)];
        let mut indexed: Vec<(Selection, bool)> = self
            .list
            .iter()
            .enumerate()
            .map(|(index, selection)| (*selection, index == self.primary.min(self.list.len() - 1)))
            .collect();
        indexed.sort_by_key(|(selection, _)| (selection.range().start, selection.range().end));
        let mut merged: Vec<(Selection, bool)> = Vec::with_capacity(indexed.len());
        for (selection, is_primary) in indexed {
            match merged.last_mut() {
                Some((last, last_primary))
                    if selection.range().start < last.range().end
                        || (selection.range().start == last.range().end
                            && (selection.is_empty() || last.is_empty())
                            && selection.range().start == last.range().start) =>
                {
                    *last = last.merge(selection);
                    *last_primary |= is_primary;
                }
                _ => merged.push((selection, is_primary)),
            }
        }
        self.primary = merged
            .iter()
            .position(|(_, is_primary)| *is_primary)
            .unwrap_or_else(|| {
                merged
                    .iter()
                    .position(|(selection, _)| selection.range().start >= primary.range().start)
                    .unwrap_or(merged.len() - 1)
            });
        self.list = merged.into_iter().map(|(selection, _)| selection).collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_selections_merge_and_keep_the_primary() {
        let selections = Selections::new(
            vec![
                Selection::new(5, 9),
                Selection::new(0, 3),
                Selection::new(8, 12),
                Selection::point(20),
            ],
            2,
        );
        assert_eq!(
            selections.as_slice(),
            [
                Selection::new(0, 3),
                Selection::new(5, 12),
                Selection::point(20)
            ]
        );
        assert_eq!(selections.primary(), Selection::new(5, 12));
        assert_eq!(selections.primary_index(), 1);
    }

    #[test]
    fn touching_cursors_merge_but_adjacent_ranges_do_not() {
        let cursors = Selections::new(vec![Selection::point(3), Selection::point(3)], 0);
        assert_eq!(cursors.len(), 1);
        let ranges = Selections::new(vec![Selection::new(0, 3), Selection::new(3, 6)], 0);
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn selections_follow_edits() {
        let selections = Selections::new(vec![Selection::point(1), Selection::new(4, 6)], 0);
        let edits = vec![Edit::insert(1, "ab"), Edit::delete(4..6)];
        let mapped = selections.map_through(&edits);
        assert_eq!(
            mapped.as_slice(),
            [Selection::point(3), Selection::point(6)]
        );
        let backward = Selections::single(Selection::new(6, 4)).map_through(&[Edit {
            range: 4..6,
            text: "Z".into(),
        }]);
        assert_eq!(backward.primary(), Selection::point(5));
    }
}
