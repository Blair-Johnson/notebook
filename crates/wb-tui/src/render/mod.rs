//! Pure, read-only rendering entrypoint (`render_app -> FrameGeometry`).
//!
//! Architectural invariant: rendering never mutates `Document`, `FocusStack`,
//! or `Coordinator` state. Only `LayoutCache` updates its internal memoization tables.

pub mod chrome;
pub mod geometry;

pub use geometry::{FrameGeometry, HitTarget, RowHit, ScrollbarGeometry};

use crate::builtins::markdown::render_markdown_cell;
use crate::config_store::CoreSettings;
use crate::focus::{CellMode, FocusLayer, FocusStack};
use crate::intelligence::{CompletionPopup, InspectPager, SignaturePopup};
use crate::keymap::Keymap;
use crate::layout::{LayoutCache, ViewportLayout};
use crate::overlays::OverlayStates;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::HashMap;
use std::path::Path;
use wb_core::{CellId, CellKind, Document, OutputId};
use wb_sdk::{OutputView, StatusCtx, StatusSegment, Theme};

/// Read-only projection of session state passed into `render_app`.
pub struct AppView<'a> {
    pub path: Option<&'a Path>,
    pub document: &'a Document,
    pub selected_cell: &'a CellId,
    pub focus: &'a FocusStack,
    pub scroll_row: u32,
    pub kernel_name: &'a str,
    pub kernel_busy: bool,
    pub kernel_alive: bool,
    pub running_cells: usize,
    pub notice: Option<&'a str>,
    pub settings: &'a CoreSettings,
    pub status_segments: &'a [Box<dyn StatusSegment>],
    pub completion: Option<&'a CompletionPopup>,
    pub signature: Option<&'a SignaturePopup>,
    pub inspect_pager: Option<&'a InspectPager>,
    pub overlays: &'a OverlayStates,
}

pub fn render_app(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
    theme: &Theme,
    keymap: &Keymap,
    cache: &mut LayoutCache,
) -> FrameGeometry {
    let dock_rows = u16::from(view.overlays.pdb_dock.is_some()) * 4;
    let expanded_ratio = match view.focus.top() {
        FocusLayer::CellEdit {
            expanded_split: true,
            ..
        } => Some(view.settings.composer_expanded_ratio_pct),
        _ => None,
    };
    let areas = ViewportLayout::measure(frame.area(), dock_rows, expanded_ratio);
    let status_ctx = StatusCtx {
        path: view.path,
        dirty: view.document.dirty_source || view.document.dirty_outputs,
        kernel_name: view.kernel_name,
        kernel_busy: view.kernel_busy,
        kernel_alive: view.kernel_alive,
        focus_badge: view.focus.top().badge(),
        cell_position: (
            view.document
                .position_of(view.selected_cell)
                .map_or(0, |position| position + 1),
            view.document.order().len(),
        ),
    };
    if areas.top_bar.height > 0 {
        frame.render_widget(
            Paragraph::new(chrome::build_top_bar(
                &status_ctx,
                view.status_segments,
                areas.top_bar.width,
                theme,
            )),
            areas.top_bar,
        );
    }
    if areas.status_bar.height > 0 {
        let cascade = chrome::StatusCascade {
            fatal_error: None,
            confirm_prompt: view.overlays.confirm.as_ref().map(|_| "confirm action"),
            copy_armed: false,
            running_cells: view.running_cells,
            transient_notice: view.notice,
        };
        frame.render_widget(
            Paragraph::new(chrome::build_status_bar(
                &cascade,
                view.focus,
                keymap,
                areas.status_bar.width,
                theme,
            )),
            areas.status_bar,
        );
    }

    let content_width = areas.notebook_area.width.saturating_sub(1).max(1);
    cache.sync_viewport(
        view.document,
        output_views,
        content_width,
        view.scroll_row,
        areas.notebook_area.height,
        view.settings.max_output_rows,
        theme,
    );
    let mut geometry = FrameGeometry {
        viewport_rect: areas.notebook_area,
        expanded_editor_rect: areas.expanded_editor_area,
        dock_rect: areas.dock_area,
        row_hits: Vec::new(),
        scrollbar: None,
    };

    draw_cells(
        frame,
        view,
        output_views,
        theme,
        cache,
        content_width,
        &mut geometry,
    );
    draw_scrollbar(
        frame,
        &areas.notebook_area,
        cache,
        view.scroll_row,
        &mut geometry,
    );
    draw_expanded_editor(frame, view, theme, cache, &areas.expanded_editor_area);
    draw_dock(frame, view, theme, &areas.dock_area);
    draw_overlays(frame, view, theme);
    geometry
}

fn draw_cells(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
    theme: &Theme,
    cache: &LayoutCache,
    content_width: u16,
    geometry: &mut FrameGeometry,
) {
    let Some((mut index, _)) = cache.heights().find_row(view.scroll_row) else {
        return;
    };
    let viewport_end = view
        .scroll_row
        .saturating_add(u32::from(geometry.viewport_rect.height));
    while index < view.document.order().len() {
        let Some(cell_id) = view.document.order().get(index) else {
            break;
        };
        let cell_start = cache.heights().prefix_sum(index);
        if cell_start >= viewport_end && index > 0 {
            break;
        }
        let cell_height = cache.heights().height(index);
        let cell_end = cell_start.saturating_add(cell_height);
        let visible_start = cell_start.max(view.scroll_row);
        let visible_end = cell_end.min(viewport_end);
        if let Some(cell) = view.document.cell(cell_id) {
            draw_one_cell(
                frame,
                view,
                cell,
                cache.cached(cell_id),
                output_views,
                theme,
                content_width,
                cell_start,
                visible_start,
                visible_end,
                geometry,
            );
        }
        index += 1;
    }
}

fn draw_one_cell(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    cell: &wb_core::Cell,
    cached: Option<&crate::layout::CachedCellLayout>,
    output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
    theme: &Theme,
    content_width: u16,
    cell_start: u32,
    visible_start: u32,
    visible_end: u32,
    geometry: &mut FrameGeometry,
) {
    let Some(cached) = cached else {
        return;
    };
    let source_lines = cached.wrapped_source.clone();
    let source_map = cached.source_line_map.clone();
    let mut output_ranges = Vec::new();
    let mut local = if cell.collapsed {
        2
    } else {
        1u32.saturating_add(source_lines.len() as u32)
    };
    if !cell.collapsed {
        for output in &cell.outputs {
            let shown = output_views
                .get_mut(&output.id)
                .map(|output_view| {
                    output_view
                        .measure(
                            output,
                            content_width.saturating_sub(3).max(1),
                            theme,
                            Some(view.settings.max_output_rows),
                        )
                        .shown_rows
                        .max(1)
                })
                .unwrap_or(1);
            output_ranges.push((output.id, local, shown));
            local = local.saturating_add(shown);
        }
    }
    let first_local = visible_start.saturating_sub(cell_start);
    let last_local = visible_end.saturating_sub(cell_start);
    for row in first_local..last_local {
        let y_offset = cell_start
            .saturating_add(row)
            .saturating_sub(view.scroll_row);
        let y = geometry.viewport_rect.y.saturating_add(y_offset as u16);
        if y >= geometry.viewport_rect.bottom() {
            continue;
        }
        if row == 0 {
            let kind_style = match cell.kind {
                CellKind::Code => Style::default().fg(theme.palette.accent_code),
                CellKind::Markdown => Style::default().fg(theme.palette.accent_markdown),
                CellKind::Raw => Style::default().fg(theme.palette.accent_raw),
            };
            let execution = cell
                .execution_count
                .map_or_else(|| "In [ ]".to_string(), |count| format!("In [{count}]"));
            let elapsed = cell
                .last_elapsed
                .map_or_else(String::new, |duration| format!("  · {:?}", duration));
            let line = Line::from(vec![
                Span::styled("▎", kind_style),
                Span::styled(
                    format!(" {execution}{elapsed}"),
                    Style::default()
                        .fg(theme.palette.fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", cell.kind_label())),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(geometry.viewport_rect.x, y, content_width, 1),
            );
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellHeader {
                    cell_id: cell.id.clone(),
                },
            });
            continue;
        }
        if cell.collapsed {
            let line = Line::from(Span::styled(
                "  … folded …",
                Style::default().fg(theme.palette.muted),
            ));
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(geometry.viewport_rect.x, y, content_width, 1),
            );
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::FoldToggle {
                    cell_id: cell.id.clone(),
                },
            });
            continue;
        }
        if row >= 1 && row < 1 + source_lines.len() as u32 {
            let source_index = row as usize - 1;
            let mut line = source_lines[source_index].clone();
            if let FocusLayer::CellEdit {
                cell_id,
                mode,
                expanded_split: _,
            } = view.focus.top()
            {
                if cell_id == &cell.id {
                    let cursor = cell.buffer.cursor();
                    let selected = matches!(mode, CellMode::Visual(_));
                    if source_map.get(source_index).copied() == Some(cursor.row) || selected {
                        line = line.patch_style(theme.cursor_inverse());
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(
                    geometry.viewport_rect.x.saturating_add(2),
                    y,
                    content_width.saturating_sub(2),
                    1,
                ),
            );
            geometry.row_hits.push(RowHit {
                screen_y: y,
                target: HitTarget::CellSource {
                    cell_id: cell.id.clone(),
                    line: source_map
                        .get(source_index)
                        .copied()
                        .unwrap_or(source_index),
                    char_col: 0,
                },
            });
            continue;
        }
        for (output_id, output_start, output_rows) in &output_ranges {
            if row < *output_start || row >= output_start.saturating_add(*output_rows) {
                continue;
            }
            let local_row = row.saturating_sub(*output_start);
            if let Some(output_item) = cell.outputs.iter().find(|output| output.id == *output_id) {
                if let Some(output_view) = output_views.get(output_id) {
                    let mut lines = Vec::new();
                    output_view.render_slice(
                        output_item,
                        content_width.saturating_sub(3).max(1),
                        0..*output_rows,
                        matches!(view.focus.top(), FocusLayer::OutputFocus { output_id: focused, .. } if focused == output_id),
                        theme,
                        &mut lines,
                    );
                    if let Some(mut line) = lines.into_iter().nth(local_row as usize) {
                        if local_row == 0 && output_item.transient_late {
                            line.spans.insert(
                                0,
                                Span::styled("LATE ", Style::default().fg(theme.palette.warning)),
                            );
                        }
                        frame.render_widget(
                            Paragraph::new(line),
                            Rect::new(
                                geometry.viewport_rect.x.saturating_add(3),
                                y,
                                content_width.saturating_sub(3),
                                1,
                            ),
                        );
                    }
                }
                geometry.row_hits.push(RowHit {
                    screen_y: y,
                    target: HitTarget::CellOutput {
                        cell_id: cell.id.clone(),
                        output_id: *output_id,
                        local_row,
                    },
                });
            }
            break;
        }
    }
}

fn draw_scrollbar(
    frame: &mut Frame<'_>,
    viewport: &Rect,
    cache: &LayoutCache,
    scroll_row: u32,
    geometry: &mut FrameGeometry,
) {
    if viewport.width == 0 || viewport.height == 0 {
        return;
    }
    let total = cache.heights().total_height().max(1);
    let visible = u32::from(viewport.height).min(total);
    let track = Rect::new(
        viewport.right().saturating_sub(1),
        viewport.y,
        1,
        viewport.height,
    );
    let thumb_height = ((u32::from(viewport.height) * visible) / total)
        .max(1)
        .min(u32::from(viewport.height)) as u16;
    let travel = viewport.height.saturating_sub(thumb_height);
    let max_scroll = total.saturating_sub(visible);
    let thumb_y = if max_scroll == 0 {
        viewport.y
    } else {
        viewport
            .y
            .saturating_add(((u32::from(travel) * scroll_row.min(max_scroll)) / max_scroll) as u16)
    };
    for row in viewport.y..viewport.bottom() {
        let symbol = if row >= thumb_y && row < thumb_y.saturating_add(thumb_height) {
            "█"
        } else {
            "│"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw(symbol))),
            Rect::new(track.x, row, 1, 1),
        );
    }
    geometry.scrollbar = Some(ScrollbarGeometry {
        track,
        thumb_y,
        thumb_height,
    });
}

fn draw_expanded_editor(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    theme: &Theme,
    cache: &LayoutCache,
    area: &Option<Rect>,
) {
    let Some(area) = area else {
        return;
    };
    let Some(cell) = view.document.cell(view.selected_cell) else {
        return;
    };
    let lines = cache
        .cached(view.selected_cell)
        .map(|cached| cached.wrapped_source.clone())
        .unwrap_or_else(|| render_markdown_cell(cell.buffer.as_str(), area.width, theme).lines);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Expanded Editor"),
        ),
        *area,
    );
}

fn draw_dock(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme, area: &Option<Rect>) {
    let (Some(area), Some(dock)) = (area, view.overlays.pdb_dock.as_ref()) else {
        return;
    };
    let text = format!("{} {}", dock.prompt, dock.composer.as_str());
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.palette.fg))
            .block(Block::default().borders(Borders::ALL).title("PDB")),
        *area,
    );
}

fn draw_overlays(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme) {
    if let Some(popup) = view.completion {
        let lines = popup
            .filtered
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                Line::from(Span::styled(
                    format!(
                        "{} {} {}",
                        candidate.replace_text, candidate.kind, candidate.detail
                    ),
                    if index == popup.selected {
                        theme.cursor_inverse()
                    } else {
                        Style::default().fg(theme.palette.fg)
                    },
                ))
            })
            .collect::<Vec<_>>();
        let area = centered_rect(frame.area(), 60, 35);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Completion")),
            area,
        );
    } else if let Some(signature) = view.signature {
        let area = centered_rect(frame.area(), 70, 20);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(signature.signature_text.clone())
                .block(Block::default().borders(Borders::ALL).title("Signature")),
            area,
        );
    } else if let Some(inspect) = view.inspect_pager {
        let area = centered_rect(frame.area(), 80, 70);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(inspect.ansi_text.clone()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(inspect.title.clone()),
            ),
            area,
        );
    }
    if let Some(prompt) = view.overlays.prompt.as_ref() {
        let area = Rect::new(
            frame.area().x,
            frame.area().bottom().saturating_sub(2),
            frame.area().width,
            2,
        );
        frame.render_widget(
            Paragraph::new(format!(":{}", prompt.input))
                .block(Block::default().borders(Borders::ALL).title("Prompt")),
            area,
        );
    }
    if view.overlays.palette.is_some() || view.overlays.settings.capturing_keybind {
        let area = centered_rect(frame.area(), 70, 60);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(if view.overlays.palette.is_some() {
                "Command Palette"
            } else {
                "Settings"
            })
            .block(Block::default().borders(Borders::ALL)),
            area,
        );
    }
    if view.overlays.log_scroll_from_bottom > 0 {
        let area = centered_rect(frame.area(), 85, 70);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new("Logs").block(Block::default().borders(Borders::ALL).title("Logs")),
            area,
        );
    }
}

fn centered_rect(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = area.width.saturating_mul(width_pct.min(100)) / 100;
    let height = area.height.saturating_mul(height_pct.min(100)) / 100;
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.max(1).min(area.width),
        height.max(1).min(area.height),
    )
}

trait CellKindLabel {
    fn kind_label(&self) -> &'static str;
}

impl CellKindLabel for wb_core::Cell {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            CellKind::Code => "code",
            CellKind::Markdown => "markdown",
            CellKind::Raw => "raw",
        }
    }
}
