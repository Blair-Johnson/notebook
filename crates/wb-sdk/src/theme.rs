//! Semantic UI color palette passed explicitly into every measurement and draw call.
//!
//! Architectural invariant: never store `Theme` in a `OnceLock` or `thread_local!`.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Palette {
    pub bg: Color,
    pub surface: Color,
    pub panel_bg: Color,
    pub panel_selection: Color,
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focused: Color,
    pub accent_code: Color,
    pub accent_markdown: Color,
    pub accent_raw: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg: Color::Reset,
            surface: Color::DarkGray,
            panel_bg: Color::Black,
            panel_selection: Color::Blue,
            fg: Color::White,
            muted: Color::Gray,
            border: Color::DarkGray,
            border_focused: Color::Cyan,
            accent_code: Color::Cyan,
            accent_markdown: Color::Magenta,
            accent_raw: Color::Yellow,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Theme {
    pub name: String,
    pub revision: u64,
    pub syntect_theme: String,
    pub palette: Palette,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "terminal".into(),
            revision: 1,
            syntect_theme: "base16-ocean.dark".into(),
            palette: Palette::default(),
        }
    }
}

impl Theme {
    pub fn cursor_inverse(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}
