//! Crossterm terminal setup, teardown, and injected coordinator effects.

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::{execute, queue};
use std::io::{self, Write, stdout};
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode};
use nb_tui::platform::{CursorShape, TerminalEffects};
use nb_tui::terminal_output::{AppTerminal, shutdown_active_writer, write_escape_via_active_writer};

const ENABLE_SGR_MOUSE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// Real terminal effects used by the coordinator in the interactive binary.
pub struct CrosstermTerminalEffects {
    last_cursor_shape: Option<CursorShape>,
    last_mouse_capture: Option<bool>,
}

impl CrosstermTerminalEffects {
    pub fn new() -> Self {
        Self {
            last_cursor_shape: None,
            last_mouse_capture: None,
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
        let mut bytes = Vec::new();
        if queue!(&mut bytes, command).is_ok() && !write_escape_via_active_writer(&bytes) {
            let mut out = stdout();
            let _ = out.write_all(&bytes);
            let _ = out.flush();
        }
        self.last_cursor_shape = Some(shape);
    }

    fn set_mouse_capture(&mut self, enabled: bool) {
        if self.last_mouse_capture == Some(enabled) {
            return;
        }
        if enabled {
            if !write_escape_via_active_writer(ENABLE_SGR_MOUSE) {
                enable_mouse();
            }
        } else {
            let mut bytes = Vec::new();
            if queue!(&mut bytes, DisableMouseCapture).is_ok()
                && !write_escape_via_active_writer(&bytes)
            {
                let _ = execute!(stdout(), DisableMouseCapture);
            }
        }
        self.last_mouse_capture = Some(enabled);
    }
}

/// Mouse reporting for clicks, drags and the wheel, in SGR encoding.
fn enable_mouse() {
    let mut out = stdout();
    let _ = out.write_all(ENABLE_SGR_MOUSE);
    let _ = out.flush();
}

/// Physically blank the terminal without issuing a cursor-position query.
pub fn clear_screen() {
    let _ = execute!(stdout(), Clear(ClearType::All));
}

/// Enter raw mode, alternate screen, panic hook, and non-blocking `AppTerminal`.
pub fn init_app_terminal(mouse_capture: bool) -> io::Result<AppTerminal> {
    std::mem::forget(ratatui::init());
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        shutdown_active_writer();
        prev_hook(info);
    }));
    clear_screen();
    enter_extras(mouse_capture);
    AppTerminal::spawn_stdout()
}

/// Flush/delete uploaded images and restore normal terminal state before `$EDITOR`.
pub fn suspend_for_editor(terminal: &AppTerminal) {
    terminal.shutdown();
    leave_extras();
    ratatui::restore();
}

/// Re-enter raw mode, alternate screen, and spawn a fresh `AppTerminal` after `$EDITOR`.
pub fn resume_from_editor(terminal: &mut AppTerminal, mouse_capture: bool) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, Clear(ClearType::All))?;
    enter_extras(mouse_capture);
    *terminal = AppTerminal::spawn_stdout()?;
    Ok(())
}

/// Cleanly flush remaining UI frames, delete uploaded Kitty images, and restore
/// every terminal mode on exit or caught panic.
pub fn restore_app_terminal(terminal: AppTerminal) {
    terminal.shutdown();
    leave_extras();
    reset_cursor_shape();
    let _ = ratatui::try_restore();
    let _ = execute!(stdout(), Show);
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
/// previous crashed session.
pub fn leave_extras() {
    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = execute!(stdout(), DisableFocusChange);
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
}

pub fn reset_cursor_shape() {
    let _ = execute!(stdout(), SetCursorStyle::DefaultUserShape);
}
