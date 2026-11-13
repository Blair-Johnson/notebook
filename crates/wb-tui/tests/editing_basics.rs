//! Basic editing keys, Enter/click focus rules, and the palette shortcut.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use wb_core::{Cell, Document, FormatBacking};
use wb_kernel::FakeKernel;
use wb_sdk::ExtensionRegistry;
use wb_tui::focus::ModalKind;
use wb_tui::render::HitTarget;
use wb_tui::{BuiltinsPlugin, CellMode, Coordinator, FocusLayer};

fn coordinator(sources: &[&str]) -> Coordinator {
    let cells = sources
        .iter()
        .map(|source| {
            let mut cell = Cell::new_code();
            cell.buffer.replace_all((*source).to_string());
            cell
        })
        .collect();
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, _handle) = FakeKernel::pair();
    Coordinator::headless(
        Document::from_cells(cells, FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    )
}

fn press(coordinator: &mut Coordinator, code: KeyCode, modifiers: KeyModifiers) {
    coordinator.on_key(KeyEvent::new(code, modifiers), Instant::now());
}

fn keys(coordinator: &mut Coordinator, text: &str) {
    for ch in text.chars() {
        press(coordinator, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

fn draw(coordinator: &mut Coordinator) {
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("terminal");
    terminal.draw(|frame| coordinator.draw(frame)).expect("draw");
}

fn mode(coordinator: &Coordinator) -> Option<CellMode> {
    match coordinator.focus.top() {
        FocusLayer::CellEdit { mode, .. } => Some(*mode),
        _ => None,
    }
}

fn source(coordinator: &Coordinator, index: usize) -> String {
    let id = &coordinator.document.order()[index];
    coordinator.document.cell(id).unwrap().buffer.as_str().to_string()
}

#[test]
fn i_opens_the_cell_in_normal_mode_and_enter_runs_it() {
    let mut c = coordinator(&["abc"]);
    press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(c.focus.top(), FocusLayer::NotebookNormal));
    let id = c.document.order()[0].clone();
    assert!(c.run_states.contains_key(&id), "enter queued the cell to run");

    let mut c = coordinator(&["abc"]);
    keys(&mut c, "i");
    assert_eq!(mode(&c), Some(CellMode::Normal));
    keys(&mut c, "i");
    assert_eq!(mode(&c), Some(CellMode::Insert));
}

#[test]
fn insert_mode_backspace_delete_and_arrows_edit_text() {
    let mut c = coordinator(&["hello"]);
    keys(&mut c, "i");
    keys(&mut c, "A");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(source(&c, 0), "hell");
    press(&mut c, KeyCode::Left, KeyModifiers::NONE);
    press(&mut c, KeyCode::Left, KeyModifiers::NONE);
    press(&mut c, KeyCode::Delete, KeyModifiers::NONE);
    assert_eq!(source(&c, 0), "hel");
    press(&mut c, KeyCode::Home, KeyModifiers::NONE);
    keys(&mut c, "X");
    assert_eq!(source(&c, 0), "Xhel");
    press(&mut c, KeyCode::End, KeyModifiers::NONE);
    keys(&mut c, " wo");
    press(&mut c, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(source(&c, 0), "Xhel ");
}

#[test]
fn ctrl_slash_opens_the_palette_in_every_encoding() {
    for (code, modifiers) in [
        (KeyCode::Char('/'), KeyModifiers::CONTROL),
        (KeyCode::Char('7'), KeyModifiers::CONTROL),
        (KeyCode::Char('\u{1f}'), KeyModifiers::NONE),
    ] {
        let mut c = coordinator(&["x"]);
        press(&mut c, code, modifiers);
        assert!(
            matches!(c.focus.top(), FocusLayer::Modal(ModalKind::CommandPalette)),
            "{code:?} {modifiers:?}"
        );
    }
}

fn click_source(c: &mut Coordinator, cell: usize, line: usize, dx: u16) {
    draw(c);
    let id = c.document.order()[cell].clone();
    let y = c
        .last_geometry
        .row_hits
        .iter()
        .find(|hit| {
            matches!(&hit.target, HitTarget::CellSource { cell_id, line: l, .. }
                if cell_id == &id && *l == line)
        })
        .expect("source row on screen")
        .screen_y;
    let column = c.last_geometry.text_x + dx;
    c.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn clicking_another_cell_moves_focus_and_cursor_there() {
    let mut c = coordinator(&["first cell", "second\nthird line"]);
    keys(&mut c, "i");
    keys(&mut c, "i");
    assert_eq!(mode(&c), Some(CellMode::Insert));

    click_source(&mut c, 1, 1, 3);
    let second = c.document.order()[1].clone();
    assert!(matches!(
        c.focus.top(),
        FocusLayer::CellEdit { cell_id, mode: CellMode::Normal, .. } if cell_id == &second
    ));
    assert_eq!(c.selected_cell, second);
    let cursor = c.document.cell(&second).unwrap().buffer.cursor();
    assert_eq!((cursor.row, cursor.col), (1, 3));
    // Only one edit layer: leaving goes straight back to the notebook.
    press(&mut c, KeyCode::Esc, KeyModifiers::NONE);
    assert!(matches!(c.focus.top(), FocusLayer::NotebookNormal));
}

#[test]
fn clicking_from_the_notebook_enters_the_clicked_cell() {
    let mut c = coordinator(&["alpha", "beta"]);
    click_source(&mut c, 0, 0, 2);
    assert_eq!(mode(&c), Some(CellMode::Normal));
    let first = c.document.order()[0].clone();
    assert_eq!(c.document.cell(&first).unwrap().buffer.cursor().col, 2);
}

#[test]
fn global_commands_work_while_editing_a_cell() {
    let mut c = coordinator(&["x = 1"]);
    keys(&mut c, "i");
    press(&mut c, KeyCode::Char('/'), KeyModifiers::CONTROL);
    assert!(matches!(c.focus.top(), FocusLayer::Modal(ModalKind::CommandPalette)));
    press(&mut c, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(mode(&c), Some(CellMode::Normal));
    keys(&mut c, ":");
    assert!(matches!(c.focus.top(), FocusLayer::Modal(ModalKind::ExCommandBar)));
}

/// Open cell 0 in normal mode with the cursor at `(row, col)`.
fn editing(sources: &[&str], row: usize, col: usize) -> Coordinator {
    let mut c = coordinator(sources);
    keys(&mut c, "i");
    let id = c.document.order()[0].clone();
    c.document
        .cell_mut(&id)
        .unwrap()
        .buffer
        .set_cursor(wb_core::CursorPos { row, col });
    c
}

#[test]
fn vim_operators_through_the_real_keymap() {
    let mut c = editing(&["x = compute(a, b)"], 0, 4);
    keys(&mut c, "d$");
    assert_eq!(source(&c, 0), "x = ");

    let mut c = editing(&["f(a, b, c)"], 0, 0);
    keys(&mut c, "df,");
    assert_eq!(source(&c, 0), " b, c)");
    keys(&mut c, "u");
    assert_eq!(source(&c, 0), "f(a, b, c)");

    let mut c = editing(&["print(\"hi\", x)"], 0, 7);
    keys(&mut c, "ci(");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    keys(&mut c, "y");
    assert_eq!(source(&c, 0), "print(y)");

    let mut c = editing(&["one\ntwo\nthree"], 0, 0);
    keys(&mut c, "ddp");
    assert_eq!(source(&c, 0), "two\none\nthree");

    let mut c = editing(&["one\ntwo\nthree"], 0, 1);
    keys(&mut c, "Vjd");
    assert_eq!(source(&c, 0), "three");
    assert_eq!(mode(&c), Some(CellMode::Normal));

    let mut c = editing(&["abc def ghi"], 0, 4);
    keys(&mut c, "vex");
    assert_eq!(source(&c, 0), "abc  ghi");

    let mut c = editing(&["first\nsecond"], 0, 0);
    keys(&mut c, "A!");
    press(&mut c, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(source(&c, 0), "first!\nsecond");
    keys(&mut c, "J");
    assert_eq!(source(&c, 0), "first! second");
}

#[test]
fn one_undo_reverts_a_whole_insert_session() {
    let mut c = editing(&["base"], 0, 3);
    keys(&mut c, "a more text");
    press(&mut c, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(source(&c, 0), "base more text");
    keys(&mut c, "u");
    assert_eq!(source(&c, 0), "base");
}
