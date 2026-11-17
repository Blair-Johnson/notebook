//! Built-in rich MIME output renderers (`image/*`, `text/vnd.mermaid`, `text/latex`, `text/markdown`).
//!
//! Implemented strictly against `nb_sdk::OutputRenderer` and `nb_sdk::OutputView`
//! (following the dogfooding rule in `builtins/mod.rs`). Because Kitty Unicode
//! placeholders are ordinary `Line<'static>` rows carrying exact `(row, col)`
//! diacritics, `render_slice` clips partially scrolled or preview-capped plots,
//! SVGs, equations, and diagrams cleanly at any row boundary, while seamlessly
//! falling back to readable text when terminal images are off or still decoding.

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use nb_core::{MimeBundle, OutputData, OutputId, OutputItem};
use nb_sdk::{
    ClipEnd, InputOutcome, KeyContextId, MountCtx, OutputHost, OutputMetrics, OutputRenderer,
    OutputView, ResolvedInput, Theme,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

use super::markdown::render_markdown_cell;
use super::plain::PlainTextView;
use super::stream::render_rows;
use crate::images::{self, NeedsBackground};
use crate::pictures::{self, RichImagePayload};
use crate::{math, mermaid};

const IMAGE_MIMES: &[(&str, u16)] = &[
    ("image/svg+xml", 90),
    ("image/png", 88),
    ("image/webp", 86),
    ("image/jpeg", 84),
    ("image/jpg", 84),
    ("image/gif", 82),
];

const MERMAID_MIMES: &[(&str, u16)] = &[
    ("text/vnd.mermaid", 81),
    ("application/vnd.mermaid", 81),
];

const LATEX_MIMES: &[(&str, u16)] = &[("text/latex", 80)];
const MARKDOWN_MIMES: &[(&str, u16)] = &[("text/markdown", 75)];

pub fn mime_text<'a>(bundle: &'a MimeBundle, key: &str) -> Option<Cow<'a, str>> {
    match bundle.get(key)? {
        Value::String(text) => Some(Cow::Borrowed(text.as_str())),
        Value::Array(parts) => {
            let mut joined = String::new();
            for part in parts {
                if let Some(s) = part.as_str() {
                    joined.push_str(s);
                }
            }
            Some(Cow::Owned(joined))
        }
        _ => None,
    }
}

fn best_mime_priority(bundle: &MimeBundle, mimes: &[(&str, u16)]) -> Option<u16> {
    mimes
        .iter()
        .filter(|(mime, _)| {
            bundle
                .get(*mime)
                .is_some_and(|val| matches!(val, Value::String(_) | Value::Array(_)))
        })
        .map(|(_, prio)| *prio)
        .max()
}

fn output_needle(id: OutputId) -> String {
    format!("output:{}", id.0)
}

#[derive(Default)]
struct LineSliceScroller {
    lines: Vec<Line<'static>>,
    key: Option<(u64, u16, u64, u64)>,
    total_rows: u32,
    shown_rows: u32,
    scroll_offset: Option<u32>,
    keep: ClipEnd,
}

impl LineSliceScroller {
    fn visible_start(&self) -> u32 {
        self.scroll_offset
            .unwrap_or_else(|| self.keep.first_row(self.total_rows, self.shown_rows))
    }

    fn max_scroll(&self) -> u32 {
        self.total_rows.saturating_sub(self.shown_rows)
    }

    fn move_scroll(&mut self, delta: i64) {
        let current = i64::from(self.visible_start());
        let next = (current + delta).clamp(0, i64::from(self.max_scroll())) as u32;
        let default = self.keep.first_row(self.total_rows, self.shown_rows);
        self.scroll_offset = (next != default).then_some(next);
    }

    fn finish_measure(&mut self, cap_rows: Option<u16>, keep: ClipEnd) -> OutputMetrics {
        self.keep = keep;
        self.total_rows = (self.lines.len() as u32).max(1);
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
            first_shown: self.visible_start(),
        }
    }

    fn render_slice(&self, rows: Range<u32>, out: &mut Vec<Line<'static>>) {
        if self.lines.is_empty() {
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
        let total = self.lines.len();
        let from = (start as usize).min(total);
        let to = (end as usize).min(total);
        out.extend(self.lines[from..to].iter().cloned());
    }

    fn handle_input(&mut self, input: &ResolvedInput) -> InputOutcome {
        if matches!(input, ResolvedInput::Command { id, .. } if id.0.as_ref() == "focus.up") {
            return InputOutcome::PopFocus;
        }
        let motion = |name: &str| match input {
            ResolvedInput::Move { motion, count, .. } if motion.0.as_ref() == name => Some(*count),
            _ => None,
        };
        if motion("motion.top").is_some() {
            self.scroll_offset = Some(0);
            return InputOutcome::Consumed;
        }
        if motion("motion.bottom").is_some() {
            self.scroll_offset = None;
            return InputOutcome::Consumed;
        }
        if let Some(count) = motion("motion.down") {
            self.move_scroll(i64::from(count));
            return InputOutcome::Consumed;
        }
        if let Some(count) = motion("motion.up") {
            self.move_scroll(-i64::from(count));
            return InputOutcome::Consumed;
        }
        if motion("motion.half_page_down").is_some() {
            self.move_scroll(i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        if motion("motion.half_page_up").is_some() {
            self.move_scroll(-i64::from(self.shown_rows.max(1) / 2));
            return InputOutcome::Consumed;
        }
        InputOutcome::Ignored
    }
}

// ---------------------------------------------------------------------------
// 1. Rich Image / SVG Renderer (`image/svg+xml`, `image/png`, `image/jpeg`, `image/webp`, `image/gif`)
// ---------------------------------------------------------------------------

pub struct RichImageRenderer;

impl OutputRenderer for RichImageRenderer {
    fn id(&self) -> &'static str {
        "core.rich_image"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        IMAGE_MIMES
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        let OutputData::Rich { bundle, .. } = &item.data else {
            return None;
        };
        best_mime_priority(bundle, IMAGE_MIMES)
    }

    fn mount(&self, _item: &OutputItem, ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(RichImageView {
            output_id: ctx.output_id,
            rev: 1,
            extracted: None,
            image_scroller: LineSliceScroller::default(),
            using_image: false,
            fallback: PlainTextView::default(),
        })
    }
}

struct ExtractedImage {
    item_rev: u64,
    payload: RichImagePayload,
    payload_hash: u64,
    needs_bg: NeedsBackground,
}

pub struct RichImageView {
    output_id: OutputId,
    rev: u64,
    extracted: Option<ExtractedImage>,
    image_scroller: LineSliceScroller,
    using_image: bool,
    fallback: PlainTextView,
}

impl RichImageView {
    fn ensure_extracted(&mut self, item: &OutputItem) -> Option<&ExtractedImage> {
        if self
            .extracted
            .as_ref()
            .is_some_and(|ext| ext.item_rev == item.rev)
        {
            return self.extracted.as_ref();
        }
        let OutputData::Rich {
            bundle, metadata, ..
        } = &item.data
        else {
            self.extracted = None;
            return None;
        };
        let needs_bg = NeedsBackground::from_metadata(metadata);
        let selected = IMAGE_MIMES.iter().find_map(|(mime, _)| {
            let text = mime_text(bundle, mime)?;
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| (*mime, text))
        })?;
        let (mime, text) = selected;
        let mut hasher = DefaultHasher::new();
        mime.hash(&mut hasher);
        text.as_ref().hash(&mut hasher);
        let payload_hash = hasher.finish();
        let payload = if mime == "image/svg+xml" {
            RichImagePayload::SvgXml(Arc::from(text.as_ref()))
        } else {
            RichImagePayload::RasterBase64(Arc::from(text.as_ref()))
        };
        self.extracted = Some(ExtractedImage {
            item_rev: item.rev,
            payload,
            payload_hash,
            needs_bg,
        });
        self.extracted.as_ref()
    }
}

impl OutputView for RichImageView {
    fn on_output_updated(&mut self, item: &OutputItem, ctx: &MountCtx) {
        self.output_id = ctx.output_id;
        self.rev = self.rev.wrapping_add(1);
        self.image_scroller.key = None;
        self.fallback.on_output_updated(item, ctx);
    }

    fn is_focusable(&self) -> bool {
        false
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev.wrapping_add(images::layout_epoch())
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
        keep: ClipEnd,
    ) -> OutputMetrics {
        let width = width.max(1);
        let epoch = images::layout_epoch();
        if images::enabled()
            && let Some(ext) = self.ensure_extracted(item)
        {
            let payload_hash = ext.payload_hash;
            let payload = ext.payload.clone();
            let needs_bg = ext.needs_bg;
            let key = (item.rev, width, theme.revision, epoch);
            if self.image_scroller.key != Some(key) {
                let needle = output_needle(self.output_id);
                if let Some(lines) = pictures::mime_image_lines(
                    payload_hash,
                    &payload,
                    needs_bg,
                    usize::from(width),
                    theme,
                    &needle,
                ) {
                    self.image_scroller.lines = lines;
                    self.image_scroller.key = Some(key);
                    self.using_image = true;
                    return self.image_scroller.finish_measure(cap_rows, keep);
                }
                self.image_scroller.key = None;
                if self.using_image && !self.image_scroller.lines.is_empty() {
                    return self.image_scroller.finish_measure(cap_rows, keep);
                }
            } else if self.using_image {
                return self.image_scroller.finish_measure(cap_rows, keep);
            }
        }
        self.using_image = false;
        self.fallback.measure(item, width, theme, cap_rows, keep)
    }

    fn render_slice(
        &self,
        item: &OutputItem,
        width: u16,
        rows: Range<u32>,
        focused: bool,
        theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        if self.using_image && images::enabled() {
            self.image_scroller.render_slice(rows, out);
        } else {
            self.fallback
                .render_slice(item, width, rows, focused, theme, out);
        }
    }

    fn handle_input(
        &mut self,
        item: &OutputItem,
        input: ResolvedInput,
        host: &mut dyn OutputHost,
    ) -> InputOutcome {
        if self.using_image {
            self.image_scroller.handle_input(&input)
        } else {
            self.fallback.handle_input(item, input, host)
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Mermaid MIME Renderer (`text/vnd.mermaid`, `application/vnd.mermaid`)
// ---------------------------------------------------------------------------

pub struct MermaidOutputRenderer;

impl OutputRenderer for MermaidOutputRenderer {
    fn id(&self) -> &'static str {
        "core.mermaid"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        MERMAID_MIMES
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        let OutputData::Rich { bundle, .. } = &item.data else {
            return None;
        };
        best_mime_priority(bundle, MERMAID_MIMES)
    }

    fn mount(&self, _item: &OutputItem, ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(MermaidOutputView {
            output_id: ctx.output_id,
            rev: 1,
            scroller: LineSliceScroller::default(),
        })
    }
}

pub struct MermaidOutputView {
    output_id: OutputId,
    rev: u64,
    scroller: LineSliceScroller,
}

impl OutputView for MermaidOutputView {
    fn on_output_updated(&mut self, _item: &OutputItem, ctx: &MountCtx) {
        self.output_id = ctx.output_id;
        self.rev = self.rev.wrapping_add(1);
        self.scroller.key = None;
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev.wrapping_add(images::layout_epoch())
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
        keep: ClipEnd,
    ) -> OutputMetrics {
        let width = width.max(1);
        let epoch = images::layout_epoch();
        let key = (item.rev, width, theme.revision, epoch);
        if self.scroller.key != Some(key) {
            let source = match &item.data {
                OutputData::Rich { bundle, .. } => mime_text(bundle, "text/vnd.mermaid")
                    .or_else(|| mime_text(bundle, "application/vnd.mermaid"))
                    .unwrap_or(Cow::Borrowed("")),
                _ => Cow::Borrowed(""),
            };
            let needle = output_needle(self.output_id);
            let lines = mermaid::diagram_lines_with_needle(
                source.as_ref(),
                usize::from(width),
                theme,
                Some(&needle),
            )
            .unwrap_or_else(|| {
                let fenced = format!("```mermaid\n{}\n```", source.trim_end_matches('\n'));
                render_markdown_cell(&fenced, width, theme).lines
            });
            self.scroller.lines = lines;
            self.scroller.key = Some(key);
        }
        self.scroller.finish_measure(cap_rows, keep)
    }

    fn render_slice(
        &self,
        _item: &OutputItem,
        _width: u16,
        rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        self.scroller.render_slice(rows, out);
    }

    fn handle_input(
        &mut self,
        _item: &OutputItem,
        input: ResolvedInput,
        _host: &mut dyn OutputHost,
    ) -> InputOutcome {
        self.scroller.handle_input(&input)
    }
}

// ---------------------------------------------------------------------------
// 3. LaTeX MIME Renderer (`text/latex`)
// ---------------------------------------------------------------------------

pub struct LatexOutputRenderer;

impl OutputRenderer for LatexOutputRenderer {
    fn id(&self) -> &'static str {
        "core.latex"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        LATEX_MIMES
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        let OutputData::Rich { bundle, .. } = &item.data else {
            return None;
        };
        best_mime_priority(bundle, LATEX_MIMES)
    }

    fn mount(&self, _item: &OutputItem, ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(LatexOutputView {
            output_id: ctx.output_id,
            rev: 1,
            scroller: LineSliceScroller::default(),
        })
    }
}

pub struct LatexOutputView {
    output_id: OutputId,
    rev: u64,
    scroller: LineSliceScroller,
}

fn is_ipython_object_repr(plain: &str) -> bool {
    let trimmed = plain.trim();
    trimmed.starts_with("<IPython.core.display.") && trimmed.ends_with('>')
}

fn wrap_plain_text_lines(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let safe = images::sanitize_placeholders(text);
    let style = Style::default().fg(theme.palette.fg);
    let mut out = Vec::new();
    for raw in safe.split('\n') {
        let clean = raw.trim_end_matches('\r');
        let mut temp = Vec::new();
        render_rows(clean, width, 0..u32::MAX / 2, &mut temp);
        for line in temp {
            out.push(Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), style))
                    .collect::<Vec<_>>(),
            ));
        }
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

impl OutputView for LatexOutputView {
    fn on_output_updated(&mut self, _item: &OutputItem, ctx: &MountCtx) {
        self.output_id = ctx.output_id;
        self.rev = self.rev.wrapping_add(1);
        self.scroller.key = None;
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev.wrapping_add(images::layout_epoch())
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
        keep: ClipEnd,
    ) -> OutputMetrics {
        let width = width.max(1);
        let epoch = images::layout_epoch();
        let key = (item.rev, width, theme.revision, epoch);
        if self.scroller.key != Some(key) {
            let OutputData::Rich { bundle, .. } = &item.data else {
                self.scroller.lines = vec![Line::default()];
                return self.scroller.finish_measure(cap_rows, keep);
            };
            let latex = mime_text(bundle, "text/latex").unwrap_or(Cow::Borrowed(""));
            let needle = output_needle(self.output_id);
            let lines = if images::enabled() {
                if let Some(formula) = math::unwrap_latex_mime(latex.as_ref()) {
                    math::display_block_lines_with_needle(
                        formula,
                        usize::from(width),
                        theme,
                        Some(&needle),
                    )
                } else {
                    Some(render_markdown_cell(latex.as_ref(), width, theme).lines)
                }
            } else {
                None
            }
            .unwrap_or_else(|| {
                let fallback_text = mime_text(bundle, "text/plain")
                    .filter(|plain| !plain.trim().is_empty() && !is_ipython_object_repr(plain))
                    .unwrap_or(latex);
                wrap_plain_text_lines(fallback_text.trim_end_matches('\n'), width, theme)
            });
            self.scroller.lines = lines;
            self.scroller.key = Some(key);
        }
        self.scroller.finish_measure(cap_rows, keep)
    }

    fn render_slice(
        &self,
        _item: &OutputItem,
        _width: u16,
        rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        self.scroller.render_slice(rows, out);
    }

    fn handle_input(
        &mut self,
        _item: &OutputItem,
        input: ResolvedInput,
        _host: &mut dyn OutputHost,
    ) -> InputOutcome {
        self.scroller.handle_input(&input)
    }
}

// ---------------------------------------------------------------------------
// 4. Markdown MIME Renderer (`text/markdown`)
// ---------------------------------------------------------------------------

pub struct MarkdownOutputRenderer;

impl OutputRenderer for MarkdownOutputRenderer {
    fn id(&self) -> &'static str {
        "core.markdown"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        MARKDOWN_MIMES
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        let OutputData::Rich { bundle, .. } = &item.data else {
            return None;
        };
        best_mime_priority(bundle, MARKDOWN_MIMES)
    }

    fn mount(&self, _item: &OutputItem, ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(MarkdownOutputView {
            output_id: ctx.output_id,
            rev: 1,
            scroller: LineSliceScroller::default(),
        })
    }
}

pub struct MarkdownOutputView {
    output_id: OutputId,
    rev: u64,
    scroller: LineSliceScroller,
}

impl OutputView for MarkdownOutputView {
    fn on_output_updated(&mut self, _item: &OutputItem, ctx: &MountCtx) {
        self.output_id = ctx.output_id;
        self.rev = self.rev.wrapping_add(1);
        self.scroller.key = None;
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev.wrapping_add(images::layout_epoch())
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
        keep: ClipEnd,
    ) -> OutputMetrics {
        let width = width.max(1);
        let epoch = images::layout_epoch();
        let key = (item.rev, width, theme.revision, epoch);
        if self.scroller.key != Some(key) {
            let md = match &item.data {
                OutputData::Rich { bundle, .. } => {
                    mime_text(bundle, "text/markdown").unwrap_or(Cow::Borrowed(""))
                }
                _ => Cow::Borrowed(""),
            };
            self.scroller.lines = render_markdown_cell(md.as_ref(), width, theme).lines;
            self.scroller.key = Some(key);
        }
        self.scroller.finish_measure(cap_rows, keep)
    }

    fn render_slice(
        &self,
        _item: &OutputItem,
        _width: u16,
        rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        self.scroller.render_slice(rows, out);
    }

    fn handle_input(
        &mut self,
        _item: &OutputItem,
        input: ResolvedInput,
        _host: &mut dyn OutputHost,
    ) -> InputOutcome {
        self.scroller.handle_input(&input)
    }
}

/// True when `item` has rich media whose rendered lines depend on `needle`.
pub fn rich_output_matches_needle(item: &OutputItem, needle: &str) -> bool {
    if let Some(id_str) = needle.strip_prefix("output:") {
        return id_str.parse::<u64>().ok() == Some(item.id.0);
    }
    let OutputData::Rich { bundle, .. } = &item.data else {
        return false;
    };
    for key in [
        "text/markdown",
        "text/latex",
        "text/vnd.mermaid",
        "application/vnd.mermaid",
    ] {
        if let Some(text) = mime_text(bundle, key)
            && !needle.is_empty()
            && text.contains(needle)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use nb_core::CellId;
    use serde_json::{Map, json};

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn rich_image_view_falls_back_to_text_when_off_and_slices_placeholders_when_ready() {
        let _ = images::configure(images::ImageMode::Off);
        let mut bundle = MimeBundle::new();
        bundle.insert("image/png".into(), json!("AAAA"));
        bundle.insert(
            "text/plain".into(),
            json!("<Figure size 640x480 with 1 Axes>"),
        );
        let item = OutputItem::new(
            OutputData::Rich {
                bundle,
                metadata: Map::new(),
                display_id: None,
                is_execute_result: false,
                execution_count: None,
            },
            false,
        );
        let ctx = MountCtx {
            cell_id: CellId("c1".into()),
            output_id: item.id,
        };
        let theme = Theme::default();
        let mut view = RichImageRenderer.mount(&item, &ctx);

        let metrics_off = view.measure(&item, 80, &theme, Some(18), ClipEnd::Top);
        assert_eq!(metrics_off.total_rows, 1);
        let mut off_lines = Vec::new();
        view.render_slice(&item, 80, 0..1, false, &theme, &mut off_lines);
        assert_eq!(line_text(&off_lines[0]), "<Figure size 640x480 with 1 Axes>");

        images::enable_for_test((12, 24));
        let mut hasher = DefaultHasher::new();
        "image/png".hash(&mut hasher);
        "AAAA".hash(&mut hasher);
        pictures::insert_mime_ready_for_test(
            hasher.finish(),
            false,
            NeedsBackground::None,
            80,
            &theme,
            vec![1, 2, 3],
            12,
            6,
        );
        let metrics_on = view.measure(&item, 80, &theme, Some(4), ClipEnd::Top);
        assert_eq!(metrics_on.total_rows, 6);
        assert_eq!(metrics_on.shown_rows, 4);
        assert!(metrics_on.clipped);
        let mut slice = Vec::new();
        view.render_slice(&item, 80, 1..3, false, &theme, &mut slice);
        assert_eq!(slice.len(), 2);
        assert!(slice[0].spans[0].content.starts_with(images::PLACEHOLDER));

        let _ = images::configure(images::ImageMode::Off);
    }

    #[test]
    fn latex_and_markdown_outputs_handle_fallbacks_and_math() {
        let _ = images::configure(images::ImageMode::Off);
        let mut bundle = MimeBundle::new();
        bundle.insert("text/latex".into(), json!("$\\displaystyle E = mc^2$"));
        bundle.insert(
            "text/plain".into(),
            json!("<IPython.core.display.Math object>"),
        );
        let item = OutputItem::new(
            OutputData::Rich {
                bundle,
                metadata: Map::new(),
                display_id: None,
                is_execute_result: false,
                execution_count: None,
            },
            false,
        );
        let ctx = MountCtx {
            cell_id: CellId("c1".into()),
            output_id: item.id,
        };
        let theme = Theme::default();
        let mut latex_view = LatexOutputRenderer.mount(&item, &ctx);
        let m = latex_view.measure(&item, 80, &theme, None, ClipEnd::Top);
        assert_eq!(m.total_rows, 1);
        let mut lines = Vec::new();
        latex_view.render_slice(&item, 80, 0..1, false, &theme, &mut lines);
        assert!(
            line_text(&lines[0]).contains("E = mc^2"),
            "suppresses useless <IPython.core.display.Math object> repr: {:?}",
            line_text(&lines[0])
        );
    }
}
