//! Opaque stable identifiers used across documents, kernel routing, and UI caches.
//!
//! Positional array indices must never be stored across asynchronous boundaries.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity of a notebook cell (serialized as an nbformat 4.5 cell `id`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct CellId(pub String);

impl CellId {
    /// Generate a fresh 8-hex-character nbformat-compatible cell identifier.
    pub fn new_random() -> Self {
        let raw = uuid::Uuid::new_v4().simple().to_string();
        Self(raw[..8].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Session-unique identity of a single output item inside a cell or log.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct OutputId(pub u64);

impl OutputId {
    /// Generate an output identity without mutable process-global state.
    pub fn next() -> Self {
        Self(uuid::Uuid::new_v4().as_u128() as u64)
    }
}

/// Jupyter message identifier (`header.msg_id`) for an execution request.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ExecId(pub String);

/// Jupyter `comm_id` identifying a bidirectional widget/plugin channel.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct CommId(pub String);

/// Abstract destination for kernel execution requests and routed outputs.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum TargetId {
    Cell(CellId),
    SessionLog,
    ConsoleEntry(u64),
    Silent(String),
}
