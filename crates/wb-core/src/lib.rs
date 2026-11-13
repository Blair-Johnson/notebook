//! Pure domain model and file codecs for Workbench.
//!
//! Architectural invariant: `wb-core` must never depend on `tokio`, ZeroMQ,
//! `ratatui`, or `crossterm`. Every operation here is synchronous, deterministic,
//! and testable in isolation.

pub mod buffer;
pub mod cell;
pub mod codec;
pub mod document;
pub mod id;
pub mod ops;
pub mod output;

pub use buffer::{BufferSnapshot, CursorPos, Selection, TextBuffer, VisualKind};
pub use cell::{Cell, CellKind, CellRevisions};
pub use codec::{CoreError, NotebookFormat};
pub use document::{Document, ExternalReloadOutcome, FormatBacking};
pub use id::{CellId, CommId, ExecId, OutputId, TargetId};
pub use ops::{DocOp, UndoHistory};
pub use output::{MimeBundle, OutputData, OutputItem, StreamBuffer, StreamKind};
