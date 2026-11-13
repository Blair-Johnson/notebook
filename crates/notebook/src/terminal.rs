//! Crossterm terminal setup, teardown, and injected coordinator effects.

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use std::io::Write;
use crossterm::terminal::{Clear, ClearType};
use std::io::stdout;
use nb_tui::platform::{CursorShape, TerminalEffects, TmuxPaneTitle};

/// Real terminal effects used by the coordinator in the interactive binary.
pub struct CrosstermTerminalEffects {
    last_cursor_shape: Option<CursorShape>,
    last_mouse_capture: Option<bool>,
    pane_title: TmuxPaneTitle,
}

impl CrosstermTerminalEffects {
    pub fn new() -> Self {
        Self {
            last_cursor_shape: None,
            last_mouse_capture: None,
            pane_title: TmuxPaneTitle::detect(),
        }
    }
}

impl Default for CrosstermTerminalEffects {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalEffects for CrosstermTerminalEffects {
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        if self.last_cursor_shape == Some(shape) {
            return;
        }
        let command = match shape {
            CursorShape::DefaultUser => SetCursorStyle::DefaultUserShape,
            CursorShape::SteadyBlock => SetCursorStyle::SteadyBlock,
            CursorShape::SteadyBar => SetCursorStyle::SteadyBar,
            CursorShape::SteadyUnderline => SetCursorStyle::SteadyUnderScore,
        };
        let _ = execute!(stdout(), command);
        self.last_cursor_shape = Some(shape);
    }

    fn set_mouse_capture(&mut self, enabled: bool) {
        if self.last_mouse_capture == Some(enabled) {
            return;
        }
        if enabled {
            enable_mouse();
        } else {
            let _ = execute!(stdout(), DisableMouseCapture);
        }
        self.last_mouse_capture = Some(enabled);
    }

    fn set_pane_title(&mut self, title: &str) {
        self.pane_title.update(title);
    }
}

/// Mouse reporting for clicks, drags and the wheel, in SGR encoding.
/// crossterm's `EnableMouseCapture` also turns on any-motion tracking
/// (1003): a report for every pointer movement, which nb never uses and
/// which floods the terminal's input buffer.
fn enable_mouse() {
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    let _ = out.flush();
}

/// Physically blank the terminal without issuing a cursor-position query.
pub fn clear_screen() {
    let _ = execute!(stdout(), Clear(ClearType::All));
}

/// Enter the terminal modes used by the TUI. Kitty keyboard enhancement is
/// requested directly; terminals that do not support it ignore the sequence.
pub fn enter_extras(mouse_capture: bool) {
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    if mouse_capture {
        enable_mouse();
    }
    let _ = execute!(stdout(), EnableFocusChange);
    let _ = execute!(stdout(), EnableBracketedPaste);
}

/// Disable every mode the runtime may have enabled, including modes left by a
/// previous crashed session. Each command is attempted independently because a
/// closed PTY must not cause teardown to panic.
pub fn leave_extras() {
    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = execute!(stdout(), DisableFocusChange);
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
}

pub fn reset_cursor_shape() {
    let _ = execute!(stdout(), SetCursorStyle::DefaultUserShape);
}
