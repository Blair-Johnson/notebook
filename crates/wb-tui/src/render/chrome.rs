//! Top bar, cell gutters/headers, status-line priority cascade, and dynamic keymap hints.

use crate::focus::FocusStack;
use crate::keymap::Keymap;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::path::Path;
use wb_sdk::{BarSlot, StatusCtx, StatusSegment, Theme};

#[derive(Clone, Debug, Default)]
pub struct StatusCascade<'a> {
    pub fatal_error: Option<&'a str>,
    pub confirm_prompt: Option<&'a str>,
    pub copy_armed: bool,
    pub running_cells: usize,
    pub transient_notice: Option<&'a str>,
}

pub fn build_top_bar(
    ctx: &StatusCtx<'_>,
    segments: &[Box<dyn StatusSegment>],
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let path = ctx
        .path
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("[No notebook]");
    let dirty = if ctx.dirty { " ●" } else { "" };
    let kernel = if !ctx.kernel_alive {
        "KERNEL DEAD"
    } else if ctx.kernel_busy {
        "BUSY"
    } else {
        "IDLE"
    };
    let kernel_style = if !ctx.kernel_alive {
        Style::default().fg(theme.palette.error)
    } else if ctx.kernel_busy {
        Style::default().fg(theme.palette.warning)
    } else {
        Style::default().fg(theme.palette.success)
    };
    let mut spans = vec![
        Span::styled(
            format!(" {path}{dirty}"),
            Style::default()
                .fg(theme.palette.fg)
                .add_modifier(if ctx.dirty {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw("  "),
        Span::styled(format!("{kernel} · {}", ctx.kernel_name), kernel_style),
        Span::raw(format!("  [{}]", ctx.focus_badge)),
    ];
    let mut plugin_spans = segments
        .iter()
        .filter(|segment| matches!(segment.slot(), BarSlot::TopLeft | BarSlot::TopRight))
        .filter_map(|segment| segment.render(ctx, theme))
        .collect::<Vec<_>>();
    if !plugin_spans.is_empty() {
        spans.push(Span::raw("  "));
        spans.append(&mut plugin_spans);
    }
    let mut line = Line::from(spans);
    if line.width() > usize::from(width) {
        line = Line::from(Span::raw(line.to_string()));
    }
    line
}

pub fn build_status_bar(
    cascade: &StatusCascade<'_>,
    focus: &FocusStack,
    keymap: &Keymap,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let (text, style) = if let Some(error) = cascade.fatal_error {
        (
            format!("FATAL: {error}"),
            Style::default()
                .fg(theme.palette.error)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(prompt) = cascade.confirm_prompt {
        (
            format!("Confirm: {prompt}  [Enter] accept  [Esc] cancel"),
            Style::default().fg(theme.palette.warning),
        )
    } else if cascade.copy_armed {
        (
            "COPY ARMED · select text or press y".to_string(),
            Style::default().fg(theme.palette.accent_code),
        )
    } else if cascade.running_cells > 0 {
        (
            format!("⠋ Running {} cell(s)", cascade.running_cells),
            Style::default().fg(theme.palette.success),
        )
    } else if let Some(notice) = cascade.transient_notice {
        (notice.to_string(), Style::default().fg(theme.palette.fg))
    } else {
        let context = focus.top().key_context();
        let (bound_count, prefix_count) = keymap.bound_count(&context);
        let hints = if bound_count == 0 && prefix_count == 0 {
            Vec::new()
        } else {
            keymap
                .help_for(&context)
                .into_iter()
                .take(4)
                .map(|row| format!("{} {}", row.keys, row.title))
                .collect::<Vec<_>>()
        };
        (
            if hints.is_empty() {
                "READY".to_string()
            } else {
                hints.join("  ·  ")
            },
            Style::default().fg(theme.palette.muted),
        )
    };
    let line = Line::from(Span::styled(format!(" {text}"), style));
    if line.width() > usize::from(width) {
        Line::from(Span::styled(line.to_string(), style))
    } else {
        line
    }
}
