//! Disk codecs for `.py` (Jupytext percent) and `.ipynb` (nbformat 4.5), plus atomic writes.

pub mod ipynb;
pub mod percent;

use crate::cell::Cell;
use crate::codec::ipynb::parse_ipynb;
use crate::codec::percent::parse_percent;
use crate::document::{Document, ExternalReloadOutcome};
use std::io::Write;
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
        NotebookFormat::PercentPy => parse_percent(&raw),
        NotebookFormat::Ipynb => parse_ipynb(&raw),
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
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let Some(file_name) = path.file_name() else {
        return Err(CoreError::Io {
            path: path.display().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name"),
        });
    };
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    let temp_path = path.with_file_name(temp_name);
    let mut file = std::fs::File::create(&temp_path).map_err(|source| CoreError::Io {
        path: temp_path.display().to_string(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| CoreError::Io {
        path: temp_path.display().to_string(),
        source,
    })?;
    file.sync_all().map_err(|source| CoreError::Io {
        path: temp_path.display().to_string(),
        source,
    })?;
    drop(file);
    std::fs::rename(&temp_path, path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Reload an externally modified file while preserving outputs for matching cells.
pub fn reload_external(
    doc: &mut Document,
    path: &Path,
) -> Result<ExternalReloadOutcome, CoreError> {
    let loaded = open_path(path)?;
    if cells_semantically_equal(doc, &loaded) {
        return Ok(ExternalReloadOutcome::Unchanged);
    }

    let is_percent = NotebookFormat::from_path(path) == NotebookFormat::PercentPy;
    let old_cells: Vec<Cell> = doc.cells_in_order().cloned().collect();
    let new_cells: Vec<Cell> = loaded.cells_in_order().cloned().collect();
    let mut used_old = vec![false; old_cells.len()];
    let mut matched_cells = 0usize;
    let mut replacement = Vec::with_capacity(new_cells.len());

    for mut new_cell in new_cells {
        let match_index = if is_percent {
            old_cells.iter().enumerate().find_map(|(index, old)| {
                (!used_old[index]
                    && old.kind == new_cell.kind
                    && old.buffer.as_str() == new_cell.buffer.as_str())
                .then_some(index)
            })
        } else {
            old_cells.iter().enumerate().find_map(|(index, old)| {
                (!used_old[index]
                    && old.id == new_cell.id
                    && old.kind == new_cell.kind
                    && old.buffer.as_str() == new_cell.buffer.as_str())
                .then_some(index)
            })
        };
        if let Some(index) = match_index {
            used_old[index] = true;
            let old = &old_cells[index];
            new_cell.id = old.id.clone();
            new_cell.outputs = old.outputs.clone();
            new_cell.execution_count = old.execution_count;
            new_cell.last_elapsed = old.last_elapsed;
            matched_cells += 1;
        }
        replacement.push(new_cell);
    }

    let backing = loaded.backing.clone();
    let mut next = Document::from_cells(replacement, backing);
    next.session_log = loaded.session_log;
    next.dirty_source = false;
    next.dirty_outputs = false;
    doc.replace_state_from(next);
    doc.history.clear();
    Ok(ExternalReloadOutcome::ReloadedPreservingOutputs { matched_cells })
}

fn cells_semantically_equal(left: &Document, right: &Document) -> bool {
    let left_cells: Vec<&Cell> = left.cells_in_order().collect();
    let right_cells: Vec<&Cell> = right.cells_in_order().collect();
    left_cells.len() == right_cells.len()
        && left_cells
            .iter()
            .zip(right_cells.iter())
            .all(|(a, b)| a.kind == b.kind && a.buffer.as_str() == b.buffer.as_str())
}
