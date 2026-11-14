//! Shared visual vocabulary: per-mode accent colors and panel styling.

use crate::focus::{CellMode, FocusLayer};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};
use nb_sdk::Theme;

/// Accent color for the active focus level. The selected cell's gutter bar
/// and the top-bar badge use it, so the level is always visible.
pub fn mode_color(focus: &FocusLayer, theme: &Theme) -> ratatui::style::Color {
    let p = &theme.palette;
    match focus {
        FocusLayer::Notebook { .. } => p.accent_code,
        FocusLayer::CellEdit {
            mode: CellMode::Normal,
            ..
        } => p.accent_markdown,
        FocusLayer::CellEdit {
            mode: CellMode::Insert,
            ..
        } => p.success,
        FocusLayer::CellEdit {
            mode: CellMode::Visual(_),
            ..
        } => p.warning,
        FocusLayer::OutputFocus { .. } => p.accent_raw,
        FocusLayer::PdbInput => p.error,
        FocusLayer::Modal(_) => p.warning,
    }
}

/// Clear `area` and draw a rounded panel with `title`; returns the inner area.
pub fn panel(frame: &mut Frame<'_>, area: Rect, title: &str, theme: &Theme) -> Rect {
    let p = &theme.palette;
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.border_focused))
        .style(Style::default().bg(p.panel_bg).fg(p.fg))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(p.border_focused)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// A rectangle `width`x`height` centered in `area` (clamped to it).
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Style for a key hint (`space`), shown before its description.
pub fn key_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.palette.accent_code)
        .add_modifier(Modifier::BOLD)
}
