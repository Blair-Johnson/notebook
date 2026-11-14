//! Configurable controls: modes, keysets, groups, macros, and rules.
//!
//! Keys trigger actions; rules decide what an action does in a given mode and
//! on a given target; a rule's steps can switch modes. Defaults and the user
//! config use the same TOML tables, merged here into one [`Controls`] value
//! that the keymap, the coordinator, help, and settings all read.

use crate::focus::CellPart;
use nb_sdk::{ActionKind, ActionSpec, ConfigWarning, ExtensionRegistry, KeyContextId, KeyContextSpec};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use toml::Value;

/// The kind of unit the cursor moves over. Fixed in code: each level has its
/// own motion and operator implementations.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Level {
    Notebook,
    Cell,
    Output,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Self::Notebook => "notebook",
            Self::Cell => "cell",
            Self::Output => "output",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "notebook" => Self::Notebook,
            "cell" => Self::Cell,
            "output" => Self::Output,
            _ => return None,
        })
    }
}

/// How keys are parsed in a mode.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum ModeKind {
    Normal,
    Visual,
    VisualLine,
    Insert,
}

impl ModeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Visual => "visual",
            Self::VisualLine => "visual_line",
            Self::Insert => "insert",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "normal" => Self::Normal,
            "visual" => Self::Visual,
            "visual_line" => Self::VisualLine,
            "insert" => Self::Insert,
            _ => return None,
        })
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

/// One state of the controls state machine.
#[derive(Clone, Debug)]
pub struct ModeSpec {
    pub id: String,
    pub title: String,
    pub level: Level,
    pub kind: ModeKind,
    pub badge: String,
    pub cursor: CursorStyle,
    pub keysets: Vec<String>,
    /// Keyset read first while an operator waits for its target (`d i w`).
    pub pending: Option<String>,
    family: BTreeMap<ModeKind, String>,
}

impl ModeSpec {
    /// The mode to use when a primitive of this mode ends in `kind` (`cw`
    /// ends in insert; an operator in visual mode ends in normal).
    pub fn family(&self, kind: ModeKind) -> String {
        self.family
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| format!("{}.{}", self.level.name(), kind.name()))
    }
}

/// One entry of a rule's or macro's `do` list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// `@action`: the action that triggered the rule, with its count,
    /// character argument, and operator target. Never re-checks rules.
    Trigger,
    /// `mode.<id>`: switch to that mode.
    Mode(String),
    /// Any other action, optionally with a character argument (`edit.replace_char:x`).
    Action { id: String, arg: Option<char> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinePos {
    First,
    Last,
    Only,
}

/// A `when` condition on where the cursor is when the key is pressed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Condition {
    Part(CellPart),
    Cell(String),
    Empty(bool),
    Outputs(bool),
    Line(LinePos),
    Count(bool),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: String,
    pub title: Option<String>,
    /// `None` applies in every mode.
    pub modes: Option<Vec<String>>,
    pub on: Vec<String>,
    /// Operator targets that must match (`dd` has the operator itself as target).
    pub with: Option<Vec<String>>,
    pub when: Vec<Condition>,
    pub steps: Vec<Step>,
    pub enabled: bool,
    /// Written (or overridden) in the user config.
    pub from_user: bool,
    /// The merged rule as TOML, for copying a default into the user config.
    pub source: toml::Table,
}

impl Rule {
    pub fn applies_in(&self, mode: &str) -> bool {
        self.modes
            .as_ref()
            .is_none_or(|modes| modes.iter().any(|m| m == mode))
    }
}

/// A user-visible action defined as a list of steps.
#[derive(Clone, Debug)]
pub struct Macro {
    pub id: String,
    pub title: String,
    pub steps: Vec<Step>,
    pub from_user: bool,
}

/// One line of "what happens" for an action in a mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    /// Plain-English conditions ("source row, empty cell"), empty when unconditional.
    pub when: String,
    pub what: String,
    pub rule_id: Option<String>,
    pub enabled: bool,
    pub from_user: bool,
}

#[derive(Clone, Debug)]
struct KeysetSpec {
    id: String,
    title: Option<String>,
    includes: Option<Vec<String>>,
    text_entry: Option<bool>,
}

#[derive(Default, Debug)]
pub struct Controls {
    modes: BTreeMap<String, ModeSpec>,
    mode_order: Vec<String>,
    keysets: Vec<KeysetSpec>,
    groups: BTreeMap<String, Vec<String>>,
    macros: HashMap<String, Macro>,
    rules: Vec<Rule>,
    by_action: HashMap<String, Vec<usize>>,
    titles: HashMap<String, String>,
    pub warnings: Vec<ConfigWarning>,
}

/// Steps may nest macros this deep before the key stops with a notice.
pub const MAX_DEPTH: usize = 8;

impl Controls {
    /// Merge the default fragments and the user config, then validate
    /// every name. Invalid entries are dropped with a warning; the rest
    /// still apply.
    pub fn compile(reg: &ExtensionRegistry, user_toml: &str) -> Self {
        let mut controls = Self::default();
        let mut merged = Merged::default();
        for (owner, fragment) in &reg.default_keymap_toml_fragments {
            match toml::from_str::<Value>(fragment) {
                Ok(doc) => merged.apply(&doc, false, owner, &mut controls.warnings),
                Err(error) => controls.warn(owner, format!("invalid TOML: {error}")),
            }
        }
        if !user_toml.trim().is_empty() {
            // The keymap compiler reports TOML syntax errors already.
            if let Ok(doc) = toml::from_str::<Value>(user_toml) {
                merged.apply(&doc, true, "user", &mut controls.warnings);
            }
        }

        let mut known_contexts: HashSet<String> = reg
            .contexts
            .iter()
            .map(|spec| spec.id.as_str().to_string())
            .collect();
        for (id, table) in &merged.keysets {
            known_contexts.insert(id.clone());
            controls.keysets.push(KeysetSpec {
                id: id.clone(),
                title: str_field(table, "title"),
                includes: str_list(table, "includes"),
                text_entry: table.get("text_entry").and_then(Value::as_bool),
            });
        }
        known_contexts.extend(merged.modes.keys().cloned());

        controls.compile_modes(&merged, &known_contexts);
        for keyset in controls.keysets.clone() {
            for include in keyset.includes.iter().flatten() {
                if !known_contexts.contains(include) {
                    controls.warn(
                        &format!("keysets.{}.includes", keyset.id),
                        format!("unknown keyset {include:?}"),
                    );
                }
            }
        }

        for spec in &reg.actions {
            controls.titles.insert(spec.id.clone(), spec.title.to_string());
        }
        for mode in controls.modes.values() {
            controls
                .titles
                .insert(format!("mode.{}", mode.id), format!("Switch to {}", mode.title));
        }
        for (id, table) in &merged.actions {
            let title = str_field(table, "title")
                .or_else(|| controls.titles.get(id).cloned())
                .unwrap_or_else(|| id.clone());
            controls.titles.insert(id.clone(), title);
        }
        controls.groups = merged.groups.clone();
        let kinds: HashMap<String, ActionKind> = reg
            .actions
            .iter()
            .map(|spec| (spec.id.clone(), spec.kind))
            .chain(merged.actions.keys().map(|id| (id.clone(), ActionKind::Command)))
            .collect();

        for (id, table) in &merged.actions {
            let source = format!("actions.{id}");
            let Some(steps) = controls.parse_steps(table.get("do"), &kinds, &source) else {
                continue;
            };
            controls.macros.insert(
                id.clone(),
                Macro {
                    id: id.clone(),
                    title: controls.titles.get(id).cloned().unwrap_or_else(|| id.clone()),
                    steps,
                    from_user: merged.user_actions.contains(id),
                },
            );
        }

        for (index, (table, from_user)) in merged.rules.iter().enumerate() {
            if let Some(rule) = controls.parse_rule(table, *from_user, index, &kinds) {
                controls.rules.push(rule);
            }
        }
        for (index, rule) in controls.rules.iter().enumerate() {
            for action in &rule.on {
                controls.by_action.entry(action.clone()).or_default().push(index);
            }
        }
        controls
    }

    fn warn(&mut self, source_key: &str, message: String) {
        self.warnings.push(ConfigWarning {
            source_key: source_key.to_string(),
            message,
        });
    }

    fn compile_modes(&mut self, merged: &Merged, known_contexts: &HashSet<String>) {
        for (id, table) in &merged.modes {
            let source = format!("modes.{id}");
            let level = str_field(table, "level").and_then(|name| Level::parse(&name));
            let kind = str_field(table, "kind").and_then(|name| ModeKind::parse(&name));
            let (Some(level), Some(kind)) = (level, kind) else {
                self.warn(
                    &source,
                    "a mode needs level = notebook|cell|output and kind = normal|visual|visual_line|insert".into(),
                );
                continue;
            };
            if level == Level::Notebook && kind != ModeKind::Normal {
                self.warn(&source, "notebook-level modes must be kind = \"normal\"".into());
                continue;
            }
            if level == Level::Output && kind == ModeKind::Insert {
                self.warn(&source, "outputs are read-only: no insert modes".into());
                continue;
            }
            let cursor = match str_field(table, "cursor").as_deref() {
                None => {
                    if kind == ModeKind::Insert {
                        CursorStyle::Bar
                    } else {
                        CursorStyle::Block
                    }
                }
                Some("block") => CursorStyle::Block,
                Some("bar") => CursorStyle::Bar,
                Some("underline") => CursorStyle::Underline,
                Some(other) => {
                    self.warn(&source, format!("unknown cursor {other:?}; use block, bar, or underline"));
                    CursorStyle::Block
                }
            };
            let mut keysets = Vec::new();
            for keyset in str_list(table, "keysets").unwrap_or_default() {
                if keyset == *id {
                    continue;
                }
                if known_contexts.contains(&keyset) {
                    keysets.push(keyset);
                } else {
                    self.warn(&format!("{source}.keysets"), format!("unknown keyset {keyset:?}"));
                }
            }
            let pending = str_field(table, "pending").filter(|pending| {
                let known = known_contexts.contains(pending);
                if !known {
                    self.warnings.push(ConfigWarning {
                        source_key: format!("{source}.pending"),
                        message: format!("unknown keyset {pending:?}"),
                    });
                }
                known
            });
            let mut family = BTreeMap::new();
            for kind in [ModeKind::Normal, ModeKind::Visual, ModeKind::VisualLine, ModeKind::Insert] {
                if let Some(target) = str_field(table, kind.name()) {
                    family.insert(kind, target);
                }
            }
            let badge = str_field(table, "badge").unwrap_or_else(|| id.to_uppercase());
            self.modes.insert(
                id.clone(),
                ModeSpec {
                    id: id.clone(),
                    title: str_field(table, "title").unwrap_or_else(|| id.clone()),
                    level,
                    kind,
                    badge,
                    cursor,
                    keysets,
                    pending,
                    family,
                },
            );
        }
        let ids: Vec<String> = self.modes.keys().cloned().collect();
        for id in ids {
            let targets: Vec<(ModeKind, String)> = self.modes[&id]
                .family
                .iter()
                .map(|(kind, target)| (*kind, target.clone()))
                .collect();
            for (kind, target) in targets {
                let valid = self
                    .modes
                    .get(&target)
                    .is_some_and(|mode| mode.kind == kind && mode.level == self.modes[&id].level);
                if !valid {
                    self.warn(
                        &format!("modes.{id}.{}", kind.name()),
                        format!("{target:?} is not a {} mode at the same level", kind.name()),
                    );
                    self.modes.get_mut(&id).expect("listed").family.remove(&kind);
                }
            }
        }
        let mut order: Vec<&ModeSpec> = self.modes.values().collect();
        order.sort_by_key(|mode| (mode.level, mode.kind, mode.id.clone()));
        self.mode_order = order.into_iter().map(|mode| mode.id.clone()).collect();
    }

    fn expand(&mut self, value: Option<&Value>, source: &str, kinds: &HashMap<String, ActionKind>) -> Option<Vec<String>> {
        let names = match value {
            None => return Some(Vec::new()),
            Some(Value::String(name)) => vec![name.clone()],
            Some(Value::Array(items)) => {
                let mut names = Vec::new();
                for item in items {
                    let Some(name) = item.as_str() else {
                        self.warn(source, "expected action names".into());
                        return None;
                    };
                    names.push(name.to_string());
                }
                names
            }
            Some(_) => {
                self.warn(source, "expected an action name or a list of them".into());
                return None;
            }
        };
        let mut out = Vec::new();
        for name in names {
            if let Some(group) = name.strip_prefix('@') {
                match self.groups.get(group) {
                    Some(members) => out.extend(members.iter().cloned()),
                    None => {
                        self.warn(source, format!("unknown group {name:?}"));
                        return None;
                    }
                }
            } else if kinds.contains_key(&name) || self.is_mode_action(&name) {
                out.push(name);
            } else {
                self.warn(source, format!("unknown action {name:?}"));
                return None;
            }
        }
        Some(out)
    }

    fn is_mode_action(&self, id: &str) -> bool {
        id.strip_prefix("mode.").is_some_and(|mode| self.modes.contains_key(mode))
    }

    fn parse_steps(&mut self, value: Option<&Value>, kinds: &HashMap<String, ActionKind>, source: &str) -> Option<Vec<Step>> {
        let key = format!("{source}.do");
        let Some(items) = value.and_then(Value::as_array) else {
            self.warn(&key, "`do` must be a list of steps".into());
            return None;
        };
        let mut steps = Vec::new();
        for item in items {
            let Some(text) = item.as_str() else {
                self.warn(&key, "every step must be a string".into());
                return None;
            };
            let step = if text == "@action" {
                Step::Trigger
            } else if let Some(mode) = text.strip_prefix("mode.") {
                if !self.modes.contains_key(mode) {
                    self.warn(&key, format!("unknown mode {mode:?}"));
                    return None;
                }
                Step::Mode(mode.to_string())
            } else {
                let (id, arg) = match text.split_once(':') {
                    Some((id, rest)) => {
                        let mut chars = rest.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) => (id, Some(c)),
                            _ => {
                                self.warn(&key, format!("{text:?}: the argument after ':' must be one character"));
                                return None;
                            }
                        }
                    }
                    None => (text, None),
                };
                let Some(kind) = kinds.get(id) else {
                    self.warn(&key, format!("unknown action {id:?}"));
                    return None;
                };
                match (kind, arg) {
                    (ActionKind::TextObject, _) => {
                        self.warn(&key, format!("{id:?} is a text object; use it after an operator key"));
                        return None;
                    }
                    (ActionKind::CharArgCommand | ActionKind::CharArgMotion, None) => {
                        self.warn(&key, format!("{id:?} needs a character, such as \"{id}:x\""));
                        return None;
                    }
                    (ActionKind::Command | ActionKind::Motion | ActionKind::Operator, Some(_)) => {
                        self.warn(&key, format!("{id:?} takes no character argument"));
                        return None;
                    }
                    _ => {}
                }
                Step::Action {
                    id: id.to_string(),
                    arg,
                }
            };
            steps.push(step);
        }
        Some(steps)
    }

    fn parse_rule(&mut self, table: &toml::Table, from_user: bool, index: usize, kinds: &HashMap<String, ActionKind>) -> Option<Rule> {
        let id = str_field(table, "id").unwrap_or_else(|| format!("user.{index}"));
        let source = format!("rules.{id}");
        let modes = match table.get("mode") {
            None => None,
            Some(Value::String(mode)) => Some(vec![mode.clone()]),
            Some(Value::Array(_)) => str_list(table, "mode"),
            Some(_) => {
                self.warn(&source, "`mode` must be a mode id or a list of them".into());
                return None;
            }
        };
        for mode in modes.iter().flatten() {
            if !self.modes.contains_key(mode) {
                self.warn(&format!("{source}.mode"), format!("unknown mode {mode:?}"));
                return None;
            }
        }
        let on = self.expand(table.get("on"), &format!("{source}.on"), kinds)?;
        if on.is_empty() {
            self.warn(&source, "`on` must name at least one action".into());
            return None;
        }
        let with = match table.get("with") {
            None => None,
            value => Some(self.expand(value, &format!("{source}.with"), kinds)?),
        };
        let mut when = Vec::new();
        if let Some(conditions) = table.get("when") {
            let Some(conditions) = conditions.as_table() else {
                self.warn(&source, "`when` must be a table, such as { part = \"source\" }".into());
                return None;
            };
            for (name, value) in conditions {
                let condition = match (name.as_str(), value) {
                    ("part", Value::String(part)) if part == "source" => Condition::Part(CellPart::Source),
                    ("part", Value::String(part)) if part == "output" => Condition::Part(CellPart::Output),
                    ("cell", Value::String(kind)) if matches!(kind.as_str(), "code" | "markdown" | "raw") => {
                        Condition::Cell(kind.clone())
                    }
                    ("empty", Value::Boolean(value)) => Condition::Empty(*value),
                    ("outputs", Value::Boolean(value)) => Condition::Outputs(*value),
                    ("count", Value::Boolean(value)) => Condition::Count(*value),
                    ("line", Value::String(line)) if line == "first" => Condition::Line(LinePos::First),
                    ("line", Value::String(line)) if line == "last" => Condition::Line(LinePos::Last),
                    ("line", Value::String(line)) if line == "only" => Condition::Line(LinePos::Only),
                    (name, value) => {
                        self.warn(
                            &format!("{source}.when.{name}"),
                            format!(
                                "unknown condition {name} = {value}; use part (source|output), cell (code|markdown|raw), empty, outputs, count (true|false), or line (first|last|only)"
                            ),
                        );
                        return None;
                    }
                };
                when.push(condition);
            }
        }
        let steps = self.parse_steps(table.get("do"), kinds, &source)?;
        Some(Rule {
            id,
            title: str_field(table, "title"),
            modes,
            on,
            with,
            when,
            steps,
            enabled: table.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            from_user,
            source: table.clone(),
        })
    }

    /// Key contexts for every keyset and mode, overriding registry contexts
    /// with the same id.
    pub fn contexts(&self, reg: &ExtensionRegistry) -> Vec<KeyContextSpec> {
        let mut out = Vec::new();
        for keyset in &self.keysets {
            let base = reg.contexts.iter().find(|spec| spec.id.as_str() == keyset.id);
            let mut spec = base.cloned().unwrap_or_else(|| KeyContextSpec {
                id: KeyContextId(Cow::Owned(keyset.id.clone())),
                title: Cow::Owned(keyset.id.clone()),
                parents: Vec::new(),
                text_entry: false,
                operator_pending: None,
                visual: false,
            });
            if let Some(title) = &keyset.title {
                spec.title = Cow::Owned(title.clone());
            }
            if let Some(includes) = &keyset.includes {
                spec.parents = includes.iter().map(|id| KeyContextId(Cow::Owned(id.clone()))).collect();
            }
            if let Some(text_entry) = keyset.text_entry {
                spec.text_entry = text_entry;
            }
            out.push(spec);
        }
        for mode in self.modes.values() {
            out.push(KeyContextSpec {
                id: KeyContextId(Cow::Owned(mode.id.clone())),
                title: Cow::Owned(mode.title.clone()),
                parents: mode
                    .keysets
                    .iter()
                    .map(|id| KeyContextId(Cow::Owned(id.clone())))
                    .collect(),
                text_entry: mode.kind == ModeKind::Insert,
                operator_pending: mode.pending.clone().map(|id| KeyContextId(Cow::Owned(id))),
                visual: mode.kind.is_visual(),
            });
        }
        out
    }

    /// Bindable actions that only exist here: a `mode.<id>` switch per mode,
    /// and every macro that is not also a built-in action.
    pub fn actions(&self, reg: &ExtensionRegistry) -> Vec<ActionSpec> {
        let mut out = Vec::new();
        for (order, id) in self.mode_order.iter().enumerate() {
            let mode = &self.modes[id];
            out.push(ActionSpec {
                id: format!("mode.{id}"),
                kind: ActionKind::Command,
                category: "Modes".into(),
                order: order as u16,
                title: format!("Switch to {}", mode.title).into(),
                cannot_unbind: false,
            });
        }
        let mut macros: Vec<&Macro> = self.macros.values().collect();
        macros.sort_by(|a, b| a.id.cmp(&b.id));
        for (order, action) in macros.into_iter().enumerate() {
            if reg.actions.iter().any(|spec| spec.id == action.id) {
                continue;
            }
            out.push(ActionSpec {
                id: action.id.clone(),
                kind: ActionKind::Command,
                category: "Custom".into(),
                order: order as u16,
                title: action.title.clone().into(),
                cannot_unbind: false,
            });
        }
        out
    }

    pub fn mode(&self, id: &str) -> Option<&ModeSpec> {
        self.modes.get(id)
    }

    /// Every mode in display order: notebook, then cell, then output.
    pub fn modes(&self) -> impl Iterator<Item = &ModeSpec> {
        self.mode_order.iter().filter_map(|id| self.modes.get(id))
    }

    pub fn macro_for(&self, id: &str) -> Option<&Macro> {
        self.macros.get(id)
    }

    pub fn title(&self, id: &str) -> String {
        self.titles.get(id).cloned().unwrap_or_else(|| id.to_string())
    }

    /// Rules for `action` that apply in `mode`, in the order they are checked
    /// (disabled ones included).
    pub fn rules_for(&self, mode: &str, action: &str) -> Vec<&Rule> {
        self.by_action
            .get(action)
            .into_iter()
            .flatten()
            .map(|&index| &self.rules[index])
            .filter(|rule| rule.applies_in(mode))
            .collect()
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// The first enabled rule for `action` in `mode` whose operator target
    /// and conditions match. `holds` answers each condition for the cursor.
    pub fn find_rule(
        &self,
        mode: &str,
        action: &str,
        target: Option<&str>,
        mut holds: impl FnMut(&Condition) -> bool,
    ) -> Option<&Rule> {
        self.rules_for(mode, action).into_iter().find(|rule| {
            rule.enabled
                && rule.with.as_ref().is_none_or(|with| {
                    target.is_some_and(|target| with.iter().any(|id| id == target))
                })
                && rule.when.iter().all(&mut holds)
        })
    }

    /// Describe steps in words, such as "→ NORMAL, Line end, → INSERT, Move right".
    pub fn describe_steps(&self, steps: &[Step], trigger: &str) -> String {
        if steps.is_empty() {
            return "does nothing".into();
        }
        steps
            .iter()
            .map(|step| match step {
                Step::Trigger => self.title(trigger),
                Step::Mode(mode) => format!(
                    "→ {}",
                    self.modes.get(mode).map_or(mode.as_str(), |mode| mode.badge.as_str())
                ),
                Step::Action { id, arg: Some(c) } => format!("{} '{c}'", self.title(id)),
                Step::Action { id, arg: None } => self.title(id),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn describe_rule(&self, rule: &Rule, trigger: &str) -> String {
        rule.title
            .clone()
            .unwrap_or_else(|| self.describe_steps(&rule.steps, trigger))
    }

    /// What `action` does in `mode`, one case per rule, then the fallback.
    pub fn cases(&self, mode: &str, action: &str, include_disabled: bool) -> Vec<Case> {
        let mut cases = Vec::new();
        let mut unconditional = false;
        for rule in self.rules_for(mode, action) {
            if !rule.enabled && !include_disabled {
                continue;
            }
            let mut when = describe_conditions(&rule.when);
            if let Some(with) = &rule.with {
                let targets = with.iter().map(|id| self.title(id)).collect::<Vec<_>>().join(" / ");
                when = if when.is_empty() {
                    format!("with {targets}")
                } else {
                    format!("{when}, with {targets}")
                };
            }
            if rule.enabled && when.is_empty() {
                unconditional = true;
            }
            cases.push(Case {
                when,
                what: self.describe_rule(rule, action),
                rule_id: Some(rule.id.clone()),
                enabled: rule.enabled,
                from_user: rule.from_user,
            });
        }
        if !unconditional {
            let what = match self.macros.get(action) {
                Some(action_macro) => self.describe_steps(&action_macro.steps, action),
                None => self.title(action),
            };
            cases.push(Case {
                when: if cases.is_empty() { String::new() } else { "otherwise".into() },
                what,
                rule_id: None,
                enabled: true,
                from_user: self.macros.get(action).is_some_and(|m| m.from_user),
            });
        }
        cases
    }

    /// True when anything but the built-in primitive decides what `action`
    /// does in `mode`.
    pub fn is_customized(&self, mode: &str, action: &str) -> bool {
        self.macros.contains_key(action) || self.rules_for(mode, action).iter().any(|rule| rule.enabled)
    }
}

/// Conditions in words, such as "source row, empty cell".
pub fn describe_conditions(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .map(|condition| match condition {
            Condition::Part(CellPart::Source) => "source row".to_string(),
            Condition::Part(CellPart::Output) => "output row".to_string(),
            Condition::Cell(kind) => format!("{kind} cell"),
            Condition::Empty(true) => "empty cell".to_string(),
            Condition::Empty(false) => "cell with text".to_string(),
            Condition::Outputs(true) => "has outputs".to_string(),
            Condition::Outputs(false) => "no outputs".to_string(),
            Condition::Line(LinePos::First) => "first line".to_string(),
            Condition::Line(LinePos::Last) => "last line".to_string(),
            Condition::Line(LinePos::Only) => "only line".to_string(),
            Condition::Count(true) => "with a count".to_string(),
            Condition::Count(false) => "without a count".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Default and user tables merged entry by entry: a user entry with a
/// default's name changes only the fields it sets.
#[derive(Default)]
struct Merged {
    modes: BTreeMap<String, toml::Table>,
    keysets: BTreeMap<String, toml::Table>,
    actions: BTreeMap<String, toml::Table>,
    user_actions: HashSet<String>,
    groups: BTreeMap<String, Vec<String>>,
    /// Defaults in file order; user rules with new ids go first.
    rules: Vec<(toml::Table, bool)>,
    user_new_rules: usize,
}

impl Merged {
    fn apply(&mut self, doc: &Value, user: bool, owner: &str, warnings: &mut Vec<ConfigWarning>) {
        for (section, target) in [
            ("modes", &mut self.modes),
            ("keysets", &mut self.keysets),
            ("actions", &mut self.actions),
        ] {
            let Some(entries) = doc.get(section) else { continue };
            let Some(entries) = entries.as_table() else {
                warnings.push(ConfigWarning {
                    source_key: format!("{owner}.{section}"),
                    message: "expected a table".into(),
                });
                continue;
            };
            for (id, entry) in entries {
                let Some(entry) = entry.as_table() else {
                    warnings.push(ConfigWarning {
                        source_key: format!("{owner}.{section}.{id}"),
                        message: "expected a table".into(),
                    });
                    continue;
                };
                let slot = target.entry(id.clone()).or_default();
                for (key, value) in entry {
                    slot.insert(key.clone(), value.clone());
                }
                if user && section == "actions" {
                    self.user_actions.insert(id.clone());
                }
            }
        }
        if let Some(groups) = doc.get("groups").and_then(Value::as_table) {
            for (name, members) in groups {
                let members = members
                    .as_array()
                    .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect());
                match members {
                    Some(members) => {
                        self.groups.insert(name.clone(), members);
                    }
                    None => warnings.push(ConfigWarning {
                        source_key: format!("{owner}.groups.{name}"),
                        message: "a group is a list of action ids".into(),
                    }),
                }
            }
        }
        let Some(rules) = doc.get("rules") else { return };
        let Some(rules) = rules.as_array() else {
            warnings.push(ConfigWarning {
                source_key: format!("{owner}.rules"),
                message: "write rules as [[rules]] tables".into(),
            });
            return;
        };
        for rule in rules {
            let Some(rule) = rule.as_table() else { continue };
            let id = rule.get("id").and_then(Value::as_str);
            let existing = id.and_then(|id| {
                self.rules
                    .iter()
                    .position(|(table, _)| table.get("id").and_then(Value::as_str) == Some(id))
            });
            match existing {
                Some(index) => {
                    let (table, from_user) = &mut self.rules[index];
                    for (key, value) in rule {
                        table.insert(key.clone(), value.clone());
                    }
                    *from_user |= user;
                }
                None if user => {
                    self.rules.insert(self.user_new_rules, (rule.clone(), true));
                    self.user_new_rules += 1;
                }
                None => self.rules.push((rule.clone(), false)),
            }
        }
    }
}

fn str_field(table: &toml::Table, key: &str) -> Option<String> {
    table.get(key).and_then(Value::as_str).map(str::to_string)
}

fn str_list(table: &toml::Table, key: &str) -> Option<Vec<String>> {
    table.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::BuiltinsPlugin;

    fn compile(user: &str) -> Controls {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        Controls::compile(&registry, user)
    }

    #[test]
    fn defaults_compile_cleanly() {
        let controls = compile("");
        assert!(controls.warnings.is_empty(), "{:?}", controls.warnings);
        assert_eq!(controls.mode("cell.insert").unwrap().kind, ModeKind::Insert);
        assert_eq!(controls.mode("cell.normal").unwrap().family(ModeKind::Insert), "cell.insert");
    }

    #[test]
    fn user_rules_go_first_and_overrides_keep_their_place() {
        let controls = compile(
            r#"
[[rules]]
id = "mine"
mode = "notebook.normal"
on = ["cell.enter_normal"]
do = ["mode.cell.insert"]

[[rules]]
id = "notebook.open_cell"
enabled = false
"#,
        );
        assert!(controls.warnings.is_empty(), "{:?}", controls.warnings);
        let ids: Vec<&str> = controls
            .rules_for("notebook.normal", "cell.enter_normal")
            .into_iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids[0], "mine");
        let open = controls.rule("notebook.open_cell").unwrap();
        assert!(!open.enabled && open.from_user);
        assert_eq!(open.steps, vec![Step::Mode("cell.normal".into())], "unset fields keep the default");
    }

    #[test]
    fn unknown_names_warn_and_drop_only_that_rule() {
        let controls = compile(
            r#"
[[rules]]
id = "bad"
mode = "cell.normal"
on = ["motion.nope"]
do = []

[[rules]]
id = "worse"
on = ["motion.down"]
when = { colour = "red" }
do = ["@action"]
"#,
        );
        assert_eq!(controls.warnings.len(), 2, "{:?}", controls.warnings);
        assert!(controls.rule("bad").is_none() && controls.rule("worse").is_none());
        assert!(controls.rule("notebook.open_cell").is_some());
    }

    #[test]
    fn macros_become_bindable_actions_and_describe_their_steps() {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        let controls = Controls::compile(
            &registry,
            r#"
[actions."my.run_twice"]
title = "Run twice"
do = ["cell.run", "cell.run"]
"#,
        );
        assert!(controls.warnings.is_empty(), "{:?}", controls.warnings);
        assert!(controls.actions(&registry).iter().any(|spec| spec.id == "my.run_twice"));
        let cases = controls.cases("notebook.normal", "my.run_twice", false);
        assert_eq!(cases[0].what, "Run cell, Run cell");
    }

    #[test]
    fn conditions_and_targets_select_the_rule() {
        let controls = compile("");
        let source = controls
            .find_rule("notebook.normal", "motion.line_end", None, |c| {
                *c == Condition::Part(CellPart::Source)
            })
            .unwrap();
        assert_eq!(source.id, "notebook.line_end_motion_appends");
        let output = controls
            .find_rule("notebook.normal", "motion.line_end", None, |c| {
                *c == Condition::Part(CellPart::Output)
            })
            .unwrap();
        assert_eq!(output.id, "notebook.line_motion_reads_output");
    }
}
