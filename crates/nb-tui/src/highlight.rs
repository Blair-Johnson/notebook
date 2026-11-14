//! Syntect regions to ratatui spans, for cell sources and markdown code.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme as SyntectTheme};

/// Spans for one highlighted line. A region's background is kept only when
/// it differs from the theme's own (the cell already has a background):
/// themes mark invalid code, such as an unmatched bracket, with a red
/// background and the background color as foreground, which without the
/// background would draw nothing at all.
pub fn spans(regions: Vec<(SyntectStyle, &str)>, theme: &SyntectTheme) -> Vec<Span<'static>> {
    let base = theme.settings.background;
    regions
        .into_iter()
        .map(|(style, text)| {
            let rgb = |c: syntect::highlighting::Color| Color::Rgb(c.r, c.g, c.b);
            let mut out = Style::default().fg(rgb(style.foreground));
            if base.is_some_and(|base| base != style.background) {
                out = out.bg(rgb(style.background));
            }
            if style.font_style.contains(FontStyle::BOLD) {
                out = out.add_modifier(Modifier::BOLD);
            }
            Span::styled(text.trim_end_matches(['\r', '\n']).to_string(), out)
        })
        .filter(|span| !span.content.is_empty())
        .collect()
}
