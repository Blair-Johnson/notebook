//! Notebook cells: header row, gutter bar, source (with cursor and
//! selection), outputs, and the scrollbar.

use super::geometry::{FrameGeometry, HitTarget, RowHit, ScrollbarGeometry};
use super::style::mode_color;
use super::{AppView, RunState};
use crate::focus::{CellMode, FocusLayer};
use crate::layout::{CachedCellLayout, GUTTER_COLS, LayoutCache};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::HashMap;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use wb_core::{Cell, CellKind, OutputId, VisualKind};
use wb_sdk::{OutputView, Theme};

/// Draw every visible cell; returns the screen position for the terminal
/// cursor when a cell is being edited.
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

    fn draw(
        &self,
        frame: &mut Frame<'_>,
        output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        geometry: &mut FrameGeometry,
    ) -> Option<(u16, u16)> {
        let view = self.view;
        let theme = self.theme;
        let p = &theme.palette;
        let selected = view.selected_cell == &self.cell.id;
        let base = view.focus.base();
        let editing_mode = match base {
            FocusLayer::CellEdit { cell_id, mode, .. } if cell_id == &self.cell.id => Some(*mode),
            _ => None,
        };
        let output_focused = match base {
            FocusLayer::OutputFocus { cell_id, output_id, .. } if cell_id == &self.cell.id => {
                Some(*output_id)
            }
            _ => None,
        };
        let accent = mode_color(base, theme);
        let bar = |frame: &mut Frame<'_>, y: u16, glyph: &str, color| {
            frame.render_widget(
                Paragraph::new(Span::styled(glyph.to_string(), Style::default().fg(color))),
                Rect::new(self.area.x, y, 1, 1),
            );
        };

        // Header row.
        if let Some(y) = self.screen_y(0) {
            if selected {
                bar(frame, y, "▌", accent);
            }
            self.draw_header(frame, y, selected, accent);
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellHeader {
                    cell_id: self.cell.id.clone(),
                },
            });
        }

        if self.cell.collapsed {
            if let Some(y) = self.screen_y(1) {
                if selected {
                    bar(frame, y, "▌", accent);
                }
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
            return None;
        }

        // Source rows.
        let lines = &self.cached.wrapped_source;
        let segments = editing_mode.map(|_| self.segments());
        // Raw source rows map 1:1 onto wrap segments; rendered markdown doesn't.
        let hit_segments = segments
            .clone()
            .or_else(|| (self.cell.kind != CellKind::Markdown).then(|| self.segments()))
            .filter(|segs| segs.len() == lines.len());
        geometry.text_x = self.area.x + GUTTER_COLS;
        let mut cursor = None;
        for (index, line) in lines.iter().enumerate() {
            let Some(y) = self.screen_y(1 + index as u32) else {
                continue;
            };
            if selected {
                bar(frame, y, "▌", accent);
            } else if self.cell.kind == CellKind::Code {
                bar(frame, y, "▏", p.border);
            }
            let rect = self.text_rect(y);
            frame.render_widget(Paragraph::new(line.clone()), rect);
            let logical = self.cached.source_line_map.get(index).copied().unwrap_or(index);
            if let (Some(mode), Some(segments)) = (editing_mode, segments.as_ref()) {
                let cursor_pos = self.cell.buffer.cursor();
                if logical == cursor_pos.row && !matches!(mode, CellMode::Visual(_)) {
                    frame
                        .buffer_mut()
                        .set_style(rect, Style::default().bg(p.surface));
                }
                if let Some(selection) = self.cell.buffer.selection() {
                    self.paint_selection(frame, rect, index, segments, selection);
                }
                if let Some((row, col)) = self.cursor_visual(segments) {
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

        // Outputs, directly below the source at the same indent.
        let mut local = 1u32.saturating_add(lines.len() as u32);
        for output in &self.cell.outputs {
            let Some(output_view) = output_views.get_mut(&output.id) else {
                local = local.saturating_add(1);
                continue;
            };
            let shown = output_view
                .measure(output, self.text_w, theme, Some(view.settings.max_output_rows))
                .shown_rows
                .max(1);
            let focused = output_focused == Some(output.id);
            let mut rendered = Vec::new();
            output_view.render_slice(output, self.text_w, 0..shown, focused, theme, &mut rendered);
            for row in 0..shown {
                let Some(y) = self.screen_y(local + row) else {
                    continue;
                };
                if focused {
                    bar(frame, y, "▌", accent);
                } else if selected {
                    bar(frame, y, "▏", accent);
                }
                if let Some(mut line) = rendered.get(row as usize).cloned() {
                    if row == 0 && output.transient_late {
                        line.spans.insert(
                            0,
                            Span::styled(
                                "late ",
                                Style::default().fg(p.warning).add_modifier(Modifier::ITALIC),
                            ),
                        );
                    }
                    frame.render_widget(Paragraph::new(line), self.text_rect(y));
                }
                geometry.row_hits.push(RowHit {
                    screen_y: y,
                    target: HitTarget::CellOutput {
                        cell_id: self.cell.id.clone(),
                        output_id: output.id,
                        local_row: row,
                    },
                });
            }
            local = local.saturating_add(shown);
        }
        cursor
    }

    fn draw_header(&self, frame: &mut Frame<'_>, y: u16, selected: bool, accent: ratatui::style::Color) {
        let p = &self.theme.palette;
        let state = self.view.run_states.get(&self.cell.id).copied();
        let label_style = if selected {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.muted)
        };
        let label = match self.cell.kind {
            CellKind::Code => {
                let count = match state {
                    Some(RunState::Running) => "*".to_string(),
                    Some(RunState::Queued) => "·".to_string(),
                    _ => self
                        .cell
                        .execution_count
                        .map_or_else(|| " ".to_string(), |count| count.to_string()),
                };
                format!("In [{count}]")
            }
            CellKind::Markdown => String::new(),
            CellKind::Raw => "raw".to_string(),
        };
        let status: Option<(String, ratatui::style::Color)> = match state {
            Some(RunState::Queued) => Some(("queued".into(), p.muted)),
            Some(RunState::Running) => Some(("● running".into(), p.warning)),
            Some(RunState::Failed) => Some((
                format!("✗ {}", self.cell.last_elapsed.map_or_else(String::new, fmt_elapsed)),
                p.error,
            )),
            Some(RunState::Done) | None => self
                .cell
                .last_elapsed
                .map(|elapsed| (format!("✓ {}", fmt_elapsed(elapsed)), p.muted)),
        };
        let rect = self.text_rect(y);
        frame.render_widget(Paragraph::new(Span::styled(label, label_style)), rect);
        if let Some((text, color)) = status {
            let text = text.trim_end().to_string();
            let width = UnicodeWidthStr::width(text.as_str()) as u16;
            if width < rect.width {
                frame.render_widget(
                    Paragraph::new(Span::styled(text, Style::default().fg(color))),
                    Rect::new(rect.right() - width, y, width, 1),
                );
            }
        }
    }

    /// Per wrapped source row: (logical line, char range) — mirrors the
    /// layout cache's character wrapping.
    fn segments(&self) -> Vec<(usize, usize, usize)> {
        let width = usize::from(self.text_w.max(1));
        let mut out = Vec::new();
        for (row, line) in self.cell.buffer.as_str().split('\n').enumerate() {
            let mut start = 0usize;
            let mut used = 0usize;
            for (index, ch) in line.chars().enumerate() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cw > 0 && used > 0 && used + cw > width {
                    out.push((row, start, index));
                    start = index;
                    used = 0;
                }
                used += cw;
            }
            out.push((row, start, line.chars().count()));
        }
        out
    }

    /// Wrapped (row index, display column) of the edit cursor.
    fn cursor_visual(&self, segments: &[(usize, usize, usize)]) -> Option<(usize, u16)> {
        let cursor = self.cell.buffer.cursor();
        let line = self.cell.buffer.line(cursor.row).unwrap_or("");
        let chars: Vec<char> = line.chars().collect();
        let rows: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter(|(_, (row, _, _))| *row == cursor.row)
            .map(|(index, _)| index)
            .collect();
        let first = *rows.first()?;
        // The segment holding the cursor: the last one starting at or before it.
        let index = rows
            .iter()
            .copied()
            .filter(|index| segments[*index].1 <= cursor.col)
            .last()
            .unwrap_or(first);
        let start = segments[index].1;
        let col: usize = chars
            .iter()
            .skip(start)
            .take(cursor.col.saturating_sub(start))
            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
            .sum();
        Some((index, col as u16))
    }

    fn paint_selection(
        &self,
        frame: &mut Frame<'_>,
        rect: Rect,
        row_index: usize,
        segments: &[(usize, usize, usize)],
        selection: wb_core::Selection,
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
        let text = self.cell.buffer.line(line).unwrap_or("");
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
        frame.buffer_mut().set_style(
            area,
            Style::default().bg(self.theme.palette.panel_selection),
        );
    }
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
