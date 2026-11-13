//! `CellId`-keyed layout cache with 64 MiB LRU eviction, idle resize warming, and per-line syntax cache.

use crate::builtins::markdown::render_markdown_cell;
use crate::layout::{CardRows, output_cap, output_sections};
use crate::layout::fenwick::FenwickTree;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::size_of;
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};
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
    /// Highlighted code lines per cell, keyed by the line's text: an edit
    /// re-highlights only the lines it changed. Each cell keeps just the
    /// lines of its latest render, so memory follows the notebook size.
    line_highlights: HashMap<CellId, LineHighlights>,
    /// `display.output_default_view`, set by the caller before syncing.
    pub default_output_view: OutputDisplay,
}

struct LineHighlights {
    theme_rev: u64,
    lines: HashMap<String, Line<'static>>,
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
            let mut at_preview = HashMap::new();
            for output in sections.iter().flat_map(|section| section.items.iter()) {
                if let Some(view) = output_views.get_mut(&output.id) {
                    let metrics = view.measure(output, width, theme, preview);
                    clips |= metrics.total_rows > metrics.shown_rows;
                    at_preview.insert(output.id, (metrics.shown_rows.max(1), metrics.total_rows));
                }
            }
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
                            // Views keep the cap of their last measure, so
                            // measure again only for a different cap.
                            let (shown, total) = match output_views.get_mut(&output.id) {
                                Some(_) if cap == preview => at_preview[&output.id],
                                Some(view) => {
                                    let metrics = view.measure(output, width, theme, cap);
                                    (metrics.shown_rows.max(1), metrics.total_rows)
                                }
                                None => (1, 1),
                            };
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
        let mut previous = match self.line_highlights.remove(&cell.id) {
            Some(cached) if cached.theme_rev == theme.revision => cached.lines,
            _ => HashMap::new(),
        };
        let mut highlighted = HashMap::new();
        for (index, source_line) in source_lines.iter().enumerate() {
            let rendered = match cell.kind {
                CellKind::Code => {
                    let line = highlighted
                        .get(*source_line)
                        .cloned()
                        .or_else(|| previous.remove(*source_line))
                        .unwrap_or_else(|| self.highlight_code_line(source_line, theme));
                    highlighted.insert((*source_line).to_string(), line.clone());
                    line
                }
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
        if !highlighted.is_empty() {
            self.line_highlights.insert(
                cell.id.clone(),
                LineHighlights {
                    theme_rev: theme.revision,
                    lines: highlighted,
                },
            );
        }
        if lines.is_empty() {
            lines.push(Line::default());
            source_map.push(0);
        }
        (lines, source_map)
    }

    /// Highlight one line on its own (no state carried between lines).
    fn highlight_code_line(&self, source: &str, theme: &Theme) -> Line<'static> {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension("py")
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let syntax_theme = self
            .theme_set
            .themes
            .get(&theme.syntect_theme)
            .or_else(|| self.theme_set.themes.get("base16-ocean.dark"))
            .or_else(|| self.theme_set.themes.values().next());
        if let Some(syntax_theme) = syntax_theme {
            let mut highlighter = HighlightLines::new(syntax, syntax_theme);
            let source_with_newline = format!("{source}\n");
            let regions = highlighter
                .highlight_line(&source_with_newline, &self.syntax_set)
                .unwrap_or_default();
            Line::from(
                regions
                    .into_iter()
                    .map(|(style, text)| {
                        Span::styled(
                            text.trim_end_matches(['\r', '\n']).to_string(),
                            Style::default().fg(Color::Rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            )),
                        )
                    })
                    .filter(|span| !span.content.is_empty())
                    .collect::<Vec<_>>(),
            )
        } else {
            Line::from(source.to_string())
        }
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
