//! Screen region layout solver, scroll anchors, and Fenwick-backed cell virtualization.

pub mod cache;
pub mod fenwick;

pub use cache::{CachedCellLayout, CellLayoutKey, LayoutCache};
pub use fenwick::FenwickTree;

use ratatui::layout::Rect;
use wb_core::CellId;

/// Preserves the exact reading position across terminal resizes, cell folds, and dock toggles.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScrollAnchor {
    pub cell_id: CellId,
    pub local_row: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TailFollow {
    #[default]
    Off,
    /// Pin the top of the cell if taller than viewport; otherwise show bottom (`azo-tui` Latest rule).
    LatestCellTopOrBottom,
    Bottom,
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
        let _ = (dock_rows, expanded_ratio_pct);
        Self {
            top_bar: Rect { x: area.x, y: area.y, width: area.width, height: 1.min(area.height) },
            notebook_area: Rect {
                x: area.x,
                y: area.y.saturating_add(1),
                width: area.width,
                height: area.height.saturating_sub(2),
            },
            expanded_editor_area: None,
            dock_area: None,
            status_bar: Rect {
                x: area.x,
                y: area.y.saturating_add(area.height.saturating_sub(1)),
                width: area.width,
                height: 1.min(area.height),
            },
        }
    }
}
