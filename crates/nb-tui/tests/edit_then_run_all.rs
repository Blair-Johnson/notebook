//! Editing a cell and then running everything (`X`) keeps the edit.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, Document, ExecId, FormatBacking, TargetId};
use nb_kernel::{FakeKernel, KernelCmd, KernelEvent, KernelSessionInfo};
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};
use ratatui::{Terminal, backend::TestBackend};

fn draw(c: &mut Coordinator) {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
}

fn press(c: &mut Coordinator, code: KeyCode) {
    c.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    draw(c);
}

fn typed(c: &mut Coordinator, text: &str) {
    for ch in text.chars() {
        press(c, KeyCode::Char(ch));
    }
}

fn sources(c: &Coordinator) -> Vec<String> {
    c.document
        .order()
        .iter()
        .map(|id| c.document.cell(id).unwrap().buffer.as_str().to_string())
        .collect()
}

fn ready() -> KernelEvent {
    KernelEvent::Ready {
        info: KernelSessionInfo {
            generation: 1,
            kernel_name: "python3".into(),
            display_name: "Python 3".into(),
            spec_dir: PathBuf::new(),
            connection_file: PathBuf::new(),
            pid: None,
            supports_subshells: false,
            supports_debugger: false,
        },
    }
}

/// `edit` leaves the cell however the user would; then `X`, kernel events
/// for every run, and a few seconds of ticks.
fn edit_then_run_all(file: Option<&str>, edit: &dyn Fn(&mut Coordinator)) -> Vec<String> {
    let cells = ["a = 1", "b = 2", "c = 3"]
        .iter()
        .map(|source| {
            let mut cell = Cell::new_code();
            cell.buffer.replace_all(source.to_string());
            cell
        })
        .collect();
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, mut handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::from_cells(cells, FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    );
    let path = file.map(|name| {
        let path = std::env::temp_dir().join(format!("{name}-{}.py", std::process::id()));
        fs::write(&path, "# %%\na = 1\n\n# %%\nb = 2\n\n# %%\nc = 3\n").unwrap();
        c.path = Some(path.clone());
        path
    });
    c.apply_kernel_event(ready());
    draw(&mut c);
    edit(&mut c);
    press(&mut c, KeyCode::Char('X'));
    for (n, command) in handle.drain_commands().into_iter().enumerate() {
        if let KernelCmd::Execute {
            target: target @ TargetId::Cell(_),
            ..
        } = command
        {
            let exec_id = ExecId(format!("exec-{n}"));
            c.apply_kernel_event(KernelEvent::ExecutionStarted {
                exec_id: exec_id.clone(),
                target: target.clone(),
                execution_count: Some(1),
            });
            c.apply_kernel_event(KernelEvent::ExecutionFinished {
                exec_id,
                target,
                ok: true,
                elapsed: Duration::from_millis(5),
            });
            draw(&mut c);
        }
    }
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        c.on_tick(Instant::now());
        c.on_autosave_tick();
        draw(&mut c);
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(path) = path {
        let _ = fs::remove_file(path);
    }
    sources(&c)
}

fn esc_esc(c: &mut Coordinator) {
    press(c, KeyCode::Esc);
    press(c, KeyCode::Esc);
}

#[test]
fn insert_then_escape_then_run_all() {
    for file in [None, Some("nb-edit-run-all-a")] {
        let got = edit_then_run_all(file, &|c| {
            press(c, KeyCode::Char('A'));
            typed(c, " + 10");
            esc_esc(c);
        });
        assert_eq!(got, ["a = 1 + 10", "b = 2", "c = 3"], "file: {file:?}");
    }
}

#[test]
fn normal_mode_edit_then_run_all() {
    for file in [None, Some("nb-edit-run-all-b")] {
        let got = edit_then_run_all(file, &|c| {
            press(c, KeyCode::Char('i'));
            typed(c, "xx"); // delete two characters in cell normal mode
            esc_esc(c);
        });
        assert_eq!(got, ["= 1", "b = 2", "c = 3"], "file: {file:?}");
    }
}

#[test]
fn edit_second_cell_then_run_all() {
    for file in [None, Some("nb-edit-run-all-c")] {
        let got = edit_then_run_all(file, &|c| {
            press(c, KeyCode::Char(']'));
            press(c, KeyCode::Char(']'));
            press(c, KeyCode::Char('A'));
            typed(c, " * 3");
            esc_esc(c);
        });
        assert_eq!(got, ["a = 1", "b = 2 * 3", "c = 3"], "file: {file:?}");
    }
}
