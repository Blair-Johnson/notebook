//! Built-in ANSI `stdout`/`stderr` stream output renderer with incremental row-slice wrapping.

use ansi_to_tui::IntoText;
use nb_core::output::TRUNCATION_MARKER;
use nb_core::{OutputData, OutputItem, StreamBuffer};
use nb_sdk::{
    InputOutcome, KeyContextId, MountCtx, OutputHost, OutputMetrics, OutputRenderer, OutputView,
    ResolvedInput, Theme,
};
use ratatui::text::{Line, Span};
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

pub struct StreamRenderer;

impl OutputRenderer for StreamRenderer {
    fn id(&self) -> &'static str {
        "core.stream"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        &[]
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        matches!(item.data, OutputData::Stream { .. }).then_some(100)
    }

    fn mount(&self, _item: &OutputItem, _ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(StreamOutputView::default())
    }
}

/// A stream view owns indexes, not the stream text. The model remains the sole owner of the
/// potentially very large `String`.
#[derive(Default)]
pub struct StreamOutputView {
    wrapped_for_width: u16,
    line_row_starts: Vec<u32>,
    line_byte_starts: Vec<usize>,
    measured_bytes: usize,
    measured_revision: u64,
    measured_truncated: usize,
    total_rows: u32,
    shown_rows: u32,
    scroll_offset: Option<u32>, // None = follow tail
    rev: u64,
}

impl StreamOutputView {
    /// Return the prefix array used for row-to-source-line lookup.
    pub fn line_row_starts(&self) -> &[u32] {
        &self.line_row_starts
    }

    /// Return the current absolute output scroll offset, if detached from the tail.
    pub fn scroll_offset(&self) -> Option<u32> {
        self.scroll_offset
    }

    /// Return the absolute rows currently visible to the viewport.
    pub fn visible_rows(&self) -> Range<u32> {
        let start = self.visible_start();
        start..start.saturating_add(self.shown_rows)
    }

    fn visible_start(&self) -> u32 {
        self.scroll_offset
            .unwrap_or_else(|| self.total_rows.saturating_sub(self.shown_rows))
    }

    fn text(item: &OutputItem) -> &str {
        Self::buffer(item).map_or("", StreamBuffer::as_str)
    }

    fn buffer(item: &OutputItem) -> Option<&StreamBuffer> {
        match &item.data {
            OutputData::Stream { buffer, .. } => Some(buffer),
            _ => None,
        }
    }

    fn rebuild_index(&mut self, text: &str, width: u16) {
        self.wrapped_for_width = width.max(1);
        self.line_row_starts.clear();
        self.line_byte_starts.clear();
        self.line_row_starts.push(0);
        self.append_index_from(text, 0, 0);
    }

    fn append_index_from(&mut self, text: &str, byte_start: usize, row_start: u32) {
        let width = self.wrapped_for_width.max(1);
        let mut row_count = row_start;
        let mut line_start = byte_start.min(text.len());
        if text.is_empty() {
            if self.line_byte_starts.is_empty() {
                self.line_byte_starts.push(0);
                self.line_row_starts.push(1);
            }
            return;
        }

        while line_start < text.len() {
            let rest = &text[line_start..];
            let line_end = rest
                .find('\n')
                .map_or(text.len(), |offset| line_start + offset);
            let content_end = if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
                line_end - 1
            } else {
                line_end
            };
            self.line_byte_starts.push(line_start);
            let rows = wrapped_row_count(&text[line_start..content_end], width);
            row_count = row_count.saturating_add(rows);
            self.line_row_starts.push(row_count);
            line_start = if line_end < text.len() {
                line_end + 1
            } else {
                text.len()
            };
        }
        if self.line_byte_starts.is_empty() {
            self.line_byte_starts.push(0);
            self.line_row_starts.push(row_start.saturating_add(1));
        }
    }

    fn ensure_index(&mut self, buffer: Option<&StreamBuffer>, width: u16) {
        let text = buffer.map_or("", StreamBuffer::as_str);
        let width = width.max(1);
        let (revision, truncated) = buffer.map_or((0, 0), |b| (b.revision(), b.truncated_lines()));
        let rebuilt = if self.line_row_starts.is_empty() || self.wrapped_for_width != width {
            true
        } else if truncated != self.measured_truncated {
            // The buffer dropped leading lines: shift the index, or start over.
            !self.drop_leading_lines(text, truncated)
        } else {
            // Shrinking without truncation means different text altogether.
            text.len() < self.measured_bytes
        };
        if rebuilt {
            self.rebuild_index(text, width);
        } else if revision != self.measured_revision || text.len() != self.measured_bytes {
            // Only the last line can change in place; re-index from its start.
            let recompute_line = self.line_byte_starts.len().saturating_sub(1);
            let byte_start = self
                .line_byte_starts
                .get(recompute_line)
                .copied()
                .unwrap_or(0);
            let row_start = self
                .line_row_starts
                .get(recompute_line)
                .copied()
                .unwrap_or(0);
            self.line_byte_starts.truncate(recompute_line);
            self.line_row_starts.truncate(recompute_line + 1);
            self.append_index_from(text, byte_start, row_start);
        }
        self.measured_revision = revision;
        self.measured_truncated = truncated;
        self.measured_bytes = text.len();
        self.total_rows = self.line_row_starts.last().copied().unwrap_or(0).max(1);
        if self.scroll_offset.is_some() {
            self.scroll_offset = Some(self.visible_start().min(self.max_scroll()));
        }
    }

    /// Rebase the index after the buffer replaced its oldest lines with the
    /// truncation marker. Lines indexed before are unchanged apart from the
    /// last one, which the caller re-indexes. Returns false when the index
    /// cannot be shifted (it never saw the new first line).
    fn drop_leading_lines(&mut self, text: &str, truncated: usize) -> bool {
        let Some(dropped) = truncated.checked_sub(self.measured_truncated) else {
            return false;
        };
        let first_kept = usize::from(self.measured_truncated > 0) + dropped;
        if first_kept >= self.line_byte_starts.len() || !text.starts_with(TRUNCATION_MARKER) {
            return false;
        }
        let old_byte = self.line_byte_starts[first_kept];
        let old_row = self.line_row_starts[first_kept];
        let marker_rows = wrapped_row_count(
            TRUNCATION_MARKER.trim_end_matches('\n'),
            self.wrapped_for_width,
        );
        self.line_byte_starts.drain(..first_kept);
        self.line_row_starts.drain(..first_kept);
        for start in &mut self.line_byte_starts {
            *start = *start - old_byte + TRUNCATION_MARKER.len();
        }
        for start in &mut self.line_row_starts {
            *start = *start - old_row + marker_rows;
        }
        self.line_byte_starts.insert(0, 0);
        self.line_row_starts.insert(0, 0);
        true
    }

    fn max_scroll(&self) -> u32 {
        self.total_rows.saturating_sub(self.shown_rows)
    }

    fn move_scroll(&mut self, delta: i64) {
        let mut offset = i64::from(self.scroll_offset.unwrap_or_else(|| self.max_scroll()));
        offset = (offset + delta).clamp(0, i64::from(self.max_scroll()));
        self.scroll_offset = Some(offset as u32);
        if offset as u32 == self.max_scroll() && delta >= 0 {
            self.scroll_offset = None;
        }
    }

    fn command_is(input: &ResolvedInput, name: &str) -> bool {
        matches!(input, ResolvedInput::Command { id, .. } if id.0.as_ref() == name)
    }

    fn motion_is(input: &ResolvedInput, name: &str) -> Option<u32> {
        match input {
            ResolvedInput::Move { motion, count, .. } if motion.0.as_ref() == name => Some(*count),
            _ => None,
        }
    }
}

impl OutputView for StreamOutputView {
    fn on_output_updated(&mut self, _item: &OutputItem, _ctx: &MountCtx) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// Plain text: entering the output gives vim navigation over it instead.
    fn is_focusable(&self) -> bool {
        false
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        _theme: &Theme,
        cap_rows: Option<u16>,
    ) -> OutputMetrics {
        self.ensure_index(Self::buffer(item), width);
        self.shown_rows = cap_rows
            .map_or(self.total_rows, u32::from)
            .min(self.total_rows)
            .max(1);
        if let Some(offset) = self.scroll_offset {
            self.scroll_offset = Some(offset.min(self.max_scroll()));
        }
        OutputMetrics {
            total_rows: self.total_rows,
            shown_rows: self.shown_rows,
            clipped: self.total_rows > self.shown_rows,
        }
    }

    fn render_slice(
        &self,
        item: &OutputItem,
        width: u16,
        rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        let source = Self::text(item);
        if source.is_empty() || self.line_row_starts.len() < 2 {
            out.push(Line::default());
            return;
        }
        let visible_start = self.visible_start();
        let local_request = rows.end <= self.shown_rows;
        let absolute_start = if local_request {
            visible_start.saturating_add(rows.start)
        } else {
            rows.start
        };
        let absolute_end = if local_request {
            visible_start.saturating_add(rows.end)
        } else {
            rows.end
        };
        let first_line = self
            .line_row_starts
            .partition_point(|start| *start <= absolute_start)
            .saturating_sub(1);
        let mut line_index = first_line;
        while line_index < self.line_byte_starts.len() {
            let row_start = self.line_row_starts[line_index];
            if row_start >= absolute_end {
                break;
            }
            let byte_start = self.line_byte_starts[line_index];
            let byte_end = self
                .line_byte_starts
                .get(line_index + 1)
                .copied()
                .unwrap_or(source.len())
                .min(source.len());
            let raw = source[byte_start..byte_end].trim_end_matches(['\r', '\n']);
            let local = absolute_start.saturating_sub(row_start)..absolute_end - row_start;
            render_rows(raw, width, local, out);
            line_index += 1;
        }
    }

    fn handle_input(
        &mut self,
        _item: &OutputItem,
        input: ResolvedInput,
        _host: &mut dyn OutputHost,
    ) -> InputOutcome {
        if Self::command_is(&input, "focus.up") {
            return InputOutcome::PopFocus;
        }
        if let Some(count) = Self::motion_is(&input, "motion.top") {
            let _ = count;
            self.scroll_offset = Some(0);
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion_is(&input, "motion.bottom") {
            let _ = count;
            self.scroll_offset = None;
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion_is(&input, "motion.down") {
            self.move_scroll(i64::from(count));
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion_is(&input, "motion.up") {
            self.move_scroll(-i64::from(count));
            return InputOutcome::Consumed;
        }
        if Self::motion_is(&input, "motion.half_page_down").is_some() {
            self.move_scroll(i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        if Self::motion_is(&input, "motion.half_page_up").is_some() {
            self.move_scroll(-i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        InputOutcome::Ignored
    }
}

/// Rows `line` wraps to; matches what [`render_rows`] draws.
pub(crate) fn wrapped_row_count(line: &str, width: u16) -> u32 {
    let mut wrap = RowWrap::new(width);
    for (_, ch) in visible_chars(line) {
        wrap.place(ch);
    }
    wrap.row + 1
}

/// One wrapping rule for measuring and drawing: a character that does not
/// fit on the current row starts the next one.
struct RowWrap {
    width: usize,
    used: usize,
    row: u32,
}

impl RowWrap {
    fn new(width: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            used: 0,
            row: 0,
        }
    }

    /// Place `ch` and return the row it lands on.
    fn place(&mut self, ch: char) -> u32 {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width > 0 && self.used > 0 && self.used + char_width > self.width {
            self.row += 1;
            self.used = 0;
        }
        self.used += char_width;
        self.row
    }
}

/// Characters of `line` that take up columns, with their byte offsets;
/// ANSI CSI and OSC escape sequences are skipped.
fn visible_chars(line: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut chars = line.char_indices().peekable();
    std::iter::from_fn(move || {
        loop {
            let (offset, ch) = chars.next()?;
            if ch != '\u{1b}' {
                return Some((offset, ch));
            }
            match chars.next() {
                Some((_, '[')) => {
                    for (_, ch) in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some((_, ']')) => {
                    while let Some((_, ch)) = chars.next() {
                        if ch == '\u{7}'
                            || (ch == '\u{1b}' && chars.next_if(|(_, ch)| *ch == '\\').is_some())
                        {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    })
}

/// Draw rows `rows` of one logical line. Plain text skips ANSI parsing,
/// and only the requested rows are built.
pub(crate) fn render_rows(line: &str, width: u16, rows: Range<u32>, out: &mut Vec<Line<'static>>) {
    let spans: Vec<Span<'_>> = if line.contains('\u{1b}') {
        match line.into_text() {
            Ok(text) => text.lines.into_iter().flat_map(|line| line.spans).collect(),
            Err(_) => vec![Span::raw(line)],
        }
    } else {
        vec![Span::raw(line)]
    };
    let mut wrap = RowWrap::new(width);
    let mut built: Vec<Vec<Span<'static>>> = Vec::new();
    'spans: for span in &spans {
        for ch in span.content.chars() {
            let row = wrap.place(ch);
            if row >= rows.end {
                break 'spans;
            }
            if row < rows.start {
                continue;
            }
            let index = (row - rows.start) as usize;
            if built.len() <= index {
                built.resize_with(index + 1, Vec::new);
            }
            let row_spans = &mut built[index];
            match row_spans.last_mut() {
                Some(last) if last.style == span.style => last.content.to_mut().push(ch),
                _ => row_spans.push(Span::styled(ch.to_string(), span.style)),
            }
        }
    }
    if built.is_empty() && rows.start == 0 && rows.end > 0 {
        built.push(Vec::new());
    }
    out.extend(built.into_iter().map(Line::from));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nb_core::StreamKind;

    fn item(buffer: &StreamBuffer) -> OutputItem {
        OutputItem::new(
            OutputData::Stream {
                stream: StreamKind::Stdout,
                buffer: buffer.clone(),
            },
            false,
        )
    }

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn measured_rows_match_drawn_rows() {
        let long = "z".repeat(150_007);
        let lines = [
            "",
            "plain text that wraps across a few rows",
            "\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mbold green\u{1b}[0m words to wrap",
            "wide 漢字漢字漢字漢字 chars",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
            long.as_str(),
        ];
        for line in lines {
            for width in [1, 7, 10, 80] {
                let rows = wrapped_row_count(line, width);
                let mut full = Vec::new();
                render_rows(line, width, 0..u32::MAX, &mut full);
                assert_eq!(full.len() as u32, rows, "{line:.40?} at width {width}");
                let window = rows / 3..(rows / 3 + 2).min(rows);
                let mut part = Vec::new();
                render_rows(line, width, window.clone(), &mut part);
                assert_eq!(
                    text_of(&part),
                    text_of(&full[window.start as usize..window.end as usize]),
                    "{line:.40?} at width {width}"
                );
            }
        }
        // Nothing is cut: the last row holds the line's last characters.
        let mut last = Vec::new();
        render_rows(
            &long,
            80,
            wrapped_row_count(&long, 80) - 1..u32::MAX,
            &mut last,
        );
        assert_eq!(text_of(&last), ["z".repeat(150_007 % 80)]);
    }

    /// Growing, rewriting and truncating keeps the index equal to a fresh one.
    #[test]
    fn incremental_index_matches_rebuild() {
        let theme = Theme::default();
        let chunks = [
            "short\n",
            "a much longer line that wraps at width ten\n",
            "progress 1",
            "\rprogress 2",
            "\rprogress 3\n",
            "é漢é漢é漢é漢é漢é漢\n",
            "tail",
            " more tail\nnext\n",
            "x\ny\nz\n",
        ];
        let mut buffer = StreamBuffer::with_cap(4);
        let mut view = StreamOutputView::default();
        for (step, chunk) in chunks.iter().cycle().take(60).enumerate() {
            buffer.push_chunk(chunk);
            let item = item(&buffer);
            let metrics = view.measure(&item, 10, &theme, None);
            let mut fresh = StreamOutputView::default();
            let expected = fresh.measure(&item, 10, &theme, None);
            assert_eq!(metrics, expected, "step {step}");
            assert_eq!(view.line_byte_starts, fresh.line_byte_starts, "step {step}");
            assert_eq!(view.line_row_starts, fresh.line_row_starts, "step {step}");
        }
    }
}
