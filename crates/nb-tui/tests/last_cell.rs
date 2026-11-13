//! Deleting the only cell, undoing and redoing never leaves an empty notebook.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, Document, FormatBacking};
use nb_kernel::FakeKernel;
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};
use ratatui::{Terminal, backend::TestBackend};

fn one_cell() -> Coordinator {
    let mut cell = Cell::new_code();
    cell.buffer.replace_all("x = 1".to_string());
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, _handle) = FakeKernel::pair();
    Coordinator::headless(
        Document::from_cells(vec![cell], FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    )
}

fn keys(c: &mut Coordinator, text: &str) {
    for ch in text.chars() {
        c.on_key(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            Instant::now(),
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("terminal");
        terminal.draw(|frame| c.draw(frame)).expect("draw");
    }
}

fn sources(c: &Coordinator) -> Vec<String> {
    c.document
        .order()
        .iter()
        .map(|id| c.document.cell(id).unwrap().buffer.as_str().to_string())
        .collect()
}

#[test]
fn delete_undo_delete_on_the_only_cell() {
    let mut c = one_cell();
    keys(&mut c, "dd");
    assert_eq!(sources(&c), [""]);
    keys(&mut c, "u");
    assert_eq!(
        sources(&c),
        ["x = 1"],
        "undo brings back exactly the deleted cell"
    );
    keys(&mut c, "dd");
    keys(&mut c, "yy");
    keys(&mut c, "u");
    keys(&mut c, "u");
    keys(&mut c, "\x12"); // redo is ctrl-r; a stray char must not crash either
    assert!(!c.document.order().is_empty());
}

#[test]
fn typing_into_the_placeholder_undoes_in_order() {
    let mut c = one_cell();
    keys(&mut c, "dd");
    keys(&mut c, "ihi");
    // Out of insert mode, then out of the cell to the notebook level.
    for _ in 0..2 {
        c.on_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Instant::now(),
        );
    }
    assert_eq!(sources(&c), ["hi"]);
    keys(&mut c, "u");
    assert_eq!(sources(&c), [""], "the typing is undone first");
    keys(&mut c, "u");
    assert_eq!(
        sources(&c),
        ["x = 1"],
        "then the delete, dropping the blank placeholder"
    );
}
