//! Snapshot a live tmux pane: `capture-pane -e` (ANSI) parsed back into a
//! ratatui [`Buffer`], plus the pane's real cursor position.

use crate::image::{CursorMark, CursorShape};
use crate::script::{Step, tmux_key};
use anyhow::{Context, Result, bail};
use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::process::Command;
use std::time::Duration;

fn tmux(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux").args(args).output().context("running tmux")?;
    if !output.status.success() {
        bail!(
            "tmux {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse ANSI text (as from `capture-pane -e -p`) into a buffer.
pub fn ansi_to_buffer(ansi: &[u8], width: u16, height: u16) -> Result<Buffer> {
    let text = ansi.into_text().context("parsing ANSI capture")?;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    for (y, line) in text.lines.iter().enumerate().take(usize::from(height)) {
        buffer.set_line(0, y as u16, line, width);
    }
    Ok(buffer)
}

/// Capture the visible screen of `pane` (e.g. `%53`).
pub fn capture_pane(pane: &str) -> Result<(Buffer, Option<CursorMark>)> {
    let info = tmux(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_width} #{pane_height} #{cursor_x} #{cursor_y} #{cursor_flag}",
    ])?;
    let fields: Vec<u16> = info
        .split_whitespace()
        .map(|field| field.parse().unwrap_or(0))
        .collect();
    let [width, height, cx, cy, visible] = fields[..] else {
        bail!("unexpected tmux pane info: {info:?}");
    };
    let output = Command::new("tmux")
        .args(["capture-pane", "-e", "-p", "-N", "-t", pane])
        .output()
        .context("capture-pane")?;
    let buffer = ansi_to_buffer(&output.stdout, width, height)?;
    let cursor = (visible == 1).then_some(CursorMark {
        x: cx,
        y: cy,
        shape: CursorShape::Block,
    });
    Ok((buffer, cursor))
}

/// Send one script step to a pane. Escape is followed by a pause so the
/// application does not read the next byte as part of an Alt sequence.
pub fn send_step(pane: &str, step: &Step) -> Result<()> {
    match step {
        Step::Key(event) => {
            let Some((literal, key)) = tmux_key(event) else {
                bail!("cannot send {event:?} through tmux");
            };
            if literal {
                tmux(&["send-keys", "-t", pane, "-l", &key])?;
            } else {
                tmux(&["send-keys", "-t", pane, &key])?;
            }
            let pause = if key == "Escape" { 120 } else { 25 };
            std::thread::sleep(Duration::from_millis(pause));
        }
        Step::Wait(ms) => std::thread::sleep(Duration::from_millis(*ms)),
        Step::Snap(_) => {}
    }
    Ok(())
}
