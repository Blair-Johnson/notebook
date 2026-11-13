//! Key scripts in vim notation: `"ihello<c-j>world<esc><esc>:w<cr>"`.
//!
//! Besides keys, a script may contain `<wait:MS>` (let the app run: kernel
//! events, ticks) and `<snap:NAME>` (write an intermediate snapshot).

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Key(KeyEvent),
    Wait(u64),
    Snap(String),
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// A printable character as a terminal reports it (uppercase carries SHIFT).
pub fn char_key(c: char) -> KeyEvent {
    if c.is_ascii_uppercase() {
        key(KeyCode::Char(c), KeyModifiers::SHIFT)
    } else {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }
}

fn named(name: &str) -> Result<Step> {
    let lower = name.to_ascii_lowercase();
    if let Some(ms) = lower.strip_prefix("wait:") {
        return Ok(Step::Wait(ms.parse()?));
    }
    if let Some(snap) = name.strip_prefix("snap:") {
        return Ok(Step::Snap(snap.to_string()));
    }
    let plain = |code| Ok(Step::Key(key(code, KeyModifiers::NONE)));
    match lower.as_str() {
        "esc" => plain(KeyCode::Esc),
        "cr" | "enter" | "return" => plain(KeyCode::Enter),
        "tab" => plain(KeyCode::Tab),
        "s-tab" => Ok(Step::Key(key(KeyCode::BackTab, KeyModifiers::SHIFT))),
        "bs" | "backspace" => plain(KeyCode::Backspace),
        "del" | "delete" => plain(KeyCode::Delete),
        "space" => plain(KeyCode::Char(' ')),
        "lt" => plain(KeyCode::Char('<')),
        "up" => plain(KeyCode::Up),
        "down" => plain(KeyCode::Down),
        "left" => plain(KeyCode::Left),
        "right" => plain(KeyCode::Right),
        "home" => plain(KeyCode::Home),
        "end" => plain(KeyCode::End),
        "pgup" | "pageup" => plain(KeyCode::PageUp),
        "pgdn" | "pagedown" => plain(KeyCode::PageDown),
        _ => {
            let (modifier, rest) = if let Some(rest) = lower.strip_prefix("c-") {
                (KeyModifiers::CONTROL, rest)
            } else if let Some(rest) = lower.strip_prefix("a-").or_else(|| lower.strip_prefix("m-")) {
                (KeyModifiers::ALT, rest)
            } else {
                bail!("unknown key <{name}>");
            };
            let code = match rest {
                "cr" | "enter" => KeyCode::Enter,
                "space" => KeyCode::Char(' '),
                other => {
                    let mut chars = other.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => KeyCode::Char(c),
                        _ => bail!("unknown key <{name}>"),
                    }
                }
            };
            Ok(Step::Key(key(code, modifier)))
        }
    }
}

pub fn parse_script(script: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut rest = script;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            steps.push(named(&rest[1..end])?);
            rest = &rest[end + 1..];
            continue;
        }
        steps.push(Step::Key(char_key(c)));
        rest = &rest[c.len_utf8()..];
    }
    Ok(steps)
}

/// `tmux send-keys` arguments for one key: `(literal, key)`; literal keys go
/// through `send-keys -l` so characters like `;` or `#` are not interpreted.
pub fn tmux_key(event: &KeyEvent) -> Option<(bool, String)> {
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    let base = match event.code {
        KeyCode::Char(c) if !ctrl && !alt => return Some((true, c.to_string())),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Esc => "Escape".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BTab".into(),
        KeyCode::Backspace => "BSpace".into(),
        KeyCode::Delete => "DC".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PPage".into(),
        KeyCode::PageDown => "NPage".into(),
        _ => return None,
    };
    let prefix = match (ctrl, alt) {
        (true, true) => "C-M-",
        (true, false) => "C-",
        (false, true) => "M-",
        (false, false) => "",
    };
    Some((false, format!("{prefix}{base}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_waits_and_snaps() {
        let steps = parse_script("iA<c-j><esc><wait:120><snap:after>:w<cr><lt>").unwrap();
        assert_eq!(steps.len(), 10);
        assert_eq!(steps[1], Step::Key(char_key('A')));
        assert_eq!(steps[2], Step::Key(key(KeyCode::Char('j'), KeyModifiers::CONTROL)));
        assert_eq!(steps[4], Step::Wait(120));
        assert_eq!(steps[5], Step::Snap("after".into()));
        assert_eq!(steps[9], Step::Key(char_key('<')));
        assert!(parse_script("<nope>").is_err());
    }
}
