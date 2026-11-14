//! Per-cell UTF-8 text buffer with strict single-cell bounds and burst undo.

use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::sync::OnceLock;

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
    /// Byte offset of each line start, built on first use after an edit:
    /// drawing looks lines up per screen row.
    line_index: OnceLock<Vec<usize>>,
}

impl TextBuffer {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: CursorPos::default(),
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            line_index: OnceLock::new(),
        }
    }

    fn line_index(&self) -> &[usize] {
        self.line_index.get_or_init(|| {
            std::iter::once(0)
                .chain(self.text.match_indices('\n').map(|(offset, _)| offset + 1))
                .collect()
        })
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
        self.line_index().len()
    }

    /// Convert a row/character-column position into a Unicode-scalar offset.
    /// Invalid rows and columns are clamped to the current single-cell buffer.
    pub fn char_offset_of(&self, pos: CursorPos) -> usize {
        let row = pos.row.min(self.line_count().saturating_sub(1));
        let mut offset = 0;
        for (index, line) in self.text.split('\n').enumerate() {
            if index == row {
                return offset + pos.col.min(line.chars().count());
            }
            offset += line.chars().count() + 1;
        }
        self.text.chars().count()
    }

    /// Record an undo checkpoint before a normal-mode edit or an insert burst.
    pub fn checkpoint(&mut self) {
        let snapshot = BufferSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        };
        if self.undo_stack.last() != Some(&snapshot) {
            self.undo_stack.push(snapshot);
        }
        self.redo_stack.clear();
    }

    /// Number of undo steps recorded so far.
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Merge every undo step recorded after `depth` into the first of them,
    /// so one key's several edits undo together.
    pub fn merge_undo_since(&mut self, depth: usize) {
        if self.undo_stack.len() > depth + 1 {
            self.undo_stack.truncate(depth + 1);
        }
    }

    /// Set the cursor, clamping it to an existing row and that row's character
    /// length. A cursor at the end of a line is valid for insertion.
    pub fn set_cursor(&mut self, pos: CursorPos) {
        self.cursor = self.clamp_cursor(pos);
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = Some(Selection {
            kind: selection.kind,
            anchor: self.clamp_cursor(selection.anchor),
            head: self.clamp_cursor(selection.head),
        });
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn line(&self, row: usize) -> Option<&str> {
        let index = self.line_index();
        let start = *index.get(row)?;
        let end = index.get(row + 1).map_or(self.text.len(), |next| next - 1);
        Some(&self.text[start..end])
    }

    pub fn line_char_len(&self, row: usize) -> usize {
        self.line(row).map_or(0, |line| line.chars().count())
    }

    /// Insert UTF-8 text at the cursor and advance through every inserted
    /// scalar, treating `\n` as a cell-local line break.
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.cursor = self.clamp_cursor(self.cursor);
        let offset = self.char_offset_of(self.cursor);
        let byte_offset = self.byte_offset_of_char(offset);
        self.text.insert_str(byte_offset, s);
        self.line_index = OnceLock::new();
        for ch in s.chars() {
            if ch == '\n' {
                self.cursor.row += 1;
                self.cursor.col = 0;
            } else {
                self.cursor.col += 1;
            }
        }
        self.selection = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_str("\n");
    }

    /// Roll back `char_count` speculatively typed characters (e.g. `jj`
    /// escape chord), including newlines immediately before the cursor.
    pub fn rollback_chars(&mut self, char_count: usize) {
        let cursor_offset = self.char_offset_of(self.cursor);
        let start = cursor_offset.saturating_sub(char_count);
        if start == cursor_offset {
            return;
        }
        let start_byte = self.byte_offset_of_char(start);
        let end_byte = self.byte_offset_of_char(cursor_offset);
        self.text.replace_range(start_byte..end_byte, "");
        self.line_index = OnceLock::new();
        self.cursor = self.cursor_at_offset(start);
        self.selection = None;
    }

    pub fn replace_all(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.line_index = OnceLock::new();
        self.cursor = CursorPos::default();
        self.selection = None;
    }

    /// Delete the scalar under the cursor. Deleting a newline joins the two
    /// adjacent lines while keeping the cursor at the join point.
    pub fn delete_char_forward(&mut self) {
        let offset = self.char_offset_of(self.cursor);
        if offset >= self.text.chars().count() {
            return;
        }
        let start = self.byte_offset_of_char(offset);
        let end = self.byte_offset_of_char(offset + 1);
        self.text.replace_range(start..end, "");
        self.line_index = OnceLock::new();
        self.cursor = self.cursor_at_offset(offset);
        self.selection = None;
    }

    pub fn delete_char_backward(&mut self) {
        self.rollback_chars(1);
    }

    /// Replace the scalar under the cursor, if one exists. The cursor remains
    /// on the same scalar position, matching normal-mode `r` behavior.
    pub fn replace_char(&mut self, ch: char) {
        let offset = self.char_offset_of(self.cursor);
        if offset >= self.text.chars().count() {
            return;
        }
        let start = self.byte_offset_of_char(offset);
        let end = self.byte_offset_of_char(offset + 1);
        self.text.replace_range(start..end, &ch.to_string());
        self.line_index = OnceLock::new();
        self.cursor = self.cursor_at_offset(offset);
        self.selection = None;
    }

    /// Remove complete lines in the half-open row range and return their exact
    /// source span, including delimiters that belonged to the removed lines.
    pub fn delete_line_range(&mut self, rows: Range<usize>) -> String {
        let (start, end) = self.line_range_offsets(rows);
        if start == end {
            return String::new();
        }
        let removed = self.char_slice(start..end);
        let start_byte = self.byte_offset_of_char(start);
        let end_byte = self.byte_offset_of_char(end);
        self.text.replace_range(start_byte..end_byte, "");
        self.line_index = OnceLock::new();
        self.cursor = self.cursor_at_offset(start);
        self.selection = None;
        removed
    }

    pub fn yank_line_range(&self, rows: Range<usize>) -> String {
        let (start, end) = self.line_range_offsets(rows);
        self.char_slice(start..end)
    }

    /// Delete a half-open character span after ordering and clamping its
    /// endpoints. Returned text is the exact UTF-8 substring removed.
    pub fn delete_span(&mut self, a: CursorPos, b: CursorPos) -> String {
        let first = self.char_offset_of(a);
        let second = self.char_offset_of(b);
        let start = first.min(second);
        let end = first.max(second);
        if start == end {
            return String::new();
        }
        let removed = self.char_slice(start..end);
        let start_byte = self.byte_offset_of_char(start);
        let end_byte = self.byte_offset_of_char(end);
        self.text.replace_range(start_byte..end_byte, "");
        self.line_index = OnceLock::new();
        self.cursor = self.cursor_at_offset(start);
        self.selection = None;
        removed
    }

    pub fn yank_span(&self, a: CursorPos, b: CursorPos) -> String {
        let first = self.char_offset_of(a);
        let second = self.char_offset_of(b);
        self.char_slice(first.min(second)..first.max(second))
    }

    /// Move to the beginning of the next non-whitespace token `count` times.
    pub fn word_next(&self, count: u32) -> CursorPos {
        let chars: Vec<char> = self.text.chars().collect();
        let mut offset = self.char_offset_of(self.cursor);
        for _ in 0..count {
            offset = next_word_offset(&chars, offset);
        }
        self.cursor_at_offset(offset)
    }

    /// Move to the beginning of the previous non-whitespace token `count`
    /// times.
    pub fn word_back(&self, count: u32) -> CursorPos {
        let chars: Vec<char> = self.text.chars().collect();
        let mut offset = self.char_offset_of(self.cursor);
        for _ in 0..count {
            offset = previous_word_offset(&chars, offset);
        }
        self.cursor_at_offset(offset)
    }

    /// Move to the final scalar of the current/next token `count` times.
    pub fn word_end(&self, count: u32) -> CursorPos {
        let chars: Vec<char> = self.text.chars().collect();
        if chars.is_empty() || count == 0 {
            return self.cursor_at_offset(self.char_offset_of(self.cursor));
        }
        let mut offset = self.char_offset_of(self.cursor).min(chars.len() - 1);
        for step in 0..count {
            if step > 0 {
                offset = next_word_offset(&chars, offset.saturating_add(1));
                if offset >= chars.len() {
                    return self.cursor_at_offset(chars.len());
                }
            } else if chars[offset].is_whitespace() {
                while offset < chars.len() && chars[offset].is_whitespace() {
                    offset += 1;
                }
                if offset >= chars.len() {
                    return self.cursor_at_offset(chars.len());
                }
            }
            let category = word_category(chars[offset]);
            while offset + 1 < chars.len()
                && !chars[offset + 1].is_whitespace()
                && word_category(chars[offset + 1]) == category
            {
                offset += 1;
            }
        }
        self.cursor_at_offset(offset)
    }

    pub fn first_nonblank_col(&self, row: usize) -> usize {
        self.line(row)
            .map(|line| {
                line.chars()
                    .position(|ch| !ch.is_whitespace())
                    .unwrap_or_else(|| line.chars().count())
            })
            .unwrap_or(0)
    }

    /// Return the half-open span of the token under (or nearest to) the
    /// cursor. Word tokens contain letters, digits, and underscores; runs of
    /// punctuation form their own tokens.
    pub fn inner_word_span(&self) -> (CursorPos, CursorPos) {
        let chars: Vec<char> = self.text.chars().collect();
        let Some((start, end)) = token_bounds(&chars, self.char_offset_of(self.cursor)) else {
            return (self.cursor, self.cursor);
        };
        (self.cursor_at_offset(start), self.cursor_at_offset(end))
    }

    /// Return an inner-word span plus adjacent horizontal whitespace. If there
    /// is no trailing whitespace, preceding horizontal whitespace is included.
    pub fn around_word_span(&self) -> (CursorPos, CursorPos) {
        let chars: Vec<char> = self.text.chars().collect();
        let Some((mut start, mut end)) = token_bounds(&chars, self.char_offset_of(self.cursor))
        else {
            return (self.cursor, self.cursor);
        };
        let original_end = end;
        while end < chars.len() && matches!(chars[end], ' ' | '\t') {
            end += 1;
        }
        if end == original_end {
            while start > 0 && matches!(chars[start - 1], ' ' | '\t') {
                start -= 1;
            }
        }
        (self.cursor_at_offset(start), self.cursor_at_offset(end))
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(BufferSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = snapshot.text;
        self.line_index = OnceLock::new();
        self.cursor = self.clamp_cursor(snapshot.cursor);
        self.selection = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(BufferSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = snapshot.text;
        self.line_index = OnceLock::new();
        self.cursor = self.clamp_cursor(snapshot.cursor);
        self.selection = None;
        true
    }

    fn clamp_cursor(&self, pos: CursorPos) -> CursorPos {
        let row = pos.row.min(self.line_count().saturating_sub(1));
        CursorPos {
            row,
            col: pos.col.min(self.line_char_len(row)),
        }
    }

    fn byte_offset_of_char(&self, offset: usize) -> usize {
        self.text
            .char_indices()
            .nth(offset)
            .map_or(self.text.len(), |(byte, _)| byte)
    }

    fn cursor_at_offset(&self, offset: usize) -> CursorPos {
        let target = offset.min(self.text.chars().count());
        let mut row = 0;
        let mut col = 0;
        for (index, ch) in self.text.chars().enumerate() {
            if index >= target {
                break;
            }
            if ch == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        CursorPos { row, col }
    }

    fn char_slice(&self, range: Range<usize>) -> String {
        self.text
            .chars()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .collect()
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (offset, ch) in self.text.chars().enumerate() {
            if ch == '\n' {
                starts.push(offset + 1);
            }
        }
        starts
    }

    fn line_range_offsets(&self, rows: Range<usize>) -> (usize, usize) {
        let starts = self.line_starts();
        let line_count = starts.len();
        let start_row = rows.start.min(line_count);
        let end_row = rows.end.min(line_count).max(start_row);
        let start = starts
            .get(start_row)
            .copied()
            .unwrap_or_else(|| self.text.chars().count());
        let end = starts
            .get(end_row)
            .copied()
            .unwrap_or_else(|| self.text.chars().count());
        (start, end)
    }
}

fn word_category(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn next_word_offset(chars: &[char], offset: usize) -> usize {
    let mut index = offset.min(chars.len());
    if index >= chars.len() {
        return index;
    }
    if chars[index].is_whitespace() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        return index;
    }
    let category = word_category(chars[index]);
    while index < chars.len()
        && !chars[index].is_whitespace()
        && word_category(chars[index]) == category
    {
        index += 1;
    }
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    index
}

fn previous_word_offset(chars: &[char], offset: usize) -> usize {
    if chars.is_empty() || offset == 0 {
        return 0;
    }
    let mut index = offset.min(chars.len());
    index -= 1;
    while index > 0 && chars[index].is_whitespace() {
        index -= 1;
    }
    if chars[index].is_whitespace() {
        return 0;
    }
    let category = word_category(chars[index]);
    while index > 0
        && !chars[index - 1].is_whitespace()
        && word_category(chars[index - 1]) == category
    {
        index -= 1;
    }
    index
}

fn token_bounds(chars: &[char], offset: usize) -> Option<(usize, usize)> {
    if chars.is_empty() {
        return None;
    }
    let mut index = offset.min(chars.len().saturating_sub(1));
    if chars[index].is_whitespace() {
        let right = (index..chars.len()).find(|&candidate| !chars[candidate].is_whitespace());
        index = right.or_else(|| {
            (0..index)
                .rev()
                .find(|&candidate| !chars[candidate].is_whitespace())
        })?;
    }
    let category = word_category(chars[index]);
    let mut start = index;
    while start > 0
        && !chars[start - 1].is_whitespace()
        && word_category(chars[start - 1]) == category
    {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && !chars[end].is_whitespace() && word_category(chars[end]) == category
    {
        end += 1;
    }
    Some((start, end))
}
