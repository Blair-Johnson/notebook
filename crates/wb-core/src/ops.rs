//! Reversible notebook-level structural and source operations (`DocOp`).

use crate::cell::{Cell, CellKind};
use crate::id::CellId;
use crate::output::OutputItem;

/// Reversible structural or committed-source mutation at the notebook level.
///
/// Intra-cell keystroke undo lives on `Cell::buffer`; `DocOp` records cell-level
/// operations and source commits when exiting cell editing.
#[derive(Clone, Debug)]
pub enum DocOp {
    InsertCells {
        at_index: usize,
        cells: Vec<Cell>,
    },
    DeleteCells {
        at_index: usize,
        removed: Vec<Cell>,
    },
    MoveCells {
        from_range: std::ops::Range<usize>,
        to_index: usize,
    },
    SetCellKind {
        cell_id: CellId,
        old_kind: CellKind,
        new_kind: CellKind,
        cleared_outputs: Vec<OutputItem>,
    },
    CommitCellSource {
        cell_id: CellId,
        old_source: String,
        new_source: String,
    },
    SplitCell {
        original: Cell,
        at_index: usize,
        left_id: CellId,
        right_id: CellId,
    },
    MergeCells {
        at_index: usize,
        originals: Vec<Cell>,
        merged_id: CellId,
    },
    ClearOutputs {
        cleared: Vec<(CellId, Vec<OutputItem>)>,
    },
}

#[derive(Default, Debug)]
pub struct UndoHistory {
    undo_stack: Vec<DocOp>,
    redo_stack: Vec<DocOp>,
}

impl UndoHistory {
    pub fn push(&mut self, op: DocOp) {
        self.undo_stack.push(op);
        self.redo_stack.clear();
    }

    pub fn pop_undo(&mut self) -> Option<DocOp> {
        self.undo_stack.pop()
    }

    pub fn push_redo(&mut self, op: DocOp) {
        self.redo_stack.push(op);
    }

    pub fn pop_redo(&mut self) -> Option<DocOp> {
        self.redo_stack.pop()
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}
