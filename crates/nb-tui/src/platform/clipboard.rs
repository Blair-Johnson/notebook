//! Three-tier clipboard copy (`arboard` -> `tmux set-buffer -w` / `load-buffer` -> OSC 52).

use crate::platform::ClipboardSink;
use crate::platform::process::run_with_timeout;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

const TMUX_ARGUMENT_LIMIT: usize = 64 * 1024;
const OSC52_LIMIT: usize = 100 * 1024;
const TMUX_BUFFER_NAME: &str = "nb-copy";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardBackend {
    NativeOs,
    TmuxBuffer,
    Osc52,
    Memory,
}

/// tmux answers in milliseconds; a hung server must not hang the UI.
const TMUX_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct SystemClipboard {
    /// Kept for the session: creating one per copy is slow on X11, and on
    /// X11 the copied text lives only as long as the instance.
    native: Option<arboard::Clipboard>,
}

impl ClipboardSink for SystemClipboard {
    fn copy(&mut self, text: &str) -> Result<ClipboardBackend, String> {
        let remote = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .into_iter()
            .any(|key| {
                std::env::var(key)
                    .ok()
                    .is_some_and(|value| !value.trim().is_empty())
            });
        let mut native_error = None;
        if !remote {
            if self.native.is_none() {
                match arboard::Clipboard::new() {
                    Ok(clipboard) => self.native = Some(clipboard),
                    Err(error) => native_error = Some(error.to_string()),
                }
            }
            if let Some(clipboard) = self.native.as_mut() {
                match clipboard.set_text(text.to_owned()) {
                    Ok(()) => return Ok(ClipboardBackend::NativeOs),
                    Err(error) => {
                        native_error = Some(error.to_string());
                        self.native = None;
                    }
                }
            }
        }

        if std::env::var_os("TMUX").is_some() {
            if text.len() <= TMUX_ARGUMENT_LIMIT
                && tmux(&["set-buffer", "-w", "-b", TMUX_BUFFER_NAME, "--", text], b"")
            {
                return Ok(ClipboardBackend::TmuxBuffer);
            }
            if tmux(&["load-buffer", "-b", TMUX_BUFFER_NAME, "-"], text.as_bytes()) {
                return Ok(ClipboardBackend::TmuxBuffer);
            }
        }

        if text.len() > OSC52_LIMIT {
            return Err(native_error.map_or_else(
                || format!("clipboard payload exceeds {OSC52_LIMIT} bytes"),
                |error| {
                    format!("native clipboard failed: {error}; payload exceeds {OSC52_LIMIT} bytes")
                },
            ));
        }
        let sequence = format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()));
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(|error| {
                native_error.map_or_else(
                    || format!("OSC 52 clipboard failed: {error}"),
                    |native| format!("native clipboard failed: {native}; OSC 52 failed: {error}"),
                )
            })?;
        Ok(ClipboardBackend::Osc52)
    }
}

/// Run a tmux command with `input` on stdin; true if it succeeded in time.
fn tmux(args: &[&str], input: &[u8]) -> bool {
    run_with_timeout(Command::new("tmux").args(args), input, TMUX_TIMEOUT)
        .is_ok_and(|done| done.status.success())
}
