//! Screen region layout solver, scroll anchors, and Fenwick-backed cell virtualization.

pub mod cache;
pub mod fenwick;

pub use cache::{CachedCellLayout, CellLayoutKey, LayoutCache};
pub use fenwick::FenwickTree;

use ratatui::layout::Rect;
use wb_core::CellId;

pub const MIN_HISTORY_ROWS: u16 = 3;

/// Preserves the exact reading position across terminal resizes, cell folds, and dock toggles.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScrollAnchor {
    pub cell_id: CellId,
    pub local_row: u32,
}

impl ScrollAnchor {
    pub fn new(cell_id: CellId, local_row: u32) -> Self {
        Self { cell_id, local_row }
    }

    pub fn top(cell_id: CellId) -> Self {
        Self::new(cell_id, 0)
    }

    pub fn clamp_local(&mut self, cell_rows: u32) {
        self.local_row = self.local_row.min(cell_rows.saturating_sub(1));
    }

    /// Resolve the anchor to a global row after a cell's height changes.
    pub fn global_row(&self, cell_index: usize, heights: &FenwickTree) -> u32 {
        heights
            .prefix_sum(cell_index)
            .saturating_add(self.local_row)
    }

    /// Pin a newly revealed cell at its top when it is taller than the available viewport.
    pub fn reveal_offset(&self, cell_rows: u32, viewport_rows: u16) -> u32 {
        if cell_rows > u32::from(viewport_rows) {
            0
        } else {
            cell_rows.saturating_sub(u32::from(viewport_rows))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TailFollow {
    #[default]
    Off,
    /// Pin the top of the cell if taller than viewport; otherwise show bottom (`azo-tui` Latest rule).
    LatestCellTopOrBottom,
    Bottom,
}

impl TailFollow {
    pub fn offset(self, total_rows: u32, viewport_rows: u16) -> Option<u32> {
        let viewport_rows = u32::from(viewport_rows);
        match self {
            Self::Off => None,
            Self::Bottom => Some(total_rows.saturating_sub(viewport_rows)),
            Self::LatestCellTopOrBottom => {
                if total_rows > viewport_rows {
                    Some(0)
                } else {
                    Some(0)
                }
            }
        }
    }

    pub fn is_following_tail(self) -> bool {
        matches!(self, Self::Bottom | Self::LatestCellTopOrBottom)
    }

    pub fn apply(self, current: Option<u32>, total_rows: u32, viewport_rows: u16) -> Option<u32> {
        match self {
            Self::Off => current,
            Self::Bottom => self.offset(total_rows, viewport_rows),
            Self::LatestCellTopOrBottom => self.offset(total_rows, viewport_rows),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ViewportLayout {
    pub top_bar: Rect,
    pub notebook_area: Rect,
    pub expanded_editor_area: Option<Rect>,
    pub dock_area: Option<Rect>,
    pub status_bar: Rect,
}

impl ViewportLayout {
    pub fn measure(area: Rect, dock_rows: u16, expanded_ratio_pct: Option<u16>) -> Self {
        let top_height = area.height.min(1);
        let status_height = area.height.saturating_sub(top_height).min(1);
        let top_bar = Rect::new(area.x, area.y, area.width, top_height);
        let status_bar = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height.saturating_sub(status_height)),
            area.width,
            status_height,
        );
        let body_y = area.y.saturating_add(top_height);
        let body_height = area
            .height
            .saturating_sub(top_height)
            .saturating_sub(status_height);
        let mut notebook = Rect::new(area.x, body_y, area.width, body_height);

        let available_for_dock = notebook.height.saturating_sub(MIN_HISTORY_ROWS);
        let actual_dock_rows = dock_rows.min(available_for_dock);
        let dock_area = if actual_dock_rows > 0 {
            let y = notebook
                .y
                .saturating_add(notebook.height - actual_dock_rows);
            let rect = Rect::new(notebook.x, y, notebook.width, actual_dock_rows);
            notebook.height = notebook.height.saturating_sub(actual_dock_rows);
            Some(rect)
        } else {
            None
        };

        let expanded_height = expanded_ratio_pct
            .map(|ratio| notebook.height.saturating_mul(u16::from(ratio.min(100))) / 100)
            .unwrap_or(0)
            .min(notebook.height.saturating_sub(MIN_HISTORY_ROWS));
        let expanded_editor_area = if expanded_height > 0 {
            let y = notebook.y.saturating_add(notebook.height - expanded_height);
            let rect = Rect::new(notebook.x, y, notebook.width, expanded_height);
            notebook.height = notebook.height.saturating_sub(expanded_height);
            Some(rect)
        } else {
            None
        };

        Self {
            top_bar,
            notebook_area: notebook,
            expanded_editor_area,
            dock_area,
            status_bar,
        }
    }
}
