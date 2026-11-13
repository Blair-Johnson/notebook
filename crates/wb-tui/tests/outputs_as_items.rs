//! Outputs are selectable notebook items that can be entered like cells.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use wb_core::{Cell, Document, FormatBacking, OutputData, OutputItem, StreamBuffer, StreamKind};
use wb_kernel::FakeKernel;
use wb_sdk::ExtensionRegistry;
use wb_tui::focus::CellPart;
use wb_tui::{BuiltinsPlugin, CellMode, Coordinator, FocusLayer};

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

/// Cells `(source, stdout)`; empty stdout means no output.
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
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("terminal");
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

#[test]
fn j_and_k_stop_on_outputs_between_cells() {
    let mut c = coordinator(&[("print(1)", "1\n"), ("x = 2", ""), ("print(3)", "3\n")]);
    let order = c.document.order().to_vec();
    keys(&mut c, "j");
    assert_eq!((&c.selected_cell, c.selected_part), (&order[0], CellPart::Output));
    keys(&mut c, "j");
    assert_eq!((&c.selected_cell, c.selected_part), (&order[1], CellPart::Source));
    keys(&mut c, "jj");
    assert_eq!((&c.selected_cell, c.selected_part), (&order[2], CellPart::Output));
    keys(&mut c, "k");
    assert_eq!((&c.selected_cell, c.selected_part), (&order[2], CellPart::Source));
    keys(&mut c, "gg");
    assert_eq!((&c.selected_cell, c.selected_part), (&order[0], CellPart::Source));
}

#[test]
fn entering_an_output_gives_read_only_vim_navigation_and_yank() {
    let mut c = coordinator(&[("print(stuff)", "alpha beta\ngamma delta\n")]);
    keys(&mut c, "ji");
    assert!(matches!(
        c.focus.top(),
        FocusLayer::OutputFocus {
            mode: CellMode::Normal,
            ..
        }
    ));
    keys(&mut c, "jw");
    let cursor = c.output_text.as_ref().unwrap().buffer.cursor();
    assert_eq!((cursor.row, cursor.col), (1, 6));
    keys(&mut c, "yiw");
    assert_eq!(c.vim_register_text(), "delta");
    // Edits are ignored: the output text never changes.
    keys(&mut c, "dd");
    keys(&mut c, "x");
    assert_eq!(
        c.output_text.as_ref().unwrap().buffer.as_str(),
        "alpha beta\ngamma delta"
    );
    keys(&mut c, "vey");
    assert_eq!(c.vim_register_text(), "delta");
    press(&mut c, KeyCode::Esc);
    assert!(matches!(c.focus.top(), FocusLayer::NotebookNormal));
    assert_eq!(c.selected_part, CellPart::Output);
}

#[test]
fn dd_on_an_output_clears_it_and_r_skips_past_outputs() {
    let mut c = coordinator(&[("print(1)", "1\n"), ("print(2)", "")]);
    let order = c.document.order().to_vec();
    keys(&mut c, "jdd");
    assert!(c.document.cell(&order[0]).unwrap().outputs.is_empty());
    assert_eq!(c.selected_part, CellPart::Source);

    let mut c = coordinator(&[("print(1)", "1\n"), ("print(2)", "")]);
    keys(&mut c, "r");
    assert_eq!(
        (&c.selected_cell, c.selected_part),
        (&c.document.order()[1].clone(), CellPart::Source)
    );
}

#[test]
fn enter_on_a_traceback_line_jumps_to_that_cell() {
    let mut c = coordinator(&[("a = 1\nb = a / 0", ""), ("run()", "")]);
    let order = c.document.order().to_vec();
    c.document.cell_mut(&order[0]).unwrap().execution_count = Some(4);
    c.document
        .cell_mut(&order[1])
        .unwrap()
        .outputs
        .push(OutputItem::new(
            OutputData::Error {
                ename: "ZeroDivisionError".into(),
                evalue: "division by zero".into(),
                traceback: vec![
                    "\u{1b}[0;31mZeroDivisionError\u{1b}[0m Traceback".into(),
                    "Cell \u{1b}[0;32mIn[4], line 2\u{1b}[0m".into(),
                ],
            },
            false,
        ));
    draw(&mut c);
    keys(&mut c, "jjji");
    keys(&mut c, "j");
    press(&mut c, KeyCode::Enter);
    assert!(matches!(
        c.focus.top(),
        FocusLayer::CellEdit { cell_id, mode: CellMode::Normal, .. } if cell_id == &order[0]
    ));
    assert_eq!(c.document.cell(&order[0]).unwrap().buffer.cursor().row, 1);
}
