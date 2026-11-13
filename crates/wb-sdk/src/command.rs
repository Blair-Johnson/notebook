//! Open action identifiers, action specs, and `VimGrammar` output sentences.

use std::borrow::Cow;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CommandId(pub Cow<'static, str>);

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct OperatorId(pub Cow<'static, str>);

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MotionId(pub Cow<'static, str>);

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextObjectId(pub Cow<'static, str>);

impl From<&'static str> for CommandId {
    fn from(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionKind {
    Command,
    Operator,
    Motion,
    TextObject,
    /// Command consuming one subsequent printable character argument (`m a`, `' a`, `r x`).
    CharArgCommand,
}

#[derive(Clone, Debug)]
pub struct ActionSpec {
    pub id: String,
    pub kind: ActionKind,
    pub category: Cow<'static, str>,
    pub order: u16,
    pub title: Cow<'static, str>,
    pub cannot_unbind: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MotionOrObject {
    Motion(MotionId),
    TextObject(TextObjectId),
}

/// Semantic sentence emitted by `VimGrammar` and dispatched to the active layer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolvedInput {
    Command {
        id: CommandId,
        count: Option<u32>,
        arg: Option<char>,
    },
    Move {
        motion: MotionId,
        count: u32,
    },
    OperateSelf {
        op: OperatorId,
        count: u32,
    },
    Operate {
        op: OperatorId,
        target: MotionOrObject,
        count: u32,
    },
    InsertText(String),
    RollbackChars(usize),
}
