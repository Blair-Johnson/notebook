//! ANSI escape sequences in output, parsed to styles that sit on the theme.
//!
//! `ansi_to_tui` turns a reset (`\e[0m`, `\e[m`) or a default color
//! (`\e[39m`, `\e[49m`) into `Color::Reset`, which ratatui draws as the
//! terminal's own default color. Inside a themed card that shows up as
//! patches of the terminal's background behind colored `ls` output or a
//! traceback. Here "default" means "no color of its own": the span inherits
//! the card's colors, like plain text does.

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;

/// Parse `text` for display on the theme; `None` if it is not valid ANSI.
pub fn parse(text: &str) -> Option<Text<'static>> {
    let mut parsed = text.into_text().ok()?;
    parsed.style = inherit_defaults(parsed.style);
    for line in &mut parsed.lines {
        line.style = inherit_defaults(line.style);
        for span in &mut line.spans {
            span.style = inherit_defaults(span.style);
        }
    }
    Some(parsed)
}

/// Drop the "terminal default" parts of a style so the theme shows through.
fn inherit_defaults(mut style: Style) -> Style {
    let reset = Some(Color::Reset);
    if style.fg == reset {
        style.fg = None;
    }
    if style.bg == reset {
        style.bg = None;
    }
    if style.underline_color == reset {
        style.underline_color = None;
    }
    // A reset also removes every modifier, including ones the card itself
    // set; only the modifiers the text turns on matter.
    style.sub_modifier = Modifier::empty();
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resets_and_default_colors_inherit_the_theme() {
        // As `ls` writes it: a blue name, then resets, then plain text.
        let text = parse("\u{1b}[34martifacts\u{1b}[m\u{1b}[m   model.mlirbc \u{1b}[49mx\u{1b}[39my")
            .expect("valid ANSI");
        let spans = &text.lines[0].spans;
        assert!(!spans.is_empty());
        for span in spans {
            assert_ne!(span.style.bg, Some(Color::Reset), "{span:?}");
            assert_ne!(span.style.fg, Some(Color::Reset), "{span:?}");
        }
        let blue = spans.iter().find(|span| span.content.contains("artifacts")).unwrap();
        assert_eq!(blue.style.fg, Some(Color::Blue));
        assert_eq!(blue.style.bg, None);
    }
}
