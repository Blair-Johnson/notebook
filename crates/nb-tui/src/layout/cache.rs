//! `CellId`-keyed layout cache with 64 MiB LRU eviction, idle resize warming, and per-line syntax cache.

use crate::builtins::markdown::render_markdown_cell;
use crate::layout::{CardRows, output_cap, output_sections};
use crate::layout::fenwick::FenwickTree;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::size_of;
use syntect::easy::HighlightLines;
use syntect::highlighting::{HighlightState, Highlighter, Theme as SyntectTheme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use unicode_width::UnicodeWidthChar;
use nb_core::{Cell, CellId, CellKind, CellRevisions, Document, OutputDisplay, OutputId};
use nb_sdk::{OutputView, Theme};

pub const RENDER_CACHE_BUDGET_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CellLayoutKey {
    pub revs: CellRevisions,
    pub output_views_rev: u64,
    pub width: u16,
    pub theme_rev: u64,
    /// Markdown shown as raw source (while it is being edited).
    pub raw_source: bool,
    pub default_view: OutputDisplay,
    pub preview_rows: u16,
}

#[derive(Debug)]
pub struct CachedCellLayout {
    pub key: CellLayoutKey,
    pub wrapped_source: Vec<Line<'static>>,
    pub total_rows: u32,
    pub last_seen_frame: u64,
    pub approx_bytes: usize,
    pub source_line_map: Vec<usize>,
    /// Some output is taller than the preview rows, so preview and full
    /// differ.
    pub clips: bool,
    /// Card rows the notebook cursor can rest on, ascending: source lines,
    /// output lines, a hidden output's summary row, a folded cell's row.
    /// Borders and section dividers are skipped.
    pub stops: Vec<u32>,
    /// First row of the output block, if the cell shows any outputs.
    pub outputs_top: Option<u32>,
    /// Where each shown output's rows are, in drawing order.
    pub output_rows: Vec<OutputRows>,
}

/// One output's rows within its card.
#[derive(Clone, Copy, Debug)]
pub struct OutputRows {
    pub output_id: OutputId,
    pub first_row: u32,
    pub shown: u32,
    pub total: u32,
}

impl CachedCellLayout {
    /// The output (and row within it) drawn at card row `row`.
    pub fn output_at(&self, row: u32) -> Option<(OutputRows, u32)> {
        self.output_rows
            .iter()
            .find(|rows| row >= rows.first_row && row < rows.first_row + rows.shown)
            .map(|rows| (*rows, row - rows.first_row))
    }

    pub fn first_stop(&self) -> u32 {
        self.stops.first().copied().unwrap_or(CardRows::SOURCE_TOP)
    }
}

pub struct LayoutCache {
    cells: HashMap<CellId, CachedCellLayout>,
    order: Vec<CellId>,
    heights: FenwickTree,
    idle_warm_queue: VecDeque<CellId>,
    frame_counter: u64,
    last_width: u16,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    /// Highlighted code lines per cell, keyed by the line's text and the
    /// parser state it starts in: an edit re-highlights from the changed
    /// line until the state matches the last render again. Each cell keeps
    /// just the lines of its latest render, so memory follows the notebook
    /// size.
    line_highlights: HashMap<CellId, LineHighlights>,
    /// `display.output_default_view`, set by the caller before syncing.
    pub default_output_view: OutputDisplay,
}

struct LineHighlights {
    theme_rev: u64,
    lines: HashMap<String, Vec<HighlightedLine>>,
}

/// Highlighter and parser state between two lines: a line's colors depend
/// on everything before it (`print(` makes the next lines' `)` a match).
type LineState = (HighlightState, ParseState);

struct HighlightedLine {
    before: LineState,
    line: Line<'static>,
    after: LineState,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            cells: HashMap::new(),
            order: Vec::new(),
            heights: FenwickTree::default(),
            idle_warm_queue: VecDeque::new(),
            frame_counter: 0,
            last_width: 0,
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            line_highlights: HashMap::new(),
            default_output_view: OutputDisplay::Preview,
        }
    }
}

impl LayoutCache {
    pub fn heights(&self) -> &FenwickTree {
        &self.heights
    }

    pub fn last_width(&self) -> u16 {
        self.last_width
    }

    pub fn cached(&self, cell_id: &CellId) -> Option<&CachedCellLayout> {
        self.cells.get(cell_id)
    }

    pub fn cached_len(&self) -> usize {
        self.cells.len()
    }

    pub fn approx_bytes(&self) -> usize {
        self.cells.values().map(|cell| cell.approx_bytes).sum()
    }

    pub fn idle_queue_len(&self) -> usize {
        self.idle_warm_queue.len()
    }

    fn output_revision(cell: &Cell, output_views: &HashMap<OutputId, Box<dyn OutputView>>) -> u64 {
        cell.outputs.iter().fold(0u64, |revision, output| {
            let view_revision = output_views
                .get(&output.id)
                .map_or(0, |view| view.view_revision());
            revision
                .wrapping_mul(1_000_003)
                .wrapping_add(output.rev)
                .wrapping_add(view_revision)
        })
    }

    fn estimate_height(cell: &Cell, max_output_rows: u16, default: OutputDisplay) -> u32 {
        if cell.collapsed {
            return CardRows { source: 1, outputs: 0 }.total();
        }
        let outputs = match (cell.outputs.len(), cell.output_display(default)) {
            (0, _) => 0,
            (_, OutputDisplay::Hidden) => 1,
            _ => output_sections(cell)
                .iter()
                .map(|section| 1 + section.items.len() as u32 * u32::from(max_output_rows.max(1)))
                .sum(),
        };
        CardRows {
            source: cell.buffer.line_count() as u32,
            outputs,
        }
        .total()
    }

    fn ensure_tree(&mut self, doc: &Document, max_output_rows: u16) {
        if self.heights.len() == doc.order().len() && self.order == doc.order() {
            return;
        }
        self.order = doc.order().to_vec();
        self.line_highlights.retain(|cell_id, _| doc.cell(cell_id).is_some());
        self.heights = FenwickTree::with_len(doc.order().len());
        let default = self.default_output_view;
        for (index, cell_id) in doc.order().iter().enumerate() {
            let height = self.cells.get(cell_id).map_or_else(
                || {
                    doc.cell(cell_id)
                        .map_or(1, |cell| Self::estimate_height(cell, max_output_rows, default))
                },
                |cell| cell.total_rows,
            );
            self.heights.set(index, height.max(1));
        }
    }

    fn visible_indices(
        &self,
        doc: &Document,
        scroll_row: u32,
        viewport_height: u16,
    ) -> HashSet<usize> {
        let mut visible = HashSet::new();
        let Some((mut index, _)) = self.heights.find_row(scroll_row) else {
            return visible;
        };
        let end_row = scroll_row.saturating_add(u32::from(viewport_height.max(1)));
        while index < doc.order().len() {
            let start = self.heights.prefix_sum(index);
            if start >= end_row && !visible.is_empty() {
                break;
            }
            visible.insert(index);
            index += 1;
        }
        visible
    }

    fn measure_cell(
        &mut self,
        doc: &Document,
        cell_id: &CellId,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        width: u16,
        max_output_rows: u16,
        theme: &Theme,
        editing: Option<&CellId>,
    ) -> Option<u32> {
        let cell = doc.cell(cell_id)?;
        let output_views_rev = Self::output_revision(cell, output_views);
        let raw_source = cell.kind == CellKind::Markdown && editing == Some(cell_id);
        let key = CellLayoutKey {
            revs: cell.revs,
            output_views_rev,
            width,
            theme_rev: theme.revision,
            raw_source,
            default_view: self.default_output_view,
            preview_rows: max_output_rows,
        };
        if let Some(cached) = self.cells.get_mut(cell_id) {
            if cached.key == key {
                cached.last_seen_frame = self.frame_counter;
                return Some(cached.total_rows);
            }
        }

        let (wrapped_source, source_line_map) = self.render_source(cell, width, theme, raw_source);
        let mut clips = false;
        let mut stops: Vec<u32> = Vec::new();
        let mut outputs_top = None;
        let mut output_rows = Vec::new();
        let rows = if cell.collapsed {
            stops.push(CardRows::SOURCE_TOP);
            CardRows { source: 1, outputs: 0 }
        } else {
            let source_rows = wrapped_source.len() as u32;
            stops.extend(CardRows::SOURCE_TOP..CardRows::SOURCE_TOP + source_rows);
            let block_top = CardRows::SOURCE_TOP + source_rows;
            let sections = output_sections(cell);
            // Preview and full only differ when some output is taller than
            // the preview rows.
            let preview = Some(max_output_rows.max(1));
            clips = sections
                .iter()
                .flat_map(|section| section.items.iter())
                .any(|output| {
                    output_views.get_mut(&output.id).is_some_and(|view| {
                        let metrics = view.measure(output, width, theme, preview);
                        metrics.total_rows > metrics.shown_rows
                    })
                });
            let display = cell
                .output_display(self.default_output_view)
                .effective(clips);
            let cap = output_cap(display, max_output_rows);
            let outputs = match (cell.outputs.len(), display) {
                (0, _) => 0,
                (_, OutputDisplay::Hidden) => {
                    outputs_top = Some(block_top);
                    stops.push(block_top);
                    1
                }
                // One divider per section, then its outputs' rows.
                _ => {
                    let mut row = block_top;
                    for section in &sections {
                        row += 1;
                        for output in &section.items {
                            let (shown, total) = output_views.get_mut(&output.id).map_or((1, 1), |view| {
                                let metrics = view.measure(output, width, theme, cap);
                                (metrics.shown_rows.max(1), metrics.total_rows)
                            });
                            output_rows.push(OutputRows {
                                output_id: output.id,
                                first_row: row,
                                shown,
                                total,
                            });
                            stops.extend(row..row + shown);
                            row += shown;
                        }
                    }
                    outputs_top = (row > block_top).then_some(block_top);
                    row - block_top
                }
            };
            CardRows {
                source: wrapped_source.len() as u32,
                outputs,
            }
        };
        let total_rows = rows.total();
        let approx_bytes = size_of::<CachedCellLayout>()
            + wrapped_source
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.len() + size_of::<Span<'static>>())
                .sum::<usize>();
        self.cells.insert(
            cell_id.clone(),
            CachedCellLayout {
                key,
                wrapped_source,
                total_rows: total_rows.max(1),
                last_seen_frame: self.frame_counter,
                approx_bytes,
                source_line_map,
                clips,
                stops,
                outputs_top,
                output_rows,
            },
        );
        Some(total_rows.max(1))
    }

    fn render_source(
        &mut self,
        cell: &Cell,
        width: u16,
        theme: &Theme,
        raw_source: bool,
    ) -> (Vec<Line<'static>>, Vec<usize>) {
        if cell.kind == CellKind::Markdown && !raw_source {
            let rendered = render_markdown_cell(cell.buffer.as_str(), width, theme);
            return (rendered.lines, rendered.source_line_map);
        }
        let mut lines = Vec::new();
        let mut source_map = Vec::new();
        // `split('\n')`, not `lines()`: a trailing empty line is a real row the
        // cursor can sit on.
        let source_lines: Vec<&str> = cell.buffer.as_str().split('\n').collect();
        let mut code_lines = Vec::new().into_iter();
        if cell.kind == CellKind::Code {
            let previous = match self.line_highlights.remove(&cell.id) {
                Some(cached) if cached.theme_rev == theme.revision => cached.lines,
                _ => HashMap::new(),
            };
            let (rendered, kept) = self.highlight_code(&source_lines, theme, previous);
            self.line_highlights.insert(
                cell.id.clone(),
                LineHighlights {
                    theme_rev: theme.revision,
                    lines: kept,
                },
            );
            code_lines = rendered.into_iter();
        }
        for (index, source_line) in source_lines.iter().enumerate() {
            let rendered = match cell.kind {
                CellKind::Code => code_lines.next().unwrap_or_default(),
                CellKind::Markdown => raw_markdown_line(source_line, theme),
                CellKind::Raw => Line::from(Span::styled(
                    (*source_line).to_string(),
                    Style::default().fg(theme.palette.fg),
                )),
            };
            for line in wrap_line(rendered, width.max(1)) {
                lines.push(line);
                source_map.push(index);
            }
        }
        if lines.is_empty() {
            lines.push(Line::default());
            source_map.push(0);
        }
        (lines, source_map)
    }

    /// Highlight one line on its own (no state carried between lines).
    fn highlight_code(
        &self,
        source_lines: &[&str],
        theme: &Theme,
        mut previous: HashMap<String, Vec<HighlightedLine>>,
    ) -> (Vec<Line<'static>>, HashMap<String, Vec<HighlightedLine>>) {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension("py")
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let Some(syntax_theme) = self.syntax_theme(theme) else {
            let lines = source_lines.iter().map(|line| Line::from(line.to_string())).collect();
            return (lines, HashMap::new());
        };
        let mut state: LineState = (
            HighlightState::new(&Highlighter::new(syntax_theme), ScopeStack::new()),
            ParseState::new(syntax),
        );
        let mut kept: HashMap<String, Vec<HighlightedLine>> = HashMap::new();
        let mut lines = Vec::with_capacity(source_lines.len());
        for source in source_lines {
            // The same text in the same state renders the same: from this
            // render (repeated lines) or the last one (unchanged lines).
            let reused = kept
                .get(*source)
                .and_then(|entries| entries.iter().find(|entry| entry.before == state))
                .map(|entry| (entry.line.clone(), entry.after.clone()));
            let (line, after) = match reused {
                Some(found) => found,
                None => {
                    let entry = previous
                        .get_mut(*source)
                        .and_then(|entries| {
                            let at = entries.iter().position(|entry| entry.before == state)?;
                            Some(entries.swap_remove(at))
                        })
                        .unwrap_or_else(|| self.highlight_code_line(source, syntax_theme, state.clone()));
                    let found = (entry.line.clone(), entry.after.clone());
                    kept.entry((*source).to_string()).or_default().push(entry);
                    found
                }
            };
            lines.push(line);
            state = after;
        }
        (lines, kept)
    }

    /// Highlight one line, starting in `before` (the state after the
    /// previous line).
    fn highlight_code_line(
        &self,
        source: &str,
        syntax_theme: &SyntectTheme,
        before: LineState,
    ) -> HighlightedLine {
        let mut highlighter =
            HighlightLines::from_state(syntax_theme, before.0.clone(), before.1.clone());
        let source_with_newline = format!("{source}\n");
        let regions = highlighter
            .highlight_line(&source_with_newline, &self.syntax_set)
            .unwrap_or_default();
        let line = Line::from(crate::highlight::spans(regions, syntax_theme));
        HighlightedLine {
            before,
            line,
            after: highlighter.state(),
        }
    }

    fn syntax_theme(&self, theme: &Theme) -> Option<&SyntectTheme> {
        self.theme_set
            .themes
            .get(&theme.syntect_theme)
            .or_else(|| self.theme_set.themes.get("base16-ocean.dark"))
            .or_else(|| self.theme_set.themes.values().next())
    }

    /// Measure the cell at `index` now (at the last viewport width), for
    /// navigation that reaches past what has been drawn.
    pub fn ensure_measured(
        &mut self,
        doc: &Document,
        index: usize,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        max_output_rows: u16,
        theme: &Theme,
    ) -> Option<&CachedCellLayout> {
        self.ensure_tree(doc, max_output_rows);
        let cell_id = doc.order().get(index)?.clone();
        let width = if self.last_width == 0 { 80 } else { self.last_width };
        let height = self.measure_cell(doc, &cell_id, output_views, width, max_output_rows, theme, None)?;
        self.heights.set(index, height);
        self.cells.get(&cell_id)
    }

    /// Synchronize visible cells immediately for `width` and enqueue off-screen cells
    /// into `idle_warm_queue` on terminal resize so frame times stay well below 16 ms.
    pub fn sync_viewport(
        &mut self,
        doc: &Document,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        width: u16,
        scroll_row: u32,
        viewport_height: u16,
        max_output_rows: u16,
        theme: &Theme,
        editing: Option<&CellId>,
    ) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
        self.ensure_tree(doc, max_output_rows);
        let width = width.max(1);
        let resized = self.last_width != 0 && self.last_width != width;
        let visible = self.visible_indices(doc, scroll_row, viewport_height);
        for index in &visible {
            if let Some(cell_id) = doc.order().get(*index) {
                if let Some(height) = self.measure_cell(
                    doc,
                    cell_id,
                    output_views,
                    width,
                    max_output_rows,
                    theme,
                    editing,
                ) {
                    self.heights.set(*index, height);
                }
            }
        }
        if self.last_width == 0 || resized {
            self.idle_warm_queue.clear();
            for (index, cell_id) in doc.order().iter().enumerate() {
                if !visible.contains(&index) && !self.idle_warm_queue.contains(cell_id) {
                    self.idle_warm_queue.push_back(cell_id.clone());
                }
            }
        }
        self.last_width = width;
        let visible_ids = visible
            .iter()
            .filter_map(|index| doc.order().get(*index).cloned())
            .collect::<HashSet<_>>();
        self.evict(&visible_ids);
    }

    fn evict(&mut self, visible: &HashSet<CellId>) {
        while self.approx_bytes() > RENDER_CACHE_BUDGET_BYTES {
            let Some(cell_id) = self
                .cells
                .iter()
                .filter(|(cell_id, _)| !visible.contains(*cell_id))
                .min_by_key(|(_, cell)| cell.last_seen_frame)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.cells.remove(&cell_id);
            self.line_highlights.remove(&cell_id);
        }
    }

    /// Measure up to `max_cells` off-screen cells during idle ticks after a terminal resize.
    pub fn warm_idle_chunk(
        &mut self,
        doc: &Document,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        max_cells: usize,
        max_output_rows: u16,
        theme: &Theme,
    ) -> bool {
        let width = self.last_width.max(1);
        let mut measured = 0usize;
        while measured < max_cells {
            let Some(cell_id) = self.idle_warm_queue.pop_front() else {
                break;
            };
            if let Some(index) = doc.position_of(&cell_id) {
                if let Some(height) = self.measure_cell(
                    doc,
                    &cell_id,
                    output_views,
                    width,
                    max_output_rows,
                    theme,
                    None,
                ) {
                    self.heights.set(index, height);
                }
            }
            measured += 1;
        }
        self.evict(&HashSet::new());
        !self.idle_warm_queue.is_empty()
    }
}

/// Markdown source shown verbatim while editing, with headings and fences tinted.
fn raw_markdown_line(source: &str, theme: &Theme) -> Line<'static> {
    let trimmed = source.trim_start();
    let style = if trimmed.starts_with('#') {
        Style::default()
            .fg(theme.palette.accent_markdown)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else if trimmed.starts_with("```") {
        Style::default().fg(theme.palette.accent_code)
    } else {
        Style::default().fg(theme.palette.fg)
    };
    Line::from(Span::styled(source.to_string(), style))
}

/// Character-wrap a styled line to `width` columns. Exposed so the renderer
/// can map a cursor column onto the same rows.
pub fn wrap_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw > 0 && used > 0 && used + cw > width {
                output.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            if let Some(last) = current.last_mut() {
                if last.style == span.style {
                    last.content.to_mut().push(ch);
                } else {
                    current.push(Span::styled(ch.to_string(), span.style));
                }
            } else {
                current.push(Span::styled(ch.to_string(), span.style));
            }
            used = used.saturating_add(cw);
        }
    }
    if current.is_empty() && output.is_empty() {
        output.push(Line::default());
    } else {
        output.push(Line::from(current));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(cache: &mut LayoutCache, cell: &Cell) -> Vec<Line<'static>> {
        cache.render_source(cell, 80, &Theme::default(), false).0
    }

    fn code(source: &str) -> Cell {
        let mut cell = Cell::new_code();
        cell.buffer.replace_all(source.to_string());
        cell
    }

    /// The style of the first `text` span on line `row`.
    fn style_of(lines: &[Line<'static>], row: usize, text: &str) -> Style {
        lines[row]
            .spans
            .iter()
            .find(|span| span.content.contains(text))
            .unwrap_or_else(|| panic!("{text:?} not on line {row}: {:?}", lines[row]))
            .style
    }

    #[test]
    fn a_bracket_closed_on_a_later_line_is_not_invalid() {
        let mut cache = LayoutCache::default();
        let lines = render(&mut cache, &code("print(\n\n)"));
        let close = style_of(&lines, 2, ")");
        let open = style_of(&lines, 0, "(");
        assert_eq!(close.fg, open.fg, "the closing bracket looks like the opening one");
        assert_eq!(close.bg, None);
    }

    #[test]
    fn an_unmatched_bracket_stays_visible() {
        let mut cache = LayoutCache::default();
        let lines = render(&mut cache, &code("x = 1)"));
        // Themes draw invalid code in the background color on a red
        // background: the background must survive, or it disappears.
        assert!(style_of(&lines, 0, ")").bg.is_some());
    }

    #[test]
    fn edits_re_highlight_the_lines_they_affect() {
        let mut cache = LayoutCache::default();
        let mut cell = code("a = [\n    1,\n]\nb = 2");
        render(&mut cache, &cell);
        // Closing the bracket on the first line makes the third line's `]`
        // unmatched, though that line's text did not change.
        cell.buffer.replace_all("a = []\n    1,\n]\nb = 2".to_string());
        let incremental = render(&mut cache, &cell);
        let fresh = render(&mut LayoutCache::default(), &cell);
        assert_eq!(incremental, fresh);
        assert!(style_of(&incremental, 2, "]").bg.is_some());
    }
}
