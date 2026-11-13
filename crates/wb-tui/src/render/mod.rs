//! Pure, read-only rendering entrypoint (`render_app -> FrameGeometry`).
//!
//! Architectural invariant: rendering never mutates `Document`, `FocusStack`,
//! or `Coordinator` state. Only `LayoutCache` updates its internal memoization tables.

pub mod chrome;
pub mod geometry;

pub use geometry::{FrameGeometry, HitTarget, RowHit, ScrollbarGeometry};

use crate::config_store::CoreSettings;
use crate::focus::FocusStack;
use crate::intelligence::{CompletionPopup, InspectPager, SignaturePopup};
use crate::keymap::Keymap;
use crate::layout::{LayoutCache, ViewportLayout};
use crate::overlays::OverlayStates;
use ratatui::Frame;
use std::collections::HashMap;
use std::path::Path;
use wb_core::{CellId, Document, OutputId};
use wb_sdk::{OutputView, StatusSegment, Theme};

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
    let _ = (view, output_views, theme, keymap, cache);
    let areas = ViewportLayout::measure(frame.area(), 0, None);
    FrameGeometry {
        viewport_rect: areas.notebook_area,
        expanded_editor_rect: areas.expanded_editor_area,
        dock_rect: areas.dock_area,
        row_hits: Vec::new(),
        scrollbar: None,
    }
}
