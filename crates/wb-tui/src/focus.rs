//! Explicit `FocusStack` replacing ad-hoc `Option`-field modal cascades.

use wb_core::{CellId, OutputId, VisualKind};
use wb_sdk::KeyContextId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellMode {
    Normal,
    Insert,
    Visual(VisualKind),
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
    NotebookNormal,
    CellEdit {
        cell_id: CellId,
        mode: CellMode,
        expanded_split: bool,
    },
    OutputFocus {
        cell_id: CellId,
        output_id: OutputId,
        context: KeyContextId,
    },
    PdbInput,
    Modal(ModalKind),
}

impl FocusLayer {
    pub fn key_context(&self) -> KeyContextId {
        match self {
            Self::NotebookNormal => KeyContextId::new_static("notebook.normal"),
            Self::CellEdit { mode: CellMode::Normal, .. } => KeyContextId::new_static("cell.normal"),
            Self::CellEdit { mode: CellMode::Insert, .. } => KeyContextId::new_static("cell.insert"),
            Self::CellEdit { mode: CellMode::Visual(_), .. } => KeyContextId::new_static("cell.visual"),
            Self::OutputFocus { context, .. } => context.clone(),
            Self::PdbInput => KeyContextId::new_static("pdb.input"),
            Self::Modal(ModalKind::Settings) => KeyContextId::new_static("modal.settings"),
            Self::Modal(_) => KeyContextId::new_static("modal.generic"),
        }
    }

    pub fn badge(&self) -> &'static str {
        match self {
            Self::NotebookNormal => "NOTEBOOK",
            Self::CellEdit { mode: CellMode::Normal, .. } => "NORMAL",
            Self::CellEdit { mode: CellMode::Insert, .. } => "INSERT",
            Self::CellEdit { mode: CellMode::Visual(_), .. } => "VISUAL",
            Self::OutputFocus { .. } => "OUTPUT",
            Self::PdbInput => "DEBUGGER",
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
        }
    }
}

#[derive(Clone, Debug)]
pub struct FocusStack {
    layers: Vec<FocusLayer>,
}

impl Default for FocusStack {
    fn default() -> Self {
        Self {
            layers: vec![FocusLayer::NotebookNormal],
        }
    }
}

impl FocusStack {
    pub fn top(&self) -> &FocusLayer {
        self.layers.last().expect("FocusStack always retains NotebookNormal root")
    }

    pub fn top_mut(&mut self) -> &mut FocusLayer {
        self.layers.last_mut().expect("FocusStack always retains NotebookNormal root")
    }

    /// The topmost non-modal layer: where keys go once modals close.
    pub fn base(&self) -> &FocusLayer {
        self.layers
            .iter()
            .rev()
            .find(|layer| !matches!(layer, FocusLayer::Modal(_)))
            .unwrap_or(&self.layers[0])
    }

    pub fn layers(&self) -> &[FocusLayer] {
        &self.layers
    }

    pub fn push(&mut self, layer: FocusLayer) {
        self.layers.push(layer);
    }

    /// Pop the top layer unless already at the `NotebookNormal` root.
    pub fn pop(&mut self) -> Option<FocusLayer> {
        if self.layers.len() > 1 {
            self.layers.pop()
        } else {
            None
        }
    }
}
