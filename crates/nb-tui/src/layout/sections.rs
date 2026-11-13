//! How a cell's outputs group into the sections its card shows, in order:
//! the streams (stdout and stderr share one section, switched with tabs),
//! then display outputs, then errors, and the `Out [n]` result last.
//!
//! The layout cache, the renderer and the read-only output text all use
//! this, so measured rows, drawn rows and text rows line up.

use nb_core::{Cell, OutputData, OutputItem, StreamKind};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionKind {
    /// stdout/stderr; `active` is the stream shown.
    Streams {
        active: StreamKind,
        stdout: bool,
        stderr: bool,
    },
    Display,
    Error,
    Result,
}

pub struct OutputSection<'a> {
    pub kind: SectionKind,
    /// The outputs drawn in this section, in arrival order.
    pub items: Vec<&'a OutputItem>,
}

impl OutputSection<'_> {
    pub fn late(&self) -> bool {
        self.items.iter().any(|item| item.transient_late)
    }
}

fn stream_of(item: &OutputItem) -> Option<StreamKind> {
    match &item.data {
        OutputData::Stream { stream, .. } => Some(*stream),
        _ => None,
    }
}

pub fn output_sections(cell: &Cell) -> Vec<OutputSection<'_>> {
    let has = |kind| cell.outputs.iter().any(|item| stream_of(item) == Some(kind));
    let (stdout, stderr) = (has(StreamKind::Stdout), has(StreamKind::Stderr));
    let mut sections = Vec::new();
    if stdout || stderr {
        let active = if !stdout || (stderr && cell.stream_tab == StreamKind::Stderr) {
            StreamKind::Stderr
        } else {
            StreamKind::Stdout
        };
        sections.push(OutputSection {
            kind: SectionKind::Streams {
                active,
                stdout,
                stderr,
            },
            items: cell
                .outputs
                .iter()
                .filter(|item| stream_of(item) == Some(active))
                .collect(),
        });
    }
    let single = |kind: SectionKind, wanted: fn(&OutputData) -> bool| {
        cell.outputs
            .iter()
            .filter(move |item| wanted(&item.data))
            .map(move |item| OutputSection {
                kind,
                items: vec![item],
            })
    };
    sections.extend(single(SectionKind::Display, |data| {
        matches!(data, OutputData::Rich { is_execute_result: false, .. })
    }));
    sections.extend(single(SectionKind::Error, |data| {
        matches!(data, OutputData::Error { .. })
    }));
    sections.extend(single(SectionKind::Result, |data| {
        matches!(data, OutputData::Rich { is_execute_result: true, .. })
    }));
    sections
}

/// Both streams have output, so the tabs can switch.
pub fn has_stream_tabs(cell: &Cell) -> bool {
    output_sections(cell).first().is_some_and(|section| {
        matches!(
            section.kind,
            SectionKind::Streams {
                stdout: true,
                stderr: true,
                ..
            }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use nb_core::StreamBuffer;

    fn stream(kind: StreamKind, text: &str) -> OutputItem {
        let mut buffer = StreamBuffer::default();
        buffer.push_chunk(text);
        OutputItem::new(OutputData::Stream { stream: kind, buffer }, false)
    }

    fn result() -> OutputItem {
        OutputItem::new(
            OutputData::Rich {
                bundle: BTreeMap::new(),
                metadata: Default::default(),
                display_id: None,
                is_execute_result: true,
                execution_count: Some(1),
            },
            false,
        )
    }

    #[test]
    fn streams_first_result_last_and_tabs_pick_a_stream() {
        let mut cell = Cell::new_code();
        cell.outputs = vec![
            result(),
            stream(StreamKind::Stdout, "a\n"),
            stream(StreamKind::Stderr, "warn\n"),
            stream(StreamKind::Stdout, "b\n"),
        ];
        let kinds: Vec<_> = output_sections(&cell).iter().map(|s| s.kind).collect();
        assert!(matches!(
            kinds[0],
            SectionKind::Streams {
                active: StreamKind::Stdout,
                ..
            }
        ));
        assert_eq!(kinds[1], SectionKind::Result);
        assert_eq!(output_sections(&cell)[0].items.len(), 2);
        assert!(has_stream_tabs(&cell));

        cell.stream_tab = StreamKind::Stderr;
        assert_eq!(output_sections(&cell)[0].items.len(), 1);

        // Only stderr: it shows regardless of the tab.
        cell.outputs = vec![stream(StreamKind::Stderr, "warn\n")];
        cell.stream_tab = StreamKind::Stdout;
        assert!(matches!(
            output_sections(&cell)[0].kind,
            SectionKind::Streams {
                active: StreamKind::Stderr,
                ..
            }
        ));
        assert!(!has_stream_tabs(&cell));
    }
}
