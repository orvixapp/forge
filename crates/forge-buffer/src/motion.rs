//! Cursor motions over a [`Buffer`]: chars, words, lines, pages. Every
//! motion maps one selection to another so it composes with multi-cursor.

use crate::{Buffer, Position, Selection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    LineStart,
    LineEnd,
    /// Rows to move; used for PageUp/PageDown.
    PageUp(usize),
    PageDown(usize),
    DocumentStart,
    DocumentEnd,
}

/// Cursor plus the column it wants to keep across vertical moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub selection: Selection,
    /// Column the user was at before vertical motion started; `None`
    /// means "the current column".
    pub goal_column: Option<usize>,
}

impl Buffer {
    /// Applies `motion` to `cursor`; with `extend` the anchor stays put.
    #[must_use]
    pub fn apply_motion(&self, cursor: Cursor, motion: Motion, extend: bool) -> Cursor {
        let head = cursor.selection.head;
        let position = self.position_of(head);
        let (new_head, goal) = match motion {
            Motion::Left => (
                if !extend && !cursor.selection.is_empty() {
                    cursor.selection.range().start
                } else {
                    head.saturating_sub(1)
                },
                None,
            ),
            Motion::Right => (
                if !extend && !cursor.selection.is_empty() {
                    cursor.selection.range().end
                } else {
                    (head + 1).min(self.len_chars())
                },
                None,
            ),
            Motion::WordLeft => (self.word_boundary_left(head), None),
            Motion::WordRight => (self.word_boundary_right(head), None),
            Motion::Up | Motion::PageUp(_) | Motion::Down | Motion::PageDown(_) => {
                let goal = cursor.goal_column.unwrap_or(position.column);
                let rows = match motion {
                    Motion::Up | Motion::Down => 1,
                    Motion::PageUp(rows) | Motion::PageDown(rows) => rows.max(1),
                    _ => unreachable!(),
                };
                let target_line = match motion {
                    Motion::Up | Motion::PageUp(_) => position.line.saturating_sub(rows),
                    _ => (position.line + rows).min(self.len_lines().saturating_sub(1)),
                };
                if target_line == position.line {
                    // First/last line: go to the document edge, like most editors.
                    let edge = match motion {
                        Motion::Up | Motion::PageUp(_) => 0,
                        _ => self.len_chars(),
                    };
                    (edge, Some(goal))
                } else {
                    (
                        self.char_at(Position {
                            line: target_line,
                            column: goal,
                        }),
                        Some(goal),
                    )
                }
            }
            Motion::LineStart => {
                let line_start = self.char_at(Position {
                    line: position.line,
                    column: 0,
                });
                let indent = self.line_indent_chars(position.line);
                // Home toggles between first non-blank and column 0.
                let first_non_blank = line_start + indent;
                (
                    if head > first_non_blank || head == line_start {
                        first_non_blank
                    } else {
                        line_start
                    },
                    None,
                )
            }
            Motion::LineEnd => (
                self.char_at(Position {
                    line: position.line,
                    column: usize::MAX,
                }),
                None,
            ),
            Motion::DocumentStart => (0, None),
            Motion::DocumentEnd => (self.len_chars(), None),
        };
        Cursor {
            selection: if extend {
                Selection::new(cursor.selection.anchor, new_head)
            } else {
                Selection::point(new_head)
            },
            goal_column: goal,
        }
    }

    /// Leading blanks of `line`, in chars.
    #[must_use]
    pub fn line_indent_chars(&self, line: usize) -> usize {
        self.line(line).map_or(0, |text| {
            text.chars().take_while(|c| *c == ' ' || *c == '\t').count()
        })
    }

    /// Start of the word to the left of `at` (skipping blanks first).
    #[must_use]
    pub fn word_boundary_left(&self, at: usize) -> usize {
        let rope = self.rope();
        let mut chars = rope.chars_at(at.min(self.len_chars()));
        let mut position = at.min(self.len_chars());
        let mut class = None;
        while let Some(c) = chars.prev() {
            let this = char_class(c);
            match class {
                None if this == CharClass::Blank && c != '\n' => {}
                None => class = Some(this),
                Some(current) if current == this => {}
                Some(_) => break,
            }
            position -= 1;
            if c == '\n' && class == Some(CharClass::Blank) {
                break;
            }
        }
        position
    }

    /// End of the word to the right of `at` (skipping blanks first).
    #[must_use]
    pub fn word_boundary_right(&self, at: usize) -> usize {
        let rope = self.rope();
        let len = self.len_chars();
        let mut position = at.min(len);
        let chars = rope.chars_at(position);
        let mut class = None;
        for c in chars {
            let this = char_class(c);
            match class {
                None if this == CharClass::Blank && c != '\n' => {}
                None => class = Some(this),
                Some(current) if current == this => {}
                Some(_) => break,
            }
            position += 1;
            if c == '\n' && class == Some(CharClass::Blank) {
                break;
            }
        }
        position
    }

    /// The word around `at`: alphanumerics/underscore, or a run of
    /// punctuation; blanks select nothing.
    #[must_use]
    pub fn word_at(&self, at: usize) -> Selection {
        let rope = self.rope();
        let len = self.len_chars();
        let at = at.min(len);
        let class = rope
            .get_char(at)
            .map(char_class)
            .filter(|class| *class != CharClass::Blank)
            .or_else(|| {
                at.checked_sub(1)
                    .and_then(|prev| rope.get_char(prev))
                    .map(char_class)
                    .filter(|class| *class != CharClass::Blank)
            });
        let Some(class) = class else {
            return Selection::point(at);
        };
        let mut start = at;
        let mut back = rope.chars_at(at);
        while let Some(c) = back.prev() {
            if char_class(c) != class {
                break;
            }
            start -= 1;
        }
        let mut end = at;
        for c in rope.chars_at(at) {
            if char_class(c) != class {
                break;
            }
            end += 1;
        }
        Selection::new(start, end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Punctuation,
    Blank,
}

fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Blank
    } else {
        CharClass::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(at: usize) -> Cursor {
        Cursor {
            selection: Selection::point(at),
            goal_column: None,
        }
    }

    #[test]
    fn vertical_motion_keeps_the_goal_column() {
        let buffer = Buffer::new("a long line\nx\nanother long\n");
        let down = buffer.apply_motion(cursor(6), Motion::Down, false);
        assert_eq!(
            buffer.position_of(down.selection.head),
            Position { line: 1, column: 1 }
        );
        assert_eq!(down.goal_column, Some(6));
        let down = buffer.apply_motion(down, Motion::Down, false);
        assert_eq!(
            buffer.position_of(down.selection.head),
            Position { line: 2, column: 6 }
        );
        let up = buffer.apply_motion(cursor(3), Motion::Up, false);
        assert_eq!(
            up.selection.head, 0,
            "up on the first line goes to the start"
        );
        let extended = buffer.apply_motion(cursor(2), Motion::Down, true);
        assert_eq!(extended.selection, Selection::new(2, 13));
    }

    #[test]
    fn word_motions_and_word_at() {
        let buffer = Buffer::new("foo_bar  baz.qux\nnext");
        assert_eq!(buffer.word_boundary_right(0), 7);
        assert_eq!(buffer.word_boundary_right(7), 12);
        assert_eq!(buffer.word_boundary_right(12), 13);
        assert_eq!(buffer.word_boundary_left(12), 9);
        assert_eq!(buffer.word_boundary_left(9), 0);
        assert_eq!(buffer.word_at(2), Selection::new(0, 7));
        assert_eq!(
            buffer.word_at(7),
            Selection::new(0, 7),
            "end of word counts"
        );
        assert_eq!(buffer.word_at(8), Selection::point(8));
        assert_eq!(
            buffer.word_boundary_right(16),
            17,
            "newline is its own stop"
        );
    }

    #[test]
    fn home_toggles_between_indent_and_column_zero() {
        let buffer = Buffer::new("    code\n");
        let at_end = buffer.apply_motion(cursor(8), Motion::LineStart, false);
        assert_eq!(at_end.selection.head, 4);
        let again = buffer.apply_motion(at_end, Motion::LineStart, false);
        assert_eq!(again.selection.head, 0);
        let back = buffer.apply_motion(again, Motion::LineStart, false);
        assert_eq!(back.selection.head, 4);
        assert_eq!(
            buffer
                .apply_motion(cursor(2), Motion::LineEnd, false)
                .selection
                .head,
            8
        );
        let left = buffer.apply_motion(
            Cursor {
                selection: Selection::new(2, 6),
                goal_column: None,
            },
            Motion::Left,
            false,
        );
        assert_eq!(
            left.selection,
            Selection::point(2),
            "left collapses to the start"
        );
    }
}
