//! Semantic UI color palette passed explicitly into every measurement and draw call.
//!
//! Architectural invariant: never store `Theme` in a `OnceLock` or `thread_local!`.

use ratatui::style::{Color, Modifier, Style};
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Palette {
    /// Notebook body background (`Reset` lets the terminal's show through).
    pub bg: Color,
    /// Raised surfaces: top bar, status bar, popups.
    pub surface: Color,
    /// Background of code source inside a cell card.
    pub code_bg: Color,
    /// Current-line highlight while editing.
    pub cursor_line: Color,
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
            code_bg: Color::Reset,
            cursor_line: Color::DarkGray,
            panel_bg: Color::Black,
            panel_selection: Color::Blue,
            fg: Color::Reset,
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
    /// Changes whenever any color does, so cached renders are redrawn.
    pub revision: u64,
    pub syntect_theme: String,
    pub palette: Palette,
}

impl Theme {
    pub fn new(name: impl Into<String>, syntect_theme: impl Into<String>, palette: Palette) -> Self {
        let name = name.into();
        let syntect_theme = syntect_theme.into();
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        syntect_theme.hash(&mut hasher);
        format!("{palette:?}").hash(&mut hasher);
        Self {
            name,
            revision: hasher.finish(),
            syntect_theme,
            palette,
        }
    }

    /// Recompute the revision after editing `syntect_theme` or `palette`.
    pub fn rehash(self) -> Self {
        Self::new(self.name, self.syntect_theme, self.palette)
    }

    pub fn cursor_inverse(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::new("terminal", "base16-ocean.dark", Palette::default())
    }
}
