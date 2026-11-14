//! Notebook-level row cursor: j/k move one row through sources and outputs,
//! J/K move several, ]]/[[ jump cells, }/{ also stop at outputs, and the
//! view only scrolls when the cursor reaches its edges.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use nb_core::{Cell, Document, FormatBacking, OutputData, OutputItem, StreamBuffer, StreamKind};
use nb_kernel::FakeKernel;
use nb_sdk::ExtensionRegistry;
use nb_tui::focus::CellPart;
use nb_tui::{BuiltinsPlugin, CellMode, Coordinator, FocusLayer};
use ratatui::{Terminal, backend::TestBackend};

fn stream(text: &str) -> OutputItem {
    let mut buffer = StreamBuffer::default();
    buffer.push_chunk(text);
    OutputItem::new(
        OutputData::Stream {
            stream: StreamKind::Stdout,
            buffer,
        },
        false,
    )
}

/// Cells `(source, stdout)`, drawn in an 80x20 terminal.
fn coordinator(cells: &[(&str, &str)]) -> Coordinator {
    let cells = cells
        .iter()
        .map(|(source, out)| {
            let mut cell = Cell::new_code();
            cell.buffer.replace_all((*source).to_string());
            if !out.is_empty() {
                cell.outputs.push(stream(out));
            }
            cell
        })
        .collect();
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, _handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::from_cells(cells, FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    );
    draw(&mut c);
    c
}

fn draw(c: &mut Coordinator) {
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
}

fn keys(c: &mut Coordinator, text: &str) {
    for ch in text.chars() {
        c.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), Instant::now());
        draw(c);
    }
}

fn press(c: &mut Coordinator, code: KeyCode) {
    c.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    draw(c);
}

fn lines(n: usize, word: &str) -> String {
    (0..n).map(|i| format!("{word} {i}\n")).collect()
}

fn at(c: &Coordinator, index: usize, part: CellPart) -> bool {
    c.selected_cell == c.document.order()[index] && c.selected_part == part
}

#[test]
fn j_walks_rows_through_a_long_output_and_into_the_next_cell() {
    let long = lines(10, "out");
    let mut c = coordinator(&[("a = 1\nb = 2", &long), ("c = 3", "")]);
    c.settings.set_value(
        &nb_sdk::SettingId("display.output_default_view".into()),
        nb_sdk::SettingValue::Text("full".into()),
        &c.registry,
    );
    draw(&mut c);
    keys(&mut c, "j");
    assert!(at(&c, 0, CellPart::Source), "second source line");
    keys(&mut c, "j");
    assert!(at(&c, 0, CellPart::Output), "first output line");
    keys(&mut c, "9j");
    assert!(at(&c, 0, CellPart::Output), "still in the output");
    keys(&mut c, "j");
    assert!(at(&c, 1, CellPart::Source), "next cell after the last output line");
    keys(&mut c, "K");
    assert!(at(&c, 0, CellPart::Output), "K moves five rows back");
}

#[test]
fn brackets_jump_cells_and_braces_also_stop_at_outputs() {
    let mut c = coordinator(&[("x", "1\n"), ("y", "2\n"), ("z", "")]);
    keys(&mut c, "]]");
    assert!(at(&c, 1, CellPart::Source));
    keys(&mut c, "]]");
    assert!(at(&c, 2, CellPart::Source));
    keys(&mut c, "[[");
    assert!(at(&c, 1, CellPart::Source));
    keys(&mut c, "gg}");
    assert!(at(&c, 0, CellPart::Output));
    keys(&mut c, "}");
    assert!(at(&c, 1, CellPart::Source));
    keys(&mut c, "{");
    assert!(at(&c, 0, CellPart::Output));
    keys(&mut c, "2]]");
    assert!(at(&c, 2, CellPart::Source), "counts repeat the jump");
}

#[test]
fn i_opens_the_cell_at_the_cursor_line_and_esc_returns_to_it() {
    let mut c = coordinator(&[("first\n    second\nthird", "")]);
    keys(&mut c, "j");
    keys(&mut c, "i");
    let id = c.document.order()[0].clone();
    let cursor = c.document.cell(&id).unwrap().buffer.cursor();
    assert_eq!((cursor.row, cursor.col), (1, 4), "line 2, first non-blank");
    assert!(matches!(
        c.focus.top(),
        FocusLayer::CellEdit {
            mode: CellMode::Normal,
            ..
        }
    ));
    keys(&mut c, "j");
    press(&mut c, KeyCode::Esc);
    assert!(matches!(c.focus.top(), FocusLayer::Notebook { .. }));
    keys(&mut c, "i");
    let cursor = c.document.cell(&id).unwrap().buffer.cursor();
    assert_eq!(cursor.row, 2, "the notebook cursor came back on line 3");
}

#[test]
fn i_on_an_output_row_reads_from_that_line() {
    let mut c = coordinator(&[("run()", &lines(3, "row"))]);
    keys(&mut c, "jjj");
    keys(&mut c, "i");
    let out = c.output_text.as_ref().expect("reading the output");
    assert_eq!(out.buffer.cursor().row, 2);
    keys(&mut c, "k");
    press(&mut c, KeyCode::Esc);
    keys(&mut c, "i");
    let out = c.output_text.as_ref().expect("reading again");
    assert_eq!(out.buffer.cursor().row, 1, "Esc kept the reading position");
}

#[test]
fn the_view_does_not_move_while_the_cursor_is_inside_it() {
    let cells: Vec<(String, String)> = (0..8).map(|n| (format!("v{n} = {n}"), String::new())).collect();
    let refs: Vec<(&str, &str)> = cells.iter().map(|(s, o)| (s.as_str(), o.as_str())).collect();
    let mut c = coordinator(&refs);
    keys(&mut c, "]]");
    assert_eq!(c.scroll_row, 0, "the next cell is on screen: no scroll");
    keys(&mut c, "i");
    press(&mut c, KeyCode::Esc);
    assert_eq!(c.scroll_row, 0, "entering and leaving a cell does not scroll");
    // Moving past the bottom edge scrolls just enough.
    keys(&mut c, "G");
    let bottom = c.scroll_row;
    assert!(bottom > 0);
    keys(&mut c, "k");
    assert_eq!(c.scroll_row, bottom, "one row up from the bottom stays put");
}

#[test]
fn the_wheel_scrolls_the_view_and_drags_the_cursor_along() {
    let cells: Vec<(String, String)> = (0..10).map(|n| (format!("v{n} = {n}"), String::new())).collect();
    let refs: Vec<(&str, &str)> = cells.iter().map(|(s, o)| (s.as_str(), o.as_str())).collect();
    let mut c = coordinator(&refs);
    for _ in 0..4 {
        c.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        draw(&mut c);
    }
    assert!(c.scroll_row > 0);
    assert_ne!(c.selected_cell, c.document.order()[0], "the cursor left the top cell");
    // The next key moves from where the cursor is, without a jump back.
    let scroll = c.scroll_row;
    keys(&mut c, "j");
    assert!(c.scroll_row >= scroll);
}

fn ten_cells() -> Coordinator {
    let cells: Vec<(String, String)> = (0..10)
        .map(|n| (format!("a{n} = 1\nb{n} = 2"), String::new()))
        .collect();
    let refs: Vec<(&str, &str)> = cells.iter().map(|(s, o)| (s.as_str(), o.as_str())).collect();
    coordinator(&refs)
}

#[test]
fn jumping_to_an_off_screen_cell_puts_its_top_at_the_top() {
    let mut c = ten_cells();
    // Stepping cell by cell scrolls minimally (the next cell is on screen);
    // one counted jump lands far below the view and aligns the target.
    keys(&mut c, "6]]");
    let index = c.document.position_of(&c.selected_cell).unwrap();
    assert_eq!(index, 6);
    let cell_top = c.layout_cache.heights().prefix_sum(index);
    let total = c.layout_cache.heights().total_height();
    let max_scroll = total - u32::from(c.last_geometry.viewport_rect.height);
    assert_eq!(
        c.scroll_row,
        cell_top.min(max_scroll),
        "the jumped-to cell starts at the top (unless the notebook ends first)"
    );
    keys(&mut c, "gg3]]");
    let index = c.document.position_of(&c.selected_cell).unwrap();
    assert_eq!(index, 3);
    assert_eq!(c.scroll_row, 0, "a jump to a cell already on screen does not scroll");
}

#[test]
fn entering_and_leaving_a_cell_mid_notebook_keeps_the_view() {
    let mut c = ten_cells();
    keys(&mut c, "G");
    keys(&mut c, "kkkkkk");
    let scroll = c.scroll_row;
    assert!(scroll > 0);
    keys(&mut c, "i");
    keys(&mut c, "j");
    press(&mut c, KeyCode::Esc);
    assert_eq!(c.scroll_row, scroll);
}

fn screen(c: &mut Coordinator) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// A counted move lands where that many single steps would, across
/// sources, long outputs and cell boundaries, in both directions.
#[test]
fn counted_moves_match_single_steps() {
    let long = lines(12, "out");
    let cells: &[(&str, &str)] = &[
        ("a = 1\nb = 2", &long),
        ("c = 3", ""),
        ("d = 4\ne = 5\nf = 6", "x\ny\n"),
        ("g = 7", ""),
    ];
    let full = |c: &mut Coordinator| {
        c.settings.set_value(
            &nb_sdk::SettingId("display.output_default_view".into()),
            nb_sdk::SettingValue::Text("full".into()),
            &c.registry,
        );
        draw(c);
    };
    let counts = [1, 2, 3, 5, 8, 13, 21, 34];
    for (count, back) in counts.into_iter().flat_map(|n| [(n, false), (n, true)]) {
        let (step, motion) = if back { ("k", "k") } else { ("j", "j") };
        let mut counted = coordinator(cells);
        let mut single = coordinator(cells);
        full(&mut counted);
        full(&mut single);
        if back {
            keys(&mut counted, "G");
            keys(&mut single, "G");
        }
        keys(&mut counted, &format!("{count}{motion}"));
        for _ in 0..count {
            keys(&mut single, step);
        }
        let position = |c: &Coordinator| c.document.position_of(&c.selected_cell);
        assert_eq!(position(&counted), position(&single), "{count}{motion}");
        assert_eq!(counted.selected_part, single.selected_part, "{count}{motion}");
        assert_eq!(screen(&mut counted), screen(&mut single), "{count}{motion}");
    }
}
