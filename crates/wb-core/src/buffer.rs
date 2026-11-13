//! Per-cell UTF-8 text buffer with strict single-cell bounds and burst undo.

use serde::{Deserialize, Serialize};

/// Zero-based `(row, char_col)` cursor coordinates within a single cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct CursorPos {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum VisualKind {
    Char,
    Line,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub kind: VisualKind,
    pub anchor: CursorPos,
    pub head: CursorPos,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BufferSnapshot {
    pub text: String,
    pub cursor: CursorPos,
}

/// Editable source buffer for a single cell.
///
/// Vertical motions on `TextBuffer` clamp strictly to `0..self.line_count()`
/// and never cross into adjacent cells.
#[derive(Clone, Debug)]
pub struct TextBuffer {
    text: String,
    cursor: CursorPos,
    selection: Option<Selection>,
    undo_stack: Vec<BufferSnapshot>,
    redo_stack: Vec<BufferSnapshot>,
}

impl TextBuffer {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: CursorPos::default(),
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> CursorPos {
        self.cursor
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }

    pub fn char_offset_of(&self, _pos: CursorPos) -> usize {
        todo!("wb-core::buffer: map (row, char_col) to unicode scalar offset")
    }

    /// Record an undo checkpoint before a normal-mode edit or an insert burst.
    pub fn checkpoint(&mut self) {
        todo!("wb-core::buffer: push deduplicated BufferSnapshot and clear redo_stack")
    }

    pub fn insert_str(&mut self, _s: &str) {
        todo!("wb-core::buffer: insert UTF-8 text at cursor and advance CursorPos")
    }

    /// Roll back `char_count` speculatively typed characters (e.g. `jj` escape chord).
    pub fn rollback_chars(&mut self, _char_count: usize) {
        todo!("wb-core::buffer: remove trailing speculative chars prior to cursor")
    }

    pub fn replace_all(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = CursorPos::default();
        self.selection = None;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        todo!("wb-core::buffer: pop undo_stack into current state")
    }

    pub fn redo(&mut self) -> bool {
        todo!("wb-core::buffer: pop redo_stack into current state")
    }
}
