//! Top bar (mode badge, file, kernel) and bottom bar (prompt, confirm
//! question, notice, or context key hints).

use super::style::{key_style, mode_color};
use super::{AppView, KernelPhase};
use crate::focus::{CellMode, FocusLayer, ModalKind};
use crate::keymap::Keymap;
use crate::overlays::{ConfirmAction, PromptKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::path::Path;
use unicode_width::UnicodeWidthStr;
use wb_sdk::{BarSlot, KeyContextId, StatusCtx, StatusSegment, Theme};

/// Dark text for use on bright badge backgrounds.
const ON_ACCENT: Color = Color::Rgb(12, 16, 28);

fn width_of(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>() as u16
}

pub fn draw_top_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &AppView<'_>,
    ctx: &StatusCtx<'_>,
    segments: &[Box<dyn StatusSegment>],
    theme: &Theme,
) {
    let p = &theme.palette;
    let bar = Style::default().bg(p.surface).fg(p.fg);
    frame.buffer_mut().set_style(area, bar);
    let focus = view.focus.top();
    let accent = mode_color(focus, theme);
    let name = ctx
        .path
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("untitled");
    let mut left = vec![
        Span::styled(
            format!(" {} ", focus.badge()),
            Style::default()
                .bg(accent)
                .fg(ON_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {name}"), bar.add_modifier(Modifier::BOLD)),
    ];
    if ctx.dirty {
        left.push(Span::styled(" ●", bar.fg(p.warning)));
    }

    let (dot, state) = match view.kernel_phase {
        KernelPhase::Starting => (p.muted, "starting"),
        KernelPhase::Dead => (p.error, "dead"),
        KernelPhase::Ready if ctx.kernel_busy => (p.warning, "busy"),
        KernelPhase::Ready => (p.success, "idle"),
    };
    let mut right = Vec::new();
    for segment in segments
        .iter()
        .filter(|segment| matches!(segment.slot(), BarSlot::TopLeft | BarSlot::TopRight))
    {
        if let Some(span) = segment.render(ctx, theme) {
            right.push(span);
            right.push(Span::styled("  ", bar));
        }
    }
    right.extend([
        Span::styled("● ", bar.fg(dot)),
        Span::styled(ctx.kernel_name.to_string(), bar),
        Span::styled(format!(" {state}"), bar.fg(p.muted)),
        Span::styled(
            format!("   cell {}/{} ", ctx.cell_position.0, ctx.cell_position.1),
            bar.fg(p.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    let right_width = width_of(&right);
    if right_width + 24 < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(area.right() - right_width, area.y, right_width, 1),
        );
    }
}

/// Curated key hints per focus level: (action id, label). Keys come from the
/// live keymap, so rebinding an action updates its hint.
fn hint_actions(focus: &FocusLayer) -> &'static [(&'static str, &'static str)] {
    match focus {
        FocusLayer::NotebookNormal => &[
            ("cell.enter_normal", "edit"),
            ("cell.run", "run"),
            ("cell.run_and_advance", "run+next"),
            ("cell.insert_below", "new cell"),
            ("op.delete", "delete"),
            ("app.palette", "commands"),
            ("app.help", "help"),
        ],
        FocusLayer::CellEdit {
            mode: CellMode::Normal,
            ..
        } => &[
            ("cell.enter_insert", "insert"),
            ("cell.run", "run"),
            ("focus.up", "back"),
            ("intel.inspect_doc", "docs"),
            ("app.help", "help"),
        ],
        FocusLayer::CellEdit {
            mode: CellMode::Insert,
            ..
        } => &[
            ("focus.up", "normal"),
            ("edit.newline", "newline"),
            ("intel.complete", "complete"),
            ("cell.toggle_expand_split", "expand"),
        ],
        FocusLayer::CellEdit {
            mode: CellMode::Visual(_),
            ..
        } => &[("op.delete", "delete"), ("op.yank", "yank"), ("focus.up", "cancel")],
        FocusLayer::OutputFocus { .. } => &[
            ("motion.down", "down"),
            ("motion.up", "up"),
            ("focus.up", "back"),
        ],
        FocusLayer::PdbInput => &[
            ("pdb.submit", "send"),
            ("pdb.history_prev", "history"),
            ("kernel.interrupt", "interrupt"),
        ],
        FocusLayer::Modal(_) => &[],
    }
}

/// Fixed hints for modals, whose keys are not remappable.
fn modal_hints(kind: ModalKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        ModalKind::Help | ModalKind::Logs | ModalKind::InspectPager => {
            &[("j/k", "scroll"), ("g/G", "top/bottom"), ("esc", "close")]
        }
        ModalKind::CommandPalette => &[("↑/↓", "select"), ("enter", "run"), ("esc", "close")],
        ModalKind::Settings => &[
            ("↑/↓", "select"),
            ("←/→", "change"),
            ("tab", "next tab"),
            ("esc", "close"),
        ],
        ModalKind::SearchPrompt | ModalKind::ExCommandBar => &[("enter", "run"), ("esc", "cancel")],
        ModalKind::ConfirmBar => &[],
    }
}

fn hint_spans(pairs: &[(String, &str)], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    for (index, (keys, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(keys.clone(), key_style(theme)));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme.palette.muted),
        ));
    }
    spans
}

/// The confirm question and its answer keys.
pub fn confirm_text(action: &ConfirmAction) -> (&'static str, &'static [(&'static str, &'static str)]) {
    match action {
        ConfirmAction::QuitUnsaved => (
            "Unsaved changes.",
            &[("s", "save & quit"), ("q", "quit without saving"), ("esc", "cancel")],
        ),
        ConfirmAction::ReloadExternalDiskChange => (
            "The file changed on disk.",
            &[("l", "load disk version"), ("esc", "keep mine")],
        ),
        ConfirmAction::RestartLiveKernel { .. } => (
            "Restart the kernel? Its state will be lost.",
            &[("R", "restart"), ("a", "restart & run all"), ("esc", "cancel")],
        ),
        ConfirmAction::RecoverAutosaveSidecar => (
            "An autosave from an earlier session was found.",
            &[("r", "restore it"), ("d", "delete it"), ("esc", "ignore")],
        ),
    }
}

/// Draws the bottom bar; returns the terminal cursor position when a prompt
/// is being typed.
pub fn draw_status_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &AppView<'_>,
    keymap: &Keymap,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let p = &theme.palette;
    if let Some(prompt) = view.overlays.prompt.as_ref() {
        let prefix = match prompt.kind {
            PromptKind::ExCommand => ":",
            PromptKind::Search => "/",
            PromptKind::SaveAs => "save as: ",
        };
        let line = Line::from(vec![
            Span::styled(format!(" {prefix}"), key_style(theme)),
            Span::styled(prompt.input.clone(), Style::default().fg(p.fg)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        let before = prompt.input.get(..prompt.cursor_byte).unwrap_or(&prompt.input);
        let x = area.x
            + 1
            + UnicodeWidthStr::width(prefix) as u16
            + UnicodeWidthStr::width(before) as u16;
        return Some((x.min(area.right().saturating_sub(1)), area.y));
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if view.running_cells > 0 {
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        right.push(Span::styled(
            format!("{} {} running ", spinner[view.spinner_tick % spinner.len()], view.running_cells),
            Style::default().fg(p.warning),
        ));
    }
    if let FocusLayer::CellEdit { cell_id, .. } = view.focus.base() {
        if let Some(cell) = view.document.cell(cell_id) {
            let cursor = cell.buffer.cursor();
            right.push(Span::styled(
                format!("ln {}, col {} ", cursor.row + 1, cursor.col + 1),
                Style::default().fg(p.muted),
            ));
        }
    }

    let left: Vec<Span<'static>> = if let Some(action) = view.overlays.confirm.as_ref() {
        let (question, answers) = confirm_text(action);
        let mut spans = vec![Span::styled(
            format!(" {question}  "),
            Style::default().fg(p.warning).add_modifier(Modifier::BOLD),
        )];
        let pairs: Vec<(String, &str)> = answers.iter().map(|(k, l)| (k.to_string(), *l)).collect();
        spans.extend(hint_spans(&pairs, theme).into_iter().skip(1));
        spans
    } else if let Some(notice) = view.notice {
        vec![Span::styled(format!(" {notice}"), Style::default().fg(p.fg))]
    } else {
        let focus = view.focus.top();
        let pairs: Vec<(String, &str)> = match focus {
            FocusLayer::Modal(kind) => modal_hints(*kind)
                .iter()
                .map(|(keys, label)| (keys.to_string(), *label))
                .collect(),
            other => {
                let context = other.key_context();
                hint_actions(other)
                    .iter()
                    .filter_map(|(action, label)| {
                        let keys = keymap.keys_for(&context, action);
                        (!keys.is_empty()).then(|| (keys.into_iter().take(2).collect::<Vec<_>>().join("/"), *label))
                    })
                    .collect()
            }
        };
        hint_spans(&pairs, theme)
    };

    let right_width = width_of(&right);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    if right_width > 0 && right_width < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(area.right() - right_width, area.y, right_width, 1),
        );
    }
    None
}

/// Context ids for building help: the focus level below any modal.
pub fn help_context(view: &AppView<'_>) -> KeyContextId {
    view.focus.base().key_context()
}
