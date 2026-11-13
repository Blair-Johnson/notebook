//! Shared multi-level `VimGrammar` engine (counts, operators, motions, text objects, `jj` chord).

use crate::keymap::Keymap;
use std::time::{Duration, Instant};
use wb_sdk::{Key, KeyContextId, OperatorId, ResolvedInput};

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

    /// Feed one normalized `Key` in `ctx` at timestamp `now`, returning zero or more
    /// `GrammarStep`s (for instance `[RollbackChars(1), Command("focus.up")]` on `jj`).
    pub fn feed(
        &mut self,
        _keymap: &Keymap,
        _ctx: &KeyContextId,
        _key: Key,
        _now: Instant,
    ) -> Vec<GrammarStep> {
        todo!("wb-tui::grammar: shared count/operator/motion/textobj + optimistic chord state machine")
    }
}
