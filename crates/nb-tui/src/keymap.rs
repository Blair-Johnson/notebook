//! Compiled hierarchical `Keymap` with inheritance, conflict validation, and help generation.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use toml::Value;
use nb_sdk::{
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

/// A small, allocation-free-at-call-site error-free parser API is useful for
/// config validation.  Invalid tokens are represented by `None`; callers that
/// need a diagnostic use the compiler, which records the source key.
pub fn parse_key(token: &str) -> Option<Key> {
    let token = token.trim();
    if token.is_empty() || token.chars().count() > 32 {
        return None;
    }

    let mut parts = token.split('+').collect::<Vec<_>>();
    let base = parts.pop()?;
    if parts.is_empty() {
        return parse_plain_key(base);
    }

    let mut control = false;
    let mut alt = false;
    let mut shift = false;
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => control = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            _ => return None,
        }
    }
    if control && alt {
        return None;
    }

    let mut key = parse_plain_key(base)?;
    if shift {
        key = apply_shift(key);
    }
    if control {
        return apply_character_modifier(key, true);
    }
    if alt {
        return apply_character_modifier(key, false);
    }
    Some(key)
}

/// Parse a space-separated key sequence such as `g g` or `z a`.
pub fn parse_key_sequence(sequence: &str) -> Option<KeySeq> {
    let tokens = sequence.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }
    tokens.into_iter().map(parse_key).collect()
}

/// Compatibility alias for callers that prefer an explicit string suffix.
pub fn parse_key_string(token: &str) -> Option<Key> {
    parse_key(token)
}

/// Compatibility alias for callers that prefer an explicit string suffix.
pub fn parse_key_sequence_string(sequence: &str) -> Option<KeySeq> {
    parse_key_sequence(sequence)
}

/// Format one normalized key using the canonical TOML spelling.
pub fn format_key(key: Key) -> String {
    match key {
        Key::Char(' ') => "space".into(),
        Key::Char('\n') => "enter".into(),
        Key::Char('\t') => "tab".into(),
        Key::Char('\u{8}') => "backspace".into(),
        Key::Char('\u{7f}') => "delete".into(),
        Key::Char(c) => c.to_string(),
        Key::Ctrl('\n') => "ctrl+enter".into(),
        Key::Ctrl('\t') => "ctrl+tab".into(),
        Key::Ctrl('\u{8}') => "ctrl+backspace".into(),
        Key::Ctrl('\u{7f}') => "ctrl+delete".into(),
        Key::Ctrl(' ') => "ctrl+space".into(),
        Key::Ctrl(c) => format!("ctrl+{}", canonical_modifier_char(c)),
        Key::Alt('\n') => "alt+enter".into(),
        Key::Alt('\t') => "alt+tab".into(),
        Key::Alt('\u{8}') => "alt+backspace".into(),
        Key::Alt('\u{7f}') => "alt+delete".into(),
        Key::Alt(' ') => "alt+space".into(),
        Key::Alt(c) => format!("alt+{}", canonical_modifier_char(c)),
        Key::Enter => "enter".into(),
        Key::Esc => "esc".into(),
        Key::Tab => "tab".into(),
        Key::BackTab => "backtab".into(),
        Key::Backspace => "backspace".into(),
        Key::Delete => "delete".into(),
        Key::Up => "up".into(),
        Key::Down => "down".into(),
        Key::Left => "left".into(),
        Key::Right => "right".into(),
        Key::Home => "home".into(),
        Key::End => "end".into(),
        Key::PageUp => "pageup".into(),
        Key::PageDown => "pagedown".into(),
        Key::F(n) => format!("f{n}"),
    }
}

/// Format a sequence the way vim users write it: runs of plain characters
/// are joined (`jj`, `gg`), anything else is separated by a space.
pub fn format_key_sequence(sequence: &[Key]) -> String {
    let parts = sequence
        .iter()
        .copied()
        .map(format_key)
        .collect::<Vec<_>>();
    let separator = if parts.iter().all(|part| part.chars().count() == 1) {
        ""
    } else {
        " "
    };
    parts.join(separator)
}

/// Compatibility alias for callers that prefer an explicit string suffix.
pub fn format_key_string(key: Key) -> String {
    format_key(key)
}

/// Compatibility alias for callers that prefer an explicit string suffix.
pub fn format_key_sequence_string(sequence: &[Key]) -> String {
    format_key_sequence(sequence)
}

fn parse_plain_key(token: &str) -> Option<Key> {
    let lower = token.to_ascii_lowercase();
    let key = match lower.as_str() {
        "space" | "sp" => Key::Char(' '),
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "backtab" | "back-tab" => Key::BackTab,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page-up" => Key::PageUp,
        "pagedown" | "page-down" => Key::PageDown,
        _ if lower.len() > 1 && lower.starts_with('f') => {
            let n = lower[1..].parse::<u8>().ok()?;
            if (1..=12).contains(&n) {
                Key::F(n)
            } else {
                return None;
            }
        }
        _ => {
            let mut chars = token.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
    };
    Some(key)
}

fn apply_shift(key: Key) -> Key {
    match key {
        Key::Char(c) if c.is_ascii_lowercase() => Key::Char(c.to_ascii_uppercase()),
        Key::Tab => Key::BackTab,
        other => other,
    }
}

fn apply_character_modifier(key: Key, control: bool) -> Option<Key> {
    let c = match key {
        Key::Char(c) => c,
        Key::Enter => '\n',
        Key::Tab => '\t',
        Key::Backspace => '\u{8}',
        Key::Delete => '\u{7f}',
        Key::Esc => '\u{1b}',
        _ => return None,
    };
    if control {
        Some(Key::Ctrl(c.to_ascii_lowercase()))
    } else {
        Some(Key::Alt(c))
    }
}

fn canonical_modifier_char(c: char) -> char {
    if c.is_ascii_alphabetic() {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

/// Normalize a `crossterm::event::KeyEvent` (including shifted uppercase and `BackTab`) into `Key`.
pub fn normalize_crossterm_key(event: KeyEvent) -> Option<Key> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }
    let modifiers = event.modifiers;
    if modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META) {
        return None;
    }
    let control = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    match event.code {
        KeyCode::Char(mut c) => {
            // Legacy terminals send ctrl+/ as 0x1f, which crossterm reports
            // as ctrl+7 (or a bare unit-separator char); ctrl+_ is the same byte.
            if c == '\u{1f}' || (control && matches!(c, '7' | '_' | '/')) {
                return Some(Key::Ctrl('/'));
            }
            // Many terminals send ctrl+space as NUL.
            if c == '\0' {
                return Some(Key::Ctrl(' '));
            }
            if c.is_ascii_control() && !control && !alt {
                if (1..=26).contains(&(c as u8)) {
                    return Some(Key::Ctrl((b'a' + c as u8 - 1) as char));
                }
                if c == '\u{7f}' {
                    return Some(Key::Backspace);
                }
            }
            if shift && c.is_ascii_lowercase() {
                c = c.to_ascii_uppercase();
            }
            if control {
                Some(Key::Ctrl(c.to_ascii_lowercase()))
            } else if alt {
                Some(Key::Alt(c))
            } else {
                Some(Key::Char(c))
            }
        }
        KeyCode::BackTab => Some(Key::BackTab),
        KeyCode::Enter => modified_special('\n', control, alt).or(Some(Key::Enter)),
        KeyCode::Tab if shift => Some(Key::BackTab),
        KeyCode::Tab => modified_special('\t', control, alt).or(Some(Key::Tab)),
        KeyCode::Backspace => modified_special('\u{8}', control, alt).or(Some(Key::Backspace)),
        KeyCode::Delete => modified_special('\u{7f}', control, alt).or(Some(Key::Delete)),
        KeyCode::Esc => modified_special('\u{1b}', control, alt).or(Some(Key::Esc)),
        KeyCode::Left if !control && !alt => Some(Key::Left),
        KeyCode::Right if !control && !alt => Some(Key::Right),
        // Word-wise arrows map onto the readline keys (alt+b / alt+f).
        KeyCode::Left => Some(Key::Alt('b')),
        KeyCode::Right => Some(Key::Alt('f')),
        KeyCode::Up if !control && !alt => Some(Key::Up),
        KeyCode::Down if !control && !alt => Some(Key::Down),
        KeyCode::Home if !control && !alt => Some(Key::Home),
        KeyCode::End if !control && !alt => Some(Key::End),
        KeyCode::PageUp if !control && !alt => Some(Key::PageUp),
        KeyCode::PageDown if !control && !alt => Some(Key::PageDown),
        KeyCode::F(n) if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            Some(Key::F(n))
        }
        _ => None,
    }
}

fn modified_special(c: char, control: bool, alt: bool) -> Option<Key> {
    match (control, alt) {
        (true, false) => Some(Key::Ctrl(c)),
        (false, true) => Some(Key::Alt(c)),
        _ => None,
    }
}

#[derive(Default, Debug)]
pub struct Keymap {
    contexts: HashMap<KeyContextId, KeyContextSpec>,
    actions: BTreeMap<String, ActionSpec>,
    tables: HashMap<KeyContextId, HashMap<KeySeq, String>>,
    prefixes: HashMap<KeyContextId, HashSet<KeySeq>>,
    pub warnings: Vec<ConfigWarning>,
}

#[derive(Default)]
struct RawContext {
    actions: BTreeMap<String, Option<Vec<KeySeq>>>,
}

impl Keymap {
    /// Compile default plugin TOML fragments plus user `config.toml` overrides,
    /// resolving parent inheritance (`vim.normal` -> `notebook.normal`, `cell.normal`, `output.*`)
    /// and enforcing `cannot_unbind` and `text_entry` safety guards.
    pub fn compile(reg: &ExtensionRegistry, user_toml: &str) -> Self {
        let contexts = reg
            .contexts
            .iter()
            .cloned()
            .map(|spec| (spec.id.clone(), spec))
            .collect::<HashMap<_, _>>();
        let actions = reg
            .actions
            .iter()
            .cloned()
            .map(|spec| (spec.id.clone(), spec))
            .collect::<BTreeMap<_, _>>();
        let mut keymap = Self {
            contexts,
            actions,
            tables: HashMap::new(),
            prefixes: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut raw = BTreeMap::<KeyContextId, RawContext>::new();

        for (owner, fragment) in &reg.default_keymap_toml_fragments {
            keymap.apply_toml(&mut raw, fragment, owner, false);
        }
        if !user_toml.trim().is_empty() {
            keymap.apply_toml(&mut raw, user_toml, "user", true);
        }

        let mut memo = HashMap::<KeyContextId, HashMap<KeySeq, String>>::new();
        let context_ids = keymap.contexts.keys().cloned().collect::<Vec<_>>();
        for ctx in context_ids {
            let mut stack = HashSet::new();
            let table = keymap.effective_table(&ctx, &raw, &mut memo, &mut stack);
            let prefixes = collect_prefixes(&table);
            keymap.warn_prefix_conflicts(&ctx, &table);
            keymap.tables.insert(ctx.clone(), table);
            keymap.prefixes.insert(ctx, prefixes);
        }
        keymap
    }

    fn apply_toml(
        &mut self,
        raw: &mut BTreeMap<KeyContextId, RawContext>,
        source: &str,
        owner: &str,
        is_user: bool,
    ) {
        let document = match toml::from_str::<Value>(source) {
            Ok(value) => value,
            Err(error) => {
                self.warn(owner.to_string(), format!("invalid TOML: {error}"));
                return;
            }
        };
        let Some(keys) = document.get("keys").and_then(Value::as_table) else {
            return;
        };
        for (ctx_name, action_table) in keys {
            let ctx = KeyContextId(Cow::Owned(ctx_name.clone()));
            if !self.contexts.contains_key(&ctx) {
                self.warn(
                    format!("{owner}.keys.{ctx_name}"),
                    "unknown key context".into(),
                );
                continue;
            }
            let Some(action_table) = action_table.as_table() else {
                self.warn(
                    format!("{owner}.keys.{ctx_name}"),
                    "context bindings must be a table".into(),
                );
                continue;
            };
            let context = raw.entry(ctx.clone()).or_default();
            for (action_id, value) in action_table {
                let source_key = format!("{owner}.keys.{ctx_name}.{action_id}");
                let Some(action) = self.actions.get(action_id) else {
                    self.warn(source_key, "unknown action".into());
                    continue;
                };
                let Some(values) = value.as_array() else {
                    self.warn(source_key, "bindings must be an array of strings".into());
                    continue;
                };
                if values.is_empty() {
                    if is_user && action.cannot_unbind {
                        self.warn(
                            format!("user.keys.{ctx_name}.{action_id}"),
                            "action cannot be unbound; default binding kept".into(),
                        );
                        continue;
                    }
                    context.actions.insert(action_id.clone(), None);
                    continue;
                }

                let mut parsed = Vec::with_capacity(values.len());
                let mut invalid = false;
                for value in values {
                    let Some(sequence) = value.as_str() else {
                        self.warn(source_key.clone(), "binding must be a string".into());
                        invalid = true;
                        continue;
                    };
                    let Some(sequence) = parse_key_sequence(sequence) else {
                        self.warn(
                            source_key.clone(),
                            format!("invalid key sequence {sequence:?}"),
                        );
                        invalid = true;
                        continue;
                    };
                    if self.contexts.get(&ctx).is_some_and(|spec| spec.text_entry)
                        && sequence.len() == 1
                        && sequence[0].printable().is_some()
                    {
                        self.warn(
                            source_key.clone(),
                            "single printable bindings are refused in text-entry contexts".into(),
                        );
                        invalid = true;
                        continue;
                    }
                    parsed.push(sequence);
                }
                if !invalid || !parsed.is_empty() {
                    context.actions.insert(action_id.clone(), Some(parsed));
                }
            }
        }
    }

    fn warn(&mut self, source_key: String, message: String) {
        self.warnings.push(ConfigWarning {
            source_key,
            message,
        });
    }

    fn effective_table(
        &mut self,
        ctx: &KeyContextId,
        raw: &BTreeMap<KeyContextId, RawContext>,
        memo: &mut HashMap<KeyContextId, HashMap<KeySeq, String>>,
        stack: &mut HashSet<KeyContextId>,
    ) -> HashMap<KeySeq, String> {
        if let Some(table) = memo.get(ctx) {
            return table.clone();
        }
        if !stack.insert(ctx.clone()) {
            self.warn(ctx.as_str().into(), "cyclic key context inheritance".into());
            return HashMap::new();
        }

        let mut table = HashMap::new();
        if let Some(spec) = self.contexts.get(ctx).cloned() {
            if spec.id.as_str() != "global" && spec.parents.is_empty() {
                let global = KeyContextId::new_static("global");
                if self.contexts.contains_key(&global) {
                    table.extend(self.effective_table(&global, raw, memo, stack));
                }
            }
            for parent in spec.parents.iter().rev() {
                let parent_table = self.effective_table(parent, raw, memo, stack);
                for (sequence, action) in parent_table {
                    table.entry(sequence).or_insert(action);
                }
            }
        }

        if let Some(local) = raw.get(ctx) {
            for (action_id, bindings) in &local.actions {
                table.retain(|_, action| action != action_id);
                if let Some(bindings) = bindings {
                    for sequence in bindings {
                        table.insert(sequence.clone(), action_id.clone());
                    }
                }
            }
            self.warn_local_conflicts(ctx, local);
        }

        stack.remove(ctx);
        memo.insert(ctx.clone(), table.clone());
        table
    }

    fn warn_local_conflicts(&mut self, ctx: &KeyContextId, local: &RawContext) {
        let mut seen = BTreeMap::<KeySeq, String>::new();
        for (action, bindings) in &local.actions {
            let Some(bindings) = bindings else { continue };
            for sequence in bindings {
                if let Some(previous) = seen.get(sequence) {
                    if previous != action {
                        self.warn(
                            format!("keys.{}.{}", ctx.as_str(), action),
                            format!(
                                "key sequence {:?} is already bound to {previous} in this context",
                                format_key_sequence(sequence)
                            ),
                        );
                    }
                } else {
                    seen.insert(sequence.clone(), action.clone());
                }
            }
        }
    }

    fn warn_prefix_conflicts(&mut self, ctx: &KeyContextId, table: &HashMap<KeySeq, String>) {
        let mut sequences = table.keys().cloned().collect::<Vec<_>>();
        sequences.sort();
        for (index, prefix) in sequences.iter().enumerate() {
            for longer in sequences.iter().skip(index + 1) {
                if longer.len() > prefix.len() && longer.starts_with(prefix) {
                    self.warn(
                        format!("keys.{}", ctx.as_str()),
                        format!(
                            "prefix conflict: {} ({}) shadows {} ({})",
                            format_key_sequence(prefix),
                            table.get(prefix).map(String::as_str).unwrap_or(""),
                            format_key_sequence(longer),
                            table.get(longer).map(String::as_str).unwrap_or("")
                        ),
                    );
                }
            }
        }
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

    /// Key sequences bound to `action_id` in `ctx` (inherited bindings
    /// included), shortest first, formatted for display.
    pub fn keys_for(&self, ctx: &KeyContextId, action_id: &str) -> Vec<String> {
        let mut keys: Vec<&KeySeq> = self
            .tables
            .get(ctx)
            .into_iter()
            .flat_map(|table| table.iter())
            .filter(|(_, action)| action.as_str() == action_id)
            .map(|(sequence, _)| sequence)
            .collect();
        keys.sort_by_key(|sequence| (sequence.len(), format_key_sequence(sequence)));
        keys.into_iter().map(|sequence| format_key_sequence(sequence)).collect()
    }

    /// Every distinct key sequence bound to `action_id` in any context.
    pub fn bindings_for(&self, action_id: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .tables
            .values()
            .flat_map(|table| table.iter())
            .filter(|(_, action)| action.as_str() == action_id)
            .map(|(sequence, _)| format_key_sequence(sequence))
            .collect();
        keys.sort_by_key(|key| (key.len(), key.clone()));
        keys.dedup();
        keys
    }

    pub fn lookup(&self, ctx: &KeyContextId, operator_pending: bool, seq: &[Key]) -> LookupResult {
        if operator_pending {
            if let Some(spec) = self.contexts.get(ctx) {
                if let Some(pending) = &spec.operator_pending {
                    let result = self.lookup_in(pending, seq);
                    if !matches!(result, LookupResult::None) {
                        return result;
                    }
                }
            }
        }
        self.lookup_in(ctx, seq)
    }

    fn lookup_in(&self, ctx: &KeyContextId, seq: &[Key]) -> LookupResult {
        let key = seq.to_vec();
        if let Some(action) = self.tables.get(ctx).and_then(|table| table.get(&key)) {
            return LookupResult::Exact(action.clone());
        }
        if self
            .prefixes
            .get(ctx)
            .is_some_and(|prefixes| prefixes.contains(&key))
        {
            LookupResult::Prefix
        } else {
            LookupResult::None
        }
    }

    pub fn help_for(&self, ctx: &KeyContextId) -> Vec<HelpRow> {
        let Some(table) = self.tables.get(ctx) else {
            return Vec::new();
        };
        let mut grouped = BTreeMap::<String, (u16, String, Vec<String>)>::new();
        for (sequence, action_id) in table {
            let Some(spec) = self.actions.get(action_id) else {
                continue;
            };
            let entry = grouped
                .entry(action_id.clone())
                .or_insert_with(|| (spec.order, spec.category.to_string(), Vec::new()));
            entry.2.push(format_key_sequence(sequence));
        }
        let mut rows = grouped
            .into_iter()
            .filter_map(|(action_id, (_, category, mut keys))| {
                let spec = self.actions.get(&action_id)?;
                keys.sort();
                Some(HelpRow {
                    category,
                    keys: keys.join(", "),
                    title: spec.title.to_string(),
                })
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            let left_order = self
                .actions
                .values()
                .find(|spec| spec.title == left.title && spec.category == left.category)
                .map_or(u16::MAX, |spec| spec.order);
            let right_order = self
                .actions
                .values()
                .find(|spec| spec.title == right.title && spec.category == right.category)
                .map_or(u16::MAX, |spec| spec.order);
            left.category
                .cmp(&right.category)
                .then(left_order.cmp(&right_order))
                .then(left.title.cmp(&right.title))
        });
        rows
    }
}

fn collect_prefixes(table: &HashMap<KeySeq, String>) -> HashSet<KeySeq> {
    let mut prefixes = HashSet::new();
    for sequence in table.keys() {
        for length in 1..sequence.len() {
            prefixes.insert(sequence[..length].to_vec());
        }
    }
    prefixes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};

    #[test]
    fn parses_and_formats_canonical_keys() {
        for spelling in [
            "j",
            "G",
            "space",
            "enter",
            "esc",
            "tab",
            "backtab",
            "backspace",
            "delete",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "f12",
            "ctrl+c",
            "ctrl+\\",
            "ctrl+",
            "alt+enter",
        ] {
            if spelling == "ctrl+" {
                continue;
            }
            let key = parse_key(spelling).expect(spelling);
            assert_eq!(parse_key(&format_key(key)), Some(key), "{spelling}");
        }
        assert_eq!(parse_key_sequence("g g").unwrap().len(), 2);
    }

    #[test]
    fn normalizes_shift_and_backtab() {
        assert_eq!(
            normalize_crossterm_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT)),
            Some(Key::Char('G'))
        );
        assert_eq!(
            normalize_crossterm_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            Some(Key::BackTab)
        );
    }
}
