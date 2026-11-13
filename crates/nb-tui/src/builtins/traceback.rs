//! Built-in Python/JAX traceback output view supporting frame folding and cell-line jumps.

use nb_core::{CellId, OutputData, OutputItem};
use nb_sdk::{
    InputOutcome, KeyContextId, MountCtx, OutputHost, OutputMetrics, OutputRenderer, OutputView,
    ResolvedInput, Theme,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::cell::RefCell;
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

pub struct TracebackRenderer;

impl OutputRenderer for TracebackRenderer {
    fn id(&self) -> &'static str {
        "core.traceback"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        &[]
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        matches!(item.data, OutputData::Error { .. }).then_some(100)
    }

    fn mount(&self, _item: &OutputItem, _ctx: &MountCtx) -> Box<dyn OutputView> {
        Box::new(TracebackView::default())
    }
}

#[derive(Default)]
pub struct TracebackView {
    fold_internal_frames: bool,
    selected_frame: usize,
    rev: u64,
    cell_id: Option<CellId>,
    total_rows: u32,
    shown_rows: u32,
    scroll_offset: Option<u32>,
    /// Built lines for one (output revision, width, folding): building parses
    /// the ANSI of the whole traceback, too slow to repeat every frame.
    built: RefCell<Option<BuiltLines>>,
}

struct BuiltLines {
    rev: u64,
    width: u16,
    folded: bool,
    lines: Vec<RenderedTraceLine>,
}

#[derive(Clone)]
struct RenderedTraceLine {
    line: Line<'static>,
    frame: Option<usize>,
    cell_line: Option<usize>,
}

impl TracebackView {
    pub fn set_fold_internal_frames(&mut self, folded: bool) {
        if self.fold_internal_frames != folded {
            self.fold_internal_frames = folded;
            self.rev = self.rev.wrapping_add(1);
        }
    }

    pub fn fold_internal_frames(&self) -> bool {
        self.fold_internal_frames
    }

    pub fn selected_frame(&self) -> usize {
        self.selected_frame
    }

    fn lines(item: &OutputItem) -> Vec<String> {
        match &item.data {
            OutputData::Error {
                ename,
                evalue,
                traceback,
            } => {
                let mut lines = traceback
                    .iter()
                    .flat_map(|entry| entry.lines().map(str::to_owned))
                    .collect::<Vec<_>>();
                let exception_line = format!("{ename}: {evalue}");
                let already_present = lines.last().is_some_and(|line| {
                    line.contains(ename) && (evalue.is_empty() || line.contains(evalue))
                });
                if lines.is_empty() || (!evalue.is_empty() && !already_present) {
                    lines.push(exception_line);
                }
                lines
            }
            _ => Vec::new(),
        }
    }

    fn visible_start(&self) -> u32 {
        self.scroll_offset
            .unwrap_or_else(|| self.total_rows.saturating_sub(self.shown_rows))
    }

    fn max_scroll(&self) -> u32 {
        self.total_rows.saturating_sub(self.shown_rows)
    }

    /// Run `f` over the built lines, building them only when stale.
    fn with_lines<R>(
        &self,
        item: &OutputItem,
        width: u16,
        f: impl FnOnce(&[RenderedTraceLine]) -> R,
    ) -> R {
        let mut built = self.built.borrow_mut();
        let fresh = built.as_ref().is_some_and(|built| {
            built.rev == item.rev
                && built.width == width
                && built.folded == self.fold_internal_frames
        });
        if !fresh {
            *built = Some(BuiltLines {
                rev: item.rev,
                width,
                folded: self.fold_internal_frames,
                lines: self.build_lines(item, width),
            });
        }
        f(&built.as_ref().expect("just built").lines)
    }

    /// Frames and cell lines do not depend on width: reuse the last build.
    fn built_width(&self) -> u16 {
        self.built
            .borrow()
            .as_ref()
            .map_or(u16::MAX, |built| built.width)
    }

    fn build_lines(&self, item: &OutputItem, width: u16) -> Vec<RenderedTraceLine> {
        let raw = Self::lines(item);
        let mut rendered = Vec::new();
        let mut frame_index = 0usize;
        let mut index = 0usize;
        while index < raw.len() {
            let Some((path, source_line)) = parse_frame(&raw[index]) else {
                rendered.extend(parse_ansi_lines(&raw[index], width, None, None));
                index += 1;
                continue;
            };
            let internal = is_internal_frame(&path);
            if internal && self.fold_internal_frames {
                let mut count = 1usize;
                index += 1;
                while index < raw.len() {
                    if let Some((next_path, _)) = parse_frame(&raw[index]) {
                        if is_internal_frame(&next_path) {
                            count += 1;
                            index += 1;
                            continue;
                        }
                    }
                    if raw[index].trim_start().starts_with("File ")
                        || (!raw[index].starts_with(' ') && !raw[index].starts_with('\t'))
                    {
                        break;
                    }
                    index += 1;
                }
                rendered.push(RenderedTraceLine {
                    line: Line::from(Span::styled(
                        format!("… {count} internal frame(s) folded …"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )),
                    frame: Some(frame_index),
                    cell_line: None,
                });
                frame_index += count;
                continue;
            }
            let cell_line = is_cell_frame(&path).then_some(source_line);
            rendered.extend(parse_ansi_lines(
                &raw[index],
                width,
                Some(frame_index),
                cell_line,
            ));
            frame_index += 1;
            index += 1;
        }
        if rendered.is_empty() {
            rendered.push(RenderedTraceLine {
                line: Line::default(),
                frame: None,
                cell_line: None,
            });
        }
        rendered
    }

    fn command_is(input: &ResolvedInput, name: &str) -> bool {
        matches!(input, ResolvedInput::Command { id, .. } if id.0.as_ref() == name)
    }

    fn motion(input: &ResolvedInput, name: &str) -> Option<u32> {
        match input {
            ResolvedInput::Move { motion, count, .. } if motion.0.as_ref() == name => Some(*count),
            _ => None,
        }
    }
}

impl OutputView for TracebackView {
    fn on_output_updated(&mut self, _item: &OutputItem, ctx: &MountCtx) {
        self.cell_id = Some(ctx.cell_id.clone());
        self.rev = self.rev.wrapping_add(1);
    }

    /// Plain text: entering the output gives vim navigation over it instead.
    fn is_focusable(&self) -> bool {
        false
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.traceback"))
    }

    fn view_revision(&self) -> u64 {
        self.rev
    }

    fn measure(
        &mut self,
        item: &OutputItem,
        width: u16,
        _theme: &Theme,
        cap_rows: Option<u16>,
    ) -> OutputMetrics {
        let (total_rows, max_frame) = self.with_lines(item, width.max(1), |lines| {
            (lines.len() as u32, max_frame(lines))
        });
        self.total_rows = total_rows;
        self.shown_rows = cap_rows
            .map_or(self.total_rows, u32::from)
            .min(self.total_rows)
            .max(1);
        if let Some(offset) = self.scroll_offset {
            self.scroll_offset = Some(offset.min(self.max_scroll()));
        }
        self.selected_frame = self.selected_frame.min(max_frame);
        OutputMetrics {
            total_rows: self.total_rows,
            shown_rows: self.shown_rows,
            clipped: self.total_rows > self.shown_rows,
        }
    }

    fn render_slice(
        &self,
        item: &OutputItem,
        width: u16,
        rows: Range<u32>,
        focused: bool,
        _theme: &Theme,
        out: &mut Vec<Line<'static>>,
    ) {
        let local = rows.end <= self.shown_rows;
        let start = if local {
            self.visible_start().saturating_add(rows.start)
        } else {
            rows.start
        } as usize;
        let end = if local {
            self.visible_start().saturating_add(rows.end)
        } else {
            rows.end
        } as usize;
        self.with_lines(item, width.max(1), |lines| {
            let end = end.min(lines.len());
            for rendered in lines.get(start..end).unwrap_or_default() {
                let line = rendered.line.clone();
                if focused && rendered.frame == Some(self.selected_frame) {
                    out.push(line.patch_style(Style::default().add_modifier(Modifier::REVERSED)));
                } else {
                    out.push(line);
                }
            }
        });
    }

    fn handle_input(
        &mut self,
        item: &OutputItem,
        input: ResolvedInput,
        host: &mut dyn OutputHost,
    ) -> InputOutcome {
        if Self::command_is(&input, "focus.up") {
            return InputOutcome::PopFocus;
        }
        if Self::command_is(&input, "traceback.toggle_internal")
            || Self::command_is(&input, "output.toggle_fold")
            || Self::command_is(&input, "fold.toggle")
        {
            self.fold_internal_frames = !self.fold_internal_frames;
            self.rev = self.rev.wrapping_add(1);
            return InputOutcome::Consumed;
        }
        let max_frame = self.with_lines(item, self.built_width(), max_frame);
        if Self::motion(&input, "motion.top").is_some() {
            self.selected_frame = 0;
            self.scroll_offset = Some(0);
            return InputOutcome::Consumed;
        }
        if Self::motion(&input, "motion.bottom").is_some() {
            self.selected_frame = max_frame;
            self.scroll_offset = None;
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion(&input, "motion.down") {
            self.selected_frame = self
                .selected_frame
                .saturating_add(count as usize)
                .min(max_frame);
            return InputOutcome::Consumed;
        }
        if let Some(count) = Self::motion(&input, "motion.up") {
            self.selected_frame = self.selected_frame.saturating_sub(count as usize);
            return InputOutcome::Consumed;
        }
        if Self::command_is(&input, "focus.enter")
            || matches!(input, ResolvedInput::Command { id, .. } if id.0.as_ref() == "ui.accept")
        {
            let selected = self.with_lines(item, self.built_width(), |lines| {
                lines
                    .iter()
                    .find(|line| line.frame == Some(self.selected_frame))
                    .map(selected_cell_line)
            });
            if let Some(cell_line) = selected {
                if let (Some(cell_id), Some(line)) = (self.cell_id.clone(), cell_line) {
                    host.jump_to_cell(cell_id, Some(line));
                    return InputOutcome::Consumed;
                }
            }
        }
        InputOutcome::Ignored
    }
}

fn selected_cell_line(line: &RenderedTraceLine) -> Option<usize> {
    line.cell_line
}

fn max_frame(lines: &[RenderedTraceLine]) -> usize {
    lines
        .iter()
        .filter_map(|line| line.frame)
        .max()
        .unwrap_or(0)
}

fn parse_frame(line: &str) -> Option<(String, usize)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("File ")?;
    let path = rest.strip_prefix('"')?.split('"').next()?.to_string();
    let line_number = rest
        .split(", line ")
        .nth(1)
        .and_then(|value| value.split(|ch: char| !ch.is_ascii_digit()).next())?
        .parse::<usize>()
        .ok()?;
    Some((path, line_number.saturating_sub(1)))
}

fn is_internal_frame(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("site-packages")
        || lower.contains("jax")
        || lower.contains("jaxlib")
        || lower.contains("<frozen")
        || lower.contains("/lib/python")
}

fn is_cell_frame(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("<cell")
        || lower.contains("ipython-input")
        || lower.contains("nb-cell")
        || lower == "<stdin>"
}

fn parse_ansi_lines(
    raw: &str,
    width: u16,
    frame: Option<usize>,
    cell_line: Option<usize>,
) -> Vec<RenderedTraceLine> {
    let parsed = crate::ansi::parse(raw);
    let source_lines = parsed.as_ref().map(|text| text.lines.as_slice());
    let Some(source_lines) = source_lines else {
        return vec![RenderedTraceLine {
            line: fix_yellow_background(Line::from(raw.to_string())),
            frame,
            cell_line,
        }];
    };
    source_lines
        .iter()
        .flat_map(|line| wrap_line(line, width.max(1)))
        .map(|line| RenderedTraceLine {
            line: fix_yellow_background(line),
            frame,
            cell_line,
        })
        .collect()
}

fn fix_yellow_background(mut line: Line<'static>) -> Line<'static> {
    for span in &mut line.spans {
        let yellow_background = match span.style.bg {
            Some(Color::Yellow | Color::LightYellow) => true,
            Some(Color::Indexed(index)) => matches!(index, 3 | 11),
            Some(Color::Rgb(red, green, blue)) => {
                red >= 180 && green >= 140 && blue <= 120 && red > blue.saturating_add(80)
            }
            _ => false,
        };
        if yellow_background {
            span.style = span.style.fg(Color::Black);
        }
    }
    line
}

fn wrap_line(line: &Line<'_>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0usize;
    for source_span in &line.spans {
        for ch in source_span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw > 0 && used > 0 && used + cw > width {
                output.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            let content = ch.to_string();
            if let Some(last) = spans.last_mut() {
                if last.style == source_span.style {
                    last.content.to_mut().push(ch);
                } else {
                    spans.push(Span::styled(content, source_span.style));
                }
            } else {
                spans.push(Span::styled(content, source_span.style));
            }
            used = used.saturating_add(cw);
        }
    }
    if spans.is_empty() && output.is_empty() {
        output.push(Line::default());
    } else {
        output.push(Line::from(spans));
    }
    output
}
