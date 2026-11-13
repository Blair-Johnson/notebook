//! Notebook cell model and granular revision counters.

use crate::buffer::TextBuffer;
use crate::id::CellId;
use crate::output::{OutputItem, StreamKind};
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
    /// Which stream the card's stdout/stderr tabs show (not saved).
    pub stream_tab: StreamKind,
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
            stream_tab: StreamKind::Stdout,
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

    /// How the cell's outputs are shown. Stored in the cell's nbformat
    /// metadata with Jupyter's own keys (`jupyter.outputs_hidden`,
    /// `scrolled`), so it survives a save and matches JupyterLab.
    pub fn output_display(&self) -> OutputDisplay {
        let Some(metadata) = self.extra.get("metadata").and_then(Value::as_object) else {
            return OutputDisplay::Preview;
        };
        let hidden = metadata
            .get("jupyter")
            .and_then(|jupyter| jupyter.get("outputs_hidden"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if hidden {
            OutputDisplay::Hidden
        } else if metadata.get("scrolled") == Some(&Value::Bool(false)) {
            OutputDisplay::Full
        } else {
            OutputDisplay::Preview
        }
    }

    pub fn set_output_display(&mut self, display: OutputDisplay) {
        let metadata = self
            .extra
            .entry("metadata")
            .or_insert_with(|| Value::Object(Map::new()));
        if !metadata.is_object() {
            *metadata = Value::Object(Map::new());
        }
        let metadata = metadata.as_object_mut().expect("metadata is an object");
        metadata.remove("scrolled");
        if let Some(jupyter) = metadata.get_mut("jupyter").and_then(Value::as_object_mut) {
            jupyter.remove("outputs_hidden");
            if jupyter.is_empty() {
                metadata.remove("jupyter");
            }
        }
        match display {
            OutputDisplay::Preview => {}
            OutputDisplay::Full => {
                metadata.insert("scrolled".into(), Value::Bool(false));
            }
            OutputDisplay::Hidden => {
                let jupyter = metadata
                    .entry("jupyter")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(jupyter) = jupyter.as_object_mut() {
                    jupyter.insert("outputs_hidden".into(), Value::Bool(true));
                }
            }
        }
        self.bump_chrome();
    }
}

/// How a cell's outputs are shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OutputDisplay {
    /// Each output capped at the configured row count (scroll by entering).
    #[default]
    Preview,
    /// Every row of every output.
    Full,
    /// Only a one-line summary.
    Hidden,
}

impl OutputDisplay {
    /// Preview → full → hidden → preview.
    pub fn next(self) -> Self {
        match self {
            Self::Preview => Self::Full,
            Self::Full => Self::Hidden,
            Self::Hidden => Self::Preview,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Full => "full",
            Self::Hidden => "hidden",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_display_round_trips_through_jupyter_metadata() {
        let mut cell = Cell::new_code();
        assert_eq!(cell.output_display(), OutputDisplay::Preview);
        for display in [OutputDisplay::Full, OutputDisplay::Hidden, OutputDisplay::Preview] {
            cell.set_output_display(display);
            assert_eq!(cell.output_display(), display);
        }
        cell.set_output_display(OutputDisplay::Hidden);
        assert_eq!(
            cell.extra["metadata"]["jupyter"]["outputs_hidden"],
            Value::Bool(true)
        );
        cell.set_output_display(OutputDisplay::Preview);
        assert_eq!(cell.extra["metadata"], Value::Object(Map::new()));
    }
}
