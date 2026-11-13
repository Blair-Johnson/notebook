//! Built-in `text/plain` fallback renderer for `OutputData::Rich` MIME bundles.

use super::stream::{render_rows, wrapped_row_count};
use nb_core::{OutputData, OutputItem};
use nb_sdk::{
    InputOutcome, KeyContextId, MountCtx, OutputHost, OutputMetrics, OutputRenderer, OutputView,
    ResolvedInput, Theme,
};
use ratatui::text::Line;
use serde_json::Value;
use std::borrow::Cow;
use std::ops::Range;

pub struct PlainTextRenderer;

impl OutputRenderer for PlainTextRenderer {
    fn id(&self) -> &'static str {
        "core.text_plain"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        &[("text/plain", 10)]
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        match &item.data {
            OutputData::Rich { .. } => Some(10),
            _ => None,
        }
    }

    fn mount(&self, _item: &OutputItem, _ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(PlainTextView {
            rev: 1,
            ..Default::default()
        })
    }
}

#[derive(Default)]
pub struct PlainTextView {
    rev: u64,
    line_row_starts: Vec<u32>,
    line_byte_starts: Vec<usize>,
    /// (output revision, width) the line index was built for.
    indexed: Option<(u64, u16)>,
    /// Text built from the bundle when it is not a single `text/plain`
    /// string, kept for the output revision it was built from.
    built: Option<(u64, String)>,
    total_rows: u32,
    shown_rows: u32,
    scroll_offset: Option<u32>,
}

impl PlainTextView {
    pub fn scroll_offset(&self) -> Option<u32> {
        self.scroll_offset
    }

    pub fn line_row_starts(&self) -> &[u32] {
        &self.line_row_starts
    }

    fn visible_start(&self) -> u32 {
        self.scroll_offset
            .unwrap_or_else(|| self.total_rows.saturating_sub(self.shown_rows))
    }

    fn max_scroll(&self) -> u32 {
        self.total_rows.saturating_sub(self.shown_rows)
    }

    fn move_scroll(&mut self, delta: i64) {
        let current = i64::from(self.scroll_offset.unwrap_or_else(|| self.max_scroll()));
        let next = (current + delta).clamp(0, i64::from(self.max_scroll())) as u32;
        self.scroll_offset = if next == self.max_scroll() && delta >= 0 {
            None
        } else {
            Some(next)
        };
    }

    /// The text shown: borrowed when `text/plain` is one string, else the
    /// cached build (or a fresh one, if `measure` has not run for this revision).
    fn source<'a>(&'a self, item: &'a OutputItem) -> Cow<'a, str> {
        let OutputData::Rich { bundle, .. } = &item.data else {
            return Cow::Borrowed("");
        };
        if let Some(Value::String(text)) = bundle.get("text/plain") {
            return Cow::Borrowed(text);
        }
        match &self.built {
            Some((rev, text)) if *rev == item.rev => Cow::Borrowed(text),
            _ => Cow::Owned(build_text(item)),
        }
    }

    fn command_is(input: &ResolvedInput, name: &str) -> bool {
        matches!(input, ResolvedInput::Command { id, .. } if id.0.as_ref() == name)
    }

    fn motion(input: &ResolvedInput, name: &str) -> Option<u32> {
        match input {
            ResolvedInput::Move { motion, count, .. } if motion.0.as_ref() == name => Some(*count),
            _ => None,
        }
    }
}

impl OutputView for PlainTextView {
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
        let width = width.max(1);
        if self.indexed != Some((item.rev, width)) {
            let direct = matches!(
                &item.data,
                OutputData::Rich { bundle, .. } if matches!(bundle.get("text/plain"), Some(Value::String(_)))
            );
            if !direct && self.built.as_ref().is_none_or(|(rev, _)| *rev != item.rev) {
                self.built = Some((item.rev, build_text(item)));
            }
            let (row_starts, byte_starts) = index_lines(&self.source(item), width);
            self.line_row_starts = row_starts;
            self.line_byte_starts = byte_starts;
            self.indexed = Some((item.rev, width));
        }
        self.total_rows = self.line_row_starts.last().copied().unwrap_or(1).max(1);
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
        let source = self.source(item);
        if self.line_row_starts.len() < 2 {
            out.push(Line::default());
            return;
        }
        let local = rows.end <= self.shown_rows;
        let start = if local {
            self.visible_start().saturating_add(rows.start)
        } else {
            rows.start
        };
        let end = if local {
            self.visible_start().saturating_add(rows.end)
        } else {
            rows.end
        };
        let mut line_index = self
            .line_row_starts
            .partition_point(|row| *row <= start)
            .saturating_sub(1);
        while line_index < self.line_byte_starts.len() {
            let line_start = self.line_row_starts[line_index];
            if line_start >= end {
                break;
            }
            let byte_start = self.line_byte_starts[line_index];
            let byte_end = self
                .line_byte_starts
                .get(line_index + 1)
                .copied()
                .unwrap_or(source.len())
                .min(source.len());
            let text = source[byte_start..byte_end].trim_end_matches(['\r', '\n']);
            let local = start.saturating_sub(line_start)..end - line_start;
            render_rows(text, width, local, out);
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
        if Self::motion(&input, "motion.top").is_some() {
            self.scroll_offset = Some(0);
            return InputOutcome::Consumed;
        }
        if Self::motion(&input, "motion.bottom").is_some() {
            self.scroll_offset = None;
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion(&input, "motion.down") {
            self.move_scroll(i64::from(count));
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion(&input, "motion.up") {
            self.move_scroll(-i64::from(count));
            return InputOutcome::Consumed;
        }
        if Self::motion(&input, "motion.half_page_down").is_some() {
            self.move_scroll(i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        if Self::motion(&input, "motion.half_page_up").is_some() {
            self.move_scroll(-i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        InputOutcome::Ignored
    }
}

fn value_to_text(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(text) => Cow::Borrowed(text),
        Value::Array(values) => Cow::Owned(
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned)
                })
                .collect::<String>(),
        ),
        Value::Null => Cow::Borrowed(""),
        other => Cow::Owned(other.to_string()),
    }
}

/// The text of a bundle without a single `text/plain` string: a joined
/// nbformat line array, or each MIME value labeled. Binary payloads (base64
/// images, audio, PDFs) show as a size; every text type is shown in full.
fn build_text(item: &OutputItem) -> String {
    let OutputData::Rich { bundle, .. } = &item.data else {
        return String::new();
    };
    if let Some(value) = bundle.get("text/plain") {
        return value_to_text(value).into_owned();
    }
    bundle
        .iter()
        .map(|(key, value)| match value {
            Value::String(text) if is_binary_mime(key) => {
                format!("[{key}] <{} KB>", text.len().div_ceil(1024))
            }
            _ => format!("[{key}] {}", value_to_text(value)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// MIME types whose payload is base64 data, not readable text.
fn is_binary_mime(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    (mime.starts_with("image/") && mime != "image/svg+xml")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || matches!(mime, "application/pdf" | "application/octet-stream")
}

/// Per logical line of `source`: the first wrapped row (with a final total)
/// and the byte offset where it starts.
fn index_lines(source: &str, width: u16) -> (Vec<u32>, Vec<usize>) {
    let mut row_starts = vec![0];
    let mut byte_starts = Vec::new();
    if source.is_empty() {
        byte_starts.push(0);
        row_starts.push(1);
        return (row_starts, byte_starts);
    }
    let mut row = 0u32;
    let mut start = 0usize;
    while start < source.len() {
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |offset| start + offset);
        let content_end = if end > start && source.as_bytes()[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        byte_starts.push(start);
        row = row.saturating_add(wrapped_row_count(&source[start..content_end], width));
        row_starts.push(row);
        start = if end < source.len() {
            end + 1
        } else {
            source.len()
        };
    }
    (row_starts, byte_starts)
}
