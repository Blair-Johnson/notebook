//! Markdown cell source renderer (headings, fenced code blocks, and width-shrinking box tables).

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use nb_sdk::Theme;

pub struct RenderedMarkdown {
    pub lines: Vec<Line<'static>>,
    /// Maps each rendered visual line back to its originating logical source line for click-to-edit.
    pub source_line_map: Vec<usize>,
}

pub fn render_markdown_cell(source: &str, width: u16, theme: &Theme) -> RenderedMarkdown {
    let width = usize::from(width.max(1));
    let source_lines: Vec<&str> = source.lines().collect();
    let mut rendered = Vec::<(Line<'static>, usize)>::new();
    let mut index = 0usize;
    while index < source_lines.len() {
        let line = source_lines[index];
        if let Some(language) = line.trim_start().strip_prefix("```") {
            let language = language.trim();
            let fence_line = index;
            index += 1;
            let code_start = index;
            while index < source_lines.len() && !source_lines[index].trim_start().starts_with("```")
            {
                index += 1;
            }
            let code_end = index;
            rendered.push((
                Line::from(Span::styled(
                    format!(
                        "┌─ {}",
                        if language.is_empty() {
                            "code"
                        } else {
                            language
                        }
                    ),
                    Style::default()
                        .fg(theme.palette.accent_code)
                        .add_modifier(Modifier::BOLD),
                )),
                fence_line,
            ));
            let code = source_lines[code_start..code_end].join("\n");
            rendered.extend(render_fenced_code(
                language, &code, code_start, width, theme,
            ));
            if index < source_lines.len() {
                rendered.push((
                    Line::from(Span::styled(
                        "└─".to_string(),
                        Style::default().fg(theme.palette.accent_code),
                    )),
                    index,
                ));
                index += 1;
            }
            continue;
        }

        if index + 1 < source_lines.len()
            && is_table_separator(source_lines[index + 1])
            && split_table_row(line).len() > 1
        {
            let header_source = index;
            let headers = split_table_row(line);
            index += 2; // header and Markdown separator
            let mut rows = Vec::new();
            let mut row_sources = Vec::new();
            while index < source_lines.len() {
                let row = source_lines[index];
                if row.trim().is_empty() || !row.contains('|') {
                    break;
                }
                rows.push(split_table_row(row));
                row_sources.push(index);
                index += 1;
            }
            rendered.extend(render_table(
                &headers,
                &rows,
                header_source,
                &row_sources,
                width,
                theme,
            ));
            continue;
        }

        rendered.extend(render_markdown_line(line, index, width, theme));
        index += 1;
    }

    if rendered.is_empty() {
        rendered.push((Line::default(), 0));
    }
    let (lines, source_line_map): (Vec<_>, Vec<_>) = rendered.into_iter().unzip();
    RenderedMarkdown {
        lines,
        source_line_map,
    }
}

fn render_markdown_line(
    line: &str,
    source_line: usize,
    width: usize,
    theme: &Theme,
) -> Vec<(Line<'static>, usize)> {
    let tag = |lines: Vec<Line<'static>>| -> Vec<(Line<'static>, usize)> {
        lines.into_iter().map(|line| (line, source_line)).collect()
    };
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('>') {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let bar = Span::styled("▎ ", Style::default().fg(theme.palette.accent_markdown));
        let mut spans = vec![bar.clone()];
        spans.extend(parse_inline(rest, theme).into_iter().map(|span| {
            let style = span.style.add_modifier(Modifier::ITALIC);
            span.style(style)
        }));
        let mut lines = wrap_words(spans, width, 2);
        for line in lines.iter_mut().skip(1) {
            line.spans[0] = bar.clone();
        }
        return tag(lines);
    }
    let heading_level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&heading_level) && trimmed.chars().nth(heading_level) == Some(' ') {
        let content = trimmed[heading_level + 1..].trim();
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        style = match heading_level {
            1 => style.fg(theme.palette.accent_markdown),
            2 => style.fg(theme.palette.accent_code),
            _ => style.fg(theme.palette.fg),
        };
        let spans = parse_inline(content, theme)
            .into_iter()
            .map(|span| {
                let merged = style.patch(span.style).add_modifier(Modifier::BOLD);
                span.style(merged)
            })
            .collect();
        return tag(wrap_words(spans, width, 0));
    }
    if trimmed.trim() == "---" || trimmed.trim() == "***" {
        return tag(vec![Line::from(Span::styled(
            "─".repeat(width.min(80)),
            Style::default().fg(theme.palette.border),
        ))]);
    }
    let bullet = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| trimmed.strip_prefix(marker));
    if let Some(rest) = bullet {
        let mut spans = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled("• ", Style::default().fg(theme.palette.muted)),
        ];
        spans.extend(parse_inline(rest, theme));
        return tag(wrap_words(spans, width, indent + 2));
    }
    let numbered = trimmed
        .find(". ")
        .filter(|end| *end > 0 && *end <= 3 && trimmed[..*end].chars().all(|c| c.is_ascii_digit()));
    if let Some(end) = numbered {
        let marker = &trimmed[..end + 2];
        let mut spans = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(marker.to_string(), Style::default().fg(theme.palette.muted)),
        ];
        spans.extend(parse_inline(&trimmed[end + 2..], theme));
        return tag(wrap_words(spans, width, indent + marker.len()));
    }
    tag(wrap_words(parse_inline(line, theme), width, 0))
}

/// Inline `code`: colored text only, no background chip.
fn code_span_style(theme: &Theme) -> Style {
    Style::default().fg(theme.palette.accent_code)
}

/// Inline Markdown: `code`, **bold**, *italic*, _italic_, [links](url).
/// Emphasis content is parsed recursively, so `**bold `code`**` keeps the
/// code styling. `_` only opens emphasis at a word boundary, so identifiers
/// like `mlp_step` in prose stay intact.
fn parse_inline(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let plain = Style::default().fg(theme.palette.fg);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = text;
    let mut prev: Option<char> = None;
    let push_plain = |spans: &mut Vec<Span<'static>>, text: &str| {
        if let Some(last) = spans.last_mut()
            && last.style == plain
        {
            last.content.to_mut().push_str(text);
        } else {
            spans.push(Span::styled(text.to_string(), plain));
        }
    };
    while !rest.is_empty() {
        let at_boundary = prev.is_none_or(|c| !c.is_alphanumeric());
        if let Some(code) = rest
            .strip_prefix('`')
            .and_then(|value| value.find('`').map(|end| &value[..end]))
        {
            spans.push(Span::styled(code.to_string(), code_span_style(theme)));
            rest = &rest[code.len() + 2..];
            prev = Some('`');
            continue;
        }
        if let Some((label, _url, consumed)) = parse_link(rest) {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme.palette.accent_code)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            rest = &rest[consumed..];
            prev = Some(')');
            continue;
        }
        let emphasis = [
            ("**", Modifier::BOLD, false),
            ("__", Modifier::BOLD, true),
            ("*", Modifier::ITALIC, false),
            ("_", Modifier::ITALIC, true),
        ]
        .into_iter()
        .find_map(|(marker, modifier, needs_boundary)| {
            if needs_boundary && !at_boundary {
                return None;
            }
            let content = rest.strip_prefix(marker)?;
            let end = content.find(marker)?;
            (end > 0).then_some((marker, modifier, &content[..end]))
        });
        if let Some((marker, modifier, content)) = emphasis {
            for span in parse_inline(content, theme) {
                let style = span.style.add_modifier(modifier);
                spans.push(span.style(style));
            }
            rest = &rest[marker.len() * 2 + content.len()..];
            prev = marker.chars().last();
            continue;
        }
        let mut chars = rest.char_indices();
        let (_, c) = chars.next().unwrap_or((0, ' '));
        let next = chars
            .find(|(_, c)| matches!(c, '`' | '*' | '_' | '['))
            .map_or(rest.len(), |(index, _)| index);
        push_plain(&mut spans, &rest[..next]);
        prev = rest[..next].chars().last().or(Some(c));
        rest = &rest[next..];
    }
    if spans.is_empty() {
        spans.push(Span::default());
    }
    spans
}

fn parse_link(text: &str) -> Option<(String, String, usize)> {
    let rest = text.strip_prefix('[')?;
    let label_end = rest.find("](")?;
    let url_end = rest[label_end + 2..].find(')')?;
    let label = rest[..label_end].to_string();
    let url = rest[label_end + 2..label_end + 2 + url_end].to_string();
    Some((label, url, label_end + 2 + url_end + 2))
}

fn is_table_separator(line: &str) -> bool {
    let cells = split_table_row(line);
    cells.len() > 1
        && cells.iter().all(|cell| {
            let value = cell.trim();
            value.len() >= 3
                && value.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
                && value.chars().any(|ch| ch == '-')
        })
}

fn split_table_row(line: &str) -> Vec<String> {
    let mut value = line.trim();
    if let Some(stripped) = value.strip_prefix('|') {
        value = stripped;
    }
    if let Some(stripped) = value.strip_suffix('|') {
        value = stripped;
    }
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if ch == '|' && !escaped {
            cells.push(current.trim().replace("\\|", "|"));
            current.clear();
        } else {
            escaped = ch == '\\' && !escaped;
            current.push(ch);
            if ch != '\\' {
                escaped = false;
            }
        }
    }
    cells.push(current.trim().replace("\\|", "|"));
    cells
}

fn render_table(
    headers: &[String],
    rows: &[Vec<String>],
    header_source: usize,
    row_sources: &[usize],
    width: usize,
    theme: &Theme,
) -> Vec<(Line<'static>, usize)> {
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }
    let make_cells = |row: &[String], bold: bool| -> Vec<Vec<Span<'static>>> {
        (0..columns)
            .map(|column| {
                let value = row.get(column).map(String::as_str).unwrap_or("");
                let mut spans = parse_inline(value, theme);
                if bold {
                    for span in &mut spans {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                spans
            })
            .collect()
    };
    let header_cells = make_cells(headers, true);
    let body_cells: Vec<_> = rows.iter().map(|row| make_cells(row, false)).collect();
    let mut natural = vec![1usize; columns];
    let mut floors = vec![1usize; columns];
    for row in std::iter::once(&header_cells).chain(body_cells.iter()) {
        for (column, spans) in row.iter().enumerate() {
            let text = spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            natural[column] = natural[column].max(UnicodeWidthStr::width(text.as_str()));
            floors[column] = floors[column].max(
                text.split_whitespace()
                    .map(UnicodeWidthStr::width)
                    .max()
                    .unwrap_or(1),
            );
        }
    }
    let target = width.saturating_sub(columns * 3 + 1).max(columns);
    let mut widths = natural;
    shrink_widths(&mut widths, &floors, target);
    shrink_widths(&mut widths, &vec![1; columns], target);

    let header_lines: Vec<_> = header_cells
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| wrap_styled(cell.clone(), *width))
        .collect();
    let body_lines: Vec<Vec<_>> = body_cells
        .iter()
        .map(|row| {
            row.iter()
                .zip(widths.iter())
                .map(|(cell, width)| wrap_styled(cell.clone(), *width))
                .collect()
        })
        .collect();
    let border = Style::default().fg(theme.palette.border);
    let rule = |left: &str, middle: &str, right: &str| {
        let segments = widths
            .iter()
            .map(|column| "─".repeat(column + 2))
            .collect::<Vec<_>>();
        Line::from(Span::styled(
            format!("{left}{}{right}", segments.join(middle)),
            border,
        ))
    };
    let row_lines = |cells: &[Vec<Line<'static>>]| -> Vec<Line<'static>> {
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        (0..height)
            .map(|index| {
                let mut spans = vec![Span::styled("│", border)];
                for (cell, column_width) in cells.iter().zip(widths.iter()) {
                    let content = cell
                        .get(index)
                        .map(|line| line.spans.clone())
                        .unwrap_or_default();
                    let used = content
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                        .sum::<usize>();
                    spans.push(Span::raw(" ".to_string()));
                    spans.extend(content);
                    spans.push(Span::raw(" ".repeat(column_width.saturating_sub(used) + 1)));
                    spans.push(Span::styled("│", border));
                }
                Line::from(spans)
            })
            .collect()
    };

    let mut output = Vec::new();
    output.push((rule("┌", "┬", "┐"), header_source));
    output.extend(
        row_lines(&header_lines)
            .into_iter()
            .map(|line| (line, header_source)),
    );
    output.push((rule("├", "┼", "┤"), header_source));
    let row_rules = body_lines
        .iter()
        .any(|row| row.iter().any(|cell| cell.len() > 1));
    for (index, row) in body_lines.iter().enumerate() {
        if row_rules && index > 0 {
            output.push((
                rule("├", "┼", "┤"),
                row_sources.get(index).copied().unwrap_or(header_source),
            ));
        }
        let source = row_sources.get(index).copied().unwrap_or(header_source);
        output.extend(row_lines(row).into_iter().map(|line| (line, source)));
    }
    output.push((
        rule("└", "┴", "┘"),
        row_sources.last().copied().unwrap_or(header_source),
    ));
    output
}

fn shrink_widths(widths: &mut [usize], floors: &[usize], target: usize) {
    while widths.iter().sum::<usize>() > target {
        let Some(column) = (0..widths.len())
            .filter(|column| widths[*column] > floors[*column])
            .max_by_key(|column| (widths[*column], std::cmp::Reverse(*column)))
        else {
            break;
        };
        widths[column] -= 1;
    }
}

/// Word-wrap styled spans to `width` columns. Breaks at spaces, hard-breaks
/// only words longer than a line, drops the spaces at a break, and indents
/// continuation lines by `hang` columns.
fn wrap_words(spans: Vec<Span<'static>>, width: usize, hang: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let hang = hang.min(width.saturating_sub(1));
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0usize;
    let width_of = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    let break_line = |lines: &mut Vec<Vec<(char, Style)>>, current: &mut Vec<(char, Style)>, used: &mut usize| {
        while current.last().is_some_and(|(c, _)| *c == ' ') {
            current.pop();
        }
        lines.push(std::mem::take(current));
        current.extend(std::iter::repeat_n((' ', Style::default()), hang));
        *used = hang;
    };
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let is_space = chars[index].0 == ' ';
        while index < chars.len() && (chars[index].0 == ' ') == is_space {
            index += 1;
        }
        let token = &chars[start..index];
        let token_width: usize = token.iter().map(|(c, _)| width_of(*c)).sum();
        if is_space {
            if used + token_width <= width {
                current.extend_from_slice(token);
                used += token_width;
            } else {
                break_line(&mut lines, &mut current, &mut used);
            }
            continue;
        }
        if used + token_width > width && used > hang && token_width <= width - hang {
            break_line(&mut lines, &mut current, &mut used);
        }
        for &(c, style) in token {
            let cw = width_of(c);
            if used + cw > width && used > hang {
                break_line(&mut lines, &mut current, &mut used);
            }
            current.push((c, style));
            used += cw;
        }
    }
    lines.push(current);
    lines
        .into_iter()
        .map(|chars| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, style) in chars {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn wrap_styled(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;
    for span in spans {
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw > 0 && used > 0 && used + cw > width {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            if let Some(last) = current.last_mut() {
                if last.style == span.style {
                    last.content.to_mut().push(ch);
                } else {
                    current.push(Span::styled(ch.to_string(), span.style));
                }
            } else {
                current.push(Span::styled(ch.to_string(), span.style));
            }
            used = used.saturating_add(cw);
        }
    }
    if current.is_empty() && lines.is_empty() {
        lines.push(Line::default());
    } else {
        lines.push(Line::from(current));
    }
    lines
}

fn render_fenced_code(
    language: &str,
    code: &str,
    source_start: usize,
    width: usize,
    theme: &Theme,
) -> Vec<(Line<'static>, usize)> {
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let theme_set = ThemeSet::load_defaults();
    let syntax = syntax_set
        .find_syntax_by_token(language)
        .or_else(|| syntax_set.find_syntax_by_extension(language))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let syntax_theme = theme_set
        .themes
        .get(&theme.syntect_theme)
        .or_else(|| theme_set.themes.get("base16-ocean.dark"))
        .or_else(|| theme_set.themes.values().next());
    let Some(syntax_theme) = syntax_theme else {
        return code
            .lines()
            .enumerate()
            .flat_map(|(index, line)| {
                wrap_styled(parse_inline(line, theme), width)
                    .into_iter()
                    .map(move |line| (line, source_start + index))
            })
            .collect();
    };
    let mut highlighter = HighlightLines::new(syntax, syntax_theme);
    let mut output = Vec::new();
    for (index, line) in LinesWithEndings::from(code).enumerate() {
        let regions = highlighter
            .highlight_line(line, &syntax_set)
            .unwrap_or_default();
        let spans = regions
            .into_iter()
            .map(|(style, text)| {
                Span::styled(
                    text.trim_end_matches(['\r', '\n']).to_string(),
                    Style::default().fg(Color::Rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    )),
                )
            })
            .filter(|span| !span.content.is_empty())
            .collect::<Vec<_>>();
        for line in wrap_styled(spans, width) {
            output.push((line, source_start + index));
        }
    }
    if output.is_empty() {
        output.push((Line::default(), source_start));
    }
    output
}
