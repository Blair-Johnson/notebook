//! Clearing an output by hand can be undone, undo selects the cell it
//! restores, and the disk-reload bar needs a deliberate key.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, Document, FormatBacking, OutputData, OutputItem, StreamBuffer, StreamKind};
use nb_kernel::FakeKernel;
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};
use ratatui::{Terminal, backend::TestBackend};

fn notebook(cells: &[(&str, &str)]) -> Coordinator {
    let cells = cells
        .iter()
        .map(|(source, out)| {
            let mut cell = Cell::new_code();
            cell.buffer.replace_all(source.to_string());
            if !out.is_empty() {
                let mut buffer = StreamBuffer::default();
                buffer.push_chunk(out);
                cell.outputs.push(OutputItem::new(
                    OutputData::Stream {
                        stream: StreamKind::Stdout,
                        buffer,
                    },
                    false,
                ));
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
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
}

fn keys(c: &mut Coordinator, text: &str) {
    for ch in text.chars() {
        c.on_key(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            Instant::now(),
        );
        draw(c);
    }
}

fn output_text(c: &Coordinator, index: usize) -> Option<String> {
    let id = &c.document.order()[index];
    c.document
        .cell(id)
        .unwrap()
        .outputs
        .first()
        .map(|o| o.plain_text())
}

#[test]
fn clearing_outputs_by_hand_can_be_undone() {
    let mut c = notebook(&[("print('a')", "a\n"), ("print('b')", "b\n")]);
    keys(&mut c, "jdd"); // the first cell's output row, then delete it
    assert_eq!(output_text(&c, 0), None);
    assert_eq!(
        c.document.order().len(),
        2,
        "dd on an output keeps the cell"
    );
    keys(&mut c, "u");
    assert_eq!(output_text(&c, 0).as_deref(), Some("a"));

    keys(&mut c, "]]c"); // clear the second cell's outputs
    assert_eq!(output_text(&c, 1), None);
    keys(&mut c, "u");
    assert_eq!(output_text(&c, 1).as_deref(), Some("b"));
}

#[test]
fn undoing_a_delete_selects_the_restored_cell() {
    let mut c = notebook(&[("a = 1", ""), ("b = 2", ""), ("c = 3", "")]);
    keys(&mut c, "]]dd");
    let restored = |c: &Coordinator| {
        c.document
            .cell(&c.selected_cell)
            .unwrap()
            .buffer
            .as_str()
            .to_string()
    };
    assert_eq!(restored(&c), "c = 3");
    keys(&mut c, "u");
    assert_eq!(restored(&c), "b = 2");
}

/// With unsaved edits, a change on disk asks first; plain `l` (enter a cell)
/// must not reload over the edits, capital `L` does.
#[test]
fn the_reload_bar_ignores_plain_l() {
    let path = std::env::temp_dir().join(format!("nb-reload-bar-{}.py", std::process::id()));
    std::fs::write(&path, "# %%\na = 1\n").unwrap();
    let mut c = notebook(&[("a = 1", "")]);
    c.path = Some(path.clone());
    keys(&mut c, "A");
    keys(&mut c, " # mine");
    c.on_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        Instant::now(),
    );
    c.on_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        Instant::now(),
    );
    c.on_focus_gained(); // records the file's current time

    std::fs::write(&path, "# %%\ndisk = 1\n").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    c.on_focus_gained();
    let source = |c: &Coordinator| {
        let id = &c.document.order()[0];
        c.document.cell(id).unwrap().buffer.as_str().to_string()
    };
    assert!(
        matches!(c.focus.top(), nb_tui::FocusLayer::Modal(_)),
        "asks first"
    );
    keys(&mut c, "l");
    assert_eq!(source(&c), "a = 1 # mine", "plain l keeps the edits");
    assert!(
        matches!(c.focus.top(), nb_tui::FocusLayer::Modal(_)),
        "still asking"
    );
    keys(&mut c, "L");
    assert_eq!(source(&c), "disk = 1");
    let _ = std::fs::remove_file(&path);
}
