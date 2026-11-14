//! Markdown cell source renderer (headings, fenced code blocks, and width-shrinking box tables).

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
};
use std::collections::HashSet;
use std::sync::OnceLock;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use nb_sdk::Theme;

pub struct RenderedMarkdown {
    pub lines: Vec<Line<'static>>,
    /// Maps each rendered visual line back to its originating logical source line for click-to-edit.
    pub source_line_map: Vec<usize>,
}

/// Inline parsing context: the theme, and the reference ids the cell
/// defines (`[id]: url`), so `[label][id]` renders as `label`.
struct Cx<'a> {
    theme: &'a Theme,
    refs: &'a HashSet<String>,
}

pub fn render_markdown_cell(source: &str, width: u16, theme: &Theme) -> RenderedMarkdown {
    let width = usize::from(width.max(1));
    let (lines, refs) = visible_lines(source);
    let cx = Cx { theme, refs: &refs };
    let mut rendered = Vec::<(Line<'static>, usize)>::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].0.as_str();
        if let Some(language) = line.trim_start().strip_prefix("```") {
            let language = language.trim();
            let fence_line = lines[index].1;
            index += 1;
            let code_start = index;
            while index < lines.len() && !lines[index].0.trim_start().starts_with("```") {
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
            let code = lines[code_start..code_end]
                .iter()
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let first = lines.get(code_start).map_or(fence_line + 1, |(_, source)| *source);
            rendered.extend(render_fenced_code(language, &code, first, width, theme));
            if index < lines.len() {
                rendered.push((
                    Line::from(Span::styled(
                        "└─".to_string(),
                        Style::default().fg(theme.palette.accent_code),
                    )),
                    lines[index].1,
                ));
                index += 1;
            }
            continue;
        }

        if starts_table(&lines, index) {
            let header_source = lines[index].1;
            let headers = split_table_row(line);
            index += 2; // header and Markdown separator
            let mut rows = Vec::new();
            let mut row_sources = Vec::new();
            while index < lines.len() {
                let row = lines[index].0.as_str();
                if row.trim().is_empty() || !row.contains('|') {
                    break;
                }
                rows.push(split_table_row(row));
                row_sources.push(lines[index].1);
                index += 1;
            }
            rendered.extend(render_table(
                &headers,
                &rows,
                header_source,
                &row_sources,
                width,
                &cx,
            ));
            continue;
        }

        let end = block_end(&lines, index);
        rendered.extend(render_block(&lines[index..end], width, &cx));
        index = end;
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

/// The lines Jupyter would show, each with its source line, and the
/// reference ids defined. HTML comments (which may span lines) and
/// reference definitions are hidden; a line that was only a comment
/// disappears. Code blocks and inline code are left alone.
fn visible_lines(source: &str) -> (Vec<(String, usize)>, HashSet<String>) {
    let mut lines = Vec::new();
    let mut refs = HashSet::new();
    let mut in_fence = false;
    let mut in_comment = false;
    for (index, line) in source.lines().enumerate() {
        if !in_comment && line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            lines.push((line.to_string(), index));
            continue;
        }
        if in_fence {
            lines.push((line.to_string(), index));
            continue;
        }
        let started_in_comment = in_comment;
        let (text, open) = strip_comments(line, in_comment);
        in_comment = open;
        let had_comment = started_in_comment || text.len() != line.len();
        if had_comment && text.trim().is_empty() {
            continue;
        }
        if let Some(id) = reference_definition(&text) {
            refs.insert(id);
            continue;
        }
        lines.push((text, index));
    }
    (lines, refs)
}

/// `line` without its HTML comments, starting inside one if `open`; also
/// whether a comment is still open at the end.
fn strip_comments(line: &str, mut open: bool) -> (String, bool) {
    let mut out = String::new();
    let mut rest = line;
    loop {
        if open {
            match rest.find("-->") {
                Some(end) => {
                    rest = &rest[end + 3..];
                    open = false;
                    // `a <!-- x --> b` reads as `a b`.
                    if out.ends_with(' ') && rest.starts_with(' ') {
                        rest = &rest[1..];
                    }
                }
                None => return (out, true),
            }
        } else {
            match comment_start(rest) {
                Some(start) => {
                    out.push_str(&rest[..start]);
                    rest = &rest[start + 4..];
                    open = true;
                }
                None => {
                    out.push_str(rest);
                    return (out, false);
                }
            }
        }
    }
}

/// Where `<!--` starts in `text`, outside inline code.
fn comment_start(text: &str) -> Option<usize> {
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with("<!--") {
            return Some(at);
        }
        if let Some(close) = rest.strip_prefix('`').and_then(|code| code.find('`')) {
            at += close + 2;
            continue;
        }
        at += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// The id of a reference definition line (`[id]: https://…`).
fn reference_definition(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find("]:")?;
    let id = &rest[..close];
    (!id.is_empty() && !id.starts_with('^') && !rest[close + 2..].trim().is_empty())
        .then(|| reference_id(id))
}

/// Reference ids match case-insensitively, with runs of white space as one.
fn reference_id(id: &str) -> String {
    id.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn starts_table(lines: &[(String, usize)], index: usize) -> bool {
    index + 1 < lines.len()
        && is_table_separator(&lines[index + 1].0)
        && split_table_row(&lines[index].0).len() > 1
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Blank,
    Quote,
    Heading(usize),
    Rule,
    Bullet,
    Numbered,
    Plain,
}

/// A line's kind and its inline content (after any marker).
fn classify(line: &str) -> (Kind, &str) {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return (Kind::Blank, line);
    }
    if let Some(rest) = trimmed.strip_prefix('>') {
        return (Kind::Quote, rest.strip_prefix(' ').unwrap_or(rest));
    }
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&level) && trimmed.chars().nth(level) == Some(' ') {
        return (Kind::Heading(level), trimmed[level + 1..].trim());
    }
    if trimmed.trim() == "---" || trimmed.trim() == "***" {
        return (Kind::Rule, "");
    }
    if let Some(rest) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| trimmed.strip_prefix(marker))
    {
        return (Kind::Bullet, rest);
    }
    if let Some(end) = numbered_marker(trimmed) {
        return (Kind::Numbered, &trimmed[end + 2..]);
    }
    (Kind::Plain, line)
}

/// End of `1. ` style list markers (the index of the `.`).
fn numbered_marker(trimmed: &str) -> Option<usize> {
    trimmed
        .find(". ")
        .filter(|end| *end > 0 && *end <= 3 && trimmed[..*end].chars().all(|c| c.is_ascii_digit()))
}

/// The end of the block starting at `start`: the line, plus the plain lines
/// that continue it (quote lines, for a quote), so a link or emphasis
/// wrapped across source lines still parses.
fn block_end(lines: &[(String, usize)], start: usize) -> usize {
    let (kind, _) = classify(&lines[start].0);
    if matches!(kind, Kind::Blank | Kind::Heading(_) | Kind::Rule) {
        return start + 1;
    }
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end].0.as_str();
        if line.trim_start().starts_with("```") || starts_table(lines, end) {
            break;
        }
        let (next, _) = classify(line);
        let continues = match kind {
            Kind::Quote => next == Kind::Quote,
            _ => next == Kind::Plain,
        };
        if !continues {
            break;
        }
        end += 1;
    }
    end
}

/// Parse a block's inline markup as one text, then render it line by line,
/// so each rendered line still maps to its source line.
fn render_block(block: &[(String, usize)], width: usize, cx: &Cx<'_>) -> Vec<(Line<'static>, usize)> {
    let parts: Vec<(Kind, &str)> = block.iter().map(|(line, _)| classify(line)).collect();
    let joined = parts.iter().map(|(_, content)| *content).collect::<Vec<_>>().join("\n");
    let per_line = split_at_newlines(parse_inline(&joined, cx), block.len());
    let mut out = Vec::new();
    for (((line, source), (kind, _)), spans) in block.iter().zip(parts).zip(per_line) {
        out.extend(
            render_line(line, kind, spans, width, cx.theme)
                .into_iter()
                .map(|rendered| (rendered, *source)),
        );
    }
    out
}

/// Spans split at the newlines inside them, one list per line.
fn split_at_newlines(spans: Vec<Span<'static>>, lines: usize) -> Vec<Vec<Span<'static>>> {
    let mut out = vec![Vec::new()];
    for span in spans {
        for (index, piece) in span.content.split('\n').enumerate() {
            if index > 0 {
                out.push(Vec::new());
            }
            if !piece.is_empty() {
                out.last_mut()
                    .expect("never empty")
                    .push(Span::styled(piece.to_string(), span.style));
            }
        }
    }
    out.resize_with(lines.max(1), Vec::new);
    out
}

/// One source line, with its content already parsed into `spans`.
fn render_line(line: &str, kind: Kind, spans: Vec<Span<'static>>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.trim_start();
    match kind {
        Kind::Quote => {
            let bar = Span::styled("▎ ", Style::default().fg(theme.palette.accent_markdown));
            let mut all = vec![bar.clone()];
            all.extend(spans.into_iter().map(|span| {
                let style = span.style.add_modifier(Modifier::ITALIC);
                span.style(style)
            }));
            let mut lines = wrap_words(all, width, 2);
            for line in lines.iter_mut().skip(1) {
                line.spans[0] = bar.clone();
            }
            lines
        }
        Kind::Heading(level) => {
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            style = match level {
                1 => style.fg(theme.palette.accent_markdown),
                2 => style.fg(theme.palette.accent_code),
                _ => style.fg(theme.palette.fg),
            };
            let spans = spans
                .into_iter()
                .map(|span| {
                    let merged = style.patch(span.style).add_modifier(Modifier::BOLD);
                    span.style(merged)
                })
                .collect();
            wrap_words(spans, width, 0)
        }
        Kind::Rule => vec![Line::from(Span::styled(
            "─".repeat(width.min(80)),
            Style::default().fg(theme.palette.border),
        ))],
        Kind::Bullet => {
            let mut all = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled("• ", Style::default().fg(theme.palette.muted)),
            ];
            all.extend(spans);
            wrap_words(all, width, indent + 2)
        }
        Kind::Numbered => {
            let end = numbered_marker(trimmed).unwrap_or(0);
            let marker = &trimmed[..end + 2];
            let mut all = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(marker.to_string(), Style::default().fg(theme.palette.muted)),
            ];
            all.extend(spans);
            wrap_words(all, width, indent + marker.len())
        }
        Kind::Blank | Kind::Plain => {
            let spans = if spans.is_empty() { vec![Span::default()] } else { spans };
            wrap_words(spans, width, 0)
        }
    }
}

/// Inline `code`: colored text only, no background chip.
fn code_span_style(theme: &Theme) -> Style {
    Style::default().fg(theme.palette.accent_code)
}

/// Inline Markdown: `code`, **bold**, *italic*, _italic_, links (`[label](url)`,
/// `[label][id]`, `<https://…>`, `<a href>`), and images (shown as their
/// alt text); URLs are never shown. Emphasis and link labels are parsed
/// recursively, so `**bold `code`**` keeps the code styling and a badge
/// `[![build](img)](url)` shows `build`. `_` only opens emphasis at a word
/// boundary, so identifiers like `mlp_step` in prose stay intact. `text`
/// may hold newlines (a block's lines); they pass through as text.
fn parse_inline(text: &str, cx: &Cx<'_>) -> Vec<Span<'static>> {
    let theme = cx.theme;
    let plain = Style::default().fg(theme.palette.fg);
    let link = |spans: Vec<Span<'static>>| -> Vec<Span<'static>> {
        spans
            .into_iter()
            .map(|span| {
                let style = span
                    .style
                    .fg(theme.palette.accent_code)
                    .add_modifier(Modifier::UNDERLINED);
                span.style(style)
            })
            .collect()
    };
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
        if let Some((alt, consumed)) = rest.strip_prefix('!').and_then(|image| parse_link(image, cx)) {
            let alt = if alt.trim().is_empty() { "image" } else { alt };
            spans.push(Span::styled(
                alt.to_string(),
                Style::default().fg(theme.palette.muted).add_modifier(Modifier::ITALIC),
            ));
            rest = &rest[1 + consumed..];
            prev = Some(')');
            continue;
        }
        if let Some((label, consumed)) = parse_link(rest, cx) {
            spans.extend(link(parse_inline(label, cx)));
            rest = &rest[consumed..];
            prev = Some(')');
            continue;
        }
        if let Some((label, consumed)) = parse_html_link(rest) {
            spans.extend(link(parse_inline(label, cx)));
            rest = &rest[consumed..];
            prev = Some('>');
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
            for span in parse_inline(content, cx) {
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
            .find(|(_, c)| matches!(c, '`' | '*' | '_' | '[' | '!' | '<'))
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

/// A link at the start of `text`: its label and the bytes it takes.
/// `[label](url "title")`, and `[label][id]`, `[label][]`, `[label]` when
/// the cell defines the id. Brackets and parentheses nest, so a badge
/// `[![b](img)](url)` and a URL like `…/Foo_(bar)` parse whole; `<url>`
/// autolinks show the URL itself, which is their label.
fn parse_link<'t>(text: &'t str, cx: &Cx<'_>) -> Option<(&'t str, usize)> {
    if let Some(inner) = text.strip_prefix('<') {
        let end = inner.find('>')?;
        let url = &inner[..end];
        let is_url = (url.contains("://") || url.starts_with("mailto:")) && !url.contains(char::is_whitespace);
        return is_url.then_some((url, end + 2));
    }
    let rest = text.strip_prefix('[')?;
    let label_end = matching(rest, '[', ']')?;
    let label = &rest[..label_end];
    let after = &rest[label_end + 1..];
    if let Some(target) = after.strip_prefix('(') {
        let close = matching(target, '(', ')')?;
        return Some((label, label_end + close + 4));
    }
    if let Some(reference) = after.strip_prefix('[') {
        let close = reference.find(']')?;
        let id = if close == 0 { label } else { &reference[..close] };
        return cx
            .refs
            .contains(&reference_id(id))
            .then_some((label, label_end + close + 4));
    }
    cx.refs
        .contains(&reference_id(label))
        .then_some((label, label_end + 2))
}

/// `<a href="…">label</a>`: the label and the bytes it takes.
fn parse_html_link(text: &str) -> Option<(&str, usize)> {
    let lower = text.get(..3)?.to_ascii_lowercase();
    if lower != "<a " && !text.starts_with("<a>") && !text.starts_with("<A>") {
        return None;
    }
    let open_end = text.find('>')? + 1;
    let close = text[open_end..].to_ascii_lowercase().find("</a>")?;
    Some((&text[open_end..open_end + close], open_end + close + 4))
}

/// Byte index of the `close` ending a bracket opened just before `text`
/// (nested pairs and `\`-escaped characters skipped).
fn matching(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(index);
            }
            depth -= 1;
        }
    }
    None
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
    cx: &Cx<'_>,
) -> Vec<(Line<'static>, usize)> {
    let theme = cx.theme;
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
                let mut spans = parse_inline(value, cx);
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
    // Loading syntect's bundled sets takes milliseconds: once per process,
    // not once per fenced block per render.
    static SETS: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    let (syntax_set, theme_set) = SETS.get_or_init(|| {
        (SyntaxSet::load_defaults_newlines(), ThemeSet::load_defaults())
    });
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
                // Code, not Markdown: shown as it is.
                let plain = Span::styled(line.to_string(), Style::default().fg(theme.palette.fg));
                wrap_styled(vec![plain], width)
                    .into_iter()
                    .map(move |line| (line, source_start + index))
            })
            .collect();
    };
    let mut highlighter = HighlightLines::new(syntax, syntax_theme);
    let mut output = Vec::new();
    for (index, line) in LinesWithEndings::from(code).enumerate() {
        let regions = highlighter
            .highlight_line(line, syntax_set)
            .unwrap_or_default();
        let spans = crate::highlight::spans(regions, syntax_theme);
        for line in wrap_styled(spans, width) {
            output.push((line, source_start + index));
        }
    }
    if output.is_empty() {
        output.push((Line::default(), source_start));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(source: &str) -> Vec<String> {
        render_markdown_cell(source, 120, &Theme::default())
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn links_show_their_label_and_never_their_url() {
        for (source, want) in [
            ("a [label](https://x.com/a) b", "a label b"),
            ("a [Foo](https://w.org/wiki/Foo_(bar)) b", "a Foo b"),
            ("a [label](https://x.com \"Title\") b", "a label b"),
            ("a [![build](https://img/x.svg)](https://ci) b", "a build b"),
            ("a ![alt](https://img/x.png) b", "a alt b"),
            ("a <https://x.com/auto> b", "a https://x.com/auto b"),
            ("a <a href=\"https://x.com\">label</a> b", "a label b"),
            ("**[label](https://x.com)**", "label"),
        ] {
            assert_eq!(text(source), [want], "{source}");
        }
    }

    #[test]
    fn a_link_wrapped_across_source_lines_still_hides_its_url() {
        let rendered = render_markdown_cell(
            "see [the label\nthat wraps](https://x.com/long) here\nnext line",
            120,
            &Theme::default(),
        );
        let lines: Vec<String> = rendered
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(lines, ["see the label", "that wraps here", "next line"]);
        assert_eq!(rendered.source_line_map, [0, 1, 2], "lines still map to their source");
        let underlined = |row: usize| {
            rendered.lines[row]
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
        };
        assert!(underlined(0) && underlined(1), "both halves are the link");
    }

    #[test]
    fn reference_links_use_their_definitions_which_are_hidden() {
        assert_eq!(
            text("see [the docs][1] and [Other] here\n\n[1]: https://x.com/1\n[other]: https://x.com/o"),
            ["see the docs and Other here", ""]
        );
        assert_eq!(text("a[0] and [not a ref]"), ["a[0] and [not a ref]"], "undefined ids stay literal");
    }

    #[test]
    fn html_comments_are_hidden_on_one_line_or_many() {
        assert_eq!(text("before <!-- note --> after"), ["before after"]);
        let rendered = render_markdown_cell(
            "# Title\n<!--\nhidden\n-->\nshown <!-- tail\nstill hidden --> end",
            120,
            &Theme::default(),
        );
        let lines: Vec<String> = rendered
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(lines, ["Title", "shown ", " end"]);
        assert_eq!(rendered.source_line_map, [0, 4, 5]);
        // Not inside code.
        assert_eq!(text("`<!-- kept -->`"), ["<!-- kept -->"]);
        assert_eq!(text("```\n<!-- kept -->\n```")[1], "<!-- kept -->");
    }
}
