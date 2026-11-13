//! Notebook document container indexed by `CellId` and `display_id`.

use crate::cell::Cell;
use crate::id::{CellId, OutputId};
use crate::ops::{DocOp, UndoHistory};
use crate::output::{MimeBundle, OutputItem, StreamKind};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Underlying persistence format metadata preserved for lossless round-tripping.
#[derive(Clone, Debug)]
pub enum FormatBacking {
    /// Jupytext `# %%` percent-formatted Python file; outputs stay in memory.
    PercentPy { header: Option<String> },
    /// Standard nbformat 4.5 JSON notebook.
    Ipynb { extra: Map<String, Value> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExternalReloadOutcome {
    Unchanged,
    ReloadedPreservingOutputs { matched_cells: usize },
}

#[derive(Debug)]
pub struct Document {
    order: Vec<CellId>,
    cells: HashMap<CellId, Cell>,
    display_index: HashMap<String, Vec<(CellId, OutputId)>>,
    pub backing: FormatBacking,
    pub session_log: Vec<OutputItem>,
    pub history: UndoHistory,
    pub dirty_source: bool,
    pub dirty_outputs: bool,
}

impl Document {
    pub fn new_empty(backing: FormatBacking) -> Self {
        let first = Cell::new_code();
        let first_id = first.id.clone();
        let mut cells = HashMap::new();
        cells.insert(first_id.clone(), first);
        Self {
            order: vec![first_id],
            cells,
            display_index: HashMap::new(),
            backing,
            session_log: Vec::new(),
            history: UndoHistory::default(),
            dirty_source: false,
            dirty_outputs: false,
        }
    }

    pub fn from_cells(cells_vec: Vec<Cell>, backing: FormatBacking) -> Self {
        if cells_vec.is_empty() {
            return Self::new_empty(backing);
        }
        let mut order = Vec::with_capacity(cells_vec.len());
        let mut cells = HashMap::with_capacity(cells_vec.len());
        for cell in cells_vec {
            order.push(cell.id.clone());
            cells.insert(cell.id.clone(), cell);
        }
        Self {
            order,
            cells,
            display_index: HashMap::new(),
            backing,
            session_log: Vec::new(),
            history: UndoHistory::default(),
            dirty_source: false,
            dirty_outputs: false,
        }
    }

    pub fn order(&self) -> &[CellId] {
        &self.order
    }

    pub fn cell(&self, id: &CellId) -> Option<&Cell> {
        self.cells.get(id)
    }

    pub fn cell_mut(&mut self, id: &CellId) -> Option<&mut Cell> {
        self.cells.get_mut(id)
    }

    pub fn position_of(&self, id: &CellId) -> Option<usize> {
        self.order.iter().position(|c| c == id)
    }

    pub fn display_targets(&self, display_id: &str) -> &[(CellId, OutputId)] {
        self.display_index.get(display_id).map_or(&[], Vec::as_slice)
    }

    pub fn apply(&mut self, _op: DocOp) {
        todo!("wb-core::document: apply DocOp, update display_index, record undo")
    }

    pub fn append_stream(&mut self, _cell_id: &CellId, _stream: StreamKind, _chunk: &str, _late: bool) {
        todo!("wb-core::document: coalesce stream chunk onto cell or session_log and bump output_rev")
    }

    pub fn update_display(&mut self, _display_id: &str, _bundle: MimeBundle, _meta: Map<String, Value>) -> Vec<CellId> {
        todo!("wb-core::document: O(1) lookup via display_index, mutate outputs, return affected CellIds")
    }
}
