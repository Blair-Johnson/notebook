//! Typed in-memory representation of cell and session outputs.
//!
//! Raw nbformat JSON exists only at serialization boundaries (`codec::ipynb`).

use crate::id::OutputId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DEFAULT_MAX_STREAM_LINES: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Coalesced terminal stream text supporting `\r` progress-bar overwrites and line caps.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StreamBuffer {
    text: String,
    max_lines: usize,
    truncated_lines: usize,
}

impl StreamBuffer {
    pub fn with_cap(max_lines: usize) -> Self {
        Self {
            text: String::new(),
            max_lines: max_lines.max(1),
            truncated_lines: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn truncated_lines(&self) -> usize {
        self.truncated_lines
    }

    /// Append a streamed chunk using terminal `\r` overwrite semantics and line capping.
    pub fn push_chunk(&mut self, _chunk: &str) {
        todo!("wb-core::output: coalesce chunk, apply bare '\\r' line overwrites, enforce max_lines")
    }
}

/// MIME-keyed representation map (e.g. `"text/plain"`, `"application/vnd.jaxtree+json"`).
pub type MimeBundle = BTreeMap<String, Value>;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum OutputData {
    Stream {
        stream: StreamKind,
        buffer: StreamBuffer,
    },
    Rich {
        bundle: MimeBundle,
        metadata: Map<String, Value>,
        display_id: Option<String>,
        is_execute_result: bool,
        execution_count: Option<i64>,
    },
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct OutputItem {
    pub id: OutputId,
    pub rev: u64,
    /// True if `iopub` delivered this output after the parent execution reached `idle`.
    pub transient_late: bool,
    pub data: OutputData,
    /// Preserves unknown nbformat output fields across load/save cycles.
    pub extra: Map<String, Value>,
}

impl OutputItem {
    pub fn new(data: OutputData, transient_late: bool) -> Self {
        Self {
            id: OutputId::next(),
            rev: 1,
            transient_late,
            data,
            extra: Map::new(),
        }
    }

    pub fn bump_rev(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }
}
