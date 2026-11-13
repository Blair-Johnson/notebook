//! Pluggable status-bar segments for top and bottom chrome.

use crate::theme::Theme;
use ratatui::text::Span;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarSlot {
    TopLeft,
    TopRight,
    BottomRight,
}

pub struct StatusCtx<'a> {
    pub path: Option<&'a Path>,
    pub dirty: bool,
    pub kernel_name: &'a str,
    pub kernel_busy: bool,
    pub kernel_alive: bool,
    pub focus_badge: &'a str,
    pub cell_position: (usize, usize),
}

pub trait StatusSegment: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn slot(&self) -> BarSlot;
    fn priority(&self) -> u16 {
        100
    }
    fn render(&self, ctx: &StatusCtx<'_>, theme: &Theme) -> Option<Span<'static>>;
}
