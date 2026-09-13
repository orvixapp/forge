use ropey::Rope;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start_char: usize,
    pub end_char: usize,
    pub replacement: String,
}

#[derive(Debug, Clone)]
struct AppliedEdit {
    forward: Edit,
    inverse: Edit,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BufferError {
    #[error("edit range {start}..{end} is invalid for {len} characters")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Buffer {
    text: Rope,
    version: u64,
    undo: Vec<AppliedEdit>,
    redo: Vec<AppliedEdit>,
}

impl Buffer {
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
            version: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Applies an edit and records its inverse in the undo history.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::InvalidRange`] when the character range is
    /// reversed or falls outside the current rope.
    pub fn apply(&mut self, edit: Edit) -> Result<u64, BufferError> {
        let applied = self.apply_without_history(edit)?;
        self.undo.push(applied);
        self.redo.clear();
        Ok(self.version)
    }

    /// Applies the inverse of the most recent edit, if one exists.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::InvalidRange`] if the stored history is no longer
    /// valid for the current rope, which indicates an internal invariant breach.
    pub fn undo(&mut self) -> Result<bool, BufferError> {
        let Some(applied) = self.undo.pop() else {
            return Ok(false);
        };
        self.apply_raw(&applied.inverse)?;
        self.redo.push(applied);
        Ok(true)
    }

    /// Reapplies the most recently undone edit, if one exists.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::InvalidRange`] if the stored history is no longer
    /// valid for the current rope, which indicates an internal invariant breach.
    pub fn redo(&mut self) -> Result<bool, BufferError> {
        let Some(applied) = self.redo.pop() else {
            return Ok(false);
        };
        self.apply_raw(&applied.forward)?;
        self.undo.push(applied);
        Ok(true)
    }

    fn apply_without_history(&mut self, edit: Edit) -> Result<AppliedEdit, BufferError> {
        self.validate(&edit)?;
        let removed = self.text.slice(edit.start_char..edit.end_char).to_string();
        let replacement_len = edit.replacement.chars().count();
        let inverse = Edit {
            start_char: edit.start_char,
            end_char: edit.start_char + replacement_len,
            replacement: removed,
        };
        self.apply_raw(&edit)?;
        Ok(AppliedEdit {
            forward: edit,
            inverse,
        })
    }

    fn apply_raw(&mut self, edit: &Edit) -> Result<(), BufferError> {
        self.validate(edit)?;
        self.text.remove(edit.start_char..edit.end_char);
        self.text.insert(edit.start_char, &edit.replacement);
        self.version = self.version.wrapping_add(1);
        Ok(())
    }

    fn validate(&self, edit: &Edit) -> Result<(), BufferError> {
        let len = self.text.len_chars();
        if edit.start_char > edit.end_char || edit.end_char > len {
            return Err(BufferError::InvalidRange {
                start: edit.start_char,
                end: edit.end_char,
                len,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_undo_redo_round_trip_with_unicode() {
        let mut buffer = Buffer::new("hola 🌎");
        buffer
            .apply(Edit {
                start_char: 5,
                end_char: 6,
                replacement: "Forge".into(),
            })
            .unwrap();
        assert_eq!(buffer.text(), "hola Forge");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), "hola 🌎");
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text(), "hola Forge");
        assert_eq!(buffer.version(), 3);
    }

    #[test]
    fn invalid_edit_does_not_change_buffer() {
        let mut buffer = Buffer::new("abc");
        let result = buffer.apply(Edit {
            start_char: 4,
            end_char: 4,
            replacement: "x".into(),
        });
        assert!(matches!(result, Err(BufferError::InvalidRange { .. })));
        assert_eq!(buffer.text(), "abc");
        assert_eq!(buffer.version(), 0);
    }
}
