//! Compiled hierarchical `Keymap` with inheritance, conflict validation, and help generation.

use crossterm::event::KeyEvent;
use std::collections::{BTreeMap, HashMap, HashSet};
use wb_sdk::{
    ActionSpec, ConfigWarning, ExtensionRegistry, Key, KeyContextId, KeyContextSpec, KeySeq,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LookupResult {
    Exact(String),
    Prefix,
    None,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HelpRow {
    pub category: String,
    pub keys: String,
    pub title: String,
}

#[derive(Default, Debug)]
pub struct Keymap {
    contexts: HashMap<KeyContextId, KeyContextSpec>,
    actions: BTreeMap<String, ActionSpec>,
    tables: HashMap<KeyContextId, HashMap<KeySeq, String>>,
    prefixes: HashMap<KeyContextId, HashSet<KeySeq>>,
    pub warnings: Vec<ConfigWarning>,
}

impl Keymap {
    /// Compile default plugin TOML fragments plus user `config.toml` overrides,
    /// resolving parent inheritance (`vim.normal` -> `notebook.normal`, `cell.normal`, `output.*`)
    /// and enforcing `cannot_unbind` and `text_entry` safety guards.
    pub fn compile(_reg: &ExtensionRegistry, _user_toml: &str) -> Self {
        todo!("wb-tui::keymap: parse key sequences, resolve inheritance chains, record conflicts")
    }

    pub fn context_spec(&self, ctx: &KeyContextId) -> Option<&KeyContextSpec> {
        self.contexts.get(ctx)
    }

    pub fn action_spec(&self, id: &str) -> Option<&ActionSpec> {
        self.actions.get(id)
    }

    pub fn bound_count(&self, ctx: &KeyContextId) -> (usize, usize) {
        (
            self.tables.get(ctx).map_or(0, HashMap::len),
            self.prefixes.get(ctx).map_or(0, HashSet::len),
        )
    }

    pub fn lookup(
        &self,
        _ctx: &KeyContextId,
        _operator_pending: bool,
        _seq: &[Key],
    ) -> LookupResult {
        todo!("wb-tui::keymap: consult operator_pending context first when armed, then ctx table")
    }

    pub fn help_for(&self, _ctx: &KeyContextId) -> Vec<HelpRow> {
        todo!("wb-tui::keymap: group bound sequences by ActionSpec category and order")
    }
}

/// Normalize a `crossterm::event::KeyEvent` (including shifted uppercase and `BackTab`) into `Key`.
pub fn normalize_crossterm_key(_event: KeyEvent) -> Option<Key> {
    todo!("wb-tui::keymap: translate crossterm KeyCode + KeyModifiers into wb_sdk::Key")
}
