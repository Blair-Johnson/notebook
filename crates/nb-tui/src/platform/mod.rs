//! Injectable terminal, clipboard, and `tmux` effects (no `#[cfg(test)]` inside prod structs).

pub mod clipboard;
pub mod process;
pub mod tmux;

pub use clipboard::{ClipboardBackend, SystemClipboard};
pub use tmux::TmuxPaneTitle;

use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    DefaultUser,
    SteadyBlock,
    SteadyBar,
    SteadyUnderline,
}

pub trait ClipboardSink: Send {
    fn copy(&mut self, text: &str) -> Result<ClipboardBackend, String>;
}

pub trait TerminalEffects: Send {
    fn set_cursor_shape(&mut self, shape: CursorShape);
    fn set_mouse_capture(&mut self, enabled: bool);
    fn set_pane_title(&mut self, title: &str);
}

/// Deterministic in-memory clipboard sink for unit and coordinator tests.
#[derive(Clone, Default)]
pub struct MemoryClipboard {
    pub copied: Arc<Mutex<Vec<String>>>,
}

impl ClipboardSink for MemoryClipboard {
    fn copy(&mut self, text: &str) -> Result<ClipboardBackend, String> {
        self.copied.lock().expect("poisoned").push(text.to_string());
        Ok(ClipboardBackend::Memory)
    }
}

/// No-op terminal effects implementation for headless tests.
#[derive(Default)]
pub struct NoopTerminalEffects;

impl TerminalEffects for NoopTerminalEffects {
    fn set_cursor_shape(&mut self, _shape: CursorShape) {}
    fn set_mouse_capture(&mut self, _enabled: bool) {}
    fn set_pane_title(&mut self, _title: &str) {}
}
