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

/// Per-cell monotonic revisions used by `nb-tui::LayoutCache` to avoid re-measuring
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
    /// The user chose `stream_tab`; output no longer switches it.
    pub stream_tab_pinned: bool,
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
            stream_tab_pinned: false,
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
    /// The view chosen for this cell, if any. Stored in the cell's nbformat
    /// metadata with Jupyter's own keys, so it survives a save and matches
    /// JupyterLab: `jupyter.outputs_hidden` (hidden), `scrolled: true`
    /// (preview), `scrolled: false` (full). `None` means "use the default".
    pub fn output_display_choice(&self) -> Option<OutputDisplay> {
        let metadata = self.extra.get("metadata").and_then(Value::as_object)?;
        let hidden = metadata
            .get("jupyter")
            .and_then(|jupyter| jupyter.get("outputs_hidden"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if hidden {
            return Some(OutputDisplay::Hidden);
        }
        match metadata.get("scrolled") {
            Some(Value::Bool(false)) => Some(OutputDisplay::Full),
            Some(Value::Bool(true)) => Some(OutputDisplay::Preview),
            _ => None,
        }
    }

    /// The view in effect: the cell's own choice, else `default`.
    pub fn output_display(&self, default: OutputDisplay) -> OutputDisplay {
        self.output_display_choice().unwrap_or(default)
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
            OutputDisplay::Preview => {
                metadata.insert("scrolled".into(), Value::Bool(true));
            }
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
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
    pub const NAMES: &'static [&'static str] = &["preview", "full", "hidden"];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "preview" => Some(Self::Preview),
            "full" => Some(Self::Full),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    /// What is actually shown: a preview of outputs that already fit within
    /// the preview rows is the full output.
    pub fn effective(self, clips: bool) -> Self {
        match self {
            Self::Preview if !clips => Self::Full,
            other => other,
        }
    }

    /// Preview → full → hidden → preview, skipping preview when it would
    /// look the same as full (`clips` is false).
    pub fn next(self, clips: bool) -> Self {
        match self.effective(clips) {
            Self::Preview => Self::Full,
            Self::Full => Self::Hidden,
            Self::Hidden if clips => Self::Preview,
            Self::Hidden => Self::Full,
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
        assert_eq!(cell.output_display_choice(), None);
        assert_eq!(cell.output_display(OutputDisplay::Full), OutputDisplay::Full);
        for display in [OutputDisplay::Full, OutputDisplay::Hidden, OutputDisplay::Preview] {
            cell.set_output_display(display);
            assert_eq!(cell.output_display(OutputDisplay::Hidden), display);
        }
        cell.set_output_display(OutputDisplay::Hidden);
        assert_eq!(
            cell.extra["metadata"]["jupyter"]["outputs_hidden"],
            Value::Bool(true)
        );
    }

    #[test]
    fn cycling_skips_preview_when_nothing_is_clipped() {
        use OutputDisplay::*;
        assert_eq!(Preview.next(true), Full);
        assert_eq!(Full.next(true), Hidden);
        assert_eq!(Hidden.next(true), Preview);
        assert_eq!(Preview.next(false), Hidden);
        assert_eq!(Hidden.next(false), Full);
        assert_eq!(Preview.effective(false), Full);
    }
}
