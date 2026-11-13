use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use wb_core::buffer::{CursorPos, Selection, VisualKind};
use wb_core::cell::{Cell, CellKind};
use wb_core::codec::{ipynb, percent, reload_external, write_atomic};
use wb_core::document::{Document, ExternalReloadOutcome, FormatBacking};
use wb_core::ops::DocOp;
use wb_core::output::{MimeBundle, OutputData, OutputItem, StreamBuffer, StreamKind};
use wb_core::{CellId, TextBuffer};

fn cell(id: &str, kind: CellKind, source: &str) -> Cell {
    let mut cell = Cell::new(kind, source);
    cell.id = CellId(id.to_owned());
    cell
}

fn rich(display_id: Option<&str>) -> OutputItem {
    let mut bundle = MimeBundle::new();
    bundle.insert("text/plain".into(), Value::String("old".into()));
    OutputItem::new(
        OutputData::Rich {
            bundle,
            metadata: Map::new(),
            display_id: display_id.map(str::to_owned),
            is_execute_result: false,
            execution_count: None,
        },
        false,
    )
}

#[test]
fn text_buffer_supports_unicode_multiline_rollback_and_history() {
    let mut buffer = TextBuffer::new("αβ\nhello");
    assert_eq!(buffer.line_count(), 2);
    assert_eq!(buffer.char_offset_of(CursorPos { row: 1, col: 2 }), 5);
    buffer.set_cursor(CursorPos { row: 1, col: 2 });
    buffer.checkpoint();
    buffer.insert_str("X\n🙂");
    assert_eq!(buffer.as_str(), "αβ\nheX\n🙂llo");
    assert_eq!(buffer.cursor(), CursorPos { row: 2, col: 1 });
    buffer.rollback_chars(2);
    assert_eq!(buffer.as_str(), "αβ\nheXllo");
    assert_eq!(buffer.cursor(), CursorPos { row: 1, col: 3 });
    assert!(buffer.undo());
    assert_eq!(buffer.as_str(), "αβ\nhello");
    assert!(buffer.redo());
    assert_eq!(buffer.as_str(), "αβ\nheXllo");

    buffer.set_cursor(CursorPos { row: 99, col: 99 });
    assert_eq!(buffer.cursor(), CursorPos { row: 1, col: 6 });
    buffer.set_selection(Selection {
        kind: VisualKind::Char,
        anchor: CursorPos { row: 99, col: 99 },
        head: CursorPos { row: 0, col: 99 },
    });
    assert_eq!(
        buffer.selection().unwrap().anchor,
        CursorPos { row: 1, col: 6 }
    );
    buffer.clear_selection();
    assert!(buffer.selection().is_none());
}

#[test]
fn text_buffer_lines_spans_and_word_motions_are_cell_local() {
    let mut buffer = TextBuffer::new("  foo bar\nbaz");
    assert_eq!(buffer.line(0), Some("  foo bar"));
    assert_eq!(buffer.line_char_len(0), 9);
    assert_eq!(buffer.first_nonblank_col(0), 2);
    assert_eq!(buffer.first_nonblank_col(1), 0);

    buffer.set_cursor(CursorPos { row: 0, col: 2 });
    assert_eq!(buffer.word_next(1), CursorPos { row: 0, col: 6 });
    assert_eq!(buffer.word_end(1), CursorPos { row: 0, col: 4 });
    buffer.set_cursor(CursorPos { row: 0, col: 6 });
    assert_eq!(buffer.word_back(1), CursorPos { row: 0, col: 2 });
    assert_eq!(
        buffer.inner_word_span(),
        (CursorPos { row: 0, col: 6 }, CursorPos { row: 0, col: 9 })
    );
    assert_eq!(
        buffer.around_word_span(),
        (CursorPos { row: 0, col: 5 }, CursorPos { row: 0, col: 9 })
    );

    assert_eq!(buffer.yank_line_range(0..1), "  foo bar\n");
    assert_eq!(buffer.delete_line_range(0..1), "  foo bar\n");
    assert_eq!(buffer.as_str(), "baz");
    buffer.set_cursor(CursorPos::default());
    buffer.insert_newline();
    buffer.insert_str("x");
    assert_eq!(buffer.as_str(), "\nxbaz");
    assert_eq!(
        buffer.delete_span(CursorPos { row: 0, col: 0 }, CursorPos { row: 1, col: 1 }),
        "\nx"
    );
    assert_eq!(buffer.as_str(), "baz");
}

#[test]
fn stream_buffer_coalesces_carriage_returns_and_caps_lines() {
    let mut stream = StreamBuffer::with_cap(10);
    stream.push_chunk("10%\r");
    stream.push_chunk("50%\r");
    stream.push_chunk("100%\ndone\n");
    assert_eq!(stream.as_str(), "100%\ndone\n");
    stream.push_chunk("12345\rab");
    assert_eq!(stream.as_str(), "100%\ndone\nab345");

    let mut crlf = StreamBuffer::with_cap(10);
    crlf.push_chunk("a\r\nb");
    assert_eq!(crlf.as_str(), "a\nb");

    let mut capped = StreamBuffer::with_cap(2);
    capped.push_chunk("1\n2\n3\n");
    assert_eq!(capped.truncated_lines(), 2);
    assert!(capped.as_str().starts_with("[... output truncated ...]\n"));
    assert!(capped.as_str().ends_with("3\n"));
    let mut default = StreamBuffer::default();
    default.push_chunk("x\n");
    assert_eq!(default.as_str(), "x\n");
}

#[test]
fn document_outputs_and_display_index_are_updated_in_constant_lookup_path() {
    let mut first = cell("first", CellKind::Code, "x");
    let output = rich(Some("display-1"));
    let output_id = output.id;
    first.outputs.push(output);
    let mut document = Document::from_cells(vec![first], FormatBacking::PercentPy { header: None });
    assert_eq!(
        document.display_targets("display-1"),
        &[(CellId("first".into()), output_id)]
    );
    let mut bundle = BTreeMap::new();
    bundle.insert("text/plain".into(), json!("new"));
    let affected = document.update_display("display-1", bundle, Map::new());
    assert_eq!(affected, vec![CellId("first".into())]);
    match &document.cell(&CellId("first".into())).unwrap().outputs[0].data {
        OutputData::Rich { bundle, .. } => assert_eq!(bundle["text/plain"], json!("new")),
        other => panic!("unexpected output: {other:?}"),
    }
    document.append_stream(&CellId("first".into()), StreamKind::Stdout, "a\n", false);
    document.append_stream(&CellId("first".into()), StreamKind::Stdout, "b\n", false);
    assert_eq!(
        document
            .cell(&CellId("first".into()))
            .unwrap()
            .outputs
            .len(),
        2
    );
    assert_eq!(
        document.cell(&CellId("first".into())).unwrap().outputs[1].rev,
        2
    );
    let cleared = document.clear_cell_outputs(&CellId("first".into()));
    assert_eq!(cleared.len(), 2);
    assert!(document.display_targets("display-1").is_empty());
    document.append_stream(&CellId("missing".into()), StreamKind::Stderr, "log\n", true);
    assert_eq!(document.session_log.len(), 1);
    document.append_stream(
        &CellId("missing".into()),
        StreamKind::Stderr,
        "next\n",
        true,
    );
    assert_eq!(document.session_log.len(), 1);
    match &document.session_log[0].data {
        OutputData::Stream { buffer, .. } => assert_eq!(buffer.as_str(), "log\nnext\n"),
        other => panic!("unexpected session output: {other:?}"),
    }
}

#[test]
fn every_document_operation_has_working_undo_and_redo() {
    let first = cell("a", CellKind::Code, "one");
    let second = cell("b", CellKind::Code, "two");
    let third = cell("c", CellKind::Markdown, "three");
    let mut document = Document::from_cells(
        vec![first.clone(), second.clone()],
        FormatBacking::Ipynb { extra: Map::new() },
    );

    document.apply(DocOp::InsertCells {
        at_index: 1,
        cells: vec![third.clone()],
    });
    assert_eq!(
        document.order(),
        &[CellId("a".into()), CellId("c".into()), CellId("b".into())]
    );
    assert!(document.undo());
    assert_eq!(document.order(), &[CellId("a".into()), CellId("b".into())]);
    assert!(document.redo());

    document.apply(DocOp::MoveCells {
        from_range: 0..1,
        to_index: 2,
    });
    assert_eq!(
        document.order(),
        &[CellId("c".into()), CellId("b".into()), CellId("a".into())]
    );
    assert!(document.undo());
    assert_eq!(
        document.order(),
        &[CellId("a".into()), CellId("c".into()), CellId("b".into())]
    );
    assert!(document.redo());

    let removed = document.cell(&CellId("c".into())).unwrap().clone();
    document.apply(DocOp::DeleteCells {
        at_index: 1,
        removed: vec![removed],
    });
    assert!(document.cell(&CellId("c".into())).is_none());
    assert!(document.undo());
    assert!(document.cell(&CellId("c".into())).is_some());

    let changed = document.cell(&CellId("b".into())).unwrap().clone();
    document.apply(DocOp::CommitCellSource {
        cell_id: changed.id.clone(),
        old_source: "two".into(),
        new_source: "updated".into(),
    });
    assert_eq!(
        document.cell(&CellId("b".into())).unwrap().buffer.as_str(),
        "updated"
    );
    assert!(document.undo());
    assert_eq!(
        document.cell(&CellId("b".into())).unwrap().buffer.as_str(),
        "two"
    );
    assert!(document.redo());

    document.apply(DocOp::SetCellKind {
        cell_id: CellId("b".into()),
        old_kind: CellKind::Code,
        new_kind: CellKind::Markdown,
        cleared_outputs: Vec::new(),
    });
    assert_eq!(
        document.cell(&CellId("b".into())).unwrap().kind,
        CellKind::Markdown
    );
    assert!(document.undo());
    assert_eq!(
        document.cell(&CellId("b".into())).unwrap().kind,
        CellKind::Code
    );

    let mut split = cell("split", CellKind::Code, "left\nright");
    split.buffer.set_cursor(CursorPos { row: 0, col: 4 });
    let split_op = DocOp::SplitCell {
        original: split.clone(),
        at_index: 0,
        left_id: CellId("left-id".into()),
        right_id: CellId("right-id".into()),
    };
    document = Document::from_cells(
        vec![split.clone()],
        FormatBacking::Ipynb { extra: Map::new() },
    );
    document.apply(split_op);
    assert_eq!(
        document.order(),
        &[CellId("left-id".into()), CellId("right-id".into())]
    );
    assert!(document.undo());
    assert_eq!(document.order(), &[CellId("split".into())]);
    assert!(document.redo());

    let left = cell("left", CellKind::Code, "a");
    let right = cell("right", CellKind::Code, "b");
    let merge_op = DocOp::MergeCells {
        at_index: 0,
        originals: vec![left.clone(), right.clone()],
        merged_id: CellId("merged".into()),
    };
    document = Document::from_cells(
        vec![left, right],
        FormatBacking::Ipynb { extra: Map::new() },
    );
    document.apply(merge_op);
    assert_eq!(
        document
            .cell(&CellId("merged".into()))
            .unwrap()
            .buffer
            .as_str(),
        "a\nb"
    );
    assert!(document.undo());
    assert_eq!(
        document.order(),
        &[CellId("left".into()), CellId("right".into())]
    );

    let mut output_cell = cell("out", CellKind::Code, "print(1)");
    let output = rich(Some("d"));
    output_cell.outputs.push(output.clone());
    document = Document::from_cells(
        vec![output_cell.clone()],
        FormatBacking::Ipynb { extra: Map::new() },
    );
    document.apply(DocOp::ClearOutputs {
        cleared: vec![(output_cell.id.clone(), vec![output])],
    });
    assert!(
        document
            .cell(&CellId("out".into()))
            .unwrap()
            .outputs
            .is_empty()
    );
    assert!(document.undo());
    assert_eq!(
        document.cell(&CellId("out".into())).unwrap().outputs.len(),
        1
    );
}

#[test]
fn ipynb_codec_upgrades_preserves_unknowns_and_serializes_canonically() {
    let input = r##"{
  "cells": [
    {
      "cell_type": "code",
      "execution_count": 2,
      "metadata": {"custom": true},
      "outputs": [
        {"name": "stdout", "output_type": "stream", "text": ["hi\n"]},
        {"data": {"text/plain": ["42"]}, "metadata": {}, "output_type": "display_data", "transient": {"display_id": "d1"}, "foreign": 7}
      ],
      "source": ["x = 42\n", "x"]
    },
    {"cell_type": "markdown", "metadata": {}, "source": "# title"}
  ],
  "metadata": {"kernelspec": {"name": "python3"}},
  "nbformat": 4,
  "nbformat_minor": 4,
  "notebook_custom": "kept"
}"##;
    let document = ipynb::parse_ipynb(input).unwrap();
    assert_eq!(document.order().len(), 2);
    assert_eq!(document.order()[0].as_str().len(), 8);
    assert_eq!(
        document.cell(&document.order()[0]).unwrap().buffer.as_str(),
        "x = 42\nx"
    );
    assert_eq!(document.display_targets("d1").len(), 1);
    let serialized = ipynb::serialize_ipynb(&document, false).unwrap();
    assert!(serialized.ends_with('\n'));
    let value: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(value["nbformat_minor"], json!(5));
    assert_eq!(value["notebook_custom"], json!("kept"));
    assert_eq!(value["cells"][0]["source"], json!(["x = 42\n", "x"]));
    assert!(serialized.contains("\"foreign\": 7"));
    assert!(!serialized.contains("transient"));

    let sidecar = ipynb::serialize_ipynb(
        &Document::from_cells(
            vec![cell("p", CellKind::Code, "x")],
            FormatBacking::PercentPy {
                header: Some("# ---\nfoo: bar\n# ---\n".into()),
            },
        ),
        true,
    )
    .unwrap();
    assert!(sidecar.contains("workbench_percent_header"));
    let restored = ipynb::parse_ipynb(&sidecar).unwrap();
    assert!(matches!(restored.backing, FormatBacking::PercentPy { .. }));
}

#[test]
fn percent_codec_handles_headers_markers_markdown_raw_and_magics() {
    let source = "# ---\nformat: percent\n# ---\n\n# %%\n# %time x\nx = 1\n\n# %% [markdown]\n# Hello\n#\n# world\n\n# %% [raw]\n# raw text\n";
    let document = percent::parse_percent(source).unwrap();
    assert_eq!(document.order().len(), 3);
    assert_eq!(
        document.cell(&document.order()[0]).unwrap().buffer.as_str(),
        "%time x\nx = 1"
    );
    assert_eq!(
        document.cell(&document.order()[1]).unwrap().kind,
        CellKind::Markdown
    );
    assert_eq!(
        document.cell(&document.order()[1]).unwrap().buffer.as_str(),
        "Hello\n\nworld"
    );
    assert_eq!(
        document.cell(&document.order()[2]).unwrap().kind,
        CellKind::Raw
    );
    let output = percent::serialize_percent(&document);
    assert!(output.contains("# %% [markdown]"));
    assert!(output.contains("# %% [raw]"));
    assert!(output.contains("# %time x"));
    assert!(output.contains("# Hello"));

    let plain = percent::parse_percent("%run foo.py\nprint(1)").unwrap();
    assert_eq!(plain.order().len(), 1);
    assert_eq!(
        plain.cell(&plain.order()[0]).unwrap().buffer.as_str(),
        "%run foo.py\nprint(1)"
    );
    assert_eq!(
        percent::serialize_percent(&plain),
        "# %run foo.py\nprint(1)"
    );
}

#[test]
fn external_py_reload_preserves_matching_runtime_outputs_and_clears_history() {
    let path = temp_path("reload.py");
    fs::write(&path, "# %%\na = 1\n\n# %% [markdown]\n# old\n").unwrap();
    let mut document = wb_core::codec::open_path(&path).unwrap();
    let code_id = document.order()[0].clone();
    document.push_output(&code_id, rich(None));
    document.apply(DocOp::CommitCellSource {
        cell_id: code_id.clone(),
        old_source: "a = 1".into(),
        new_source: "a = 1".into(),
    });
    fs::write(&path, "# %%\na = 1\n\n# %% [markdown]\n# new\n").unwrap();
    let result = reload_external(&mut document, &path).unwrap();
    assert_eq!(
        result,
        ExternalReloadOutcome::ReloadedPreservingOutputs { matched_cells: 1 }
    );
    assert_eq!(document.cell(&code_id).unwrap().outputs.len(), 1);
    assert_eq!(
        document.cell(&document.order()[1]).unwrap().buffer.as_str(),
        "new"
    );
    assert!(!document.history.pop_undo().is_some());

    fs::write(&path, "# %%\na = 1\n\n# %% [markdown]\n# new\n").unwrap();
    assert_eq!(
        reload_external(&mut document, &path).unwrap(),
        ExternalReloadOutcome::Unchanged
    );
    fs::remove_file(path).ok();
}

#[test]
fn atomic_write_uses_sibling_temp_and_replaces_target() {
    let path = temp_path("atomic.txt");
    write_atomic(&path, b"new contents").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new contents");
    assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists());
    fs::remove_file(path).ok();
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wb-core-{}-{name}", std::process::id()))
}
