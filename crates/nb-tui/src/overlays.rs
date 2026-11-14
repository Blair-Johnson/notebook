//! State models for modals, prompts, logs, settings menu, and the docked `pdb` input bar.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::TextBuffer;
use nb_sdk::{
    ActionSpec, ExtensionRegistry, Key, KeyContextId, SettingKind, SettingSpec, SettingValue,
};

use crate::config_store::SettingsStore;
use crate::controls::{Case, ModeSpec};
use crate::keymap::{Keymap, format_key_sequence_toml, normalize_crossterm_key};

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
    ReloadExternalDiskChange,
    RestartLiveKernel { run_all_after: bool },
    RecoverAutosaveSidecar,
}

#[derive(Clone, Debug)]
pub struct CommandPaletteState {
    pub query: String,
    pub selected: usize,
    /// First visible entry, moved by drawing (see `scroll::follow`).
    pub top: Cell<usize>,
}

/// What choosing a palette entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteKind {
    /// Run the action.
    Command,
    /// Open the settings screen on this setting.
    Setting,
}

#[derive(Clone, Debug)]
pub struct PaletteEntry<'a> {
    pub kind: PaletteKind,
    pub id: &'a str,
    pub title: &'a str,
    pub category: String,
    pub keys: String,
    pub bound_keys: String,
    pub score: usize,
    order: u32,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            top: Cell::new(0),
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
                    kind: PaletteKind::Command,
                    id: action.id.as_str(),
                    title: action.title.as_ref(),
                    category: action.category.to_string(),
                    keys: keys.clone(),
                    bound_keys: keys,
                    score,
                    order: u32::from(action.order),
                })
            })
            .collect::<Vec<_>>();
        // Settings are searchable here too; choosing one opens it in the
        // settings screen. Listed after commands when nothing is typed.
        entries.extend(reg.settings.iter().enumerate().filter_map(|(index, spec)| {
            let fields = [
                spec.title.as_ref(),
                spec.section.as_ref(),
                spec.id.0.as_ref(),
                spec.description.as_ref(),
                "setting",
            ];
            let score = if self.query.trim().is_empty() {
                0
            } else {
                fields
                    .into_iter()
                    .filter_map(|field| score_field(field, &self.query))
                    .max()?
            };
            Some(PaletteEntry {
                kind: PaletteKind::Setting,
                id: spec.id.0.as_ref(),
                title: spec.title.as_ref(),
                category: format!("Setting · {}", spec.section),
                keys: String::new(),
                bound_keys: String::new(),
                score,
                order: 100_000 + index as u32,
            })
        }));

        entries.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then(left.order.cmp(&right.order))
                .then(left.category.cmp(&right.category))
                .then(left.title.cmp(&right.title))
                .then(left.id.cmp(&right.id))
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
    /// Show what these keys do in this mode (the Controls tab's "try keys").
    ExplainRequested { mode: String, keys: Vec<Key> },
    /// Open the config file in an editor at this rule.
    EditRequested(EditTarget),
    Notice(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditTarget {
    Rule(String),
    /// A new rule for `action` in `mode`, starting from what it does now.
    NewRule { mode: String, action: String },
}

/// What a key capture in the Controls tab is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePurpose {
    Rebind,
    AddKey,
    Try,
}

#[derive(Clone, Debug)]
pub struct KeyCapture {
    pub purpose: CapturePurpose,
    pub keys: Vec<Key>,
    pub last: Instant,
}

/// A captured key sequence is saved this long after its last key.
pub const CAPTURE_PAUSE: Duration = Duration::from_millis(1000);

/// One action in the Controls tab, for the selected mode.
#[derive(Clone, Debug)]
pub struct ControlRow {
    pub action: String,
    pub title: String,
    pub category: String,
    /// Keys in this mode, inherited ones included, as config files spell them.
    pub keys: Vec<String>,
    pub cases: Vec<Case>,
    pub cannot_unbind: bool,
}

/// The Controls tab's index among `SettingsModalState::tabs()`.
pub const CONTROLS_TAB: usize = 4;

#[derive(Clone, Debug, Default)]
pub struct SettingsModalState {
    pub active_tab: usize,
    pub filter: String,
    pub selected_row: usize,
    /// Controls tab: typed characters edit the filter (after `/`).
    pub filtering: bool,
    /// Controls tab: the mode shown, an index into `Controls::modes`.
    pub mode_index: usize,
    pub capture: Option<KeyCapture>,
    /// Controls tab: the rules panel for the selected action is open, on
    /// this rule.
    pub rules_view: Option<usize>,
    /// The last "try keys" trace.
    pub explain: Vec<String>,
    /// First visible row of the settings or Controls list, and of the rules
    /// panel. Drawing moves them so the selection keeps a margin of context
    /// (`display.menu_scrolloff_pct`); they are remembered view positions,
    /// like the notebook's scroll row.
    pub list_top: Cell<usize>,
    pub rules_top: Cell<usize>,
}

impl SettingsModalState {
    pub fn tabs() -> &'static [&'static str] {
        &[
            "Display", "Editor", "Kernel", "Theme", "Controls", "Plugins",
        ]
    }

    /// Show `spec` in the screen: its tab, filtered down to it.
    pub fn focus_setting(&mut self, spec: &SettingSpec) {
        self.active_tab = (0..Self::tabs().len())
            .find(|tab| setting_in_tab(spec, *tab))
            .unwrap_or(0);
        self.filter = spec.title.to_string();
        self.selected_row = 0;
        self.reset_controls_state();
    }

    /// Open the Controls tab on `mode` (the mode the user was in).
    pub fn show_mode(&mut self, keymap: &Keymap, mode: &str) {
        if let Some(index) = keymap.controls.modes().position(|spec| spec.id == mode) {
            if index != self.mode_index {
                self.mode_index = index;
                self.selected_row = 0;
            }
        }
    }

    fn reset_controls_state(&mut self) {
        self.capture = None;
        self.rules_view = None;
        self.filtering = false;
        self.explain.clear();
    }

    pub fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % Self::tabs().len();
        self.selected_row = 0;
        self.reset_controls_state();
    }

    pub fn prev_tab(&mut self) {
        self.active_tab = if self.active_tab == 0 {
            Self::tabs().len() - 1
        } else {
            self.active_tab - 1
        };
        self.selected_row = 0;
        self.reset_controls_state();
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

    /// The mode the Controls tab shows.
    pub fn controls_mode<'a>(&self, keymap: &'a Keymap) -> Option<&'a ModeSpec> {
        let count = keymap.controls.modes().count();
        keymap
            .controls
            .modes()
            .nth(self.mode_index.min(count.saturating_sub(1)))
    }

    /// The actions bound in the shown mode, and with a filter, every
    /// matching action (bound or not), in help order.
    pub fn control_rows(&self, keymap: &Keymap) -> Vec<ControlRow> {
        let Some(mode) = self.controls_mode(keymap) else {
            return Vec::new();
        };
        let ctx = KeyContextId(Cow::Owned(mode.id.clone()));
        let mut ids: BTreeSet<String> = keymap.bound_actions(&ctx).into_iter().collect();
        if !self.filter.trim().is_empty() {
            ids.extend(keymap.action_specs().map(|spec| spec.id.clone()));
        }
        let mut rows: Vec<(u16, ControlRow)> = ids
            .into_iter()
            .filter_map(|id| {
                let spec = keymap.action_spec(&id)?;
                // Text objects only follow an operator.
                if spec.kind == nb_sdk::ActionKind::TextObject {
                    return None;
                }
                let keys: Vec<String> = keymap
                    .sequences_for(&ctx, &id)
                    .iter()
                    .map(|sequence| format_key_sequence_toml(sequence))
                    .collect();
                let cases = keymap.controls.cases(&mode.id, &id, false);
                let what = cases.iter().map(|case| case.what.as_str()).collect::<Vec<_>>().join(" ");
                let keys_text = keys.join(" ");
                filter_matches(
                    &self.filter,
                    [id.as_str(), spec.title.as_ref(), spec.category.as_ref(), keys_text.as_str(), what.as_str()],
                )
                .then(|| {
                    (
                        spec.order,
                        ControlRow {
                            action: id.clone(),
                            title: spec.title.to_string(),
                            category: spec.category.to_string(),
                            keys,
                            cases,
                            cannot_unbind: spec.cannot_unbind,
                        },
                    )
                })
            })
            .collect();
        rows.sort_by(|(left_order, left), (right_order, right)| {
            left.keys
                .is_empty()
                .cmp(&right.keys.is_empty())
                .then(left.category.cmp(&right.category))
                .then(left_order.cmp(right_order))
                .then(left.title.cmp(&right.title))
        });
        rows.into_iter().map(|(_, row)| row).collect()
    }

    /// Handle the emergency, non-configurable settings keys.  This path is
    /// deliberately independent of the user keymap so a broken keybind cannot
    /// make the settings screen impossible to navigate or repair.
    pub fn handle_raw_key(
        &mut self,
        key: KeyEvent,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(capture) = self.capture.as_mut() {
            if ctrl_c {
                self.capture = None;
                return SettingsModalOutcome::CaptureCancelled;
            }
            let Some(normalized) = normalize_crossterm_key(key) else {
                return SettingsModalOutcome::Noop;
            };
            // The first key is always literal, so Enter and Esc can be bound.
            if normalized == Key::Enter && !capture.keys.is_empty() {
                return self.finish_capture(store, reg);
            }
            capture.keys.push(normalized);
            capture.last = Instant::now();
            return SettingsModalOutcome::Noop;
        }
        if ctrl_c {
            return SettingsModalOutcome::Close;
        }
        if self.active_tab == CONTROLS_TAB {
            return self.handle_controls_key(key, store, reg);
        }

        if key.code == KeyCode::Esc {
            if !self.filter.is_empty() {
                self.filter.clear();
                self.selected_row = 0;
                return SettingsModalOutcome::FilterChanged;
            }
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
                self.select_prev(store, reg, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::Down => {
                self.select_next(store, reg, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageUp => {
                self.select_prev(store, reg, 5);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageDown => {
                self.select_next(store, reg, 5);
                SettingsModalOutcome::Noop
            }
            KeyCode::Home => {
                self.selected_row = 0;
                SettingsModalOutcome::Noop
            }
            KeyCode::End => {
                self.selected_row = self.row_count(store, reg).saturating_sub(1);
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
            KeyCode::Enter => self.change_selected_setting(1, store, reg),
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

    /// Save a capture whose last key was `CAPTURE_PAUSE` ago.
    pub fn on_tick(
        &mut self,
        now: Instant,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
        let due = self.capture.as_ref().is_some_and(|capture| {
            !capture.keys.is_empty()
                && now
                    .checked_duration_since(capture.last)
                    .is_some_and(|elapsed| elapsed >= CAPTURE_PAUSE)
        });
        if due {
            self.finish_capture(store, reg)
        } else {
            SettingsModalOutcome::Noop
        }
    }

    fn start_capture(&mut self, purpose: CapturePurpose) -> SettingsModalOutcome {
        self.capture = Some(KeyCapture {
            purpose,
            keys: Vec::new(),
            last: Instant::now(),
        });
        SettingsModalOutcome::CaptureStarted
    }

    fn finish_capture(&mut self, store: &mut SettingsStore, reg: &ExtensionRegistry) -> SettingsModalOutcome {
        let Some(capture) = self.capture.take() else {
            return SettingsModalOutcome::Noop;
        };
        if capture.keys.is_empty() {
            return SettingsModalOutcome::CaptureCancelled;
        }
        let snap = store.snapshot().clone();
        let keymap = &snap.keymap;
        let Some(mode) = self.controls_mode(keymap).map(|mode| mode.id.clone()) else {
            return SettingsModalOutcome::CaptureCancelled;
        };
        if capture.purpose == CapturePurpose::Try {
            return SettingsModalOutcome::ExplainRequested {
                mode,
                keys: capture.keys,
            };
        }
        let Some(row) = self.control_rows(keymap).into_iter().nth(self.selected_row) else {
            return SettingsModalOutcome::CaptureCancelled;
        };
        let sequence = format_key_sequence_toml(&capture.keys);
        let mut keys = if capture.purpose == CapturePurpose::AddKey {
            row.keys.clone()
        } else {
            Vec::new()
        };
        if !keys.contains(&sequence) {
            keys.push(sequence.clone());
        }
        let ctx = KeyContextId(Cow::Owned(mode));
        store.set_keybinding(&ctx, &row.action, &keys, reg);
        SettingsModalOutcome::KeybindCaptured {
            action_id: row.action,
            key: sequence,
        }
    }

    fn handle_controls_key(
        &mut self,
        key: KeyEvent,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
        let snap = store.snapshot().clone();
        let keymap = &snap.keymap;
        let Some(mode) = self.controls_mode(keymap).map(|mode| mode.id.clone()) else {
            return SettingsModalOutcome::Close;
        };
        let rows = self.control_rows(keymap);
        let row = rows.get(self.selected_row).cloned();
        let ctx = KeyContextId(Cow::Owned(mode.clone()));

        if let Some(selected) = self.rules_view {
            let Some(row) = row else {
                self.rules_view = None;
                return SettingsModalOutcome::Noop;
            };
            let rules: Vec<(String, bool)> = keymap
                .controls
                .rules_for(&mode, &row.action)
                .into_iter()
                .map(|rule| (rule.id.clone(), rule.enabled))
                .collect();
            return match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.rules_view = Some(selected.saturating_sub(1));
                    SettingsModalOutcome::Noop
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.rules_view = Some((selected + 1).min(rules.len().saturating_sub(1)));
                    SettingsModalOutcome::Noop
                }
                KeyCode::Char(' ') => match rules.get(selected) {
                    Some((id, enabled)) => {
                        store.set_rule_enabled(id, !enabled, reg);
                        SettingsModalOutcome::Changed
                    }
                    None => SettingsModalOutcome::Noop,
                },
                KeyCode::Char('e') | KeyCode::Enter => match rules.get(selected) {
                    Some((id, _)) => SettingsModalOutcome::EditRequested(EditTarget::Rule(id.clone())),
                    None => SettingsModalOutcome::Noop,
                },
                KeyCode::Char('n') => SettingsModalOutcome::EditRequested(EditTarget::NewRule {
                    mode,
                    action: row.action,
                }),
                KeyCode::Esc | KeyCode::Char('r') | KeyCode::Left | KeyCode::Char('h') => {
                    self.rules_view = None;
                    SettingsModalOutcome::Noop
                }
                _ => SettingsModalOutcome::Noop,
            };
        }

        if self.filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.filtering = false;
                    return SettingsModalOutcome::FilterChanged;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected_row = 0;
                    return SettingsModalOutcome::FilterChanged;
                }
                KeyCode::Char(c) if plain_key(key) && !c.is_control() => {
                    self.filter.push(c);
                    self.selected_row = 0;
                    return SettingsModalOutcome::FilterChanged;
                }
                _ => {}
            }
        }

        let count = rows.len();
        let mode_count = keymap.controls.modes().count().max(1);
        self.explain.clear();
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Esc => SettingsModalOutcome::Close,
            KeyCode::Tab => {
                self.next_tab();
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::BackTab => {
                self.prev_tab();
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_row = step_back(self.selected_row, count, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected_row = step_forward(self.selected_row, count, 1);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageUp => {
                self.selected_row = step_back(self.selected_row, count, 10);
                SettingsModalOutcome::Noop
            }
            KeyCode::PageDown => {
                self.selected_row = step_forward(self.selected_row, count, 10);
                SettingsModalOutcome::Noop
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected_row = 0;
                SettingsModalOutcome::Noop
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected_row = count.saturating_sub(1);
                SettingsModalOutcome::Noop
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.mode_index = (self.mode_index.min(mode_count - 1) + mode_count - 1) % mode_count;
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.mode_index = (self.mode_index.min(mode_count - 1) + 1) % mode_count;
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
                self.selected_row = 0;
                SettingsModalOutcome::FilterChanged
            }
            KeyCode::Char('t') => self.start_capture(CapturePurpose::Try),
            _ if row.is_none() => SettingsModalOutcome::Noop,
            KeyCode::Enter => self.start_capture(CapturePurpose::Rebind),
            KeyCode::Char('+') | KeyCode::Char('a') => self.start_capture(CapturePurpose::AddKey),
            KeyCode::Char('x') | KeyCode::Delete => {
                let row = row.expect("checked above");
                if row.cannot_unbind {
                    return SettingsModalOutcome::Notice(format!("{} cannot be unbound", row.title));
                }
                store.set_keybinding(&ctx, &row.action, Vec::<String>::new(), reg);
                SettingsModalOutcome::Changed
            }
            KeyCode::Char('u') => {
                let row = row.expect("checked above");
                store.remove_keybinding(&ctx, &row.action, reg);
                SettingsModalOutcome::Changed
            }
            KeyCode::Char('r') | KeyCode::Char('e') => {
                let row = row.expect("checked above");
                if keymap.controls.rules_for(&mode, &row.action).is_empty() {
                    if key.code == KeyCode::Char('e') {
                        return SettingsModalOutcome::EditRequested(EditTarget::NewRule {
                            mode,
                            action: row.action,
                        });
                    }
                    return SettingsModalOutcome::Notice(format!(
                        "no rules for {} here: press e to write one",
                        row.title
                    ));
                }
                self.rules_view = Some(0);
                SettingsModalOutcome::Noop
            }
            _ => SettingsModalOutcome::Noop,
        }
    }

    fn row_count(&self, store: &SettingsStore, reg: &ExtensionRegistry) -> usize {
        if self.active_tab == CONTROLS_TAB {
            self.control_rows(&store.snapshot().keymap).len()
        } else {
            self.filtered_settings(reg).len()
        }
    }

    fn select_next(&mut self, store: &SettingsStore, reg: &ExtensionRegistry, by: usize) {
        self.selected_row = step_forward(self.selected_row, self.row_count(store, reg), by);
    }

    fn select_prev(&mut self, store: &SettingsStore, reg: &ExtensionRegistry, by: usize) {
        self.selected_row = step_back(self.selected_row, self.row_count(store, reg), by);
    }

    fn change_selected_setting(
        &mut self,
        direction: i8,
        store: &mut SettingsStore,
        reg: &ExtensionRegistry,
    ) -> SettingsModalOutcome {
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

fn step_forward(row: usize, count: usize, by: usize) -> usize {
    if count == 0 { 0 } else { (row + by) % count }
}

fn step_back(row: usize, count: usize, by: usize) -> usize {
    if count == 0 { 0 } else { (row + count - (by % count)) % count }
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
