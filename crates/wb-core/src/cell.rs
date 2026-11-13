//! Notebook cell model and granular revision counters.

use crate::buffer::TextBuffer;
use crate::id::CellId;
use crate::output::OutputItem;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
}

/// Per-cell monotonic revisions used by `wb-tui::LayoutCache` to avoid re-measuring
/// unaffected cells when a single cell streams output or changes source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CellRevisions {
    pub source_rev: u64,
    pub output_rev: u64,
    pub chrome_rev: u64,
}

impl Default for CellRevisions {
    fn default() -> Self {
        Self {
            source_rev: 1,
            output_rev: 1,
            chrome_rev: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub id: CellId,
    pub kind: CellKind,
    pub buffer: TextBuffer,
    pub outputs: Vec<OutputItem>,
    pub execution_count: Option<i64>,
    pub collapsed: bool,
    pub last_elapsed: Option<Duration>,
    pub revs: CellRevisions,
    /// Preserves unknown nbformat cell fields (`metadata`, custom keys).
    pub extra: Map<String, Value>,
}

impl Cell {
    pub fn new(kind: CellKind, source: impl Into<String>) -> Self {
        Self {
            id: CellId::new_random(),
            kind,
            buffer: TextBuffer::new(source),
            outputs: Vec::new(),
            execution_count: None,
            collapsed: false,
            last_elapsed: None,
            revs: CellRevisions::default(),
            extra: Map::new(),
        }
    }

    pub fn new_code() -> Self {
        Self::new(CellKind::Code, "")
    }

    pub fn bump_source(&mut self) {
        self.revs.source_rev = self.revs.source_rev.wrapping_add(1);
    }

    pub fn bump_output(&mut self) {
        self.revs.output_rev = self.revs.output_rev.wrapping_add(1);
    }

    pub fn bump_chrome(&mut self) {
        self.revs.chrome_rev = self.revs.chrome_rev.wrapping_add(1);
    }
}
