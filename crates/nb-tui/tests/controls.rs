//! Configurable controls end to end: the notebook-level line motions that
//! start typing, one undo step per key, and user-defined rules, macros, and
//! modes loaded from a real config file.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, CellKind, CursorPos, Document, FormatBacking, OutputData, OutputItem, StreamBuffer, StreamKind};
use nb_kernel::FakeKernel;
use nb_sdk::{ExtensionRegistry, Key};
use nb_tui::config_store::SettingsStore;
use nb_tui::platform::{MemoryClipboard, NoopTerminalEffects};
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

fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    registry
}

fn cells(cells: &[(&str, &str)]) -> Document {
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
    Document::from_cells(cells, FormatBacking::PercentPy { header: None })
}

fn coordinator(cells_: &[(&str, &str)]) -> Coordinator {
    let (kernel, _handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(cells(cells_), registry(), Box::new(kernel));
    draw(&mut c);
    c
}

/// A coordinator whose config file holds `user_toml`.
fn configured(cells_: &[(&str, &str)], user_toml: &str) -> Coordinator {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("nb-controls-{}-{stamp}.toml", std::process::id()));
    std::fs::write(&path, user_toml).unwrap();
    let registry = registry();
    let settings = SettingsStore::load_or_default(Some(path.clone()), &registry);
    let _ = std::fs::remove_file(&path);
    assert!(
        settings.snapshot().warnings.is_empty(),
        "{:?}",
        settings.snapshot().warnings
    );
    let (kernel, _handle) = FakeKernel::pair();
    let mut c = Coordinator::new(
        None,
        cells(cells_),
        registry,
        settings,
        Box::new(kernel),
        Box::new(MemoryClipboard::default()),
        Box::new(NoopTerminalEffects),
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

fn cursor(c: &Coordinator, index: usize) -> (usize, usize) {
    let id = &c.document.order()[index];
    let cursor = c.document.cell(id).unwrap().buffer.cursor();
    (cursor.row, cursor.col)
}

fn source(c: &Coordinator, index: usize) -> String {
    let id = &c.document.order()[index];
    c.document.cell(id).unwrap().buffer.as_str().to_string()
}

fn mode(c: &Coordinator) -> Option<CellMode> {
    match c.focus.top() {
        FocusLayer::CellEdit { mode, .. } => Some(*mode),
        _ => None,
    }
}

fn mode_id(c: &Coordinator) -> String {
    c.focus.top().mode_id().unwrap_or("").to_string()
}

#[test]
fn dollar_from_the_notebook_types_after_the_selected_line() {
    let mut c = coordinator(&[("first\n    second\nthird", "")]);
    keys(&mut c, "j$");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    assert_eq!(cursor(&c, 0), (1, 10), "after the last character of line 2");
    keys(&mut c, "!");
    assert_eq!(source(&c, 0), "first\n    second!\nthird");
    press(&mut c, KeyCode::Esc);
    assert_eq!(mode(&c), Some(CellMode::Normal));
    assert_eq!(cursor(&c, 0), (1, 10), "Esc steps back onto the '!'");
    press(&mut c, KeyCode::Esc);
    assert!(matches!(c.focus.top(), FocusLayer::Notebook { .. }));
}

#[test]
fn line_start_motions_type_at_their_point() {
    for (key, col) in [("_", 4), ("^", 4), ("0", 0)] {
        let mut c = coordinator(&[("first\n    second\nthird", "")]);
        keys(&mut c, "j");
        keys(&mut c, key);
        assert_eq!(mode(&c), Some(CellMode::Insert), "{key}");
        assert_eq!(cursor(&c, 0), (1, col), "{key}");
    }
}

#[test]
fn counts_carry_through_the_rule() {
    let mut c = coordinator(&[("one\n  two\nthree", "")]);
    keys(&mut c, "2_");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    assert_eq!(cursor(&c, 0), (1, 2), "2_ is one line down, as in vim");
}

#[test]
fn on_an_output_row_line_motions_read_the_output() {
    let mut c = coordinator(&[("run()", "row 0\nrow 1\nrow 2\n")]);
    keys(&mut c, "jjj$");
    assert!(matches!(
        c.focus.top(),
        FocusLayer::OutputFocus {
            mode: CellMode::Normal,
            ..
        }
    ));
    let out = c.output_text.as_ref().expect("reading the output");
    assert_eq!(out.buffer.cursor(), CursorPos { row: 2, col: 4 });
}

#[test]
fn empty_and_markdown_cells_open_for_typing_too() {
    let mut c = coordinator(&[("", "")]);
    keys(&mut c, "$");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    keys(&mut c, "x");
    assert_eq!(source(&c, 0), "x");

    let mut markdown = Cell::new_code();
    markdown.kind = CellKind::Markdown;
    markdown.buffer.replace_all("# Title".to_string());
    let (kernel, _handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::from_cells(vec![markdown], FormatBacking::PercentPy { header: None }),
        registry(),
        Box::new(kernel),
    );
    draw(&mut c);
    keys(&mut c, "$");
    assert_eq!(mode(&c), Some(CellMode::Insert));
    assert_eq!(cursor(&c, 0), (0, 7));
}

#[test]
fn each_key_undoes_as_one_step() {
    let mut c = coordinator(&[("    a = 1\nb = 2", "")]);
    keys(&mut c, "$");
    keys(&mut c, " + 1");
    press(&mut c, KeyCode::Esc);
    keys(&mut c, "u");
    assert_eq!(source(&c, 0), "    a = 1\nb = 2", "one u undoes the whole insert");

    // `o` adds a line and types on it: also one step.
    keys(&mut c, "o");
    keys(&mut c, "c = 3");
    press(&mut c, KeyCode::Esc);
    assert_eq!(source(&c, 0), "    a = 1\n    c = 3\nb = 2");
    keys(&mut c, "u");
    assert_eq!(source(&c, 0), "    a = 1\nb = 2");
}

#[test]
fn home_and_end_in_a_cell_move_within_the_line() {
    let mut c = coordinator(&[("one\ntwo three\nfour", "")]);
    keys(&mut c, "ij");
    press(&mut c, KeyCode::End);
    assert_eq!(cursor(&c, 0), (1, 8));
    press(&mut c, KeyCode::Home);
    assert_eq!(cursor(&c, 0), (1, 0));
}

#[test]
fn explain_marks_the_rule_that_matches_here() {
    let mut c = coordinator(&[("x = 1", "")]);
    let text = c.explain_keys("notebook.normal", &[Key::Char('$')]).join("\n");
    assert!(text.contains("▶ rule notebook.line_end_motion_appends"), "{text}");
    assert!(text.contains("rule notebook.line_motion_reads_output when output row"), "{text}");
    keys(&mut c, ":explain gg");
    press(&mut c, KeyCode::Enter);
    let pager = c.inspect_pager.as_ref().expect("explain opens a pager");
    assert!(pager.ansi_text.contains("motion.top"), "{}", pager.ansi_text);
}

#[test]
fn a_user_rule_replaces_a_default_and_disabling_one_falls_back() {
    let mut c = configured(
        &[("x = 1", "")],
        r#"
[[rules]]
id = "i_types"
mode = "notebook.normal"
on = ["cell.enter_normal"]
when = { part = "source" }
do = ["mode.cell.insert"]

[[rules]]
id = "cell.insert_done"
enabled = false
"#,
    );
    keys(&mut c, "i");
    assert_eq!(mode(&c), Some(CellMode::Insert), "the user's rule is checked first");
    press(&mut c, KeyCode::Esc);
    assert_eq!(mode(&c), Some(CellMode::Insert), "Esc's default rule is off");
}

#[test]
fn macros_and_new_modes_come_from_the_config() {
    let mut c = configured(
        &[("a\nb", "")],
        r#"
[actions."my.duplicate_line"]
title = "Duplicate line"
do = ["edit.yank_line", "edit.paste_after"]

[keys."cell.normal"]
"my.duplicate_line" = ["g d"]

[modes."cell.reader"]
title = "Cell reader"
level = "cell"
kind = "normal"
badge = "READ"
keysets = ["text.motions", "vim.normal"]

[keys."notebook.normal"]
"mode.cell.reader" = ["g r"]

[[rules]]
id = "reader_back"
mode = "cell.reader"
on = ["focus.up"]
do = ["mode.notebook.normal"]
"#,
    );
    keys(&mut c, "igd");
    assert_eq!(source(&c, 0), "a\na\nb");
    keys(&mut c, "u");
    assert_eq!(source(&c, 0), "a\nb", "a macro undoes as one step");
    press(&mut c, KeyCode::Esc);

    keys(&mut c, "gr");
    assert_eq!(mode_id(&c), "cell.reader");
    let badge = c.focus.top().badge(&c.settings.snapshot().keymap.controls);
    assert_eq!(badge, "READ");
    keys(&mut c, "i");
    assert_eq!(mode_id(&c), "cell.reader", "i is not bound in this mode");
    press(&mut c, KeyCode::Esc);
    assert!(matches!(c.focus.top(), FocusLayer::Notebook { .. }));
}

#[test]
fn a_failed_step_stops_the_rest() {
    let mut c = configured(
        &[("x = 1", ""), ("y = 2", "")],
        r#"
[[rules]]
id = "read_then_down"
mode = "notebook.normal"
on = ["cell.focus_output"]
do = ["mode.output.normal", "motion.down"]
"#,
    );
    keys(&mut c, "zo");
    assert!(matches!(c.focus.top(), FocusLayer::Notebook { .. }));
    assert_eq!(c.selected_cell, c.document.order()[0], "motion.down did not run");
}

fn screen_text(c: &mut Coordinator, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

/// `:settings`, then Tab to Controls.
fn open_controls(c: &mut Coordinator) {
    keys(c, ":settings");
    press(c, KeyCode::Enter);
    assert!(matches!(c.focus.top(), FocusLayer::Modal(nb_tui::ModalKind::Settings)));
    for _ in 0..4 {
        press(c, KeyCode::Tab);
    }
}

#[test]
fn the_last_controls_row_stays_above_the_rule() {
    let mut c = coordinator(&[("x = 1", "")]);
    open_controls(&mut c);
    keys(&mut c, "G");
    let rows = c.overlays.settings.control_rows(&c.settings.snapshot().keymap);
    let last = rows.last().expect("rows").title.clone();
    for height in [24, 30, 36, 41] {
        let lines = screen_text(&mut c, 110, height);
        // The detail below the rule repeats the title: look above it. The
        // rule runs from the modal's left border.
        let rule = lines
            .iter()
            .position(|line| line.contains("│────────────────────────────────────────"))
            .expect("the rule above the detail");
        assert!(
            lines[..rule].iter().any(|line| line.contains(&last)),
            "height {height}: the selected row {last:?} is hidden:\n{}",
            lines.join("\n")
        );
    }
}

#[test]
fn primitive_rows_and_unsendable_keys_are_labelled() {
    let mut c = coordinator(&[("x = 1", "")]);
    open_controls(&mut c);
    let text = screen_text(&mut c, 120, 40).join("\n");
    assert!(text.contains("primitive"), "{text}");
    assert!(!text.contains("built-in"), "{text}");
    assert!(text.contains("⚠ ctrl+,"), "ctrl+, is flagged:\n{text}");
}

/// The row on screen showing the selected Controls entry, and the rows of
/// the list area (from the header to the rule above the detail).
fn controls_selection(c: &mut Coordinator, height: u16) -> (usize, usize, usize) {
    let snap = c.settings.snapshot().clone();
    let rows = c.overlays.settings.control_rows(&snap.keymap);
    let title = rows[c.overlays.settings.selected_row].title.clone();
    let lines = screen_text(c, 110, height);
    let header = lines.iter().position(|line| line.contains("What happens in")).expect("header");
    let rule = lines
        .iter()
        .position(|line| line.contains("│────────────────────────────────────────"))
        .expect("rule");
    let at = (header + 1..rule)
        .find(|&y| lines[y].contains(&format!(" {title} ")))
        .unwrap_or_else(|| panic!("{title:?} is not on screen:\n{}", lines.join("\n")));
    (header + 1, at, rule)
}

#[test]
fn menus_keep_rows_of_context_around_the_selection() {
    let mut c = coordinator(&[("x = 1", "")]);
    open_controls(&mut c);
    // 41 rows tall: the list has 24 rows, so 10% keeps 2 below and above.
    for step in 0..30 {
        let (first, at, end) = controls_selection(&mut c, 41);
        assert!(at >= first, "step {step}");
        let below = end - 1 - at;
        let total = c.overlays.settings.control_rows(&c.settings.snapshot().keymap).len();
        if c.overlays.settings.selected_row + 2 < total {
            assert!(below >= 2, "step {step}: only {below} rows below the selection");
        }
        c.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), Instant::now());
    }
    // Moving back up inside the view does not scroll it (drawn at one height).
    controls_selection(&mut c, 41);
    let top = c.overlays.settings.list_top.get();
    for _ in 0..3 {
        c.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), Instant::now());
        controls_selection(&mut c, 41);
        assert_eq!(c.overlays.settings.list_top.get(), top);
    }
}

#[test]
fn the_notebook_keeps_a_margin_around_the_edit_cursor() {
    let source: String = (0..60).map(|n| format!("line_{n} = {n}\n")).collect();
    let mut c = coordinator(&[(source.trim_end(), "")]);
    keys(&mut c, "i");
    let height = c.last_geometry.viewport_rect.height as u32;
    for _ in 0..30 {
        keys(&mut c, "j");
        let cursor = cursor(&c, 0).0 as u32;
        // Row 0 of the cell is its border; source line n is on row n + 1.
        let row = c.layout_cache.heights().prefix_sum(0) + 1 + cursor;
        assert!(
            row + 2 < c.scroll_row + height,
            "line {cursor}: fewer than 2 rows below the cursor (scroll {})",
            c.scroll_row
        );
    }
}

#[test]
fn the_scroll_margin_percentage_is_a_setting() {
    let source: String = (0..60).map(|n| format!("line_{n} = {n}\n")).collect();
    let mut c = configured(&[(source.trim_end(), "")], "[display]\nscrolloff = 0\nscrolloff_pct = 40\n");
    keys(&mut c, "i");
    let height = c.last_geometry.viewport_rect.height as u32;
    let margin = (height * 40 + 50) / 100;
    let margin = margin.min((height - 1) / 2);
    for _ in 0..20 {
        keys(&mut c, "j");
    }
    let row = 1 + cursor(&c, 0).0 as u32;
    assert_eq!(c.scroll_row + height - 1 - row, margin, "the cursor sits {margin} rows above the bottom");
}
