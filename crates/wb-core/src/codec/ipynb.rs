//! Lossless nbformat 4.5 reader/writer and autosave sidecar serializer.

use crate::codec::CoreError;
use crate::document::Document;

/// Parse an nbformat 4.x JSON string, upgrading pre-4.5 notebooks with deterministic cell IDs.
pub fn parse_ipynb(_json: &str) -> Result<Document, CoreError> {
    todo!("wb-core::codec::ipynb: parse nbformat JSON into typed Cell/OutputData while preserving extra maps")
}

/// Serialize `Document` as canonical 1-space-indented, sorted-key nbformat 4.5 JSON.
///
/// When `is_autosave_sidecar` is true, `PercentPy` header info is also embedded in
/// notebook metadata so crash recovery of a `.py` session restores both header and outputs.
pub fn serialize_ipynb(_doc: &Document, _is_autosave_sidecar: bool) -> Result<String, CoreError> {
    todo!("wb-core::codec::ipynb: strip transient fields, format with 1-space indent + trailing newline")
}
