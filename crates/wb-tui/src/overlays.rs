//! State models for modals, prompts, logs, settings menu, and the docked `pdb` input bar.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wb_core::TextBuffer;
use wb_sdk::{ActionSpec, ExtensionRegistry, KeyContextId, SettingKind, SettingSpec, SettingValue};

use crate::config_store::SettingsStore;
use crate::keymap::{Keymap, format_key, normalize_crossterm_key};

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

impl PromptBar {
    pub fn new(kind: PromptKind, initial: impl Into<String>) -> Self {
        let input = first_line(&initial.into()).to_string();
        let cursor_byte = input.len();
        Self {
            kind,
            input,
            cursor_byte,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' || c == '\r' {
            return;
        }
        self.normalize_cursor();
        self.input.insert(self.cursor_byte, c);
        self.cursor_byte += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let first = first_line(s);
        for c in first.chars() {
            self.insert_char(c);
        }
    }

    pub fn backspace(&mut self) {
        self.normalize_cursor();
        if self.cursor_byte == 0 {
            return;
        }
        let start = self.input[..self.cursor_byte]
            .char_indices()
            .last()
            .map_or(0, |(index, _)| index);
        self.input.drain(start..self.cursor_byte);
        self.cursor_byte = start;
    }

    pub fn delete_forward(&mut self) {
        self.normalize_cursor();
        let Some(width) = self.input[self.cursor_byte..]
            .chars()
            .next()
            .map(char::len_utf8)
        else {
            return;
        };
        let next = self.cursor_byte + width;
        self.input.drain(self.cursor_byte..next);
    }

    pub fn move_left(&mut self) {
        self.normalize_cursor();
        if self.cursor_byte > 0 {
            self.cursor_byte = self.input[..self.cursor_byte]
                .char_indices()
                .last()
                .map_or(0, |(index, _)| index);
        }
    }

    pub fn move_right(&mut self) {
        self.normalize_cursor();
        if let Some(width) = self.input[self.cursor_byte..]
            .chars()
            .next()
            .map(char::len_utf8)
        {
            self.cursor_byte += width;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_byte = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_byte = self.input.len();
    }

    fn normalize_cursor(&mut self) {
        if self.cursor_byte > self.input.len() {
            self.cursor_byte = self.input.len();
        }
        while self.cursor_byte > 0 && !self.input.is_char_boundary(self.cursor_byte) {
            self.cursor_byte -= 1;
        }
    }
}

fn first_line(input: &str) -> &str {
    input.split(['\n', '\r']).next().unwrap_or("")
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

#[derive(Clone, Debug)]
pub struct PaletteEntry<'a> {
    pub action: &'a ActionSpec,
    pub spec: &'a ActionSpec,
    pub id: &'a str,
    pub title: &'a str,
    pub category: &'a str,
    pub keys: String,
    pub bound_keys: String,
    pub score: usize,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
        }
    }

    pub fn query_matches<'a>(
        &self,
        reg: &'a ExtensionRegistry,
        keymap: &Keymap,
        active_ctx: &KeyContextId,
    ) -> Vec<PaletteEntry<'a>> {
        let mut keys_by_action = HashMap::<(String, String), String>::new();
        for row in keymap.help_for(active_ctx) {
            keys_by_action.insert((row.title, row.category), row.keys);
        }

        let mut entries = reg
            .actions
            .iter()
            .filter_map(|action| {
                let keys = keys_by_action
                    .get(&(action.title.to_string(), action.category.to_string()))
                    .cloned()
                    .unwrap_or_default();
                let score = score_action(action, &keys, &self.query)?;
                Some(PaletteEntry {
                    action,
                    spec: action,
                    id: action.id.as_str(),
                    title: action.title.as_ref(),
                    category: action.category.as_ref(),
                    keys: keys.clone(),
                    bound_keys: keys,
                    score,
                })
            })
            .collect::<Vec<_>>();

        entries.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then(left.action.order.cmp(&right.action.order))
                .then(left.action.category.cmp(&right.action.category))
                .then(left.action.title.cmp(&right.action.title))
                .then(left.action.id.cmp(&right.action.id))
        });
        entries
    }

    pub fn select_next(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub fn select_prev(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = count - 1;
        } else {
            self.selected -= 1;
        }
    }
}

fn score_action(action: &ActionSpec, keys: &str, query: &str) -> Option<usize> {
    if query.trim().is_empty() {
        return Some(0);
    }
    [
        action.title.as_ref(),
        action.category.as_ref(),
        action.id.as_str(),
        keys,
    ]
    .into_iter()
    .filter_map(|field| score_field(field, query))
    .max()
}

fn score_field(field: &str, query: &str) -> Option<usize> {
    let haystack = field.to_lowercase();
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(position) = haystack.find(&needle) {
        return Some(20_000 + needle.chars().count() * 100 - position);
    }

    let query_chars = needle.chars().collect::<Vec<_>>();
    let mut query_index = 0;
    let mut first = None;
    let mut last = 0;
    for (index, c) in haystack.chars().enumerate() {
        if query_index < query_chars.len() && c == query_chars[query_index] {
            first.get_or_insert(index);
            last = index;
            query_index += 1;
        }
    }
    (query_index == query_chars.len())
        .then(|| 10_000 + query_chars.len() * 50 + first.unwrap_or(0).saturating_sub(last))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsModalOutcome {
    Noop,
    Changed,
    Close,
    FilterChanged,
    CaptureStarted,
    CaptureCancelled,
    KeybindCaptured { action_id: String, key: String },
}

#[derive(Clone, Debug, Default)]
pub struct SettingsModalState {
    pub active_tab: usize,
    pub filter: String,
    pub selected_row: usize,
    pub capturing_keybind: bool,
}

impl SettingsModalState {
    pub fn tabs() -> &'static [&'static str] {
        &[
            "Display", "Editor", "Kernel", "Theme", "Keybinds", "Plugins",
        ]
    }

    pub fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % Self::tabs().len();
        self.selected_row = 0;
        self.capturing_keybind = false;
    }

    pub fn prev_tab(&mut self) {
        self.active_tab = if self.active_tab == 0 {
            Self::tabs().len() - 1
        } else {
            self.active_tab - 1
        };
        self.selected_row = 0;
        self.capturing_keybind = false;
    }

    pub fn filtered_settings<'a>(&self, reg: &'a ExtensionRegistry) -> Vec<&'a SettingSpec> {
        reg.settings
            .iter()
            .filter(|spec| setting_in_tab(spec, self.active_tab))
            .filter(|spec| {
                filter_matches(
                    &self.filter,
                    [
                        spec.id.0.as_ref(),
                        spec.title.as_ref(),
                        spec.description.as_ref(),
                    ],
                )
            })
            .collect()
    }

    pub fn filtered_actions<'a>(&self, reg: &'a ExtensionRegistry) -> Vec<&'a ActionSpec> {
        if self.active_tab != 4 {
            return Vec::new();
        }
        reg.actions
            .iter()
            .filter(|action| {
                filter_matches(
                    &self.filter,
                    [
                        action.id.as_str(),
                        action.title.as_ref(),
                        action.category.as_ref(),
                    ],
                )
            })
            .collect()
    }

    /// Handle the emergency, non-configurable settings keys.  This path is
    /// deliberately independent of the user keymap so a broken keybind cannot
    /// make `/config` impossible to navigate or repair.
    pub fn handle_raw_key(
        &mut self,
        key: KeyEvent,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
        if self.capturing_keybind {
            if key.code == KeyCode::Esc {
                self.capturing_keybind = false;
                return SettingsModalOutcome::CaptureCancelled;
            }
            let Some(normalized) = normalize_crossterm_key(key) else {
                return SettingsModalOutcome::Noop;
            };
            let Some(action_id) = self
                .filtered_actions(reg)
                .get(self.selected_row)
                .map(|action| action.id.clone())
            else {
                self.capturing_keybind = false;
                return SettingsModalOutcome::CaptureCancelled;
            };
            let key_string = format_key(normalized);
            let context = KeyContextId::new_static("global");
            store.set_keybinding(&context, &action_id, [key_string.as_str()], reg);
            self.capturing_keybind = false;
            return SettingsModalOutcome::KeybindCaptured {
                action_id,
                key: key_string,
            };
        }

        if key.code == KeyCode::Esc {
            if !self.filter.is_empty() {
                self.filter.clear();
                self.selected_row = 0;
                return SettingsModalOutcome::FilterChanged;
            }
            return SettingsModalOutcome::Close;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return SettingsModalOutcome::Close;
        }

        match key.code {
            KeyCode::Tab => {
                self.next_tab();
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::BackTab => {
                self.prev_tab();
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Up => {
                self.select_prev(reg, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::Down => {
                self.select_next(reg, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageUp => {
                self.select_prev(reg, 5);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageDown => {
                self.select_next(reg, 5);
                SettingsModalOutcome::Noop
            }
            KeyCode::Home => {
                self.selected_row = 0;
                SettingsModalOutcome::Noop
            }
            KeyCode::End => {
                self.selected_row = self.row_count(reg).saturating_sub(1);
                SettingsModalOutcome::Noop
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.selected_row = 0;
                    SettingsModalOutcome::FilterChanged
                } else {
                    SettingsModalOutcome::Noop
                }
            }
            KeyCode::Char('/') if plain_key(key) => {
                self.filter.clear();
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Left => self.change_selected_setting(-1, store, reg),
            KeyCode::Right => self.change_selected_setting(1, store, reg),
            KeyCode::Enter => {
                if self.active_tab == 4 {
                    if self.filtered_actions(reg).get(self.selected_row).is_some() {
                        self.capturing_keybind = true;
                        SettingsModalOutcome::CaptureStarted
                    } else {
                        SettingsModalOutcome::Noop
                    }
                } else {
                    self.change_selected_setting(1, store, reg)
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') if plain_key(key) => {
                self.change_selected_setting(1, store, reg)
            }
            KeyCode::Char('-') if plain_key(key) => self.change_selected_setting(-1, store, reg),
            KeyCode::Char(' ') if plain_key(key) => self.change_selected_setting(0, store, reg),
            KeyCode::Char(c) if plain_key(key) && !c.is_control() => {
                self.filter.push(c);
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            _ => SettingsModalOutcome::Noop,
        }
    }

    fn row_count(&self, reg: &ExtensionRegistry) -> usize {
        if self.active_tab == 4 {
            self.filtered_actions(reg).len()
        } else {
            self.filtered_settings(reg).len()
        }
    }

    fn select_next(&mut self, reg: &ExtensionRegistry, by: usize) {
        let count = self.row_count(reg);
        if count == 0 {
            self.selected_row = 0;
        } else {
            self.selected_row = (self.selected_row + by) % count;
        }
    }

    fn select_prev(&mut self, reg: &ExtensionRegistry, by: usize) {
        let count = self.row_count(reg);
        if count == 0 {
            self.selected_row = 0;
        } else {
            self.selected_row = (self.selected_row + count - (by % count)) % count;
        }
    }

    fn change_selected_setting(
        &mut self,
        direction: i8,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
        if self.active_tab == 4 {
            return SettingsModalOutcome::Noop;
        }
        let Some(spec) = self.filtered_settings(reg).get(self.selected_row).cloned() else {
            return SettingsModalOutcome::Noop;
        };
        let current = store
            .snapshot()
            .plugin_values
            .get(&spec.id)
            .cloned()
            .unwrap_or_else(|| setting_default(&spec.kind));
        let Some(next) = change_setting(&spec.kind, current, direction) else {
            return SettingsModalOutcome::Noop;
        };
        store.set_value(&spec.id, next, reg);
        SettingsModalOutcome::Changed
    }
}

fn plain_key(event: KeyEvent) -> bool {
    !event.modifiers.intersects(
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META,
    )
}

fn setting_in_tab(spec: &SettingSpec, tab: usize) -> bool {
    let section = spec.section.as_ref();
    let id = spec.id.0.as_ref();
    match tab {
        0 => section == "display" && id != "display.theme" && id != "display.syntect_theme",
        1 => section == "editor",
        2 => section == "kernel",
        3 => id == "display.theme" || id == "display.syntect_theme",
        5 => section.starts_with("plugin") || section.starts_with("extension"),
        _ => false,
    }
}

fn filter_matches<'a, I>(filter: &str, fields: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let query = filter.trim().to_lowercase();
    query.is_empty()
        || fields
            .into_iter()
            .any(|field| field.to_lowercase().contains(&query))
}

fn setting_default(kind: &SettingKind) -> SettingValue {
    match kind {
        SettingKind::Bool { default } => SettingValue::Bool(*default),
        SettingKind::Int { default, .. } => SettingValue::Int(*default),
        SettingKind::Choice { default, .. } => SettingValue::Text((*default).into()),
        SettingKind::Text { default } => SettingValue::Text((*default).into()),
    }
}

fn change_setting(
    kind: &SettingKind,
    current: SettingValue,
    direction: i8,
) -> Option<SettingValue> {
    match kind {
        SettingKind::Bool { .. } => match current {
            SettingValue::Bool(value) => Some(SettingValue::Bool(!value)),
            _ => None,
        },
        SettingKind::Int { min, max, .. } => match current {
            SettingValue::Int(value) => Some(SettingValue::Int(
                value.saturating_add(i64::from(direction)).clamp(*min, *max),
            )),
            _ => None,
        },
        SettingKind::Choice { options, .. } => {
            if options.is_empty() {
                return None;
            }
            let current_index = match current {
                SettingValue::Text(value) => options.iter().position(|option| *option == value),
                _ => None,
            }
            .unwrap_or(0);
            let next = if direction < 0 {
                if current_index == 0 {
                    options.len() - 1
                } else {
                    current_index - 1
                }
            } else {
                (current_index + 1) % options.len()
            };
            Some(SettingValue::Text(options[next].to_string()))
        }
        SettingKind::Text { .. } => None,
    }
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

impl PdbDockState {
    pub fn new(prompt: impl Into<String>, password: bool, history: Vec<String>) -> Self {
        Self {
            prompt: prompt.into(),
            password,
            composer: TextBuffer::new(""),
            history,
            history_index: None,
            unsent_draft: None,
        }
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.unsent_draft = Some(self.composer.as_str().to_string());
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.composer.replace_all(self.history[index].clone());
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.composer.replace_all(self.history[next].clone());
        } else {
            self.history_index = None;
            self.composer
                .replace_all(self.unsent_draft.take().unwrap_or_default());
        }
    }

    pub fn take_submission(&mut self) -> (String, Vec<String>) {
        let submission = self.composer.as_str().to_string();
        if !self.password {
            let mut compact = Vec::with_capacity(self.history.len() + 1);
            for command in self.history.drain(..) {
                if command.trim().is_empty() {
                    continue;
                }
                if compact.last() != Some(&command) {
                    compact.push(command);
                }
            }
            if !submission.trim().is_empty() && compact.last() != Some(&submission) {
                compact.push(submission.clone());
            }
            self.history = compact;
        }
        self.history_index = None;
        self.unsent_draft = None;
        self.composer.replace_all("");
        (submission, self.history.clone())
    }
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
