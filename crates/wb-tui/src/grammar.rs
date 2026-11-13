//! Shared multi-level `VimGrammar` engine (counts, operators, motions, text objects, `jj` chord).

use crate::keymap::{Keymap, LookupResult};
use std::borrow::Cow;
use std::time::{Duration, Instant};
use wb_sdk::{
    ActionKind, CommandId, Key, KeyContextId, MotionId, MotionOrObject, OperatorId, ResolvedInput,
    TextObjectId,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GrammarStep {
    Resolved(ResolvedInput),
    Pending,
    Cancelled,
    Unhandled(Key),
}

#[derive(Debug)]
pub struct VimGrammar {
    count: Option<u32>,
    pending_op: Option<(OperatorId, Option<u32>)>,
    seq: Vec<Key>,
    seq_started: Option<Instant>,
    speculative_chars: usize,
    pending_char_arg: Option<(String, Option<u32>)>,
    pub insert_escape_chord: Duration,
}

impl Default for VimGrammar {
    fn default() -> Self {
        Self {
            count: None,
            pending_op: None,
            seq: Vec::new(),
            seq_started: None,
            speculative_chars: 0,
            pending_char_arg: None,
            insert_escape_chord: Duration::from_millis(350),
        }
    }
}

impl VimGrammar {
    pub fn reset(&mut self) {
        let chord = self.insert_escape_chord;
        *self = Self {
            insert_escape_chord: chord,
            ..Self::default()
        };
    }

    pub fn is_pending(&self) -> bool {
        self.count.is_some()
            || self.pending_op.is_some()
            || !self.seq.is_empty()
            || self.pending_char_arg.is_some()
    }

    pub fn speculative_state(&self) -> (usize, Option<Instant>) {
        (self.speculative_chars, self.seq_started)
    }

    /// Expire an unresolved sequence.  Speculatively inserted characters are
    /// deliberately retained; only the grammar state is discarded.
    pub fn expire_chord(&mut self, now: Instant) -> bool {
        let Some(started) = self.seq_started else {
            return false;
        };
        if now
            .checked_duration_since(started)
            .is_some_and(|elapsed| elapsed >= self.insert_escape_chord)
        {
            self.seq.clear();
            self.seq_started = None;
            self.speculative_chars = 0;
            return true;
        }
        false
    }

    /// Feed one normalized `Key` in `ctx` at timestamp `now`, returning zero or more
    /// `GrammarStep`s (for instance `[RollbackChars(1), Command("focus.up")]` on `jj`).
    pub fn feed(
        &mut self,
        keymap: &Keymap,
        ctx: &KeyContextId,
        key: Key,
        now: Instant,
    ) -> Vec<GrammarStep> {
        let text_entry = keymap.context_spec(ctx).is_some_and(|spec| spec.text_entry);
        if text_entry {
            self.feed_text_entry(keymap, ctx, key, now)
        } else {
            self.feed_normal(keymap, ctx, key, now)
        }
    }

    fn feed_text_entry(
        &mut self,
        keymap: &Keymap,
        ctx: &KeyContextId,
        key: Key,
        now: Instant,
    ) -> Vec<GrammarStep> {
        if !self.seq.is_empty() {
            let _ = self.expire_chord(now);
        }

        if self.seq.is_empty() {
            self.feed_text_start(keymap, ctx, key, now)
        } else {
            let mut candidate = self.seq.clone();
            candidate.push(key);
            match keymap.lookup(ctx, false, &candidate) {
                LookupResult::Exact(action) => {
                    let speculative = self.speculative_chars;
                    self.clear_sequence();
                    let mut steps = Vec::new();
                    if speculative > 0 {
                        steps.push(GrammarStep::Resolved(ResolvedInput::RollbackChars(
                            speculative,
                        )));
                    }
                    if let Some(resolved) = self.resolve_command_action(keymap, &action, None, None)
                    {
                        steps.push(GrammarStep::Resolved(resolved));
                    } else {
                        steps.push(GrammarStep::Unhandled(key));
                    }
                    steps
                }
                LookupResult::Prefix => {
                    self.seq = candidate;
                    if self.seq_started.is_none() {
                        self.seq_started = Some(now);
                    }
                    if let Some(c) = key.printable() {
                        self.speculative_chars = self.speculative_chars.saturating_add(1);
                        vec![GrammarStep::Resolved(ResolvedInput::InsertText(
                            c.to_string(),
                        ))]
                    } else {
                        vec![GrammarStep::Pending]
                    }
                }
                LookupResult::None => {
                    // The already inserted speculative prefix remains literal.  Process
                    // the breaking key independently, so `jk` emits both characters.
                    self.clear_sequence();
                    self.feed_text_start(keymap, ctx, key, now)
                }
            }
        }
    }

    fn feed_text_start(
        &mut self,
        keymap: &Keymap,
        ctx: &KeyContextId,
        key: Key,
        now: Instant,
    ) -> Vec<GrammarStep> {
        let candidate = [key];
        match keymap.lookup(ctx, false, &candidate) {
            LookupResult::Exact(action) => {
                if let Some(resolved) = self.resolve_command_action(keymap, &action, None, None) {
                    vec![GrammarStep::Resolved(resolved)]
                } else {
                    vec![GrammarStep::Unhandled(key)]
                }
            }
            LookupResult::Prefix => {
                self.seq.push(key);
                self.seq_started = Some(now);
                if let Some(c) = key.printable() {
                    self.speculative_chars = 1;
                    vec![GrammarStep::Resolved(ResolvedInput::InsertText(
                        c.to_string(),
                    ))]
                } else {
                    vec![GrammarStep::Pending]
                }
            }
            LookupResult::None => match key.printable() {
                Some(c) => vec![GrammarStep::Resolved(ResolvedInput::InsertText(
                    c.to_string(),
                ))],
                None => vec![GrammarStep::Unhandled(key)],
            },
        }
    }

    fn feed_normal(
        &mut self,
        keymap: &Keymap,
        ctx: &KeyContextId,
        key: Key,
        now: Instant,
    ) -> Vec<GrammarStep> {
        if key == Key::Esc && self.is_pending() {
            self.reset();
            return vec![GrammarStep::Cancelled];
        }

        if let Some((action, count)) = self.pending_char_arg.clone() {
            if let Some(argument) = key.printable() {
                self.reset();
                return vec![GrammarStep::Resolved(ResolvedInput::Command {
                    id: command_id(&action),
                    count,
                    arg: Some(argument),
                })];
            }
            self.reset();
            return if key == Key::Esc {
                vec![GrammarStep::Cancelled]
            } else {
                vec![GrammarStep::Unhandled(key)]
            };
        }

        if key.printable().is_some_and(|c| c.is_ascii_digit()) && self.seq.is_empty() {
            let digit = key
                .printable()
                .expect("checked above")
                .to_digit(10)
                .unwrap_or(0);
            if self.pending_op.is_some() || self.count.is_some() || digit != 0 {
                self.count = Some(
                    self.count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit),
                );
                return vec![GrammarStep::Pending];
            }
        }

        let mut candidate = self.seq.clone();
        candidate.push(key);
        let lookup = keymap.lookup(ctx, self.pending_op.is_some(), &candidate);
        match lookup {
            LookupResult::Prefix => {
                self.seq = candidate;
                if self.seq_started.is_none() {
                    self.seq_started = Some(now);
                }
                vec![GrammarStep::Pending]
            }
            LookupResult::None => {
                self.clear_sequence();
                if self.pending_op.is_some() {
                    let had_operator = self.pending_op.take();
                    self.count = None;
                    if had_operator.is_some() {
                        return vec![GrammarStep::Unhandled(key)];
                    }
                }
                vec![GrammarStep::Unhandled(key)]
            }
            LookupResult::Exact(action) => self.resolve_exact(keymap, ctx, &action),
        }
    }

    fn resolve_exact(
        &mut self,
        keymap: &Keymap,
        _ctx: &KeyContextId,
        action: &str,
    ) -> Vec<GrammarStep> {
        let Some(kind) = keymap.action_spec(action).map(|spec| spec.kind) else {
            self.reset();
            return vec![];
        };
        let count = self.count.take();
        self.clear_sequence();

        if let Some((pending_operator, operator_count)) = self.pending_op.clone() {
            match kind {
                ActionKind::Operator if pending_operator.0.as_ref() == action => {
                    let motion_count = count.unwrap_or(1);
                    let total = operator_count.unwrap_or(1).saturating_mul(motion_count);
                    self.reset();
                    return vec![GrammarStep::Resolved(ResolvedInput::OperateSelf {
                        op: pending_operator,
                        count: total,
                    })];
                }
                ActionKind::Motion => {
                    let total = operator_count
                        .unwrap_or(1)
                        .saturating_mul(count.unwrap_or(1));
                    self.reset();
                    return vec![GrammarStep::Resolved(ResolvedInput::Operate {
                        op: pending_operator,
                        target: MotionOrObject::Motion(motion_id(action)),
                        count: total,
                    })];
                }
                ActionKind::TextObject => {
                    let total = operator_count
                        .unwrap_or(1)
                        .saturating_mul(count.unwrap_or(1));
                    self.reset();
                    return vec![GrammarStep::Resolved(ResolvedInput::Operate {
                        op: pending_operator,
                        target: MotionOrObject::TextObject(text_object_id(action)),
                        count: total,
                    })];
                }
                _ => {
                    self.reset();
                    return vec![GrammarStep::Unhandled(Key::Esc)];
                }
            }
        }

        match kind {
            ActionKind::Operator => {
                self.pending_op = Some((operator_id(action), count));
                vec![GrammarStep::Pending]
            }
            ActionKind::Motion => {
                self.reset();
                vec![GrammarStep::Resolved(ResolvedInput::Move {
                    motion: motion_id(action),
                    count: count.unwrap_or(1),
                })]
            }
            ActionKind::CharArgCommand => {
                self.pending_char_arg = Some((action.to_string(), count));
                vec![GrammarStep::Pending]
            }
            ActionKind::Command => {
                self.reset();
                vec![GrammarStep::Resolved(ResolvedInput::Command {
                    id: command_id(action),
                    count,
                    arg: None,
                })]
            }
            ActionKind::TextObject => {
                self.reset();
                vec![GrammarStep::Unhandled(Key::Esc)]
            }
        }
    }

    fn resolve_command_action(
        &mut self,
        keymap: &Keymap,
        action: &str,
        count: Option<u32>,
        arg: Option<char>,
    ) -> Option<ResolvedInput> {
        let spec = keymap.action_spec(action)?;
        match spec.kind {
            ActionKind::Command | ActionKind::CharArgCommand => Some(ResolvedInput::Command {
                id: command_id(action),
                count,
                arg,
            }),
            ActionKind::Motion => Some(ResolvedInput::Move {
                motion: motion_id(action),
                count: count.unwrap_or(1),
            }),
            _ => None,
        }
    }

    fn clear_sequence(&mut self) {
        self.seq.clear();
        self.seq_started = None;
        self.speculative_chars = 0;
    }
}

fn command_id(id: &str) -> CommandId {
    CommandId(Cow::Owned(id.to_string()))
}

fn operator_id(id: &str) -> OperatorId {
    OperatorId(Cow::Owned(id.to_string()))
}

fn motion_id(id: &str) -> MotionId {
    MotionId(Cow::Owned(id.to_string()))
}

fn text_object_id(id: &str) -> TextObjectId {
    TextObjectId(Cow::Owned(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::BuiltinsPlugin;
    use wb_sdk::ExtensionRegistry;

    fn keymap() -> Keymap {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        Keymap::compile(&registry, "")
    }

    fn ctx(id: &'static str) -> KeyContextId {
        KeyContextId::new_static(id)
    }

    #[test]
    fn counts_multiply_and_text_objects_use_pending_context() {
        let map = keymap();
        let mut grammar = VimGrammar::default();
        let now = Instant::now();
        assert!(matches!(
            grammar.feed(&map, &ctx("cell.normal"), Key::Char('2'), now)[0],
            GrammarStep::Pending
        ));
        assert!(matches!(
            grammar.feed(&map, &ctx("cell.normal"), Key::Char('d'), now)[0],
            GrammarStep::Pending
        ));
        let result = grammar.feed(&map, &ctx("cell.normal"), Key::Char('3'), now);
        assert!(matches!(result[0], GrammarStep::Pending));
        let result = grammar.feed(&map, &ctx("cell.normal"), Key::Char('j'), now);
        assert!(matches!(
            result[0],
            GrammarStep::Resolved(ResolvedInput::Operate { count: 6, .. })
        ));

        let mut grammar = VimGrammar::default();
        grammar.feed(&map, &ctx("cell.normal"), Key::Char('d'), now);
        grammar.feed(&map, &ctx("cell.normal"), Key::Char('i'), now);
        let result = grammar.feed(&map, &ctx("cell.normal"), Key::Char('w'), now);
        assert!(matches!(
            result[0],
            GrammarStep::Resolved(ResolvedInput::Operate {
                target: MotionOrObject::TextObject(_),
                ..
            })
        ));
    }
}
