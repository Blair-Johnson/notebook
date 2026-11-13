//! Interactive cell output views (`OutputRenderer`, `OutputView`, `OutputHost`).
//!
//! `OutputItem` stays owned by `nb_core::Document`; `OutputView` stores only
//! view state (wrap offsets, folded tree paths, cursor row) so large streams
//! are never duplicated in memory.

use crate::command::ResolvedInput;
use crate::key::KeyContextId;
use crate::theme::Theme;
use ratatui::text::Line;
use serde_json::{Map, Value};
use std::ops::Range;
use nb_core::{CellId, CommId, OutputId, OutputItem};

#[derive(Clone, Debug)]
pub struct MountCtx {
    pub cell_id: CellId,
    pub output_id: OutputId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutputMetrics {
    pub total_rows: u32,
    pub shown_rows: u32,
    pub clipped: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputOutcome {
    Consumed,
    Ignored,
    PopFocus,
}

/// Controlled callback surface exposed to an active `OutputView`.
pub trait OutputHost {
    fn send_comm(&mut self, comm_id: CommId, data: Map<String, Value>);
    fn eval_inspect_subshell(&mut self, tag: &'static str, python_code: String);
    fn copy_text(&mut self, text: String);
    fn jump_to_cell(&mut self, cell_id: CellId, line: Option<usize>);
    fn post_notice(&mut self, message: String);
}

pub trait OutputRenderer: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    /// Priority-ordered `(mime_type, priority)` pairs claimed by this renderer.
    fn claimed_mimes(&self) -> &'static [(&'static str, u16)];
    fn can_render(&self, item: &OutputItem) -> Option<u16>;
    fn mount(&self, item: &OutputItem, ctx: &MountCtx) -> Box<dyn OutputView>;
}

pub trait OutputView: Send {
    fn on_output_updated(&mut self, item: &OutputItem, ctx: &MountCtx);
    fn on_comm_message(
        &mut self,
        _comm_id: &CommId,
        _data: &Map<String, Value>,
        _host: &mut dyn OutputHost,
    ) {
    }
    fn is_focusable(&self) -> bool {
        false
    }
    fn key_context(&self) -> Option<KeyContextId> {
        None
    }
    fn view_revision(&self) -> u64;
    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
    ) -> OutputMetrics;
    fn render_slice(
        &self,
        item: &OutputItem,
        width: u16,
        rows: Range<u32>,
        focused: bool,
        theme: &Theme,
        out: &mut Vec<Line<'static>>,
    );
    fn handle_input(
        &mut self,
        _item: &OutputItem,
        _input: ResolvedInput,
        _host: &mut dyn OutputHost,
    ) -> InputOutcome {
        InputOutcome::Ignored
    }
}
