//! Notebook document container indexed by `CellId` and `display_id`.

use crate::cell::Cell;
use crate::id::{CellId, OutputId};
use crate::ops::{DocOp, UndoHistory};
use crate::output::{
    DEFAULT_MAX_STREAM_LINES, MimeBundle, OutputData, OutputItem, StreamBuffer, StreamKind,
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

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
    /// Line cap for stream outputs started from now on.
    pub stream_line_cap: usize,
    /// The empty cell added outside history when an operation removed the
    /// last cell; undo and redo drop it again while it is untouched.
    placeholder: Option<CellId>,
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
            stream_line_cap: DEFAULT_MAX_STREAM_LINES,
            placeholder: None,
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
        let mut document = Self {
            order,
            cells,
            display_index: HashMap::new(),
            backing,
            session_log: Vec::new(),
            history: UndoHistory::default(),
            dirty_source: false,
            dirty_outputs: false,
            stream_line_cap: DEFAULT_MAX_STREAM_LINES,
            placeholder: None,
        };
        document.rebuild_display_index();
        document
    }

    pub fn order(&self) -> &[CellId] {
        &self.order
    }

    pub fn cells_in_order(&self) -> impl Iterator<Item = &Cell> {
        self.order.iter().filter_map(|id| self.cells.get(id))
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
        self.display_index
            .get(display_id)
            .map_or(&[], Vec::as_slice)
    }

    /// Apply a forward document operation and record it in structural history.
    pub fn apply(&mut self, op: DocOp) {
        self.apply_forward(&op);
        self.history.push(op);
        self.keep_one_cell(false);
    }

    pub fn undo(&mut self) -> bool {
        let Some(op) = self.history.pop_undo() else {
            return false;
        };
        self.apply_reverse(&op);
        self.history.push_redo(op);
        self.keep_one_cell(true);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(op) = self.history.pop_redo() else {
            return false;
        };
        self.apply_forward(&op);
        self.history.push(op);
        self.keep_one_cell(true);
        true
    }

    /// A notebook always has a cell. Removing the last one adds an empty
    /// placeholder outside history; when undo or redo brings other cells
    /// back, an untouched placeholder goes away again.
    fn keep_one_cell(&mut self, replaying: bool) {
        if let Some(id) = self.placeholder.clone() {
            match self.cells.get(&id) {
                None => self.placeholder = None,
                Some(cell) if replaying && self.order.len() > 1 => {
                    if cell.buffer.as_str().is_empty() && cell.outputs.is_empty() {
                        self.remove_ids(&[id]);
                    }
                    self.placeholder = None;
                }
                Some(_) => {}
            }
        }
        if self.order.is_empty() {
            let cell = Cell::new_code();
            self.placeholder = Some(cell.id.clone());
            self.insert_cells(0, &[cell]);
        }
    }

    pub fn append_stream(&mut self, cell_id: &CellId, stream: StreamKind, chunk: &str, late: bool) {
        let cap = self.stream_line_cap;
        let Some(cell) = self.cells.get_mut(cell_id) else {
            self.append_session_log(OutputItem::new(
                OutputData::Stream {
                    stream,
                    buffer: stream_buffer_with_chunk(chunk, cap),
                },
                late,
            ));
            return;
        };
        if let Some(last) = cell.outputs.last_mut()
            && last.transient_late == late
            && let OutputData::Stream {
                stream: last_stream,
                buffer,
            } = &mut last.data
            && *last_stream == stream
        {
            buffer.push_chunk(chunk);
            last.bump_rev();
            cell.bump_output();
            self.dirty_outputs = true;
            return;
        }
        let item = OutputItem::new(
            OutputData::Stream {
                stream,
                buffer: stream_buffer_with_chunk(chunk, cap),
            },
            late,
        );
        cell.outputs.push(item);
        cell.bump_output();
        self.dirty_outputs = true;
    }

    pub fn push_output(&mut self, cell_id: &CellId, item: OutputItem) {
        let Some(cell) = self.cells.get_mut(cell_id) else {
            self.append_session_log(item);
            return;
        };
        cell.outputs.push(item);
        cell.bump_output();
        self.dirty_outputs = true;
        self.rebuild_display_index();
    }

    pub fn append_session_log(&mut self, item: OutputItem) {
        if let Some(last) = self.session_log.last_mut()
            && last.transient_late == item.transient_late
            && let OutputData::Stream {
                stream: last_stream,
                buffer,
            } = &mut last.data
            && let OutputData::Stream {
                stream: item_stream,
                buffer: item_buffer,
            } = &item.data
            && *last_stream == *item_stream
        {
            let chunk = item_buffer.as_str().to_owned();
            buffer.push_chunk(&chunk);
            last.bump_rev();
            self.dirty_outputs = true;
            return;
        }
        self.session_log.push(item);
        self.dirty_outputs = true;
    }

    pub fn clear_cell_outputs(&mut self, cell_id: &CellId) -> Vec<OutputItem> {
        let Some(cell) = self.cells.get_mut(cell_id) else {
            return Vec::new();
        };
        let cleared = std::mem::take(&mut cell.outputs);
        if !cleared.is_empty() {
            cell.bump_output();
            self.dirty_outputs = true;
            self.rebuild_display_index();
        }
        cleared
    }

    pub fn update_display(
        &mut self,
        display_id: &str,
        bundle: MimeBundle,
        meta: Map<String, Value>,
    ) -> Vec<CellId> {
        let targets = self
            .display_index
            .get(display_id)
            .cloned()
            .unwrap_or_default();
        let mut affected = Vec::new();
        let mut seen = HashSet::new();
        for (cell_id, output_id) in targets {
            let Some(cell) = self.cells.get_mut(&cell_id) else {
                continue;
            };
            let Some(output) = cell
                .outputs
                .iter_mut()
                .find(|output| output.id == output_id)
            else {
                continue;
            };
            let OutputData::Rich {
                bundle: old_bundle,
                metadata,
                ..
            } = &mut output.data
            else {
                continue;
            };
            *old_bundle = bundle.clone();
            *metadata = meta.clone();
            output.bump_rev();
            cell.bump_output();
            if seen.insert(cell_id.clone()) {
                affected.push(cell_id);
            }
        }
        if !affected.is_empty() {
            self.dirty_outputs = true;
        }
        affected
    }

    pub(crate) fn rebuild_display_index(&mut self) {
        self.display_index.clear();
        for cell_id in &self.order {
            let Some(cell) = self.cells.get(cell_id) else {
                continue;
            };
            for output in &cell.outputs {
                if let OutputData::Rich {
                    display_id: Some(display_id),
                    ..
                } = &output.data
                {
                    self.display_index
                        .entry(display_id.clone())
                        .or_default()
                        .push((cell_id.clone(), output.id));
                }
            }
        }
    }

    pub(crate) fn replace_state_from(&mut self, mut other: Document) {
        other.rebuild_display_index();
        *self = other;
    }

    fn apply_forward(&mut self, op: &DocOp) {
        match op {
            DocOp::InsertCells { at_index, cells } => {
                self.insert_cells(*at_index, cells);
                self.dirty_source = true;
            }
            DocOp::DeleteCells { removed, .. } => {
                let had_outputs = removed.iter().any(|cell| !cell.outputs.is_empty());
                self.remove_cells(removed);
                self.dirty_source = true;
                self.dirty_outputs |= had_outputs;
            }
            DocOp::MoveCells {
                from_range,
                to_index,
            } => {
                self.move_range(from_range.clone(), *to_index);
                self.dirty_source = true;
            }
            DocOp::SetCellKind {
                cell_id,
                new_kind,
                cleared_outputs,
                ..
            } => {
                if let Some(cell) = self.cells.get_mut(cell_id) {
                    cell.kind = *new_kind;
                    cell.bump_source();
                    cell.bump_chrome();
                    if !cell.outputs.is_empty() || !cleared_outputs.is_empty() {
                        cell.outputs.clear();
                        cell.bump_output();
                        self.dirty_outputs = true;
                    }
                }
                self.dirty_source = true;
            }
            DocOp::CommitCellSource {
                cell_id,
                new_source,
                ..
            } => {
                if let Some(cell) = self.cells.get_mut(cell_id) {
                    cell.buffer.replace_all(new_source.clone());
                    cell.bump_source();
                }
                self.dirty_source = true;
            }
            DocOp::SplitCell {
                original,
                at_index,
                left_id,
                right_id,
            } => {
                self.remove_cells(std::slice::from_ref(original));
                let (left, right) = split_cells(original, left_id.clone(), right_id.clone());
                self.insert_cells(*at_index, &[left, right]);
                self.dirty_source = true;
                self.dirty_outputs |= !original.outputs.is_empty();
            }
            DocOp::MergeCells {
                at_index,
                originals,
                merged_id,
            } => {
                self.remove_cells(originals);
                let merged = merge_cells(originals, merged_id.clone());
                self.insert_cells(*at_index, &[merged]);
                self.dirty_source = true;
                self.dirty_outputs |= originals.iter().any(|cell| !cell.outputs.is_empty());
            }
            DocOp::ClearOutputs { cleared } => {
                let mut changed = false;
                for (cell_id, outputs) in cleared {
                    if let Some(cell) = self.cells.get_mut(cell_id) {
                        changed |= !cell.outputs.is_empty() || !outputs.is_empty();
                        cell.outputs.clear();
                        cell.bump_output();
                    }
                }
                self.dirty_outputs |= changed;
            }
        }
        self.rebuild_display_index();
    }

    fn apply_reverse(&mut self, op: &DocOp) {
        match op {
            DocOp::InsertCells { cells, .. } => {
                self.remove_cells(cells);
                self.dirty_source = true;
            }
            DocOp::DeleteCells { at_index, removed } => {
                self.insert_cells(*at_index, removed);
                self.dirty_source = true;
                self.dirty_outputs |= removed.iter().any(|cell| !cell.outputs.is_empty());
            }
            DocOp::MoveCells {
                from_range,
                to_index,
            } => {
                let len = from_range.end.saturating_sub(from_range.start);
                let source_start = (*to_index).min(self.order.len().saturating_sub(len));
                self.move_range(
                    source_start..source_start.saturating_add(len),
                    from_range.start.min(self.order.len()),
                );
                self.dirty_source = true;
            }
            DocOp::SetCellKind {
                cell_id,
                old_kind,
                cleared_outputs,
                new_kind: _,
            } => {
                if let Some(cell) = self.cells.get_mut(cell_id) {
                    cell.kind = *old_kind;
                    cell.outputs = cleared_outputs.clone();
                    cell.bump_source();
                    cell.bump_chrome();
                    cell.bump_output();
                }
                self.dirty_source = true;
                self.dirty_outputs |= !cleared_outputs.is_empty();
            }
            DocOp::CommitCellSource {
                cell_id,
                old_source,
                ..
            } => {
                if let Some(cell) = self.cells.get_mut(cell_id) {
                    cell.buffer.replace_all(old_source.clone());
                    cell.bump_source();
                }
                self.dirty_source = true;
            }
            DocOp::SplitCell {
                original,
                at_index,
                left_id,
                right_id,
            } => {
                self.remove_ids(&[left_id.clone(), right_id.clone()]);
                self.insert_cells(*at_index, std::slice::from_ref(original));
                self.dirty_source = true;
                self.dirty_outputs |= !original.outputs.is_empty();
            }
            DocOp::MergeCells {
                at_index,
                originals,
                merged_id,
            } => {
                self.remove_ids(std::slice::from_ref(merged_id));
                self.insert_cells(*at_index, originals);
                self.dirty_source = true;
                self.dirty_outputs |= originals.iter().any(|cell| !cell.outputs.is_empty());
            }
            DocOp::ClearOutputs { cleared } => {
                let mut changed = false;
                for (cell_id, outputs) in cleared {
                    if let Some(cell) = self.cells.get_mut(cell_id) {
                        changed |= !outputs.is_empty();
                        cell.outputs = outputs.clone();
                        cell.bump_output();
                    }
                }
                self.dirty_outputs |= changed;
            }
        }
        self.rebuild_display_index();
    }

    fn insert_cells(&mut self, at_index: usize, cells: &[Cell]) {
        let index = at_index.min(self.order.len());
        for (offset, cell) in cells.iter().enumerate() {
            self.cells.insert(cell.id.clone(), cell.clone());
            self.order.insert(index + offset, cell.id.clone());
        }
    }

    fn remove_cells(&mut self, cells: &[Cell]) {
        let ids: Vec<CellId> = cells.iter().map(|cell| cell.id.clone()).collect();
        self.remove_ids(&ids);
    }

    fn remove_ids(&mut self, ids: &[CellId]) {
        for id in ids {
            if let Some(index) = self.order.iter().position(|current| current == id) {
                self.order.remove(index);
            }
            self.cells.remove(id);
        }
    }

    fn move_range(&mut self, range: Range<usize>, to_index: usize) {
        let start = range.start.min(self.order.len());
        let end = range.end.min(self.order.len()).max(start);
        if start == end {
            return;
        }
        let moved: Vec<CellId> = self.order.drain(start..end).collect();
        let destination = to_index.min(self.order.len());
        self.order.splice(destination..destination, moved);
    }
}

fn stream_buffer_with_chunk(chunk: &str, max_lines: usize) -> StreamBuffer {
    let mut buffer = StreamBuffer::with_cap(max_lines);
    buffer.push_chunk(chunk);
    buffer
}

fn split_cells(original: &Cell, left_id: CellId, right_id: CellId) -> (Cell, Cell) {
    let split = original.buffer.char_offset_of(original.buffer.cursor());
    let source: Vec<char> = original.buffer.as_str().chars().collect();
    let left_source: String = source[..split.min(source.len())].iter().collect();
    let right_source: String = source[split.min(source.len())..].iter().collect();
    let mut left = original.clone();
    left.id = left_id;
    left.buffer.replace_all(left_source);
    left.outputs.clear();
    left.execution_count = None;
    left.last_elapsed = None;
    left.bump_source();
    left.bump_output();
    left.bump_chrome();
    let mut right = original.clone();
    right.id = right_id;
    right.buffer.replace_all(right_source);
    right.outputs.clear();
    right.execution_count = None;
    right.last_elapsed = None;
    right.bump_source();
    right.bump_output();
    right.bump_chrome();
    (left, right)
}

fn merge_cells(originals: &[Cell], merged_id: CellId) -> Cell {
    let first = originals.first().cloned().unwrap_or_else(Cell::new_code);
    let mut merged = first.clone();
    merged.id = merged_id;
    let source = originals
        .iter()
        .map(|cell| cell.buffer.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    merged.buffer.replace_all(source);
    merged.outputs = originals
        .iter()
        .flat_map(|cell| cell.outputs.iter().cloned())
        .collect();
    merged.execution_count = originals.iter().rev().find_map(|cell| cell.execution_count);
    merged.last_elapsed = originals.iter().rev().find_map(|cell| cell.last_elapsed);
    merged.bump_source();
    merged.bump_output();
    merged.bump_chrome();
    merged
}
