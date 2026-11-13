//! Pure, read-only rendering entrypoint (`render_app -> FrameGeometry`).
//!
//! Architectural invariant: rendering never mutates `Document`, `FocusStack`,
//! or `Coordinator` state. Only `LayoutCache` updates its internal memoization tables.

mod cells;
pub mod chrome;
pub mod geometry;
mod panels;
pub mod style;

pub use geometry::{FrameGeometry, HitTarget, RowHit, ScrollbarGeometry};

use crate::config_store::CoreSettings;
use crate::focus::{FocusLayer, FocusStack, ModalKind};
use crate::intelligence::{CompletionPopup, InspectPager, SignaturePopup};
use crate::keymap::Keymap;
use crate::layout::{LayoutCache, ViewportLayout, text_width};
use crate::overlays::OverlayStates;
use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use nb_core::{CellId, Document, OutputId};
use nb_sdk::{ExtensionRegistry, OutputView, SettingId, SettingValue, StatusCtx, StatusSegment, Theme};

/// Execution state of a cell, shown in its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Done,
    Failed,
}

/// Kernel lifecycle as shown in the top bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KernelPhase {
    #[default]
    Starting,
    Ready,
    Dead,
}

/// Read-only projection of session state passed into `render_app`.
pub struct AppView<'a> {
    /// The notebook cursor's row in the selected cell, at notebook level.
    pub notebook_row: Option<u32>,
    pub path: Option<&'a Path>,
    pub document: &'a Document,
    pub selected_cell: &'a CellId,
    /// Source or outputs of the selected cell (notebook level).
    pub selected_part: crate::focus::CellPart,
    /// The entered output's read-only text, if any.
    pub output_text: Option<&'a crate::vim::OutputText>,
    pub focus: &'a FocusStack,
    pub scroll_row: u32,
    pub kernel_name: &'a str,
    pub kernel_busy: bool,
    pub kernel_alive: bool,
    pub kernel_phase: KernelPhase,
    pub running_cells: usize,
    pub run_states: &'a HashMap<CellId, RunState>,
    pub spinner_tick: usize,
    pub notice: Option<&'a str>,
    pub settings: &'a CoreSettings,
    pub setting_values: &'a HashMap<SettingId, SettingValue>,
    pub registry: &'a ExtensionRegistry,
    pub status_segments: &'a [Box<dyn StatusSegment>],
    pub completion: Option<&'a CompletionPopup>,
    pub signature: Option<&'a SignaturePopup>,
    pub inspect_pager: Option<&'a InspectPager>,
    pub overlays: &'a OverlayStates,
    pub help_scroll: usize,
    pub logs: &'a VecDeque<String>,
}

/// Screen regions for a frame of `area`, shared by `render_app` and the
/// coordinator's scroll bookkeeping.
pub fn frame_layout(
    area: Rect,
    has_dock: bool,
    focus: &FocusStack,
    settings: &CoreSettings,
) -> ViewportLayout {
    let dock_rows = if has_dock { 4 } else { 0 };
    let expanded_ratio = match focus.base() {
        FocusLayer::CellEdit {
            expanded_split: true,
            ..
        } => Some(settings.composer_expanded_ratio_pct),
        _ => None,
    };
    ViewportLayout::measure(area, dock_rows, expanded_ratio)
}

pub fn render_app(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
    theme: &Theme,
    keymap: &Keymap,
    cache: &mut LayoutCache,
) -> FrameGeometry {
    let areas = frame_layout(
        frame.area(),
        view.overlays.pdb_dock.is_some(),
        view.focus,
        view.settings,
    );
    // The whole body takes the theme's colors, not just the chrome.
    let area = frame.area();
    frame.buffer_mut().set_style(
        area,
        ratatui::style::Style::default()
            .bg(theme.palette.bg)
            .fg(theme.palette.fg),
    );
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
        chrome::draw_top_bar(frame, areas.top_bar, view, &status_ctx, view.status_segments, theme);
    }

    let text_w = text_width(areas.notebook_area.width);
    let editing = match view.focus.base() {
        FocusLayer::CellEdit { cell_id, .. } => Some(cell_id),
        _ => None,
    };
    cache.default_output_view = view.settings.output_default_view;
    cache.sync_viewport(
        view.document,
        output_views,
        text_w,
        view.scroll_row,
        areas.notebook_area.height,
        view.settings.max_output_rows,
        theme,
        editing,
    );
    let mut geometry = FrameGeometry {
        viewport_rect: areas.notebook_area,
        expanded_editor_rect: areas.expanded_editor_area,
        dock_rect: areas.dock_area,
        row_hits: Vec::new(),
        scrollbar: None,
        cursor: None,
        text_x: areas.notebook_area.x,
    };

    let cell_cursor = cells::draw_cells(
        frame,
        view,
        output_views,
        theme,
        cache,
        areas.notebook_area,
        text_w,
        &mut geometry,
    );
    cells::draw_scrollbar(frame, areas.notebook_area, cache, view.scroll_row, theme, &mut geometry);
    draw_expanded_editor(frame, view, theme, cache, areas.expanded_editor_area);
    let dock_cursor = panels::draw_dock(frame, view, theme, areas.dock_area);
    panels::draw_cursor_popups(frame, view, cell_cursor, areas.notebook_area, theme);
    let prompt_cursor = if areas.status_bar.height > 0 {
        chrome::draw_status_bar(frame, areas.status_bar, view, keymap, theme)
    } else {
        None
    };
    panels::draw_modal(frame, view, keymap, theme);

    // The terminal cursor goes wherever keys are typed.
    let cursor = match view.focus.top() {
        FocusLayer::CellEdit { .. } | FocusLayer::OutputFocus { .. } => cell_cursor,
        FocusLayer::PdbInput => dock_cursor,
        FocusLayer::Modal(ModalKind::SearchPrompt | ModalKind::ExCommandBar) => prompt_cursor,
        _ => None,
    };
    if let Some(position) = cursor {
        frame.set_cursor_position(position);
    }
    geometry.cursor = cursor.or(cell_cursor);
    geometry
}

fn draw_expanded_editor(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    theme: &Theme,
    cache: &LayoutCache,
    area: Option<Rect>,
) {
    let Some(area) = area else {
        return;
    };
    let lines: Vec<Line> = cache
        .cached(view.selected_cell)
        .map(|cached| cached.wrapped_source.clone())
        .unwrap_or_default();
    let inner = style::panel(frame, area, "cell · expanded", theme);
    frame.render_widget(Paragraph::new(lines), inner);
}
