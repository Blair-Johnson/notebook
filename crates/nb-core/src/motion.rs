//! Vim motions and text objects as pure position math over a `TextBuffer`.
//!
//! Nothing here mutates text. A motion returns where the cursor goes and how
//! an operator should treat the span it covers; a text object returns a span.
//! Callers (normal mode, visual mode, read-only output buffers) decide what to
//! do with the result.

use crate::buffer::{CursorPos, TextBuffer};

/// How an operator treats the span between the cursor and a motion target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MotionKind {
    /// The target character is not included (`w`, `h`, `0`).
    Exclusive,
    /// The target character is included (`e`, `$`, `f`).
    Inclusive,
    /// Whole lines from the cursor row to the target row (`j`, `gg`, `_`).
    Linewise,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    FirstNonBlank,
    /// `_`: first non-blank of the line `count - 1` below; linewise.
    CurrentLine,
    LineEnd,
    WordNext,
    WordBack,
    WordEnd,
    BigWordNext,
    BigWordBack,
    BigWordEnd,
    Top,
    Bottom,
    ParagraphNext,
    ParagraphPrev,
    MatchPair,
    FindForward(char),
    FindBackward(char),
    TillForward(char),
    TillBackward(char),
    HalfPageDown,
    HalfPageUp,
}

impl Motion {
    /// Map a keymap motion id (plus its character argument) onto a motion.
    pub fn from_id(id: &str, arg: Option<char>) -> Option<Self> {
        Some(match id {
            "motion.left" => Self::Left,
            "motion.right" => Self::Right,
            "motion.up" => Self::Up,
            "motion.down" => Self::Down,
            "motion.line_start" => Self::LineStart,
            "motion.line_first_nonblank" => Self::FirstNonBlank,
            "motion.current_line" => Self::CurrentLine,
            "motion.line_end" => Self::LineEnd,
            "motion.word_next" => Self::WordNext,
            "motion.word_back" => Self::WordBack,
            "motion.word_end" => Self::WordEnd,
            "motion.bigword_next" => Self::BigWordNext,
            "motion.bigword_back" => Self::BigWordBack,
            "motion.bigword_end" => Self::BigWordEnd,
            "motion.top" => Self::Top,
            "motion.bottom" => Self::Bottom,
            "motion.paragraph_next" => Self::ParagraphNext,
            "motion.paragraph_prev" => Self::ParagraphPrev,
            "motion.match_pair" => Self::MatchPair,
            "motion.find_forward" => Self::FindForward(arg?),
            "motion.find_backward" => Self::FindBackward(arg?),
            "motion.till_forward" => Self::TillForward(arg?),
            "motion.till_backward" => Self::TillBackward(arg?),
            "motion.half_page_down" => Self::HalfPageDown,
            "motion.half_page_up" => Self::HalfPageUp,
            _ => return None,
        })
    }

    /// Vertical motions keep the remembered ("sticky") column.
    pub fn is_vertical(self) -> bool {
        matches!(
            self,
            Self::Up | Self::Down | Self::HalfPageDown | Self::HalfPageUp
        )
    }

    /// The same find in the opposite direction, for `,`.
    pub fn reversed_find(self) -> Option<Self> {
        Some(match self {
            Self::FindForward(c) => Self::FindBackward(c),
            Self::FindBackward(c) => Self::FindForward(c),
            Self::TillForward(c) => Self::TillBackward(c),
            Self::TillBackward(c) => Self::TillForward(c),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MotionTarget {
    pub pos: CursorPos,
    pub kind: MotionKind,
}

/// Inputs a motion needs beyond the buffer itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct MotionCtx {
    /// Column vertical motions aim for (vim's `curswant`).
    pub desired_col: Option<usize>,
    /// Rows moved by one half-page motion.
    pub half_page: usize,
}

/// Where `motion` repeated `count` times takes the cursor, or `None` when it
/// cannot move (an `f` with no match, `%` with no bracket).
pub fn motion_target(
    buf: &TextBuffer,
    motion: Motion,
    count: u32,
    ctx: MotionCtx,
) -> Option<MotionTarget> {
    let count = count.max(1) as usize;
    let cursor = buf.cursor();
    let last_row = buf.line_count().saturating_sub(1);
    let line_len = |row: usize| buf.line_char_len(row);
    let vertical = |row: usize| CursorPos {
        row,
        col: ctx.desired_col.unwrap_or(cursor.col).min(line_len(row)),
    };
    let exclusive = |pos| MotionTarget {
        pos,
        kind: MotionKind::Exclusive,
    };
    let inclusive = |pos| MotionTarget {
        pos,
        kind: MotionKind::Inclusive,
    };
    let linewise = |pos| MotionTarget {
        pos,
        kind: MotionKind::Linewise,
    };
    let text = Chars::new(buf);

    Some(match motion {
        Motion::Left => exclusive(CursorPos {
            row: cursor.row,
            col: cursor.col.saturating_sub(count),
        }),
        Motion::Right => exclusive(CursorPos {
            row: cursor.row,
            col: (cursor.col + count).min(line_len(cursor.row)),
        }),
        Motion::Up => linewise(vertical(cursor.row.saturating_sub(count))),
        Motion::Down => linewise(vertical((cursor.row + count).min(last_row))),
        Motion::HalfPageDown => {
            linewise(vertical((cursor.row + count * ctx.half_page.max(1)).min(last_row)))
        }
        Motion::HalfPageUp => {
            linewise(vertical(cursor.row.saturating_sub(count * ctx.half_page.max(1))))
        }
        Motion::LineStart => exclusive(CursorPos {
            row: cursor.row,
            col: 0,
        }),
        Motion::FirstNonBlank => exclusive(CursorPos {
            row: cursor.row,
            col: buf.first_nonblank_col(cursor.row),
        }),
        Motion::CurrentLine => {
            let row = (cursor.row + count - 1).min(last_row);
            linewise(CursorPos {
                row,
                col: buf.first_nonblank_col(row),
            })
        }
        Motion::LineEnd => {
            let row = (cursor.row + count - 1).min(last_row);
            inclusive(CursorPos {
                row,
                col: line_len(row).saturating_sub(1),
            })
        }
        Motion::Top | Motion::Bottom => {
            let row = if count > 1 {
                (count - 1).min(last_row)
            } else if motion == Motion::Top {
                0
            } else {
                last_row
            };
            linewise(CursorPos {
                row,
                col: buf.first_nonblank_col(row),
            })
        }
        Motion::WordNext | Motion::BigWordNext => {
            let big = motion == Motion::BigWordNext;
            let mut offset = text.offset(cursor);
            for _ in 0..count {
                offset = text.word_next(offset, big);
            }
            exclusive(text.pos(offset))
        }
        Motion::WordBack | Motion::BigWordBack => {
            let big = motion == Motion::BigWordBack;
            let mut offset = text.offset(cursor);
            for _ in 0..count {
                offset = text.word_back(offset, big);
            }
            exclusive(text.pos(offset))
        }
        Motion::WordEnd | Motion::BigWordEnd => {
            let big = motion == Motion::BigWordEnd;
            let mut offset = text.offset(cursor);
            for _ in 0..count {
                offset = text.word_end(offset, big);
            }
            inclusive(text.pos(offset))
        }
        Motion::ParagraphNext => {
            let mut row = cursor.row;
            for _ in 0..count {
                row = next_paragraph_boundary(buf, row, true);
            }
            if row >= last_row && !is_blank(buf, last_row) {
                exclusive(CursorPos {
                    row: last_row,
                    col: line_len(last_row),
                })
            } else {
                exclusive(CursorPos { row, col: 0 })
            }
        }
        Motion::ParagraphPrev => {
            let mut row = cursor.row;
            for _ in 0..count {
                row = next_paragraph_boundary(buf, row, false);
            }
            exclusive(CursorPos { row, col: 0 })
        }
        Motion::MatchPair => inclusive(text.pos(text.match_pair(text.offset(cursor))?)),
        Motion::FindForward(c) | Motion::TillForward(c) => {
            let line: Vec<char> = buf.line(cursor.row).unwrap_or("").chars().collect();
            let till = matches!(motion, Motion::TillForward(_));
            // `t` repeated must not stick on the match right next to it.
            let mut col = cursor.col + usize::from(till);
            for _ in 0..count {
                col = (col + 1..line.len()).find(|&i| line[i] == c)?;
            }
            inclusive(CursorPos {
                row: cursor.row,
                col: if till { col - 1 } else { col },
            })
        }
        Motion::FindBackward(c) | Motion::TillBackward(c) => {
            let line: Vec<char> = buf.line(cursor.row).unwrap_or("").chars().collect();
            let till = matches!(motion, Motion::TillBackward(_));
            let mut col = cursor.col.saturating_sub(usize::from(till));
            for _ in 0..count {
                col = (0..col).rev().find(|&i| line[i] == c)?;
            }
            exclusive(CursorPos {
                row: cursor.row,
                col: if till { col + 1 } else { col },
            })
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextObject {
    Word,
    BigWord,
    /// Quoted string delimited by this quote character.
    Quote(char),
    /// Bracketed block, by its opening bracket.
    Bracket(char),
    Paragraph,
}

impl TextObject {
    /// Map a keymap text-object id onto `(object, around)`.
    pub fn from_id(id: &str) -> Option<(Self, bool)> {
        let (around, name) = if let Some(name) = id.strip_prefix("textobj.inner_") {
            (false, name)
        } else {
            (true, id.strip_prefix("textobj.around_")?)
        };
        let object = match name {
            "word" => Self::Word,
            "bigword" => Self::BigWord,
            "dquote" => Self::Quote('"'),
            "squote" => Self::Quote('\''),
            "backtick" => Self::Quote('`'),
            "paren" => Self::Bracket('('),
            "bracket" => Self::Bracket('['),
            "brace" => Self::Bracket('{'),
            "angle" => Self::Bracket('<'),
            "paragraph" => Self::Paragraph,
            _ => return None,
        };
        Some((object, around))
    }
}

/// A span chosen by a text object. Charwise spans are half-open; linewise
/// spans cover rows `start.row..=end.row`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: CursorPos,
    pub end: CursorPos,
    pub linewise: bool,
}

pub fn text_object(buf: &TextBuffer, object: TextObject, around: bool) -> Option<Span> {
    let text = Chars::new(buf);
    let cursor = text.offset(buf.cursor());
    let charwise = |start: usize, end: usize| Span {
        start: text.pos(start),
        end: text.pos(end),
        linewise: false,
    };
    match object {
        TextObject::Word | TextObject::BigWord => {
            let (start, end) = text.word_object(cursor, object == TextObject::BigWord, around)?;
            Some(charwise(start, end))
        }
        TextObject::Quote(quote) => {
            let row = buf.cursor().row;
            let line: Vec<char> = buf.line(row).unwrap_or("").chars().collect();
            let (open, close) = quote_pair(&line, buf.cursor().col, quote)?;
            let line_start = text.offset(CursorPos { row, col: 0 });
            if around {
                let mut end = close + 1;
                while end < line.len() && matches!(line[end], ' ' | '\t') {
                    end += 1;
                }
                Some(charwise(line_start + open, line_start + end))
            } else {
                Some(charwise(line_start + open + 1, line_start + close))
            }
        }
        TextObject::Bracket(open_char) => {
            let close_char = closing(open_char)?;
            let (open, close) = text.enclosing_pair(cursor, open_char, close_char)?;
            if around {
                return Some(charwise(open, close + 1));
            }
            // A block spanning lines keeps its brackets on their own lines.
            let mut start = open + 1;
            let mut end = close;
            if text.get(start) == Some('\n') {
                start += 1;
                let mut line_start = end;
                while line_start > start && matches!(text.get(line_start - 1), Some(' ' | '\t')) {
                    line_start -= 1;
                }
                if text.get(line_start.wrapping_sub(1)) == Some('\n') && line_start > start {
                    end = line_start;
                }
            }
            Some(charwise(start, end.max(start)))
        }
        TextObject::Paragraph => {
            let row = buf.cursor().row;
            let blank = is_blank(buf, row);
            let last = buf.line_count().saturating_sub(1);
            let mut start = row;
            while start > 0 && is_blank(buf, start - 1) == blank {
                start -= 1;
            }
            let mut end = row;
            while end < last && is_blank(buf, end + 1) == blank {
                end += 1;
            }
            if around {
                while end < last && is_blank(buf, end + 1) != blank {
                    end += 1;
                }
            }
            Some(Span {
                start: CursorPos { row: start, col: 0 },
                end: CursorPos { row: end, col: 0 },
                linewise: true,
            })
        }
    }
}

fn closing(open: char) -> Option<char> {
    Some(match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        _ => return None,
    })
}

fn is_blank(buf: &TextBuffer, row: usize) -> bool {
    buf.line(row).is_none_or(|line| line.trim().is_empty())
}

/// Row of the next (or previous) blank line beyond the current paragraph.
fn next_paragraph_boundary(buf: &TextBuffer, row: usize, forward: bool) -> usize {
    let last = buf.line_count().saturating_sub(1);
    let mut row = row;
    let step = |row: usize| if forward { (row < last).then(|| row + 1) } else { row.checked_sub(1) };
    let edge = if forward { last } else { 0 };
    // Leave any blank run we start in, then stop at the next blank line.
    while is_blank(buf, row) {
        match step(row) {
            Some(next) => row = next,
            None => return edge,
        }
    }
    while let Some(next) = step(row) {
        row = next;
        if is_blank(buf, row) {
            return row;
        }
    }
    edge
}

/// Quote pair on one line containing `col`, or the first pair after it.
fn quote_pair(line: &[char], col: usize, quote: char) -> Option<(usize, usize)> {
    let positions: Vec<usize> = (0..line.len())
        .filter(|&i| line[i] == quote && (i == 0 || line[i - 1] != '\\'))
        .collect();
    positions
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .find(|&(_, close)| col <= close)
}

/// Character class used by word motions: whitespace, punctuation, or word.
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        2
    } else {
        1
    }
}

/// The buffer as one flat character array with row/column conversion.
struct Chars {
    chars: Vec<char>,
    line_starts: Vec<usize>,
}

impl Chars {
    fn new(buf: &TextBuffer) -> Self {
        let chars: Vec<char> = buf.as_str().chars().collect();
        let mut line_starts = vec![0];
        for (index, &c) in chars.iter().enumerate() {
            if c == '\n' {
                line_starts.push(index + 1);
            }
        }
        Self { chars, line_starts }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn get(&self, offset: usize) -> Option<char> {
        self.chars.get(offset).copied()
    }

    fn offset(&self, pos: CursorPos) -> usize {
        let row = pos.row.min(self.line_starts.len() - 1);
        let start = self.line_starts[row];
        let end = self
            .line_starts
            .get(row + 1)
            .map_or(self.len(), |next| next - 1);
        (start + pos.col).min(end)
    }

    fn pos(&self, offset: usize) -> CursorPos {
        let offset = offset.min(self.len());
        let row = self.line_starts.partition_point(|&start| start <= offset) - 1;
        CursorPos {
            row,
            col: offset - self.line_starts[row],
        }
    }

    fn class(&self, offset: usize, big: bool) -> u8 {
        self.get(offset).map_or(0, |c| class(c, big))
    }

    fn word_next(&self, offset: usize, big: bool) -> usize {
        let mut i = offset;
        if i >= self.len() {
            return self.len();
        }
        let start_class = self.class(i, big);
        if start_class != 0 {
            while i < self.len() && self.class(i, big) == start_class {
                i += 1;
            }
        }
        while i < self.len() && self.class(i, big) == 0 {
            // An empty line counts as a word of its own.
            if self.get(i) == Some('\n') && i > offset && self.get(i - 1) == Some('\n') {
                return i;
            }
            i += 1;
        }
        i
    }

    fn word_back(&self, offset: usize, big: bool) -> usize {
        if offset == 0 {
            return 0;
        }
        let mut i = offset.min(self.len()) - 1;
        while i > 0 && self.class(i, big) == 0 {
            i -= 1;
        }
        let word_class = self.class(i, big);
        if word_class == 0 {
            return 0;
        }
        while i > 0 && self.class(i - 1, big) == word_class {
            i -= 1;
        }
        i
    }

    fn word_end(&self, offset: usize, big: bool) -> usize {
        let last = self.len().saturating_sub(1);
        let mut i = offset + 1;
        while i < self.len() && self.class(i, big) == 0 {
            i += 1;
        }
        if i >= self.len() {
            return last;
        }
        let word_class = self.class(i, big);
        while i + 1 < self.len() && self.class(i + 1, big) == word_class {
            i += 1;
        }
        i
    }

    /// `iw`/`aw`: the run of one class under the cursor (whitespace included),
    /// plus trailing (else leading) blanks for `aw`.
    fn word_object(&self, offset: usize, big: bool, around: bool) -> Option<(usize, usize)> {
        if self.len() == 0 {
            return None;
        }
        let offset = offset.min(self.len() - 1);
        if self.get(offset) == Some('\n') {
            return Some((offset, offset));
        }
        let blank = |i: usize| matches!(self.get(i), Some(' ' | '\t'));
        let run_class = self.class(offset, big);
        let same = |i: usize| {
            self.get(i).is_some_and(|c| c != '\n') && self.class(i, big) == run_class
        };
        let mut start = offset;
        while start > 0 && same(start - 1) {
            start -= 1;
        }
        let mut end = offset + 1;
        while same(end) {
            end += 1;
        }
        if around && run_class != 0 {
            let before = end;
            while blank(end) {
                end += 1;
            }
            if end == before {
                while start > 0 && blank(start - 1) {
                    start -= 1;
                }
            }
        }
        Some((start, end))
    }

    /// `%`: the bracket at or after the cursor on its line, and its partner.
    fn match_pair(&self, offset: usize) -> Option<usize> {
        let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
        let mut i = offset;
        let found = loop {
            let c = self.get(i)?;
            if c == '\n' {
                return None;
            }
            if pairs.iter().any(|&(open, close)| c == open || c == close) {
                break i;
            }
            i += 1;
        };
        let c = self.get(found)?;
        if let Some(&(open, close)) = pairs.iter().find(|&&(open, _)| open == c) {
            let mut depth = 0usize;
            for j in found..self.len() {
                match self.get(j) {
                    Some(x) if x == open => depth += 1,
                    Some(x) if x == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j);
                        }
                    }
                    _ => {}
                }
            }
            None
        } else {
            let &(open, close) = pairs.iter().find(|&&(_, close)| close == c)?;
            let mut depth = 0usize;
            for j in (0..=found).rev() {
                match self.get(j) {
                    Some(x) if x == close => depth += 1,
                    Some(x) if x == open => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
    }

    /// Innermost `open`…`close` pair surrounding the cursor (a cursor on
    /// either bracket counts as inside).
    fn enclosing_pair(&self, offset: usize, open: char, close: char) -> Option<(usize, usize)> {
        let offset = offset.min(self.len().saturating_sub(1));
        let start = if self.get(offset) == Some(open) {
            offset
        } else {
            // Walk back to the first unmatched `open`; a cursor sitting on
            // `close` starts just before it.
            let from = if self.get(offset) == Some(close) {
                offset.checked_sub(1)?
            } else {
                offset
            };
            let mut depth = 0usize;
            let mut found = None;
            for j in (0..=from).rev() {
                match self.get(j) {
                    Some(x) if x == close => depth += 1,
                    Some(x) if x == open => {
                        if depth == 0 {
                            found = Some(j);
                            break;
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            found?
        };
        let mut depth = 0usize;
        for j in start + 1..self.len() {
            match self.get(j) {
                Some(x) if x == open => depth += 1,
                Some(x) if x == close => {
                    if depth == 0 {
                        return Some((start, j));
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, row: usize, col: usize) -> TextBuffer {
        let mut buf = TextBuffer::new(text);
        buf.set_cursor(CursorPos { row, col });
        buf
    }

    fn go(buf: &TextBuffer, motion: Motion, count: u32) -> (usize, usize, MotionKind) {
        let target = motion_target(buf, motion, count, MotionCtx::default()).expect("moves");
        (target.pos.row, target.pos.col, target.kind)
    }

    #[test]
    fn word_motions_respect_classes_and_bigwords() {
        let buf = at("foo.bar(baz) qux", 0, 0);
        assert_eq!(go(&buf, Motion::WordNext, 1).1, 3);
        assert_eq!(go(&buf, Motion::WordNext, 2).1, 4);
        assert_eq!(go(&buf, Motion::BigWordNext, 1).1, 13);
        assert_eq!(go(&buf, Motion::WordEnd, 1), (0, 2, MotionKind::Inclusive));
        assert_eq!(go(&buf, Motion::BigWordEnd, 1).1, 11);
        let end = at("foo.bar(baz) qux", 0, 13);
        assert_eq!(go(&end, Motion::BigWordBack, 1).1, 0);
        assert_eq!(go(&end, Motion::WordBack, 1).1, 11);
    }

    #[test]
    fn line_motions_and_finds() {
        let buf = at("    x = f(a, b)", 0, 6);
        assert_eq!(go(&buf, Motion::LineStart, 1).1, 0);
        assert_eq!(go(&buf, Motion::FirstNonBlank, 1).1, 4);
        assert_eq!(go(&buf, Motion::LineEnd, 1), (0, 14, MotionKind::Inclusive));
        assert_eq!(go(&buf, Motion::FindForward(','), 1).1, 11);
        assert_eq!(go(&buf, Motion::TillForward(','), 1).1, 10);
        assert_eq!(go(&buf, Motion::FindBackward('x'), 1).1, 4);
        assert_eq!(go(&buf, Motion::TillBackward('x'), 1).1, 5);
        assert!(motion_target(&buf, Motion::FindForward('z'), 1, MotionCtx::default()).is_none());
    }

    #[test]
    fn match_pair_and_paragraphs() {
        let buf = at("f(a[1], {b})\n\nnext\npara\n\nend", 0, 0);
        assert_eq!(go(&buf, Motion::MatchPair, 1).1, 11);
        let close = at("f(a[1], {b})", 0, 11);
        assert_eq!(go(&close, Motion::MatchPair, 1).1, 1);
        assert_eq!(go(&buf, Motion::ParagraphNext, 1), (1, 0, MotionKind::Exclusive));
        assert_eq!(go(&buf, Motion::ParagraphNext, 2).0, 4);
        let bottom = at("a\n\nb\nc", 3, 0);
        assert_eq!(go(&bottom, Motion::ParagraphPrev, 1).0, 1);
    }

    #[test]
    fn vertical_motions_use_the_sticky_column() {
        let buf = at("long line\nab\nanother long", 0, 7);
        let ctx = MotionCtx {
            desired_col: Some(7),
            half_page: 10,
        };
        let down = motion_target(&buf, Motion::Down, 2, ctx).unwrap();
        assert_eq!((down.pos.row, down.pos.col), (2, 7));
        let short = motion_target(&buf, Motion::Down, 1, ctx).unwrap();
        assert_eq!((short.pos.row, short.pos.col), (1, 2));
    }

    fn span(buf: &TextBuffer, object: TextObject, around: bool) -> String {
        let span = text_object(buf, object, around).expect("span");
        buf.yank_span(span.start, span.end)
    }

    #[test]
    fn text_objects() {
        let buf = at("call(\"a b\", x[0]) end", 0, 7);
        assert_eq!(span(&buf, TextObject::Quote('"'), false), "a b");
        assert_eq!(span(&buf, TextObject::Quote('"'), true), "\"a b\"");
        assert_eq!(span(&buf, TextObject::Bracket('('), false), "\"a b\", x[0]");
        assert_eq!(span(&buf, TextObject::Bracket('('), true), "(\"a b\", x[0])");
        let word = at("alpha beta gamma", 0, 7);
        assert_eq!(span(&word, TextObject::Word, false), "beta");
        assert_eq!(span(&word, TextObject::Word, true), "beta ");
        let block = at("if x {\n    a\n    b\n}", 1, 4);
        assert_eq!(span(&block, TextObject::Bracket('{'), false), "    a\n    b\n");
        let para = at("a\nb\n\nc", 0, 0);
        let p = text_object(&para, TextObject::Paragraph, true).unwrap();
        assert_eq!((p.start.row, p.end.row, p.linewise), (0, 2, true));
    }
}
