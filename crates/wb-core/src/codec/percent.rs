//! Jupytext `# %%` percent-format and plain `.py` script reader/writer.

use crate::cell::{Cell, CellKind};
use crate::codec::CoreError;
use crate::document::{Document, FormatBacking};

/// Parse a `# %%` percent-formatted `.py` file (or plain Python script) into cells,
/// uncommenting IPython magics and preserving any leading `# ---` YAML header.
pub fn parse_percent(source: &str) -> Result<Document, CoreError> {
    let (header, body) = split_header(source);
    let lines: Vec<String> = body
        .split_inclusive('\n')
        .map(|line| {
            line.trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_owned()
        })
        .collect();
    let marker_positions: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| marker_kind(line).map(|_| index))
        .collect();

    let mut cells = Vec::new();
    if marker_positions.is_empty() {
        let code = uncomment_magics(&body);
        cells.push(Cell::new(CellKind::Code, code));
    } else {
        let first_marker = marker_positions[0];
        if lines[..first_marker]
            .iter()
            .any(|line| !line.trim().is_empty())
        {
            let preamble = lines[..first_marker].join("\n");
            cells.push(Cell::new(CellKind::Code, uncomment_magics(&preamble)));
        }
        for (position, marker_index) in marker_positions.iter().copied().enumerate() {
            let next = marker_positions
                .get(position + 1)
                .copied()
                .unwrap_or(lines.len());
            let mut content = lines[marker_index + 1..next].to_vec();
            while content.last().is_some_and(|line| line.trim().is_empty()) {
                content.pop();
            }
            let kind = marker_kind(&lines[marker_index]).unwrap_or(CellKind::Code);
            let text = content.join("\n");
            let source = match kind {
                CellKind::Code => uncomment_magics(&text),
                CellKind::Markdown | CellKind::Raw => content
                    .iter()
                    .map(|line| uncomment_comment(line))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            cells.push(Cell::new(kind, source));
        }
    }
    if cells.is_empty() {
        cells.push(Cell::new_code());
    }
    Ok(Document::from_cells(
        cells,
        FormatBacking::PercentPy { header },
    ))
}

/// Serialize `Document` cells back to clean `# %%` percent format (sources only, no outputs).
pub fn serialize_percent(doc: &Document) -> String {
    let cells: Vec<&Cell> = doc.cells_in_order().collect();
    let header = match &doc.backing {
        FormatBacking::PercentPy { header } => header.as_deref().unwrap_or_default(),
        FormatBacking::Ipynb { .. } => "",
    };
    let plain = cells.len() == 1 && cells[0].kind == CellKind::Code && header.is_empty();
    if plain {
        return comment_magics(cells[0].buffer.as_str());
    }

    let mut output = String::new();
    if !header.is_empty() {
        output.push_str(header);
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str("# %%");
        match cell.kind {
            CellKind::Code => {}
            CellKind::Markdown => output.push_str(" [markdown]"),
            CellKind::Raw => output.push_str(" [raw]"),
        }
        output.push('\n');
        let mut lines: Vec<String> = cell
            .buffer
            .as_str()
            .split('\n')
            .map(str::to_owned)
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        match cell.kind {
            CellKind::Code => {
                for line in lines {
                    output.push_str(&comment_magic_line(&line));
                    output.push('\n');
                }
            }
            CellKind::Markdown | CellKind::Raw => {
                for line in lines {
                    if line.is_empty() {
                        output.push('#');
                    } else {
                        output.push_str("# ");
                        output.push_str(&line);
                    }
                    output.push('\n');
                }
            }
        }
    }
    output
}

fn split_header(source: &str) -> (Option<String>, String) {
    let segments: Vec<&str> = source.split_inclusive('\n').collect();
    let Some(open) = segments
        .iter()
        .position(|segment| is_yaml_rule(segment.trim_end_matches(['\n', '\r'])))
    else {
        return (None, source.to_owned());
    };
    let Some(close_relative) = segments[open + 1..]
        .iter()
        .position(|segment| is_yaml_rule(segment.trim_end_matches(['\n', '\r'])))
    else {
        return (None, source.to_owned());
    };
    let mut end = open + close_relative + 2;
    while end < segments.len()
        && segments[end]
            .trim_end_matches(['\n', '\r'])
            .trim()
            .is_empty()
    {
        end += 1;
    }
    let header = segments[..end].concat();
    let body = segments[end..].concat();
    (Some(header), body)
}

fn is_yaml_rule(line: &str) -> bool {
    uncomment_comment(line).trim() == "---"
}

fn marker_kind(line: &str) -> Option<CellKind> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('#')?.trim_start();
    let after = rest.strip_prefix("%%")?;
    if !(after.is_empty() || after.starts_with(char::is_whitespace)) {
        return None;
    }
    let options = after.trim();
    if options.to_ascii_lowercase().contains("[markdown]") {
        Some(CellKind::Markdown)
    } else if options.to_ascii_lowercase().contains("[raw]") {
        Some(CellKind::Raw)
    } else {
        Some(CellKind::Code)
    }
}

fn uncomment_comment(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let Some(rest) = trimmed.strip_prefix('#') else {
        return line.to_owned();
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    format!("{indent}{rest}")
}

fn uncomment_magics(source: &str) -> String {
    let mut continuation = false;
    let mut result = Vec::new();
    for line in source.split('\n') {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let candidate = trimmed
            .strip_prefix("# ")
            .or_else(|| trimmed.strip_prefix('#'))
            .unwrap_or(trimmed);
        let is_magic = candidate.starts_with('%') || candidate.starts_with('!');
        if is_magic || continuation {
            let uncommented = if trimmed.starts_with('#') {
                format!("{indent}{candidate}")
            } else {
                line.to_owned()
            };
            continuation = uncommented.trim_end().ends_with('\\');
            result.push(uncommented);
        } else {
            continuation = false;
            result.push(line.to_owned());
        }
    }
    result.join("\n")
}

fn comment_magics(source: &str) -> String {
    source
        .split('\n')
        .map(comment_magic_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn comment_magic_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if trimmed.starts_with('%') || trimmed.starts_with('!') {
        format!("{indent}# {trimmed}")
    } else {
        line.to_owned()
    }
}
