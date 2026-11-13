//! Top bar, cell gutters/headers, status-line priority cascade, and dynamic keymap hints.

use crate::focus::FocusStack;
use crate::keymap::Keymap;
use ratatui::text::Line;
use wb_sdk::{StatusCtx, StatusSegment, Theme};

#[derive(Clone, Debug, Default)]
pub struct StatusCascade<'a> {
    pub fatal_error: Option<&'a str>,
    pub confirm_prompt: Option<&'a str>,
    pub copy_armed: bool,
    pub running_cells: usize,
    pub transient_notice: Option<&'a str>,
}

pub fn build_top_bar(
    _ctx: &StatusCtx<'_>,
    _segments: &[Box<dyn StatusSegment>],
    _width: u16,
    _theme: &Theme,
) -> Line<'static> {
    todo!("wb-tui::render::chrome: compose file/dirty, kernel status, focus badge, and plugin segments")
}

pub fn build_status_bar(
    _cascade: &StatusCascade<'_>,
    _focus: &FocusStack,
    _keymap: &Keymap,
    _width: u16,
    _theme: &Theme,
) -> Line<'static> {
    todo!("wb-tui::render::chrome: render highest-priority status state or dynamic key hints")
}
