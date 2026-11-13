//! Value-type `FrameGeometry` hit map produced by `render_app` and read by mouse handlers.

use ratatui::layout::Rect;
use nb_core::{CellId, OutputId};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HitTarget {
    CellHeader {
        cell_id: CellId,
    },
    CellSource {
        cell_id: CellId,
        line: usize,
        char_col: usize,
    },
    CellOutput {
        cell_id: CellId,
        output_id: OutputId,
        local_row: u32,
    },
    FoldToggle {
        cell_id: CellId,
    },
    Hyperlink {
        url: String,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RowHit {
    pub screen_y: u16,
    pub target: HitTarget,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollbarGeometry {
    pub track: Rect,
    pub thumb_y: u16,
    pub thumb_height: u16,
}

#[derive(Clone, Debug, Default)]
pub struct FrameGeometry {
    pub viewport_rect: Rect,
    pub expanded_editor_rect: Option<Rect>,
    pub dock_rect: Option<Rect>,
    pub row_hits: Vec<RowHit>,
    pub scrollbar: Option<ScrollbarGeometry>,
    /// Screen cell of the edit cursor, when a cell or input is being edited.
    pub cursor: Option<(u16, u16)>,
    /// Screen x where cell text begins (right of the gutter); clicks map
    /// `x - text_x` display columns onto a `CellSource` row's characters.
    pub text_x: u16,
}

impl FrameGeometry {
    pub fn hit_at(&self, _x: u16, y: u16) -> Option<&HitTarget> {
        self.row_hits
            .iter()
            .find(|hit| hit.screen_y == y)
            .map(|hit| &hit.target)
    }
}
