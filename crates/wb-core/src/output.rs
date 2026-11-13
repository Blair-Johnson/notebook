//! Typed in-memory representation of cell and session outputs.
//!
//! Raw nbformat JSON exists only at serialization boundaries (`codec::ipynb`).

use crate::id::OutputId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DEFAULT_MAX_STREAM_LINES: usize = 10_000;
const TRUNCATION_MARKER: &str = "[... output truncated ...]\n";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Coalesced terminal stream text supporting `\r` progress-bar overwrites and line caps.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StreamBuffer {
    text: String,
    max_lines: usize,
    truncated_lines: usize,
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::with_cap(DEFAULT_MAX_STREAM_LINES)
    }
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
    pub fn push_chunk(&mut self, chunk: &str) {
        let existing = self
            .text
            .strip_prefix(TRUNCATION_MARKER)
            .unwrap_or(&self.text);
        let mut lines: Vec<Vec<char>> = vec![Vec::new()];
        let mut cursor = 0usize;
        for ch in existing.chars() {
            match ch {
                '\n' => {
                    lines.push(Vec::new());
                    cursor = 0;
                }
                '\r' => {
                    cursor = 0;
                }
                ch => {
                    let line = lines.last_mut().expect("stream always has one line");
                    if cursor < line.len() {
                        line[cursor] = ch;
                    } else {
                        line.push(ch);
                    }
                    cursor += 1;
                }
            }
        }

        let normalized = chunk.replace("\r\n", "\n");
        let mut pending_overwrite = existing.ends_with('\r');
        for ch in normalized.chars() {
            match ch {
                '\n' => {
                    lines.push(Vec::new());
                    cursor = 0;
                    pending_overwrite = false;
                }
                '\r' => {
                    cursor = 0;
                    pending_overwrite = true;
                }
                ch => {
                    let line = lines.last_mut().expect("stream always has one line");
                    if cursor < line.len() {
                        line[cursor] = ch;
                    } else {
                        line.push(ch);
                    }
                    cursor += 1;
                    pending_overwrite = false;
                }
            }
        }

        let mut logical = lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        if pending_overwrite {
            logical.push('\r');
        }

        let pending = logical.ends_with('\r');
        if pending {
            logical.pop();
        }
        let mut logical_lines: Vec<&str> = logical.split('\n').collect();
        if logical_lines.len() > self.max_lines {
            let dropped = logical_lines.len() - self.max_lines;
            self.truncated_lines = self.truncated_lines.saturating_add(dropped);
            logical_lines.drain(..dropped);
        }
        let mut result = logical_lines.join("\n");
        if self.truncated_lines > 0 {
            result.insert_str(0, TRUNCATION_MARKER);
        }
        if pending {
            result.push('\r');
        }
        self.text = result;
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
