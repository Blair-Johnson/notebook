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
    /// The first row shown when clipped (0 when the top is shown).
    pub first_shown: u32,
}

/// Which end of an output taller than its preview stays visible
/// (`display.output_preview_end`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ClipEnd {
    /// The first rows.
    #[default]
    Top,
    /// The last rows, following the output as it grows.
    Bottom,
}

impl ClipEnd {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }

    /// The first row shown of `total` rows when `shown` fit.
    pub fn first_row(self, total: u32, shown: u32) -> u32 {
        match self {
            Self::Top => 0,
            Self::Bottom => total.saturating_sub(shown),
        }
    }
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
    /// Measure at `width`, showing at most `cap_rows` (all when `None`) and,
    /// when clipped, the `keep` end.
    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        theme: &Theme,
        cap_rows: Option<u16>,
        keep: ClipEnd,
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
