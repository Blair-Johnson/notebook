//! Explicit `FocusStack` replacing ad-hoc `Option`-field modal cascades.
//!
//! The notebook, cell, and output layers each carry the id of their
//! configured mode (`controls::ModeSpec`); the mode decides the key context,
//! badge, and cursor. `CellMode` caches the mode's kind for editing code.

use crate::controls::{Controls, ModeKind};
use nb_core::{CellId, VisualKind};
use nb_sdk::KeyContextId;
use std::borrow::Cow;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellMode {
    Normal,
    Insert,
    Visual(VisualKind),
}

impl From<ModeKind> for CellMode {
    fn from(kind: ModeKind) -> Self {
        match kind {
            ModeKind::Normal => Self::Normal,
            ModeKind::Insert => Self::Insert,
            ModeKind::Visual => Self::Visual(VisualKind::Char),
            ModeKind::VisualLine => Self::Visual(VisualKind::Line),
        }
    }
}

impl CellMode {
    pub fn kind(self) -> ModeKind {
        match self {
            Self::Normal => ModeKind::Normal,
            Self::Insert => ModeKind::Insert,
            Self::Visual(VisualKind::Char) => ModeKind::Visual,
            Self::Visual(VisualKind::Line) => ModeKind::VisualLine,
        }
    }
}

/// Which half of a cell the notebook-level selection is on. Outputs are
/// selectable items in their own right, between their cell and the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CellPart {
    #[default]
    Source,
    Output,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModalKind {
    CommandPalette,
    SearchPrompt,
    ExCommandBar,
    ConfirmBar,
    Help,
    Settings,
    Logs,
    InspectPager,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FocusLayer {
    Notebook {
        mode_id: String,
    },
    CellEdit {
        cell_id: CellId,
        mode: CellMode,
        mode_id: String,
        expanded_split: bool,
    },
    /// A cell's outputs, entered as read-only text (normal or visual mode).
    OutputFocus {
        cell_id: CellId,
        mode: CellMode,
        mode_id: String,
    },
    PdbInput,
    Modal(ModalKind),
}

impl FocusLayer {
    /// The configured mode of a notebook, cell, or output layer.
    pub fn mode_id(&self) -> Option<&str> {
        match self {
            Self::Notebook { mode_id }
            | Self::CellEdit { mode_id, .. }
            | Self::OutputFocus { mode_id, .. } => Some(mode_id),
            Self::PdbInput | Self::Modal(_) => None,
        }
    }

    /// Every mode is a key context of the same id.
    pub fn key_context(&self) -> KeyContextId {
        match self {
            Self::PdbInput => KeyContextId::new_static("pdb.input"),
            Self::Modal(ModalKind::Settings) => KeyContextId::new_static("modal.settings"),
            Self::Modal(_) => KeyContextId::new_static("modal.generic"),
            layer => KeyContextId(Cow::Owned(layer.mode_id().unwrap_or_default().to_string())),
        }
    }

    pub fn badge(&self, controls: &Controls) -> String {
        if let Some(mode) = self.mode_id() {
            return controls
                .mode(mode)
                .map_or_else(|| mode.to_uppercase(), |spec| spec.badge.clone());
        }
        match self {
            Self::PdbInput => "INPUT",
            Self::Modal(kind) => match kind {
                ModalKind::CommandPalette => "COMMANDS",
                ModalKind::SearchPrompt => "SEARCH",
                ModalKind::ExCommandBar => "COMMAND",
                ModalKind::ConfirmBar => "CONFIRM",
                ModalKind::Help => "HELP",
                ModalKind::Settings => "SETTINGS",
                ModalKind::Logs => "LOGS",
                ModalKind::InspectPager => "DOCS",
            },
            _ => "",
        }
        .to_string()
    }
}

#[derive(Clone, Debug)]
pub struct FocusStack {
    layers: Vec<FocusLayer>,
}

impl Default for FocusStack {
    fn default() -> Self {
        Self {
            layers: vec![FocusLayer::Notebook {
                mode_id: "notebook.normal".into(),
            }],
        }
    }
}

impl FocusStack {
    pub fn top(&self) -> &FocusLayer {
        self.layers.last().expect("FocusStack always retains the notebook root")
    }

    pub fn top_mut(&mut self) -> &mut FocusLayer {
        self.layers.last_mut().expect("FocusStack always retains the notebook root")
    }

    /// The topmost non-modal layer: where keys go once modals close.
    pub fn base(&self) -> &FocusLayer {
        self.layers
            .iter()
            .rev()
            .find(|layer| !matches!(layer, FocusLayer::Modal(_)))
            .unwrap_or(&self.layers[0])
    }

    /// The notebook layer at the bottom of the stack.
    pub fn root_mut(&mut self) -> &mut FocusLayer {
        &mut self.layers[0]
    }

    pub fn layers(&self) -> &[FocusLayer] {
        &self.layers
    }

    pub fn push(&mut self, layer: FocusLayer) {
        self.layers.push(layer);
    }

    /// Pop the top layer unless already at the notebook root.
    pub fn pop(&mut self) -> Option<FocusLayer> {
        if self.layers.len() > 1 {
            self.layers.pop()
        } else {
            None
        }
    }
}
