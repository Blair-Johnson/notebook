//! Disk codecs for `.py` (Jupytext percent) and `.ipynb` (nbformat 4.5), plus atomic writes.

pub mod ipynb;
pub mod percent;

use crate::document::{Document, ExternalReloadOutcome};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid notebook JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid percent script: {0}")]
    Percent(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotebookFormat {
    PercentPy,
    Ipynb,
}

impl NotebookFormat {
    pub fn from_path(path: &Path) -> Self {
        if path.extension().is_some_and(|ext| ext == "py") {
            Self::PercentPy
        } else {
            Self::Ipynb
        }
    }
}

pub fn open_path(path: &Path) -> Result<Document, CoreError> {
    let raw = std::fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    match NotebookFormat::from_path(path) {
        NotebookFormat::PercentPy => percent::parse_percent(&raw),
        NotebookFormat::Ipynb => ipynb::parse_ipynb(&raw),
    }
}

pub fn save_path(doc: &Document, path: &Path) -> Result<(), CoreError> {
    let bytes = match NotebookFormat::from_path(path) {
        NotebookFormat::PercentPy => percent::serialize_percent(doc).into_bytes(),
        NotebookFormat::Ipynb => ipynb::serialize_ipynb(doc, false)?.into_bytes(),
    };
    write_atomic(path, &bytes)
}

/// Write `bytes` to a sibling `<name>.tmp`, `fsync`, and atomically rename onto `path`.
pub fn write_atomic(_path: &Path, _bytes: &[u8]) -> Result<(), CoreError> {
    todo!("wb-core::codec: sibling temp file write + sync_all + rename")
}

/// Reload an externally modified file while preserving outputs for matching cells.
pub fn reload_external(_doc: &mut Document, _path: &Path) -> Result<ExternalReloadOutcome, CoreError> {
    todo!("wb-core::codec: match cells by CellId (.ipynb) or content hash (.py) to retain outputs")
}
