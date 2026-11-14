//! Popups and modals: completion, signature help, help, command palette,
//! settings, logs, docs pager, confirm dialog, input / debugger dock.

use super::chrome::{confirm_text, help_context};
use super::style::{centered, key_style, panel};
use super::AppView;
use crate::focus::{FocusLayer, ModalKind};
use crate::keymap::Keymap;
use crate::overlays::{CONTROLS_TAB, CapturePurpose, SettingsModalState};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use nb_sdk::{SettingKind, SettingValue, Theme};

const COMPLETION_ROWS: usize = 8;

fn text_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text) as u16
}

fn fit(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return format!("{text:<width$}");
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw + 1 > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Popups anchored at the edit cursor. `cursor` is the terminal cursor cell.
pub fn draw_cursor_popups(
    frame: &mut Frame<'_>,
    view: &AppView<'_>,
    cursor: Option<(u16, u16)>,
    bounds: Rect,
    theme: &Theme,
) {
    let Some((cx, cy)) = cursor else {
        return;
    };
    let insert = matches!(
        view.focus.top(),
        FocusLayer::CellEdit {
            mode: crate::focus::CellMode::Insert,
            ..
        }
    );
    let p = &theme.palette;
    let mut signature_above = true;
    if let (true, Some(popup)) = (insert, view.completion) {
        if !popup.filtered.is_empty() {
            let rows = popup.filtered.len().min(COMPLETION_ROWS);
            let top = popup.scroll_top.min(popup.filtered.len().saturating_sub(rows));
            let visible = &popup.filtered[top..top + rows];
            let label_w = visible
                .iter()
                .map(|item| UnicodeWidthStr::width(item.replace_text.trim_start_matches('.')))
                .max()
                .unwrap_or(4)
                .clamp(8, 36);
            let kind_w = visible
                .iter()
                .map(|item| UnicodeWidthStr::width(item.kind.as_str()))
                .max()
                .unwrap_or(0)
                .min(12);
            let width = (label_w + kind_w + 4) as u16;
            let typed = popup.replace_end_char.saturating_sub(popup.replace_start_char) as u16;
            let x = cx
                .saturating_sub(typed + 1)
                .max(bounds.x)
                .min(bounds.right().saturating_sub(width));
            let below = bounds.bottom().saturating_sub(cy + 1);
            let y = if below as usize >= rows || cy < bounds.y + rows as u16 {
                cy + 1
            } else {
                cy - rows as u16
            };
            // Signature help goes on the other side of the cursor line.
            signature_above = y > cy;
            let area = Rect::new(x, y, width.min(bounds.width), (rows as u16).min(bounds.bottom().saturating_sub(y)));
            frame.render_widget(Clear, area);
            let lines: Vec<Line> = visible
                .iter()
                .enumerate()
                .map(|(offset, item)| {
                    let selected = top + offset == popup.selected;
                    let base = if selected {
                        Style::default().bg(p.panel_selection).fg(p.fg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(p.panel_bg).fg(p.fg)
                    };
                    Line::from(vec![
                        Span::styled(" ", base),
                        Span::styled(fit(item.replace_text.trim_start_matches('.'), label_w), base),
                        Span::styled("  ", base),
                        Span::styled(fit(&item.kind, kind_w), base.fg(p.muted).remove_modifier(Modifier::BOLD)),
                        Span::styled(" ", base),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), area);
            if popup.filtered.len() > rows {
                let thumb = (top * rows / popup.filtered.len()) as u16;
                frame.render_widget(
                    Paragraph::new(Span::styled("▐", Style::default().fg(p.border_focused))),
                    Rect::new(area.right() - 1, area.y + thumb.min(area.height - 1), 1, 1),
                );
            }
        }
    }
    if let (true, Some(signature)) = (insert, view.signature) {
        let text = signature.signature_text.lines().next().unwrap_or("").to_string();
        if text.is_empty() {
            return;
        }
        let width = (text_width(&text) + 2).min(bounds.width);
        let y = if signature_above && cy > bounds.y {
            cy - 1
        } else if cy + 1 < bounds.bottom() {
            cy + 1
        } else {
            return;
        };
        let x = cx.saturating_sub(4).max(bounds.x).min(bounds.right().saturating_sub(width));
        let base = Style::default().bg(p.panel_bg).fg(p.muted);
        let mut spans = vec![Span::styled(" ", base)];
        match signature.active_param_byte_range.clone() {
            Some(range) if range.end <= text.len() && text.is_char_boundary(range.start) && text.is_char_boundary(range.end) => {
                spans.push(Span::styled(text[..range.start].to_string(), base));
                spans.push(Span::styled(
                    text[range.clone()].to_string(),
                    base.fg(p.accent_code).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(text[range.end..].to_string(), base));
            }
            _ => spans.push(Span::styled(text, base)),
        }
        spans.push(Span::styled(" ", base));
        let area = Rect::new(x, y, width, 1);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

pub fn draw_modal(frame: &mut Frame<'_>, view: &AppView<'_>, keymap: &Keymap, theme: &Theme) {
    let screen = frame.area();
    match view.focus.top() {
        FocusLayer::Modal(ModalKind::Help) => draw_help(frame, view, keymap, theme, screen),
        FocusLayer::Modal(ModalKind::CommandPalette) => draw_palette(frame, view, keymap, theme, screen),
        FocusLayer::Modal(ModalKind::Settings) => draw_settings(frame, view, keymap, theme, screen),
        FocusLayer::Modal(ModalKind::Logs) => draw_logs(frame, view, theme, screen),
        FocusLayer::Modal(ModalKind::InspectPager) => draw_inspect(frame, view, theme, screen),
        FocusLayer::Modal(ModalKind::ConfirmBar) => draw_confirm(frame, view, theme, screen),
        _ => {}
    }
}

fn modal_rect(screen: Rect, max_width: u16, height_margin: u16) -> Rect {
    let width = screen.width.saturating_sub(4).min(max_width);
    let height = screen.height.saturating_sub(height_margin);
    centered(screen, width, height)
}

fn draw_help(frame: &mut Frame<'_>, view: &AppView<'_>, keymap: &Keymap, theme: &Theme, screen: Rect) {
    let p = &theme.palette;
    let context = help_context(view);
    let title = keymap
        .context_spec(&context)
        .map_or_else(|| context.as_str().to_string(), |spec| spec.title.to_string());
    let area = modal_rect(screen, 96, 4);
    let inner = panel(frame, area, &format!("Keys · {title}"), theme);
    let rows = keymap.help_for(&context);
    let key_w = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.keys.as_str()))
        .max()
        .unwrap_or(8)
        .clamp(8, 22);
    let mut lines = Vec::new();
    let mut category = String::new();
    for row in rows {
        if row.category != category {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            category = row.category.clone();
            lines.push(Line::from(Span::styled(
                category.clone(),
                Style::default().fg(p.accent_markdown).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", fit(&row.keys, key_w)), key_style(theme)),
            Span::styled(format!("  {}", row.title), Style::default().fg(p.fg)),
        ]));
        // Where rules or a macro decide what the key does, say so, case by case.
        if keymap.controls.is_customized(context.as_str(), &row.action) {
            for case in keymap.controls.cases(context.as_str(), &row.action, false) {
                let text = if case.when.is_empty() {
                    case.what
                } else {
                    format!("{}: {}", case.when, case.what)
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}    {text}", " ".repeat(key_w)),
                    Style::default().fg(p.muted),
                )));
            }
        }
    }
    let body_h = usize::from(inner.height.saturating_sub(1));
    let max_scroll = lines.len().saturating_sub(body_h);
    let scroll = view.help_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(scroll).take(body_h).collect::<Vec<_>>()),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height.saturating_sub(1)),
    );
    footer(frame, inner, &[("j/k", "scroll"), ("esc", "close")], theme);
}

fn footer(frame: &mut Frame<'_>, inner: Rect, pairs: &[(&str, &str)], theme: &Theme) {
    if inner.height == 0 {
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    for (index, (keys, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(keys.to_string(), key_style(theme)));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(theme.palette.muted)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

fn draw_palette(frame: &mut Frame<'_>, view: &AppView<'_>, keymap: &Keymap, theme: &Theme, screen: Rect) {
    let p = &theme.palette;
    let Some(palette) = view.overlays.palette.as_ref() else {
        return;
    };
    let entries = palette.query_matches(view.registry, keymap, &help_context(view));
    let list_rows = entries.len().clamp(1, 12) as u16;
    let width = screen.width.saturating_sub(4).min(84);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + 2,
        width,
        (list_rows + 4).min(screen.height.saturating_sub(3)),
    );
    let inner = panel(frame, area, "Commands & settings", theme);
    let query = Line::from(vec![
        Span::styled("› ", key_style(theme)),
        Span::styled(palette.query.clone(), Style::default().fg(p.fg)),
    ]);
    frame.render_widget(Paragraph::new(query), Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1));
    frame.set_cursor_position((
        inner.x + 3 + text_width(&palette.query),
        inner.y,
    ));
    frame.render_widget(
        Paragraph::new(Span::styled("─".repeat(usize::from(inner.width)), Style::default().fg(p.border))),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    let list = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(2));
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  nothing matches", Style::default().fg(p.muted))),
            list,
        );
        return;
    }
    let rows = usize::from(list.height);
    let selected = palette.selected.min(entries.len() - 1);
    let top = selected.saturating_sub(rows.saturating_sub(1));
    for (offset, entry) in entries.iter().skip(top).take(rows).enumerate() {
        let is_selected = top + offset == selected;
        let base = if is_selected {
            Style::default().bg(p.panel_selection).fg(p.fg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(p.panel_bg).fg(p.fg)
        };
        let y = list.y + offset as u16;
        let row = Rect::new(list.x, y, list.width, 1);
        frame.buffer_mut().set_style(row, base);
        let keys = if entry.bound_keys.is_empty() { entry.keys.clone() } else { entry.bound_keys.clone() };
        let keys_w = text_width(&keys);
        let left = Line::from(vec![
            Span::styled(format!("  {}", entry.title), base),
            Span::styled(format!("  {}", entry.category), base.fg(p.muted).remove_modifier(Modifier::BOLD)),
        ]);
        frame.render_widget(Paragraph::new(left), row);
        if keys_w + 2 < row.width {
            frame.render_widget(
                Paragraph::new(Span::styled(keys, base.fg(p.accent_code))),
                Rect::new(row.right() - keys_w - 2, y, keys_w, 1),
            );
        }
    }
}

fn setting_value_text(kind: &SettingKind, value: Option<&SettingValue>) -> String {
    match (kind, value) {
        (_, Some(SettingValue::Bool(true))) => "on".into(),
        (_, Some(SettingValue::Bool(false))) => "off".into(),
        (_, Some(SettingValue::Int(value))) => value.to_string(),
        (SettingKind::Choice { .. }, Some(SettingValue::Text(value))) => format!("‹ {value} ›"),
        (_, Some(SettingValue::Text(value))) => value.clone(),
        (SettingKind::Bool { default }, None) => if *default { "on" } else { "off" }.into(),
        (SettingKind::Int { default, .. }, None) => default.to_string(),
        (SettingKind::Choice { default, .. }, None) => format!("‹ {default} ›"),
        (SettingKind::Text { default }, None) => (*default).to_string(),
    }
}

fn draw_settings(frame: &mut Frame<'_>, view: &AppView<'_>, keymap: &Keymap, theme: &Theme, screen: Rect) {
    let p = &theme.palette;
    let state = &view.overlays.settings;
    let area = modal_rect(screen, 100, 4);
    let inner = panel(frame, area, "Settings", theme);
    // Tabs.
    let mut tabs = vec![Span::raw(" ")];
    for (index, tab) in SettingsModalState::tabs().iter().enumerate() {
        let style = if index == state.active_tab {
            Style::default().fg(p.panel_bg).bg(p.border_focused).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.muted)
        };
        tabs.push(Span::styled(format!(" {tab} "), style));
        tabs.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), Rect::new(inner.x, inner.y, inner.width, 1));
    if state.active_tab == CONTROLS_TAB {
        return draw_controls_tab(frame, state, keymap, theme, inner);
    }
    let filter = if state.filter.is_empty() {
        Span::styled(" type to filter", Style::default().fg(p.muted))
    } else {
        Span::styled(format!(" filter: {}", state.filter), Style::default().fg(p.fg))
    };
    frame.render_widget(Paragraph::new(filter), Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let list = Rect::new(inner.x, inner.y + 3, inner.width, inner.height.saturating_sub(6));
    let rows = usize::from(list.height);
    let title_w = 30usize.min(usize::from(inner.width) / 2);
    let mut description = String::new();
    let render_row = |frame: &mut Frame<'_>, offset: usize, selected: bool, title: &str, value: String| {
        let base = if selected {
            Style::default().bg(p.panel_selection).fg(p.fg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(p.panel_bg).fg(p.fg)
        };
        let row = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        frame.buffer_mut().set_style(row, base);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("  {}", fit(title, title_w)), base),
                Span::styled(format!("  {value}"), base.fg(p.accent_code)),
            ])),
            row,
        );
    };
    {
        let settings = state.filtered_settings(view.registry);
        let top = state.selected_row.saturating_sub(rows.saturating_sub(1));
        for (offset, spec) in settings.iter().skip(top).take(rows).enumerate() {
            let selected = top + offset == state.selected_row;
            let value = setting_value_text(&spec.kind, view.setting_values.get(&spec.id));
            render_row(frame, offset, selected, &spec.title, value);
            if selected {
                description = format!("{} ({})", spec.description, spec.id.0);
            }
        }
        if settings.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("  nothing here yet", Style::default().fg(p.muted))),
                list,
            );
        }
    }
    frame.render_widget(
        Paragraph::new(Span::styled(format!(" {description}"), Style::default().fg(p.muted)))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.bottom().saturating_sub(3), inner.width, 2),
    );
    footer(
        frame,
        inner,
        &[("↑/↓", "select"), ("←/→", "change"), ("tab", "next tab"), ("esc", "close")],
        theme,
    );
}

/// The Controls tab: for one mode at a time, what each key does there.
fn draw_controls_tab(
    frame: &mut Frame<'_>,
    state: &SettingsModalState,
    keymap: &Keymap,
    theme: &Theme,
    inner: Rect,
) {
    let p = &theme.palette;
    let muted = Style::default().fg(p.muted);
    let Some(mode) = state.controls_mode(keymap) else {
        return;
    };

    // Mode strip: every mode, the shown one highlighted; scrolled so the
    // shown one is visible.
    let modes: Vec<_> = keymap.controls.modes().collect();
    let selected_mode = state.mode_index.min(modes.len().saturating_sub(1));
    let labels: Vec<String> = modes.iter().map(|spec| format!(" {} ", spec.title)).collect();
    let mut first = 0;
    let fits = |from: usize| {
        labels[from..=selected_mode].iter().map(|label| text_width(label) + 1).sum::<u16>() + 7
            <= inner.width
    };
    while first < selected_mode && !fits(first) {
        first += 1;
    }
    let mut strip = vec![Span::styled(" Mode ", muted)];
    if first > 0 {
        strip.push(Span::styled("‹ ", muted));
    }
    for (index, label) in labels.iter().enumerate().skip(first) {
        let style = if index == selected_mode {
            Style::default().fg(p.panel_bg).bg(p.accent_code).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.fg)
        };
        strip.push(Span::styled(label.clone(), style));
        strip.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(strip)), Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let rows = state.control_rows(keymap);
    let row = rows.get(state.selected_row);
    let status = if let Some(capture) = &state.capture {
        let keys = crate::keymap::format_key_sequence_toml(&capture.keys);
        let what = match capture.purpose {
            CapturePurpose::Try => "Type keys to try".to_string(),
            CapturePurpose::Rebind => format!("Press the new keys for {}", row.map_or("", |row| row.title.as_str())),
            CapturePurpose::AddKey => format!("Press keys to add to {}", row.map_or("", |row| row.title.as_str())),
        };
        Line::from(vec![
            Span::styled(format!(" {what} (pause or enter to finish, ctrl+c cancels): "), Style::default().fg(p.warning)),
            Span::styled(keys, key_style(theme)),
        ])
    } else if state.filtering || !state.filter.is_empty() {
        Line::from(vec![
            Span::styled(" filter: ", muted),
            Span::styled(
                format!("{}{}", state.filter, if state.filtering { "▏" } else { "" }),
                Style::default().fg(p.fg),
            ),
        ])
    } else {
        Line::from(Span::styled(
            " Keys trigger actions; rules decide what an action does in this mode and on this target.",
            muted,
        ))
    };
    frame.render_widget(Paragraph::new(status), Rect::new(inner.x, inner.y + 2, inner.width, 1));

    let detail_h = 6u16;
    let list = Rect::new(
        inner.x,
        inner.y + 4,
        inner.width,
        inner.height.saturating_sub(5 + detail_h),
    );
    let visible = usize::from(list.height);
    let keys_w = 14usize;
    let title_w = 28usize;
    let base_style = |selected: bool| {
        if selected {
            Style::default().bg(p.panel_selection).fg(p.fg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(p.panel_bg).fg(p.fg)
        }
    };

    if let (Some(rule_index), Some(row)) = (state.rules_view, row) {
        let header = format!(" Rules for {} in {}, checked in order (the first match runs):", row.title, mode.title);
        frame.render_widget(
            Paragraph::new(Span::styled(header, muted)),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );
        let rules = keymap.controls.rules_for(&mode.id, &row.action);
        let top = rule_index.saturating_sub(visible.saturating_sub(1));
        for (offset, rule) in rules.iter().skip(top).take(visible).enumerate() {
            let selected = top + offset == rule_index;
            let style = base_style(selected);
            let area = Rect::new(list.x, list.y + offset as u16, list.width, 1);
            frame.buffer_mut().set_style(area, style);
            let when = crate::controls::describe_conditions(&rule.when);
            let line = Line::from(vec![
                Span::styled(if rule.enabled { "  [on]  " } else { "  [off] " }, style.fg(if rule.enabled { p.success } else { p.muted })),
                Span::styled(rule.id.clone(), style),
                Span::styled(if rule.from_user { " (yours)" } else { " (default)" }, style.fg(p.muted)),
                Span::styled(if when.is_empty() { String::new() } else { format!("  when {when}") }, style.fg(p.accent_markdown)),
                Span::styled(format!("  do: {}", keymap.controls.describe_steps(&rule.steps, &row.action)), style.fg(p.accent_code)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
        footer(
            frame,
            inner,
            &[("↑/↓", "select"), ("space", "on/off"), ("e", "edit in $EDITOR"), ("n", "new rule"), ("esc", "back")],
            theme,
        );
    } else {
        let header = format!(
            "  {}{}What happens in {}",
            fit("Keys", keys_w),
            fit("Action", title_w + 2),
            mode.title
        );
        frame.render_widget(
            Paragraph::new(Span::styled(header, muted.add_modifier(Modifier::BOLD))),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("  nothing matches", muted)),
                list,
            );
        }
        let top = state.selected_row.saturating_sub(visible.saturating_sub(1));
        for (offset, entry) in rows.iter().skip(top).take(visible).enumerate() {
            let selected = top + offset == state.selected_row;
            let style = base_style(selected);
            let area = Rect::new(list.x, list.y + offset as u16, list.width, 1);
            frame.buffer_mut().set_style(area, style);
            let keys = if entry.keys.is_empty() {
                "—".to_string()
            } else {
                entry.keys.join(" · ")
            };
            let what = entry
                .cases
                .iter()
                .map(|case| {
                    if case.when.is_empty() {
                        case.what.clone()
                    } else {
                        format!("{}: {}", case.when, case.what)
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            let customized = keymap.controls.is_customized(&mode.id, &entry.action);
            let what = if customized { what } else { "built-in".to_string() };
            let line = Line::from(vec![
                Span::styled(format!("  {}", fit(&keys, keys_w)), style.fg(p.accent_code)),
                Span::styled(fit(&entry.title, title_w), style),
                Span::styled(if customized { " ◆" } else { "  " }, style.fg(p.accent_markdown)),
                Span::styled(format!(" {what}"), style.fg(if customized { p.fg } else { p.muted })),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
        footer(
            frame,
            inner,
            &[
                ("←/→", "mode"),
                ("enter", "rebind"),
                ("+", "add key"),
                ("x", "unbind"),
                ("u", "default keys"),
                ("r", "rules"),
                ("e", "edit"),
                ("t", "try keys"),
                ("/", "filter"),
            ],
            theme,
        );
    }

    // Detail: the try-keys trace, config warnings, or the selected action.
    let mut detail: Vec<Line> = Vec::new();
    if !state.explain.is_empty() {
        detail.extend(state.explain.iter().map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(p.fg)))));
    } else {
        for warning in keymap.warnings.iter().take(2) {
            detail.push(Line::from(Span::styled(
                format!(" ⚠ {}: {}", warning.source_key, warning.message),
                Style::default().fg(p.warning),
            )));
        }
        if let Some(row) = row {
            detail.push(Line::from(Span::styled(
                format!(" {} · {} · {}", row.title, row.category, row.action),
                Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
            )));
            for case in &row.cases {
                let text = if case.when.is_empty() {
                    format!("   {}", case.what)
                } else {
                    format!("   {}: {}", case.when, case.what)
                };
                detail.push(Line::from(Span::styled(text, muted)));
            }
        }
    }
    let area = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1 + detail_h),
        inner.width,
        detail_h,
    );
    frame.render_widget(
        Paragraph::new(Span::styled("─".repeat(usize::from(inner.width)), Style::default().fg(p.border))),
        Rect::new(area.x, area.y.saturating_sub(1), area.width, 1),
    );
    frame.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }), area);
}

fn draw_logs(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme, screen: Rect) {
    let p = &theme.palette;
    let area = modal_rect(screen, 140, 4);
    let inner = panel(frame, area, "Kernel log", theme);
    let body_h = usize::from(inner.height.saturating_sub(1));
    let total = view.logs.len();
    let from_bottom = view.overlays.log_scroll_from_bottom.min(total.saturating_sub(body_h));
    let end = total - from_bottom;
    let start = end.saturating_sub(body_h);
    let lines: Vec<Line> = if total == 0 {
        vec![Line::from(Span::styled(" no kernel output yet", Style::default().fg(p.muted)))]
    } else {
        view.logs
            .iter()
            .skip(start)
            .take(end - start)
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(p.fg))))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height.saturating_sub(1)),
    );
    footer(frame, inner, &[("j/k", "scroll"), ("esc", "close")], theme);
}

fn draw_inspect(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme, screen: Rect) {
    let Some(pager) = view.inspect_pager else {
        return;
    };
    let area = modal_rect(screen, 110, 4);
    let inner = panel(frame, area, &pager.title, theme);
    let text = crate::ansi::parse(&pager.ansi_text)
        .unwrap_or_else(|| Text::raw(pager.ansi_text.clone()));
    frame.render_widget(
        Paragraph::new(text).scroll((pager.scroll_top.min(u16::MAX as usize) as u16, 0)),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height.saturating_sub(1)),
    );
    footer(frame, inner, &[("j/k", "scroll"), ("esc", "close")], theme);
}

fn draw_confirm(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme, screen: Rect) {
    let Some(action) = view.overlays.confirm.as_ref() else {
        return;
    };
    let p = &theme.palette;
    let (question, answers) = confirm_text(action);
    let width = (text_width(question) + 8).max(40).min(screen.width.saturating_sub(4));
    let height = answers.len() as u16 + 5;
    let area = centered(screen, width, height);
    let inner = panel(frame, area, "Confirm", theme);
    let mut lines = vec![
        Line::from(Span::styled(question, Style::default().fg(p.fg).add_modifier(Modifier::BOLD))),
        Line::default(),
    ];
    for (key, label) in answers {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:>5}"), key_style(theme)),
            Span::styled(format!("  {label}"), Style::default().fg(p.fg)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height),
    );
}

/// Debugger input dock; returns the cursor position in its input line.
pub fn draw_dock(frame: &mut Frame<'_>, view: &AppView<'_>, theme: &Theme, area: Option<Rect>) -> Option<(u16, u16)> {
    let (Some(area), Some(dock)) = (area, view.overlays.pdb_dock.as_ref()) else {
        return None;
    };
    let p = &theme.palette;
    // The same dock serves pdb and plain `input()`: name it after the prompt.
    let prompt = dock.prompt.trim();
    let title = if prompt.starts_with("(Pdb") || prompt.starts_with("ipdb>") {
        "debugger"
    } else if dock.password {
        "password"
    } else {
        "input"
    };
    let inner = panel(frame, area, title, theme);
    let shown = if dock.password {
        "•".repeat(dock.composer.as_str().chars().count())
    } else {
        dock.composer.as_str().to_string()
    };
    let prompt = dock.prompt.trim_end().to_string() + " ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.clone(), Style::default().fg(p.error).add_modifier(Modifier::BOLD)),
            Span::styled(shown.clone(), Style::default().fg(p.fg)),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height > 1 {
        footer(
            frame,
            inner,
            &[("enter", "send"), ("↑/↓", "history"), ("ctrl+c", "interrupt")],
            theme,
        );
    }
    let cursor_col = dock.composer.cursor().col;
    let before: String = shown.chars().take(cursor_col).collect();
    Some((
        (inner.x + text_width(&prompt) + text_width(&before)).min(inner.right().saturating_sub(1)),
        inner.y,
    ))
}
