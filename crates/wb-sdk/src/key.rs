//! Normalized key representations and hierarchical `KeyContextSpec` definitions.

use std::borrow::Cow;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct KeyContextId(pub Cow<'static, str>);

impl KeyContextId {
    pub const fn new_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for KeyContextId {
    fn from(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }
}

/// Canonical, terminal-normalized key event used by `Keymap` and `VimGrammar`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

impl Key {
    pub fn printable(self) -> Option<char> {
        match self {
            Self::Char(c) => Some(c),
            _ => None,
        }
    }
}

pub type KeySeq = Vec<Key>;

/// Metadata for an input context, supporting `vim.normal` base inheritance
/// and operator-pending text-object sub-contexts (`text.pending`).
#[derive(Clone, Debug)]
pub struct KeyContextSpec {
    pub id: KeyContextId,
    pub title: Cow<'static, str>,
    pub parents: Vec<KeyContextId>,
    pub text_entry: bool,
    pub operator_pending: Option<KeyContextId>,
}
