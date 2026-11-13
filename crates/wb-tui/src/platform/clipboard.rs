//! Three-tier clipboard copy (`arboard` -> `tmux set-buffer -w` / `load-buffer` -> OSC 52).

use crate::platform::ClipboardSink;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardBackend {
    NativeOs,
    TmuxBuffer,
    Osc52,
    Memory,
}

#[derive(Default)]
pub struct SystemClipboard;

impl ClipboardSink for SystemClipboard {
    fn copy(&mut self, _text: &str) -> Result<ClipboardBackend, String> {
        todo!("wb-tui::platform::clipboard: try arboard (if non-SSH), then tmux buffer, then OSC 52")
    }
}
