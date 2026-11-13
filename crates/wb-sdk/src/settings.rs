//! Declarative setting specifications shared by `config.toml` and the settings screen.

use std::borrow::Cow;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SettingId(pub Cow<'static, str>);

#[derive(Clone, PartialEq, Debug)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Clone, Debug)]
pub enum SettingKind {
    Bool { default: bool },
    Int { default: i64, min: i64, max: i64 },
    Choice { default: &'static str, options: &'static [&'static str] },
    Text { default: &'static str },
}

#[derive(Clone, Debug)]
pub struct SettingSpec {
    pub id: SettingId,
    pub section: Cow<'static, str>,
    pub title: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub kind: SettingKind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigWarning {
    pub source_key: String,
    pub message: String,
}
