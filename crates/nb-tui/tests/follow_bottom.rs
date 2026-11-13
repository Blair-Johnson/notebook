//! At the bottom of the notebook, growing output keeps the view at the bottom
//! until the user scrolls away.

use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use nb_core::{Cell, Document, FormatBacking, StreamKind, TargetId};
use nb_kernel::{FakeKernel, KernelEvent, KernelSessionInfo, WireOutput};
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};

fn notebook() -> Coordinator {
    let cells = (0..4)
        .map(|n| {
            let mut cell = Cell::new_code();
            cell.buffer
                .replace_all(format!("a{n} = 1\nb = 2\nc = 3\nd = 4\nprint(a{n})"));
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
    c.apply_kernel_event(KernelEvent::Ready {
        info: KernelSessionInfo {
            generation: 1,
            kernel_name: "python".into(),
            display_name: "Python".into(),
            spec_dir: PathBuf::new(),
            connection_file: PathBuf::new(),
            pid: None,
            supports_subshells: false,
            supports_debugger: false,
        },
    });
    draw(&mut c);
    c
}

fn draw(c: &mut Coordinator) {
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal.draw(|frame| c.draw(frame)).expect("draw");
}

fn key(c: &mut Coordinator, code: KeyCode) {
    c.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    draw(c);
}

fn print_line(c: &mut Coordinator, n: usize) {
    let last = c.document.order().last().unwrap().clone();
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(last),
        late: false,
        payload: WireOutput::Stream {
            stream: StreamKind::Stdout,
            text: format!("line {n}\n"),
        },
    });
    draw(c);
}

fn max_scroll(c: &Coordinator) -> u32 {
    let total = c.layout_cache.heights().total_height();
    total.saturating_sub(u32::from(c.last_geometry.viewport_rect.height))
}

fn wheel(c: &mut Coordinator, kind: MouseEventKind) {
    c.on_mouse(MouseEvent {
        kind,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });
    draw(c);
}

#[test]
fn running_the_last_cell_at_the_bottom_follows_its_output() {
    let mut c = notebook();
    key(&mut c, KeyCode::Char('G'));
    assert_eq!(c.scroll_row, max_scroll(&c), "at the bottom");
    key(&mut c, KeyCode::Enter);
    let start = max_scroll(&c);
    for n in 0..10 {
        print_line(&mut c, n);
        assert_eq!(c.scroll_row, max_scroll(&c), "still at the bottom after line {n}");
    }
    assert!(max_scroll(&c) > start, "the output grew the notebook");
}

#[test]
fn scrolling_away_stops_following_and_returning_resumes() {
    let mut c = notebook();
    key(&mut c, KeyCode::Char('G'));
    key(&mut c, KeyCode::Enter);
    print_line(&mut c, 0);
    wheel(&mut c, MouseEventKind::ScrollUp);
    let parked = c.scroll_row;
    assert!(parked < max_scroll(&c));
    for n in 1..6 {
        print_line(&mut c, n);
    }
    assert_eq!(c.scroll_row, parked, "the view stays where the user put it");

    for _ in 0..20 {
        wheel(&mut c, MouseEventKind::ScrollDown);
    }
    assert_eq!(c.scroll_row, max_scroll(&c));
    for n in 6..10 {
        print_line(&mut c, n);
        assert_eq!(c.scroll_row, max_scroll(&c));
    }
}

#[test]
fn keys_that_do_not_move_the_selection_keep_following() {
    let mut c = notebook();
    key(&mut c, KeyCode::Char('G'));
    key(&mut c, KeyCode::Enter);
    for n in 0..10 {
        print_line(&mut c, n);
    }
    // Interrupt is a key, but it doesn't move anything: no jump to the cell top.
    c.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), Instant::now());
    draw(&mut c);
    print_line(&mut c, 10);
    assert_eq!(c.scroll_row, max_scroll(&c));
}

#[test]
fn not_at_the_bottom_means_not_following() {
    let mut c = notebook();
    key(&mut c, KeyCode::Char('G'));
    key(&mut c, KeyCode::Char('g'));
    key(&mut c, KeyCode::Char('g'));
    assert_eq!(c.scroll_row, 0);
    // Run the last cell from the palette-free path: select it without scrolling.
    let last = c.document.order().last().unwrap().clone();
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(last),
        late: false,
        payload: WireOutput::Stream {
            stream: StreamKind::Stdout,
            text: "line\n".into(),
        },
    });
    draw(&mut c);
    assert_eq!(c.scroll_row, 0);
}
