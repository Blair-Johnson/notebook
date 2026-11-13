//! Three-tier clipboard copy (`arboard` -> `tmux set-buffer -w` / `load-buffer` -> OSC 52).

use crate::platform::ClipboardSink;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::{self, Write};
use std::process::{Command, Stdio};

const TMUX_ARGUMENT_LIMIT: usize = 64 * 1024;
const OSC52_LIMIT: usize = 100 * 1024;
const TMUX_BUFFER_NAME: &str = "workbench-copy";

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
            match arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
            {
                Ok(()) => return Ok(ClipboardBackend::NativeOs),
                Err(error) => native_error = Some(error.to_string()),
            }
        }

        if std::env::var_os("TMUX").is_some() {
            if text.len() <= TMUX_ARGUMENT_LIMIT {
                let status = Command::new("tmux")
                    .args(["set-buffer", "-w", "-b", TMUX_BUFFER_NAME, "--", text])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if status.is_ok_and(|status| status.success()) {
                    return Ok(ClipboardBackend::TmuxBuffer);
                }
            }
            if load_tmux_buffer(text).is_ok_and(|success| success) {
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

fn load_tmux_buffer(text: &str) -> io::Result<bool> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-b", TMUX_BUFFER_NAME, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "tmux load-buffer stdin unavailable",
        ));
    };
    let write_result = stdin.write_all(text.as_bytes());
    drop(stdin);
    write_result?;
    child.wait().map(|status| status.success())
}
