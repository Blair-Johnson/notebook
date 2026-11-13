//! Notebook cells drawn as cards: a titled top border, the source (code on
//! its own panel), an `Out` divider with the outputs, and a bottom border.
//!
//! ```text
//! ╭─ In [3] ───────────────────────────── ✓ 81 ms ─╮
//! │ y = mlp_step(w, x)                              │   code panel
//! ├─ Out [3] ───────────────────────────────────────┤
//! │ {'shape': [512, 512], 'dtype': 'float32'}       │   output
//! ╰─────────────────────────────────────────────────╯
//! ```
//!
//! Type shows in the title color and the source background; the selected
//! item (source or outputs) has its borders in the focus mode's color.

use super::geometry::{FrameGeometry, HitTarget, RowHit, ScrollbarGeometry};
use super::style::mode_color;
use super::{AppView, RunState};
use crate::focus::{CellMode, CellPart, FocusLayer};
use crate::layout::{CachedCellLayout, CardRows, GUTTER_COLS, LayoutCache};
use crate::vim::{OutputText, segment_of, wrap_segments};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::HashMap;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;
use wb_core::{Cell, CellKind, OutputData, OutputId, StreamKind, TextBuffer, VisualKind};
use wb_sdk::{OutputView, Theme};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draw every visible cell; returns the screen position for the terminal
/// cursor when a cell or output is being edited.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_cells(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
    theme: &Theme,
    cache: &LayoutCache,
    area: Rect,
    text_w: u16,
    geometry: &mut FrameGeometry,
) -> Option<(u16, u16)> {
    geometry.text_x = area.x + GUTTER_COLS;
    let mut cursor = None;
    let Some((mut index, _)) = cache.heights().find_row(view.scroll_row) else {
        return None;
    };
    let viewport_end = view.scroll_row.saturating_add(u32::from(area.height));
    while let Some(cell_id) = view.document.order().get(index) {
        let cell_start = cache.heights().prefix_sum(index);
        if cell_start >= viewport_end {
            break;
        }
        if let (Some(cell), Some(cached)) = (view.document.cell(cell_id), cache.cached(cell_id)) {
            let ctx = CellCtx {
                view,
                cell,
                cached,
                theme,
                area,
                text_w,
                cell_start,
            };
            if let Some(position) = ctx.draw(frame, output_views, geometry) {
                cursor = Some(position);
            }
        }
        index += 1;
    }
    cursor
}

struct CellCtx<'a> {
    view: &'a AppView<'a>,
    cell: &'a Cell,
    cached: &'a CachedCellLayout,
    theme: &'a Theme,
    area: Rect,
    text_w: u16,
    cell_start: u32,
}

/// Per-cell drawing state worked out once.
struct Look {
    editing: Option<CellMode>,
    output_mode: Option<CellMode>,
    source_selected: bool,
    outputs_selected: bool,
    source_border: Color,
    output_border: Color,
    accent: Color,
}

impl CellCtx<'_> {
    fn screen_y(&self, local_row: u32) -> Option<u16> {
        let global = self.cell_start.checked_add(local_row)?;
        if global < self.view.scroll_row {
            return None;
        }
        let offset = global - self.view.scroll_row;
        (offset < u32::from(self.area.height)).then(|| self.area.y + offset as u16)
    }

    fn text_rect(&self, y: u16) -> Rect {
        Rect::new(self.area.x + GUTTER_COLS, y, self.text_w, 1)
    }

    /// Everything between the two side borders (text plus its padding).
    fn inner_rect(&self, y: u16) -> Rect {
        Rect::new(self.area.x + 1, y, self.text_w + 2, 1)
    }

    fn right_x(&self) -> u16 {
        self.area.x + GUTTER_COLS + self.text_w + 1
    }

    fn look(&self) -> Look {
        let view = self.view;
        let p = &self.theme.palette;
        let base = view.focus.base();
        let editing = match base {
            FocusLayer::CellEdit { cell_id, mode, .. } if cell_id == &self.cell.id => Some(*mode),
            _ => None,
        };
        let output_mode = match base {
            FocusLayer::OutputFocus { cell_id, mode } if cell_id == &self.cell.id => Some(*mode),
            _ => None,
        };
        let selected = view.selected_cell == &self.cell.id;
        let outputs_selected =
            selected && (output_mode.is_some() || view.selected_part == CellPart::Output);
        let source_selected = selected && !outputs_selected;
        let accent = mode_color(base, self.theme);
        let running = matches!(
            view.run_states.get(&self.cell.id),
            Some(RunState::Running | RunState::Queued)
        );
        let idle = if running { p.warning } else { p.border };
        Look {
            editing,
            output_mode,
            source_selected,
            outputs_selected,
            source_border: if source_selected { accent } else { idle },
            output_border: if outputs_selected { accent } else { idle },
            accent,
        }
    }

    fn draw(
        &self,
        frame: &mut Frame<'_>,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        geometry: &mut FrameGeometry,
    ) -> Option<(u16, u16)> {
        let look = self.look();
        let source_rows = if self.cell.collapsed {
            1
        } else {
            self.cached.wrapped_source.len() as u32
        };
        let rows = CardRows::from_total(self.cached.total_rows, source_rows);

        if let Some(y) = self.screen_y(0) {
            self.draw_top_border(frame, y, &look);
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellHeader {
                    cell_id: self.cell.id.clone(),
                },
            });
        }

        let mut cursor = if self.cell.collapsed {
            self.draw_folded(frame, &look, geometry);
            None
        } else {
            self.draw_source(frame, &look, geometry)
        };

        let mut hidden_rows = 0u32;
        if rows.outputs > 0 {
            if let Some(y) = self.screen_y(rows.divider()) {
                self.draw_divider(frame, y, &look);
                if let Some(output) = self.cell.outputs.first() {
                    geometry.row_hits.push(RowHit {
                        screen_y: y,
                        target: HitTarget::CellOutput {
                            cell_id: self.cell.id.clone(),
                            output_id: output.id,
                            local_row: 0,
                        },
                    });
                }
            }
            let (position, hidden) =
                self.draw_outputs(frame, output_views, &look, rows, geometry);
            cursor = position.or(cursor);
            hidden_rows = hidden;
        }

        if let Some(y) = self.screen_y(rows.total() - 1) {
            let color = if rows.outputs > 0 {
                look.output_border
            } else {
                look.source_border
            };
            let label = (hidden_rows > 0 && look.output_mode.is_none()).then(|| {
                vec![Span::styled(
                    format!(" {hidden_rows} more lines · enter to scroll "),
                    Style::default().fg(self.theme.palette.muted).bg(self.theme.palette.bg),
                )]
            });
            self.hline(frame, y, ("╰", "╯"), color, &[], &label.unwrap_or_default());
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellHeader {
                    cell_id: self.cell.id.clone(),
                },
            });
        }
        cursor
    }

    /// A horizontal border row: `left` + title + rule + right label + `right`.
    fn hline(
        &self,
        frame: &mut Frame<'_>,
        y: u16,
        (left, right): (&str, &str),
        color: Color,
        title: &[Span<'static>],
        right_label: &[Span<'static>],
    ) {
        let p = &self.theme.palette;
        let border = Style::default().fg(color).bg(p.bg);
        let width = usize::from(self.text_w + 4);
        let title_w: usize = title.iter().map(|span| span.width()).sum();
        let right_w: usize = right_label.iter().map(|span| span.width()).sum();
        let mut spans = vec![Span::styled(format!("{left}─"), border)];
        let mut used = 2;
        if title_w > 0 && used + title_w + 2 <= width {
            spans.extend(title.iter().cloned());
            used += title_w;
        }
        let show_right = right_w > 0 && used + right_w + 3 <= width;
        let fill = width.saturating_sub(used + 2 + if show_right { right_w } else { 0 });
        spans.push(Span::styled("─".repeat(fill), border));
        if show_right {
            spans.extend(right_label.iter().cloned());
        }
        spans.push(Span::styled(format!("─{right}"), border));
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(self.area.x, y, self.text_w + 4, 1),
        );
    }

    fn side_borders(&self, frame: &mut Frame<'_>, y: u16, color: Color, fill: Color) {
        let p = &self.theme.palette;
        let style = Style::default().fg(color).bg(p.bg);
        let buf = frame.buffer_mut();
        buf.set_style(self.inner_rect(y), Style::default().bg(fill));
        buf.set_string(self.area.x, y, "│", style);
        buf.set_string(self.right_x(), y, "│", style);
    }

    fn draw_top_border(&self, frame: &mut Frame<'_>, y: u16, look: &Look) {
        let p = &self.theme.palette;
        let bg = p.bg;
        let state = self.view.run_states.get(&self.cell.id).copied();
        let strong = |color: Color, bold: bool| {
            let style = Style::default().fg(color).bg(bg);
            if bold { style.add_modifier(Modifier::BOLD) } else { style }
        };
        let (label, kind_color) = match self.cell.kind {
            CellKind::Code => {
                let count = match state {
                    Some(RunState::Running) => "*".to_string(),
                    Some(RunState::Queued) => "·".to_string(),
                    _ => self
                        .cell
                        .execution_count
                        .map_or_else(|| " ".to_string(), |count| count.to_string()),
                };
                (format!("In [{count}]"), p.accent_code)
            }
            CellKind::Markdown => ("Markdown".to_string(), p.accent_markdown),
            CellKind::Raw => ("Raw".to_string(), p.accent_raw),
        };
        let mut title = vec![Span::styled(
            format!(" {label} "),
            strong(kind_color, look.source_selected),
        )];
        if let Some(mode) = look.editing {
            let name = match mode {
                CellMode::Normal => "normal",
                CellMode::Insert => "insert",
                CellMode::Visual(VisualKind::Char) => "visual",
                CellMode::Visual(VisualKind::Line) => "visual line",
            };
            title.push(Span::styled(format!("{name} "), strong(look.accent, false)));
        }
        let status: Option<(String, Color)> = match state {
            Some(RunState::Queued) => Some(("queued".into(), p.muted)),
            Some(RunState::Running) => Some((
                format!("{} running", SPINNER[self.view.spinner_tick % SPINNER.len()]),
                p.warning,
            )),
            Some(RunState::Failed) => Some((
                format!("✗ {}", self.cell.last_elapsed.map_or_else(String::new, fmt_elapsed)),
                p.error,
            )),
            Some(RunState::Done) | None => self
                .cell
                .last_elapsed
                .map(|elapsed| (format!("✓ {}", fmt_elapsed(elapsed)), p.muted)),
        };
        let right = status
            .map(|(text, color)| {
                vec![Span::styled(
                    format!(" {} ", text.trim_end()),
                    strong(color, false),
                )]
            })
            .unwrap_or_default();
        self.hline(frame, y, ("╭", "╮"), look.source_border, &title, &right);
    }

    fn source_fill(&self) -> Color {
        let p = &self.theme.palette;
        match self.cell.kind {
            CellKind::Markdown => p.bg,
            CellKind::Code | CellKind::Raw => p.code_bg,
        }
    }

    fn draw_folded(&self, frame: &mut Frame<'_>, look: &Look, geometry: &mut FrameGeometry) {
        let Some(y) = self.screen_y(CardRows::SOURCE_TOP) else {
            return;
        };
        let p = &self.theme.palette;
        self.side_borders(frame, y, look.source_border, self.source_fill());
        let hidden = self.cell.buffer.line_count();
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("⋯ {hidden} line{} folded", if hidden == 1 { "" } else { "s" }),
                Style::default().fg(p.muted),
            )),
            self.text_rect(y),
        );
        geometry.row_hits.push(RowHit {
            screen_y: y,
            target: HitTarget::FoldToggle {
                cell_id: self.cell.id.clone(),
            },
        });
    }

    fn draw_source(
        &self,
        frame: &mut Frame<'_>,
        look: &Look,
        geometry: &mut FrameGeometry,
    ) -> Option<(u16, u16)> {
        let p = &self.theme.palette;
        let lines = &self.cached.wrapped_source;
        let segments = look
            .editing
            .map(|_| wrap_segments(self.cell.buffer.as_str(), self.text_w));
        // Raw source rows map 1:1 onto wrap segments; rendered markdown doesn't.
        let hit_segments = segments
            .clone()
            .or_else(|| {
                (self.cell.kind != CellKind::Markdown)
                    .then(|| wrap_segments(self.cell.buffer.as_str(), self.text_w))
            })
            .filter(|segs| segs.len() == lines.len());
        let fill = self.source_fill();
        let mut cursor = None;
        for (index, line) in lines.iter().enumerate() {
            let Some(y) = self.screen_y(CardRows::SOURCE_TOP + index as u32) else {
                continue;
            };
            self.side_borders(frame, y, look.source_border, fill);
            let rect = self.text_rect(y);
            frame.render_widget(Paragraph::new(line.clone()), rect);
            let logical = self.cached.source_line_map.get(index).copied().unwrap_or(index);
            if let (Some(mode), Some(segments)) = (look.editing, segments.as_ref()) {
                let buf = &self.cell.buffer;
                if logical == buf.cursor().row && !matches!(mode, CellMode::Visual(_)) {
                    frame
                        .buffer_mut()
                        .set_style(self.inner_rect(y), Style::default().bg(p.cursor_line));
                }
                if let Some(selection) = buf.selection() {
                    paint_selection(frame, rect, index, segments, buf, selection, self.theme);
                }
                if let Some((row, col)) = cursor_visual(buf, segments) {
                    if row == index {
                        cursor = Some((rect.x + col.min(self.text_w.saturating_sub(1)), y));
                    }
                }
            }
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellSource {
                    cell_id: self.cell.id.clone(),
                    line: logical,
                    char_col: hit_segments
                        .as_ref()
                        .and_then(|segs| segs.get(index))
                        .map_or(0, |seg| seg.1),
                },
            });
        }
        cursor
    }

    /// `├─ Out [3] ──┤`, `├─ Error · ValueError ──┤`, or the stream names.
    fn draw_divider(&self, frame: &mut Frame<'_>, y: u16, look: &Look) {
        let p = &self.theme.palette;
        let bg = p.bg;
        let mut error = None;
        let mut result = false;
        let (mut stdout, mut stderr, mut late) = (false, false, false);
        for output in &self.cell.outputs {
            late |= output.transient_late;
            match &output.data {
                OutputData::Error { ename, .. } => error = Some(ename.clone()),
                OutputData::Rich { .. } => result = true,
                OutputData::Stream {
                    stream: StreamKind::Stdout,
                    ..
                } => stdout = true,
                OutputData::Stream {
                    stream: StreamKind::Stderr,
                    ..
                } => stderr = true,
            }
        }
        let bold = |color: Color| {
            let style = Style::default().fg(color).bg(bg);
            if look.outputs_selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        };
        let mut title = Vec::new();
        if let Some(ename) = error {
            title.push(Span::styled(format!(" Error · {ename} "), bold(p.error)));
        } else if result {
            let count = self
                .cell
                .execution_count
                .map_or_else(String::new, |count| format!(" [{count}]"));
            title.push(Span::styled(format!(" Out{count} "), bold(p.success)));
        } else {
            let name = match (stdout, stderr) {
                (true, true) => "stdout · stderr",
                (false, true) => "stderr",
                _ => "stdout",
            };
            let color = if stderr { p.warning } else { p.muted };
            title.push(Span::styled(format!(" {name} "), bold(color)));
        }
        if late {
            title.push(Span::styled(
                "late ",
                Style::default()
                    .fg(p.warning)
                    .bg(bg)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        let right = match look.output_mode {
            Some(CellMode::Visual(_)) => vec![Span::styled(" visual ", bold(look.accent))],
            Some(_) => vec![Span::styled(" reading ", bold(look.accent))],
            None => Vec::new(),
        };
        self.hline(frame, y, ("├", "┤"), look.output_border, &title, &right);
    }

    /// Output rows; returns the edit cursor (when entered) and how many
    /// rows the views clipped.
    fn draw_outputs(
        &self,
        frame: &mut Frame<'_>,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        look: &Look,
        rows: CardRows,
        geometry: &mut FrameGeometry,
    ) -> (Option<(u16, u16)>, u32) {
        let p = &self.theme.palette;
        let theme = self.theme;
        let top = rows.outputs_top();
        let entered = look
            .output_mode
            .zip(self.view.output_text.filter(|out| out.cell_id == self.cell.id));
        if let Some((mode, out)) = entered {
            let cursor = self.draw_output_text(frame, out, mode, top, rows.outputs, look, geometry);
            return (cursor, 0);
        }
        let mut local = top;
        let mut hidden = 0u32;
        for output in &self.cell.outputs {
            let Some(output_view) = output_views.get_mut(&output.id) else {
                local = local.saturating_add(1);
                continue;
            };
            let metrics = output_view.measure(
                output,
                self.text_w,
                theme,
                Some(self.view.settings.max_output_rows),
            );
            let shown = metrics.shown_rows.max(1);
            hidden += metrics.total_rows.saturating_sub(shown);
            let mut rendered = Vec::new();
            output_view.render_slice(output, self.text_w, 0..shown, false, theme, &mut rendered);
            for row in 0..shown {
                let Some(y) = self.screen_y(local + row) else {
                    continue;
                };
                self.side_borders(frame, y, look.output_border, p.bg);
                if let Some(line) = rendered.get(row as usize).cloned() {
                    frame.render_widget(Paragraph::new(line), self.text_rect(y));
                }
                geometry.row_hits.push(RowHit {
                    screen_y: y,
                    target: HitTarget::CellOutput {
                        cell_id: self.cell.id.clone(),
                        output_id: output.id,
                        // Row within the whole output block (≈ its text row).
                        local_row: local - top + row,
                    },
                });
            }
            local = local.saturating_add(shown);
        }
        (None, hidden)
    }

    /// An entered output: its plain text with cursor, selection and its own
    /// scroll, drawn in the rows the output views normally occupy.
    #[allow(clippy::too_many_arguments)]
    fn draw_output_text(
        &self,
        frame: &mut Frame<'_>,
        out: &OutputText,
        mode: CellMode,
        first_local: u32,
        region: u32,
        look: &Look,
        geometry: &mut FrameGeometry,
    ) -> Option<(u16, u16)> {
        let p = &self.theme.palette;
        let buf = &out.buffer;
        let segments = wrap_segments(buf.as_str(), self.text_w);
        let cursor_at = cursor_visual(buf, &segments);
        let output_id = self.cell.outputs.first().map(|item| item.id)?;
        let mut cursor = None;
        for row in 0..region {
            let Some(y) = self.screen_y(first_local + row) else {
                continue;
            };
            let index = (out.scroll + row) as usize;
            self.side_borders(frame, y, look.output_border, p.bg);
            let rect = self.text_rect(y);
            if let Some(&(line, start, end)) = segments.get(index) {
                let text: String = buf
                    .line(line)
                    .unwrap_or("")
                    .chars()
                    .skip(start)
                    .take(end - start)
                    .collect();
                if line == buf.cursor().row && !matches!(mode, CellMode::Visual(_)) {
                    frame
                        .buffer_mut()
                        .set_style(self.inner_rect(y), Style::default().bg(p.cursor_line));
                }
                frame.render_widget(
                    Paragraph::new(Span::styled(text, Style::default().fg(p.fg))),
                    rect,
                );
                if let Some(selection) = buf.selection() {
                    paint_selection(frame, rect, index, &segments, buf, selection, self.theme);
                }
                if let Some((cursor_row, col)) = cursor_at {
                    if cursor_row == index {
                        cursor = Some((rect.x + col.min(self.text_w.saturating_sub(1)), y));
                    }
                }
            }
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellOutput {
                    cell_id: self.cell.id.clone(),
                    output_id,
                    local_row: index as u32,
                },
            });
        }
        cursor
    }
}

/// Wrapped (row index, display column) of a buffer's cursor.
fn cursor_visual(buf: &TextBuffer, segments: &[(usize, usize, usize)]) -> Option<(usize, u16)> {
    let cursor = buf.cursor();
    let index = segment_of(segments, cursor);
    let &(row, start, _) = segments.get(index)?;
    if row != cursor.row {
        return None;
    }
    let col: usize = buf
        .line(cursor.row)
        .unwrap_or("")
        .chars()
        .skip(start)
        .take(cursor.col.saturating_sub(start))
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    Some((index, col as u16))
}

fn paint_selection(
    frame: &mut Frame<'_>,
    rect: Rect,
    row_index: usize,
    segments: &[(usize, usize, usize)],
    buf: &TextBuffer,
    selection: wb_core::Selection,
    theme: &Theme,
) {
    let Some(&(line, seg_start, seg_end)) = segments.get(row_index) else {
        return;
    };
    let (start, end) = if (selection.anchor.row, selection.anchor.col)
        <= (selection.head.row, selection.head.col)
    {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    };
    if line < start.row || line > end.row {
        return;
    }
    let (from, to) = match selection.kind {
        VisualKind::Line => (seg_start, seg_end.max(seg_start + 1)),
        VisualKind::Char => {
            let from = if line == start.row { start.col } else { 0 };
            let to = if line == end.row { end.col + 1 } else { usize::MAX };
            (from.max(seg_start), to.min(seg_end.max(seg_start + 1)))
        }
    };
    if from >= to {
        return;
    }
    let text = buf.line(line).unwrap_or("");
    let width_of = |a: usize, b: usize| -> u16 {
        text.chars()
            .skip(a)
            .take(b.saturating_sub(a))
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum::<usize>()
            .max(usize::from(b > a && a >= text.chars().count())) as u16
    };
    let x = rect.x + width_of(seg_start, from);
    let w = width_of(from, to).max(1);
    let area = Rect::new(x, rect.y, w.min(rect.right().saturating_sub(x)), 1);
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(theme.palette.panel_selection));
}

fn fmt_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", elapsed.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let whole = elapsed.as_secs();
        format!("{}m{:02}s", whole / 60, whole % 60)
    }
}

/// A thin scrollbar on the right edge, drawn only when content overflows.
pub(super) fn draw_scrollbar(
    frame: &mut Frame<'_>,
    viewport: Rect,
    cache: &LayoutCache,
    scroll_row: u32,
    theme: &Theme,
    geometry: &mut FrameGeometry,
) {
    if viewport.width == 0 || viewport.height == 0 {
        return;
    }
    let total = cache.heights().total_height();
    let visible = u32::from(viewport.height);
    if total <= visible {
        return;
    }
    let track = Rect::new(viewport.right() - 1, viewport.y, 1, viewport.height);
    let thumb_height = ((visible * visible) / total).clamp(1, visible) as u16;
    let travel = viewport.height - thumb_height;
    let max_scroll = total - visible;
    let thumb_y = viewport.y + ((u32::from(travel) * scroll_row.min(max_scroll)) / max_scroll) as u16;
    for row in thumb_y..thumb_y + thumb_height {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "▐",
                Style::default().fg(theme.palette.border),
            ))),
            Rect::new(track.x, row, 1, 1),
        );
    }
    geometry.scrollbar = Some(ScrollbarGeometry {
        track,
        thumb_y,
        thumb_height,
    });
}
