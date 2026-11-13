//! Markdown cell source renderer (headings, fenced code blocks, and width-shrinking box tables).

use ratatui::text::Line;
use wb_sdk::Theme;

pub struct RenderedMarkdown {
    pub lines: Vec<Line<'static>>,
    /// Maps each rendered visual line back to its originating logical source line for click-to-edit.
    pub source_line_map: Vec<usize>,
}

pub fn render_markdown_cell(_source: &str, _width: u16, _theme: &Theme) -> RenderedMarkdown {
    todo!("wb-tui::builtins::markdown: render headings, syntect fences, and narrow-tmux box tables")
}
