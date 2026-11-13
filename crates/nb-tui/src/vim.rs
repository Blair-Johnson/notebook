//! Vim editing over any `TextBuffer`: motions, operators, and normal/visual
//! commands. Cell sources and read-only output buffers share this, so every
//! motion behaves identically wherever the cursor is.
//!
//! The coordinator owns focus and modes; this module only edits text and
//! reports what should happen next through [`Outcome`].

use nb_core::motion::{motion_target, text_object};
use nb_core::{
    CursorPos, Motion, MotionCtx, MotionKind, Selection, TextBuffer, TextObject, VisualKind,
};
use nb_sdk::MotionOrObject;

/// The unnamed register: last yank or delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

/// Mode of the buffer being edited, as far as editing rules care.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Visual(VisualKind),
}

/// What the coordinator should do after an edit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The command was recognized (even if it did nothing).
    pub handled: bool,
    /// Text changed.
    pub changed: bool,
    /// Switch to insert mode (the undo checkpoint is already taken).
    pub insert: bool,
    /// Return from visual to normal mode.
    pub normal: bool,
    /// Enter (or switch to) visual mode of this kind.
    pub visual: Option<VisualKind>,
    /// The register was written; mirror it to the system clipboard.
    pub yanked: bool,
}

impl Outcome {
    fn handled() -> Self {
        Self {
            handled: true,
            ..Self::default()
        }
    }
}

/// Lines `first..=last` or a half-open character span.
#[derive(Clone, Copy, Debug)]
enum Range {
    Chars(CursorPos, CursorPos),
    Lines(usize, usize),
}

#[derive(Debug)]
pub struct VimState {
    pub register: Register,
    last_find: Option<Motion>,
    /// Column vertical motions aim for; `usize::MAX` after `$`.
    desired_col: Option<usize>,
    /// Rows moved by ctrl+d / ctrl+u.
    pub half_page: usize,
    /// Spaces per `>` / `<` (`editor.indent_width`).
    pub indent_width: usize,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            register: Register::default(),
            last_find: None,
            desired_col: None,
            half_page: 10,
            indent_width: 4,
        }
    }
}

impl VimState {
    /// Forget the sticky column (the cursor was placed by something else).
    pub fn reset_column(&mut self) {
        self.desired_col = None;
    }

    fn resolve(&mut self, id: &str, arg: Option<char>) -> Option<Motion> {
        match id {
            "motion.repeat_find" => self.last_find,
            "motion.repeat_find_reverse" => self.last_find.and_then(Motion::reversed_find),
            _ => {
                let motion = Motion::from_id(id, arg)?;
                if motion.reversed_find().is_some() {
                    self.last_find = Some(motion);
                }
                Some(motion)
            }
        }
    }

    fn target(
        &mut self,
        buf: &TextBuffer,
        id: &str,
        arg: Option<char>,
        count: u32,
    ) -> Option<(Motion, nb_core::MotionTarget)> {
        let motion = self.resolve(id, arg)?;
        let ctx = MotionCtx {
            desired_col: self.desired_col,
            half_page: self.half_page,
        };
        Some((motion, motion_target(buf, motion, count, ctx)?))
    }

    /// Move the cursor (extending the selection in visual mode).
    pub fn move_cursor(
        &mut self,
        buf: &mut TextBuffer,
        mode: Mode,
        id: &str,
        arg: Option<char>,
        count: u32,
    ) -> Outcome {
        let Some((motion, target)) = self.target(buf, id, arg, count) else {
            return Outcome::handled();
        };
        if motion.is_vertical() {
            self.desired_col.get_or_insert(buf.cursor().col);
        } else if motion == Motion::LineEnd {
            self.desired_col = Some(usize::MAX);
        } else {
            self.desired_col = None;
        }
        let selection = buf.selection();
        buf.set_cursor(target.pos);
        clamp_normal(buf);
        if let (Mode::Visual(kind), Some(selection)) = (mode, selection) {
            buf.set_selection(Selection {
                kind,
                anchor: selection.anchor,
                head: buf.cursor(),
            });
        }
        Outcome::handled()
    }

    /// Apply an operator to a motion/text object, or (`target == None`) to
    /// its own line(s) (`dd`, `yy`) or, in visual mode, to the selection.
    pub fn operate(
        &mut self,
        buf: &mut TextBuffer,
        mode: Mode,
        op: &str,
        target: Option<&MotionOrObject>,
        count: u32,
        read_only: bool,
    ) -> Outcome {
        let cursor = buf.cursor();
        let count = count.max(1);
        let range = match (mode, target) {
            (Mode::Visual(_), None) => match selection_range(buf) {
                Some(range) => range,
                None => return Outcome::handled(),
            },
            (Mode::Normal, None) => Range::Lines(
                cursor.row,
                (cursor.row + count as usize - 1).min(buf.line_count() - 1),
            ),
            (_, Some(MotionOrObject::TextObject(id))) => {
                let Some((object, around)) = TextObject::from_id(id.0.as_ref()) else {
                    return Outcome::handled();
                };
                let Some(span) = text_object(buf, object, around) else {
                    return Outcome::handled();
                };
                if span.linewise {
                    Range::Lines(span.start.row, span.end.row)
                } else {
                    Range::Chars(span.start, span.end)
                }
            }
            (_, Some(MotionOrObject::Motion(id) | MotionOrObject::MotionWithArg(id, _))) => {
                let arg = match target {
                    Some(MotionOrObject::MotionWithArg(_, c)) => Some(*c),
                    _ => None,
                };
                let mut id = id.0.as_ref();
                // `cw` changes to the end of the word, like `ce`.
                let on_word = buf
                    .line(cursor.row)
                    .and_then(|line| line.chars().nth(cursor.col))
                    .is_some_and(|c| !c.is_whitespace());
                if op == "op.change" && on_word {
                    id = match id {
                        "motion.word_next" => "motion.word_end",
                        "motion.bigword_next" => "motion.bigword_end",
                        other => other,
                    };
                }
                let Some((motion, target)) = self.target(buf, id, arg, count) else {
                    return Outcome::handled();
                };
                motion_range(buf, cursor, motion, target)
            }
        };
        let mut outcome = self.apply_range(buf, op, range, read_only);
        if matches!(mode, Mode::Visual(_)) && !outcome.insert {
            outcome.normal = true;
        }
        outcome
    }

    fn apply_range(&mut self, buf: &mut TextBuffer, op: &str, range: Range, read_only: bool) -> Outcome {
        let mut outcome = Outcome::handled();
        if op == "op.yank" {
            self.register = yank_range(buf, range);
            outcome.yanked = true;
            buf.clear_selection();
            // The cursor moves to the start of what was yanked.
            let start = match range {
                Range::Chars(a, b) => min_pos(a, b),
                Range::Lines(first, _) => CursorPos {
                    row: first,
                    col: buf.cursor().col,
                },
            };
            if start.row < buf.cursor().row
                || (start.row == buf.cursor().row && start.col < buf.cursor().col)
            {
                buf.set_cursor(start);
            }
            clamp_normal(buf);
            return outcome;
        }
        if read_only {
            buf.clear_selection();
            return outcome;
        }
        buf.checkpoint();
        match (op, range) {
            ("op.delete", _) => {
                self.register = delete_range(buf, range);
                outcome.yanked = true;
                clamp_normal(buf);
            }
            ("op.change", Range::Chars(a, b)) => {
                self.register = Register {
                    text: buf.delete_span(a, b),
                    linewise: false,
                };
                outcome.yanked = true;
                outcome.insert = true;
            }
            ("op.change", Range::Lines(first, last)) => {
                self.register = yank_range(buf, range);
                outcome.yanked = true;
                let indent: String = buf
                    .line(first)
                    .unwrap_or("")
                    .chars()
                    .take_while(|c| matches!(c, ' ' | '\t'))
                    .collect();
                let end = CursorPos {
                    row: last,
                    col: buf.line_char_len(last),
                };
                buf.delete_span(CursorPos { row: first, col: 0 }, end);
                buf.insert_str(&indent);
                outcome.insert = true;
            }
            ("op.indent" | "op.dedent", range) => {
                let (first, last) = match range {
                    Range::Lines(first, last) => (first, last),
                    Range::Chars(a, b) => (a.row.min(b.row), a.row.max(b.row)),
                };
                for row in first..=last {
                    shift_line(buf, row, op == "op.indent", self.indent_width);
                }
                buf.set_cursor(CursorPos {
                    row: first,
                    col: buf.first_nonblank_col(first),
                });
                clamp_normal(buf);
            }
            _ => return outcome,
        }
        buf.clear_selection();
        outcome.changed = true;
        outcome
    }

    /// Normal/visual commands that are not motions or operators. Returns an
    /// unhandled outcome for ids this module doesn't own.
    pub fn command(
        &mut self,
        buf: &mut TextBuffer,
        mode: Mode,
        id: &str,
        count: u32,
        arg: Option<char>,
        read_only: bool,
    ) -> Outcome {
        let count = count.max(1) as usize;
        let cursor = buf.cursor();
        let visual = matches!(mode, Mode::Visual(_));
        let line_len = buf.line_char_len(cursor.row);
        let line_end = CursorPos {
            row: cursor.row,
            col: line_len,
        };
        // Mode changes and yanks work on read-only buffers too.
        match id {
            "visual.char" | "visual.line" => {
                let kind = if id == "visual.char" {
                    VisualKind::Char
                } else {
                    VisualKind::Line
                };
                let mut outcome = Outcome::handled();
                match (mode, buf.selection()) {
                    (Mode::Visual(current), _) if current == kind => {
                        buf.clear_selection();
                        outcome.normal = true;
                    }
                    (Mode::Visual(_), Some(selection)) => {
                        buf.set_selection(Selection { kind, ..selection });
                        outcome.visual = Some(kind);
                    }
                    _ => {
                        buf.set_selection(Selection {
                            kind,
                            anchor: cursor,
                            head: cursor,
                        });
                        outcome.visual = Some(kind);
                    }
                }
                return outcome;
            }
            "visual.swap_ends" => {
                if let Some(selection) = buf.selection() {
                    buf.set_cursor(selection.anchor);
                    buf.set_selection(Selection {
                        kind: selection.kind,
                        anchor: selection.head,
                        head: selection.anchor,
                    });
                }
                return Outcome::handled();
            }
            "edit.yank_line" => {
                let range = if visual {
                    selection_rows(buf)
                } else {
                    Some(Range::Lines(cursor.row, (cursor.row + count - 1).min(buf.line_count() - 1)))
                };
                let mut outcome = range.map_or_else(Outcome::handled, |range| {
                    self.apply_range(buf, "op.yank", range, read_only)
                });
                outcome.normal = visual;
                return outcome;
            }
            _ => {}
        }
        if read_only {
            return Outcome {
                handled: is_edit_command(id),
                ..Outcome::default()
            };
        }

        let mut outcome = Outcome::handled();
        match id {
            "edit.undo" => {
                outcome.changed = buf.undo();
                clamp_normal(buf);
                return outcome;
            }
            "edit.redo" => {
                outcome.changed = buf.redo();
                clamp_normal(buf);
                return outcome;
            }
            // Insert entry points.
            "cell.enter_insert" | "edit.insert_line_start" | "cell.append_char"
            | "edit.append_line_end" | "edit.open_below" | "edit.open_above" => {
                buf.checkpoint();
                if let (true, Some(selection)) = (visual, buf.selection()) {
                    // `I`/`A` in visual insert at the selection's edge.
                    let (start, end) = ordered(selection.anchor, selection.head);
                    buf.clear_selection();
                    buf.set_cursor(if id == "edit.append_line_end" { end } else { start });
                } else {
                    match id {
                        "edit.insert_line_start" => buf.set_cursor(CursorPos {
                            row: cursor.row,
                            col: buf.first_nonblank_col(cursor.row),
                        }),
                        "cell.append_char" => buf.set_cursor(CursorPos {
                            row: cursor.row,
                            col: (cursor.col + 1).min(line_len),
                        }),
                        "edit.append_line_end" => buf.set_cursor(line_end),
                        "edit.open_below" | "edit.open_above" => {
                            let indent = leading_ws(buf, cursor.row);
                            if id == "edit.open_below" {
                                buf.set_cursor(line_end);
                                buf.insert_str(&format!("\n{indent}"));
                            } else {
                                buf.set_cursor(CursorPos {
                                    row: cursor.row,
                                    col: 0,
                                });
                                buf.insert_str(&format!("{indent}\n"));
                                buf.set_cursor(CursorPos {
                                    row: cursor.row,
                                    col: indent.chars().count(),
                                });
                            }
                            outcome.changed = true;
                        }
                        _ => {}
                    }
                }
                outcome.insert = true;
                return outcome;
            }
            _ => {}
        }

        // Visual-mode forms of the single-key edits act on the selection.
        if visual {
            let range = selection_range(buf);
            return match (id, range) {
                ("edit.delete_char" | "edit.delete_char_back", Some(range)) => {
                    self.apply_range(buf, "op.delete", range, false).with_normal()
                }
                ("edit.delete_to_eol", _) => match selection_rows(buf) {
                    Some(range) => self.apply_range(buf, "op.delete", range, false).with_normal(),
                    None => outcome,
                },
                ("edit.substitute", Some(range)) => self.apply_range(buf, "op.change", range, false),
                ("edit.substitute_line" | "edit.change_to_eol", _) => match selection_rows(buf) {
                    Some(range) => self.apply_range(buf, "op.change", range, false),
                    None => outcome,
                },
                ("edit.replace_char", Some(Range::Chars(a, b))) => {
                    let Some(c) = arg else { return outcome };
                    buf.checkpoint();
                    map_span(buf, a, b, |old| if old == '\n' { old } else { c });
                    buf.set_cursor(a);
                    outcome.changed = true;
                    outcome.with_normal()
                }
                ("edit.replace_char", Some(Range::Lines(first, last))) => {
                    let Some(c) = arg else { return outcome };
                    buf.checkpoint();
                    let end = CursorPos {
                        row: last,
                        col: buf.line_char_len(last),
                    };
                    map_span(buf, CursorPos { row: first, col: 0 }, end, |old| {
                        if old == '\n' { old } else { c }
                    });
                    outcome.changed = true;
                    outcome.with_normal()
                }
                ("edit.toggle_case", Some(range)) => {
                    buf.checkpoint();
                    let (a, b) = chars_of(buf, range);
                    map_span(buf, a, b, toggle_case);
                    buf.set_cursor(a);
                    outcome.changed = true;
                    outcome.with_normal()
                }
                ("edit.join_lines", _) => {
                    if let Some(Range::Lines(first, last)) = selection_rows(buf) {
                        buf.checkpoint();
                        join_lines(buf, first, (last - first).max(1));
                        outcome.changed = true;
                    }
                    outcome.with_normal()
                }
                ("edit.paste_after" | "edit.paste_before", Some(range)) => {
                    let register = self.register.clone();
                    buf.checkpoint();
                    self.register = delete_range(buf, range);
                    let after = matches!(range, Range::Lines(..)) && !register.linewise;
                    paste(buf, &register, !after && register.linewise, 1);
                    outcome.changed = true;
                    outcome.yanked = true;
                    outcome.with_normal()
                }
                _ => Outcome {
                    handled: is_edit_command(id),
                    ..Outcome::default()
                },
            };
        }

        match id {
            "edit.delete_char" => {
                if line_len == 0 {
                    return outcome;
                }
                let end = CursorPos {
                    row: cursor.row,
                    col: (cursor.col + count).min(line_len),
                };
                return self.apply_range(buf, "op.delete", Range::Chars(cursor, end), false);
            }
            "edit.delete_char_back" => {
                if cursor.col == 0 {
                    return outcome;
                }
                let start = CursorPos {
                    row: cursor.row,
                    col: cursor.col.saturating_sub(count),
                };
                return self.apply_range(buf, "op.delete", Range::Chars(start, cursor), false);
            }
            "edit.substitute" => {
                let end = CursorPos {
                    row: cursor.row,
                    col: (cursor.col + count).min(line_len),
                };
                return self.apply_range(buf, "op.change", Range::Chars(cursor, end), false);
            }
            "edit.substitute_line" => {
                let last = (cursor.row + count - 1).min(buf.line_count() - 1);
                return self.apply_range(buf, "op.change", Range::Lines(cursor.row, last), false);
            }
            "edit.delete_to_eol" | "edit.change_to_eol" => {
                let last = (cursor.row + count - 1).min(buf.line_count() - 1);
                let end = CursorPos {
                    row: last,
                    col: buf.line_char_len(last),
                };
                let op = if id == "edit.delete_to_eol" {
                    "op.delete"
                } else {
                    "op.change"
                };
                return self.apply_range(buf, op, Range::Chars(cursor, end), false);
            }
            "edit.replace_char" => {
                let Some(c) = arg else { return outcome };
                if cursor.col + count > line_len {
                    return outcome;
                }
                buf.checkpoint();
                let end = CursorPos {
                    row: cursor.row,
                    col: cursor.col + count,
                };
                map_span(buf, cursor, end, |_| c);
                buf.set_cursor(CursorPos {
                    row: cursor.row,
                    col: cursor.col + count - 1,
                });
                outcome.changed = true;
            }
            "edit.toggle_case" => {
                if line_len == 0 {
                    return outcome;
                }
                buf.checkpoint();
                let end = CursorPos {
                    row: cursor.row,
                    col: (cursor.col + count).min(line_len),
                };
                map_span(buf, cursor, end, toggle_case);
                buf.set_cursor(end);
                clamp_normal(buf);
                outcome.changed = true;
            }
            "edit.join_lines" => {
                if cursor.row + 1 >= buf.line_count() {
                    return outcome;
                }
                buf.checkpoint();
                join_lines(buf, cursor.row, count.saturating_sub(1).max(1));
                outcome.changed = true;
            }
            "edit.paste_after" | "edit.paste_before" => {
                if self.register.text.is_empty() {
                    return outcome;
                }
                buf.checkpoint();
                let register = self.register.clone();
                paste(buf, &register, id == "edit.paste_before", count);
                outcome.changed = true;
            }
            _ => {
                return Outcome::default();
            }
        }
        outcome
    }
}

impl Outcome {
    fn with_normal(mut self) -> Self {
        self.normal = true;
        self
    }
}

/// A cell's outputs entered as read-only text.
#[derive(Debug)]
pub struct OutputText {
    pub cell_id: nb_core::CellId,
    pub buffer: TextBuffer,
    /// First wrapped text row shown in the output region.
    pub scroll: u32,
    /// Output revision the text was built from.
    pub revision: u64,
}

impl OutputText {
    pub fn new(cell: &nb_core::Cell) -> Self {
        Self {
            cell_id: cell.id.clone(),
            buffer: TextBuffer::new(outputs_text(cell)),
            scroll: 0,
            revision: outputs_revision(cell),
        }
    }

    /// Rebuild after outputs changed (late output), keeping the cursor.
    pub fn refresh(&mut self, cell: &nb_core::Cell) {
        let revision = outputs_revision(cell);
        if revision != self.revision {
            let cursor = self.buffer.cursor();
            self.buffer = TextBuffer::new(outputs_text(cell));
            self.buffer.set_cursor(cursor);
            clamp_normal(&mut self.buffer);
            self.revision = revision;
        }
    }
}

/// A cell's outputs as one block of plain text, in the order and with the
/// stream tab the card shows.
pub fn outputs_text(cell: &nb_core::Cell) -> String {
    crate::layout::output_sections(cell)
        .iter()
        .flat_map(|section| section.items.iter().map(|item| item.plain_text()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn outputs_revision(cell: &nb_core::Cell) -> u64 {
    let tab = cell.stream_tab as u64;
    cell.outputs.iter().fold(cell.outputs.len() as u64 + tab, |acc, output| {
        acc.wrapping_mul(1_000_003).wrapping_add(output.rev)
    })
}

/// Character-wrap `text` at `width` display columns: one
/// `(logical row, start char, end char)` per screen row.
pub fn wrap_segments(text: &str, width: u16) -> Vec<(usize, usize, usize)> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for (row, line) in text.split('\n').enumerate() {
        let mut start = 0usize;
        let mut used = 0usize;
        for (index, ch) in line.chars().enumerate() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw > 0 && used > 0 && used + cw > width {
                out.push((row, start, index));
                start = index;
                used = 0;
            }
            used += cw;
        }
        out.push((row, start, line.chars().count()));
    }
    out
}

/// Screen row (index into `segments`) holding `pos`.
pub fn segment_of(segments: &[(usize, usize, usize)], pos: CursorPos) -> usize {
    segments
        .iter()
        .rposition(|&(row, start, _)| row == pos.row && start <= pos.col)
        .unwrap_or(0)
}

/// Ids `command` treats as text edits (ignored, not unknown, when read-only).
fn is_edit_command(id: &str) -> bool {
    id.starts_with("edit.") || matches!(id, "cell.enter_insert" | "cell.append_char")
}

/// In normal mode the cursor rests on a character, never past the line end.
pub fn clamp_normal(buf: &mut TextBuffer) {
    let cursor = buf.cursor();
    let len = buf.line_char_len(cursor.row);
    if cursor.col >= len && len > 0 {
        buf.set_cursor(CursorPos {
            row: cursor.row,
            col: len - 1,
        });
    }
}

fn min_pos(a: CursorPos, b: CursorPos) -> CursorPos {
    ordered(a, b).0
}

fn ordered(a: CursorPos, b: CursorPos) -> (CursorPos, CursorPos) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// One character past `pos`, wrapping onto the next line at a line end.
fn after(buf: &TextBuffer, pos: CursorPos) -> CursorPos {
    if pos.col < buf.line_char_len(pos.row) {
        CursorPos {
            row: pos.row,
            col: pos.col + 1,
        }
    } else if pos.row + 1 < buf.line_count() {
        CursorPos {
            row: pos.row + 1,
            col: 0,
        }
    } else {
        pos
    }
}

fn motion_range(
    buf: &TextBuffer,
    cursor: CursorPos,
    motion: Motion,
    target: nb_core::MotionTarget,
) -> Range {
    match target.kind {
        MotionKind::Linewise => Range::Lines(
            cursor.row.min(target.pos.row),
            cursor.row.max(target.pos.row),
        ),
        MotionKind::Inclusive => {
            let (start, end) = ordered(cursor, target.pos);
            Range::Chars(start, after(buf, end))
        }
        MotionKind::Exclusive => {
            let (start, mut end) = ordered(cursor, target.pos);
            // `dw` on a line's last word stops at the line end.
            let word = matches!(motion, Motion::WordNext | Motion::BigWordNext);
            if word && end.row > start.row {
                end = CursorPos {
                    row: start.row,
                    col: buf.line_char_len(start.row),
                };
                if start.col == 0 && end.col == 0 && start.row + 1 < buf.line_count() {
                    end = CursorPos {
                        row: start.row + 1,
                        col: 0,
                    };
                }
            }
            Range::Chars(start, end)
        }
    }
}

fn selection_range(buf: &TextBuffer) -> Option<Range> {
    let selection = buf.selection()?;
    let (start, end) = ordered(selection.anchor, selection.head);
    Some(match selection.kind {
        VisualKind::Line => Range::Lines(start.row, end.row),
        VisualKind::Char => Range::Chars(start, after(buf, end)),
    })
}

fn selection_rows(buf: &TextBuffer) -> Option<Range> {
    let selection = buf.selection()?;
    let (start, end) = ordered(selection.anchor, selection.head);
    Some(Range::Lines(start.row, end.row))
}

fn chars_of(buf: &TextBuffer, range: Range) -> (CursorPos, CursorPos) {
    match range {
        Range::Chars(a, b) => (a, b),
        Range::Lines(first, last) => (
            CursorPos { row: first, col: 0 },
            CursorPos {
                row: last,
                col: buf.line_char_len(last),
            },
        ),
    }
}

fn yank_range(buf: &TextBuffer, range: Range) -> Register {
    match range {
        Range::Chars(a, b) => Register {
            text: buf.yank_span(a, b),
            linewise: false,
        },
        Range::Lines(first, last) => {
            let mut text = buf.yank_line_range(first..last + 1);
            if !text.ends_with('\n') {
                text.push('\n');
            }
            Register {
                text,
                linewise: true,
            }
        }
    }
}

fn delete_range(buf: &mut TextBuffer, range: Range) -> Register {
    match range {
        Range::Chars(a, b) => Register {
            text: buf.delete_span(a, b),
            linewise: false,
        },
        Range::Lines(first, last) => {
            let register = yank_range(buf, range);
            let line_count = buf.line_count();
            if last + 1 >= line_count && first > 0 {
                // Deleting through the end also removes the newline before.
                let start = CursorPos {
                    row: first - 1,
                    col: buf.line_char_len(first - 1),
                };
                let end = CursorPos {
                    row: last,
                    col: buf.line_char_len(last),
                };
                buf.delete_span(start, end);
                buf.set_cursor(CursorPos {
                    row: first - 1,
                    col: buf.first_nonblank_col(first - 1),
                });
            } else {
                buf.delete_line_range(first..last + 1);
                let row = first.min(buf.line_count() - 1);
                buf.set_cursor(CursorPos {
                    row,
                    col: buf.first_nonblank_col(row),
                });
            }
            register
        }
    }
}

fn paste(buf: &mut TextBuffer, register: &Register, before: bool, count: usize) {
    let text = register.text.repeat(count.max(1));
    let cursor = buf.cursor();
    if register.linewise {
        let body = text.strip_suffix('\n').unwrap_or(&text);
        if before {
            buf.set_cursor(CursorPos {
                row: cursor.row,
                col: 0,
            });
            buf.insert_str(&format!("{body}\n"));
            buf.set_cursor(CursorPos {
                row: cursor.row,
                col: buf.first_nonblank_col(cursor.row),
            });
        } else {
            buf.set_cursor(CursorPos {
                row: cursor.row,
                col: buf.line_char_len(cursor.row),
            });
            buf.insert_str(&format!("\n{body}"));
            buf.set_cursor(CursorPos {
                row: cursor.row + 1,
                col: buf.first_nonblank_col(cursor.row + 1),
            });
        }
    } else {
        let at = if before || buf.line_char_len(cursor.row) == 0 {
            cursor
        } else {
            CursorPos {
                row: cursor.row,
                col: cursor.col + 1,
            }
        };
        buf.set_cursor(at);
        buf.insert_str(&text);
        // Rest on the last pasted character.
        let end = buf.cursor();
        buf.set_cursor(CursorPos {
            row: end.row,
            col: end.col.saturating_sub(1),
        });
    }
}

/// Replace every character in `[a, b)` with `f(char)`.
fn map_span(buf: &mut TextBuffer, a: CursorPos, b: CursorPos, f: impl Fn(char) -> char) {
    let (a, b) = ordered(a, b);
    let old = buf.yank_span(a, b);
    let new: String = old.chars().map(f).collect();
    if new != old {
        buf.delete_span(a, b);
        buf.insert_str(&new);
    }
    buf.set_cursor(a);
}

fn toggle_case(c: char) -> char {
    if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c.to_uppercase().next().unwrap_or(c)
    }
}

fn leading_ws(buf: &TextBuffer, row: usize) -> String {
    buf.line(row)
        .unwrap_or("")
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .collect()
}

/// `J`: join `joins` following lines onto `row`, separated by one space.
fn join_lines(buf: &mut TextBuffer, row: usize, joins: usize) {
    for _ in 0..joins {
        if row + 1 >= buf.line_count() {
            break;
        }
        let len = buf.line_char_len(row);
        let next_indent = leading_ws(buf, row + 1).chars().count();
        let next_empty = buf.line_char_len(row + 1) == next_indent;
        let end_blank = buf.line(row).is_some_and(|line| line.ends_with(' ') || line.is_empty());
        buf.delete_span(
            CursorPos { row, col: len },
            CursorPos {
                row: row + 1,
                col: next_indent,
            },
        );
        buf.set_cursor(CursorPos { row, col: len });
        if !next_empty && !end_blank {
            buf.insert_str(" ");
        }
        buf.set_cursor(CursorPos { row, col: len });
    }
}

fn shift_line(buf: &mut TextBuffer, row: usize, indent: bool, width: usize) {
    if indent {
        if buf.line_char_len(row) > 0 {
            buf.set_cursor(CursorPos { row, col: 0 });
            buf.insert_str(&" ".repeat(width));
        }
    } else {
        let ws = leading_ws(buf, row);
        let remove = if ws.starts_with('\t') {
            1
        } else {
            ws.chars().take(width).take_while(|c| *c == ' ').count()
        };
        if remove > 0 {
            buf.delete_span(CursorPos { row, col: 0 }, CursorPos { row, col: remove });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use nb_sdk::{MotionId, TextObjectId};

    fn buf(text: &str, row: usize, col: usize) -> TextBuffer {
        let mut buf = TextBuffer::new(text);
        buf.set_cursor(CursorPos { row, col });
        buf
    }

    fn motion(id: &'static str) -> MotionOrObject {
        MotionOrObject::Motion(MotionId(Cow::Borrowed(id)))
    }

    fn object(id: &'static str) -> MotionOrObject {
        MotionOrObject::TextObject(TextObjectId(Cow::Borrowed(id)))
    }

    #[test]
    fn delete_to_line_end_and_horizontal_motions_are_charwise() {
        let mut vim = VimState::default();
        let mut b = buf("x = compute(a)\nnext", 0, 4);
        vim.operate(&mut b, Mode::Normal, "op.delete", Some(&motion("motion.line_end")), 1, false);
        assert_eq!(b.as_str(), "x = \nnext");
        assert_eq!(b.cursor(), CursorPos { row: 0, col: 3 });
        let mut b = buf("abcdef", 0, 2);
        vim.operate(&mut b, Mode::Normal, "op.delete", Some(&motion("motion.left")), 1, false);
        assert_eq!(b.as_str(), "acdef");
        vim.operate(&mut b, Mode::Normal, "op.delete", Some(&motion("motion.right")), 1, false);
        assert_eq!(b.as_str(), "adef");
        let mut b = buf("    abc", 0, 6);
        vim.operate(&mut b, Mode::Normal, "op.delete", Some(&motion("motion.line_first_nonblank")), 1, false);
        assert_eq!(b.as_str(), "    c");
    }

    #[test]
    fn dd_then_p_pastes_the_line_below() {
        let mut vim = VimState::default();
        let mut b = buf("one\ntwo\nthree", 0, 1);
        vim.operate(&mut b, Mode::Normal, "op.delete", None, 1, false);
        assert_eq!(b.as_str(), "two\nthree");
        assert!(vim.register.linewise);
        vim.command(&mut b, Mode::Normal, "edit.paste_after", 1, None, false);
        assert_eq!(b.as_str(), "two\none\nthree");
        assert_eq!(b.cursor().row, 1);
        // Deleting the last line takes its leading newline with it.
        let mut b = buf("a\nb", 1, 0);
        vim.operate(&mut b, Mode::Normal, "op.delete", None, 1, false);
        assert_eq!(b.as_str(), "a");
    }

    #[test]
    fn change_word_keeps_trailing_space_and_cc_keeps_indent() {
        let mut vim = VimState::default();
        let mut b = buf("foo bar", 0, 0);
        let out = vim.operate(&mut b, Mode::Normal, "op.change", Some(&motion("motion.word_next")), 1, false);
        assert!(out.insert);
        assert_eq!(b.as_str(), " bar");
        let mut b = buf("    return x\nnext", 0, 6);
        vim.operate(&mut b, Mode::Normal, "op.change", None, 1, false);
        assert_eq!(b.as_str(), "    \nnext");
        assert_eq!(b.cursor(), CursorPos { row: 0, col: 4 });
    }

    #[test]
    fn find_motions_with_operators_and_repeat() {
        let mut vim = VimState::default();
        let mut b = buf("f(a, b, c)", 0, 0);
        vim.operate(
            &mut b,
            Mode::Normal,
            "op.delete",
            Some(&MotionOrObject::MotionWithArg(MotionId(Cow::Borrowed("motion.till_forward")), ',')),
            1,
            false,
        );
        assert_eq!(b.as_str(), ", b, c)");
        vim.move_cursor(&mut b, Mode::Normal, "motion.find_forward", Some(','), 1);
        assert_eq!(b.cursor().col, 0 + 3);
        vim.move_cursor(&mut b, Mode::Normal, "motion.repeat_find_reverse", None, 1);
        assert_eq!(b.cursor().col, 0);
    }

    #[test]
    fn text_objects_through_operators() {
        let mut vim = VimState::default();
        let mut b = buf("print(\"hi there\", x)", 0, 9);
        vim.operate(&mut b, Mode::Normal, "op.change", Some(&object("textobj.inner_dquote")), 1, false);
        assert_eq!(b.as_str(), "print(\"\", x)");
        let mut b = buf("print(\"hi\", x)", 0, 8);
        vim.operate(&mut b, Mode::Normal, "op.delete", Some(&object("textobj.inner_paren")), 1, false);
        assert_eq!(b.as_str(), "print()");
    }

    #[test]
    fn visual_selection_is_inclusive_and_linewise_visual_deletes_lines() {
        let mut vim = VimState::default();
        let mut b = buf("abcdef\nxyz", 0, 1);
        vim.command(&mut b, Mode::Normal, "visual.char", 1, None, false);
        vim.move_cursor(&mut b, Mode::Visual(VisualKind::Char), "motion.right", None, 2);
        let out = vim.operate(&mut b, Mode::Visual(VisualKind::Char), "op.delete", None, 1, false);
        assert!(out.normal);
        assert_eq!(b.as_str(), "aef\nxyz");
        let mut b = buf("one\ntwo\nthree", 0, 0);
        vim.command(&mut b, Mode::Normal, "visual.line", 1, None, false);
        vim.move_cursor(&mut b, Mode::Visual(VisualKind::Line), "motion.down", None, 1);
        vim.operate(&mut b, Mode::Visual(VisualKind::Line), "op.delete", None, 1, false);
        assert_eq!(b.as_str(), "three");
    }

    #[test]
    fn single_key_edits() {
        let mut vim = VimState::default();
        let mut b = buf("hello world", 0, 5);
        vim.command(&mut b, Mode::Normal, "edit.delete_to_eol", 1, None, false);
        assert_eq!(b.as_str(), "hello");
        assert_eq!(b.cursor().col, 4);
        vim.command(&mut b, Mode::Normal, "edit.delete_char_back", 2, None, false);
        assert_eq!(b.as_str(), "heo");
        vim.command(&mut b, Mode::Normal, "edit.toggle_case", 1, None, false);
        let mut b = buf("a\n    b", 0, 0);
        vim.command(&mut b, Mode::Normal, "edit.join_lines", 1, None, false);
        assert_eq!(b.as_str(), "a b");
        let mut b = buf("x", 0, 0);
        vim.operate(&mut b, Mode::Normal, "op.indent", None, 1, false);
        assert_eq!(b.as_str(), "    x");
        vim.operate(&mut b, Mode::Normal, "op.dedent", None, 1, false);
        assert_eq!(b.as_str(), "x");
        let out = vim.command(&mut b, Mode::Normal, "edit.append_line_end", 1, None, false);
        assert!(out.insert);
        assert_eq!(b.cursor().col, 1);
    }

    #[test]
    fn read_only_buffers_move_and_yank_but_never_change() {
        let mut vim = VimState::default();
        let mut b = buf("out line one\nout two", 0, 0);
        vim.operate(&mut b, Mode::Normal, "op.delete", None, 1, true);
        assert_eq!(b.as_str(), "out line one\nout two");
        let out = vim.operate(&mut b, Mode::Normal, "op.yank", Some(&motion("motion.word_end")), 1, true);
        assert!(out.yanked);
        assert_eq!(vim.register.text, "out");
        let out = vim.command(&mut b, Mode::Normal, "cell.enter_insert", 1, None, true);
        assert!(!out.insert);
    }
}
