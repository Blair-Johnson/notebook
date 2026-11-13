//! Built-in Python/JAX traceback output view supporting frame folding and cell-line jumps.

use ratatui::text::Line;
use std::ops::Range;
use wb_core::{OutputData, OutputItem};
use wb_sdk::{KeyContextId, MountCtx, OutputMetrics, OutputRenderer, OutputView, Theme};

pub struct TracebackRenderer;

impl OutputRenderer for TracebackRenderer {
    fn id(&self) -> &'static str {
        "core.traceback"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        &[]
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        matches!(item.data, OutputData::Error { .. }).then_some(100)
    }

    fn mount(&self, _item: &OutputItem, _ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(TracebackView::default())
    }
}

#[derive(Default)]
pub struct TracebackView {
    fold_internal_frames: bool,
    selected_frame: usize,
    rev: u64,
}

impl OutputView for TracebackView {
    fn on_output_updated(&mut self, _item: &OutputItem, _ctx: &MountCtx) {
        self.rev = self.rev.wrapping_add(1);
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.traceback"))
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
        let _ = (&self.fold_internal_frames, &self.selected_frame);
        todo!("wb-tui::builtins::traceback: measure ANSI-styled traceback with optional frame folding")
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
        todo!("wb-tui::builtins::traceback: render ANSI traceback slice fixing unreadable yellow bg spans")
    }
}
