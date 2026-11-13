//! State models for modals, prompts, logs, settings menu, and the docked `pdb` input bar.

use wb_core::TextBuffer;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    Search,
    ExCommand,
    SaveAs,
}

#[derive(Clone, Debug)]
pub struct PromptBar {
    pub kind: PromptKind,
    pub input: String,
    pub cursor_byte: usize,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    QuitUnsaved,
    ReloadExternalDiskChange,
    RestartLiveKernel { run_all_after: bool },
    RecoverAutosaveSidecar,
}

#[derive(Clone, Debug)]
pub struct CommandPaletteState {
    pub query: String,
    pub selected: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SettingsModalState {
    pub active_tab: usize,
    pub filter: String,
    pub selected_row: usize,
    pub capturing_keybind: bool,
}

#[derive(Clone, Debug)]
pub struct PdbDockState {
    pub prompt: String,
    pub password: bool,
    pub composer: TextBuffer,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub unsent_draft: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct OverlayStates {
    pub prompt: Option<PromptBar>,
    pub confirm: Option<ConfirmAction>,
    pub palette: Option<CommandPaletteState>,
    pub settings: SettingsModalState,
    pub pdb_dock: Option<PdbDockState>,
    pub log_scroll_from_bottom: usize,
}
