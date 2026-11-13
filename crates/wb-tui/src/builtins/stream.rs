//! Built-in ANSI `stdout`/`stderr` stream output renderer with incremental row-slice wrapping.

use ratatui::text::Line;
use std::ops::Range;
use wb_core::{OutputData, OutputItem};
use wb_sdk::{KeyContextId, MountCtx, OutputMetrics, OutputRenderer, OutputView, Theme};

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

#[derive(Default)]
pub struct StreamOutputView {
    wrapped_for_width: u16,
    line_row_starts: Vec<u32>,
    scroll_offset: Option<u32>, // None = follow tail
    rev: u64,
}

impl OutputView for StreamOutputView {
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
        let _ = (&self.wrapped_for_width, &self.line_row_starts, &self.scroll_offset);
        todo!("wb-tui::builtins::stream: incrementally compute row starts for new lines")
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
        todo!("wb-tui::builtins::stream: binary-search line_row_starts, parse ANSI SGR, emit slice")
    }
}
