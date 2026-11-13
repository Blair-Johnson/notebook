//! Built-in `text/plain` fallback renderer for `OutputData::Rich` MIME bundles.

use ratatui::text::Line;
use std::ops::Range;
use wb_core::{OutputData, OutputItem};
use wb_sdk::{KeyContextId, MountCtx, OutputMetrics, OutputRenderer, OutputView, Theme};

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
        Box::new(PlainTextView { rev: 1 })
    }
}

pub struct PlainTextView {
    rev: u64,
}

impl OutputView for PlainTextView {
    fn on_output_updated(&mut self, _item: &OutputItem, _ctx: &MountCtx) {
        self.rev = self.rev.wrapping_add(1);
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        self.rev
    }

    fn measure(
        &mut self,
        _item: &OutputItem,
        _width: u16,
        _theme: &Theme,
        _cap_rows: Option<u16>,
    ) -> OutputMetrics {
        todo!("wb-tui::builtins::plain: measure text/plain representation or list available MIME keys")
    }

    fn render_slice(
        &self,
        _item: &OutputItem,
        _width: u16,
        _rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        _out: &mut Vec<Line<'static>>,
    ) {
        todo!("wb-tui::builtins::plain: render visible wrapped slice of text/plain")
    }
}
