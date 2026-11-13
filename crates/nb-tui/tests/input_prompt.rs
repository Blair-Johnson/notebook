//! Esc on an input() / pdb prompt only steps away from it: the kernel is
//! still waiting, so Enter brings the prompt back and answers it.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, Document, FormatBacking};
use nb_kernel::{FakeKernel, KernelCmd, KernelEvent};
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator, FocusLayer};

fn key(c: &mut Coordinator, code: KeyCode) {
    c.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
}

#[test]
fn esc_keeps_the_pending_prompt_and_enter_returns_to_it() {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, mut handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::from_cells(
            vec![Cell::new_code()],
            FormatBacking::PercentPy { header: None },
        ),
        registry,
        Box::new(kernel),
    );
    c.apply_kernel_event(KernelEvent::InputRequested {
        prompt: "(Pdb) ".into(),
        password: false,
    });
    assert!(matches!(c.focus.top(), FocusLayer::PdbInput));

    key(&mut c, KeyCode::Esc);
    assert!(!matches!(c.focus.top(), FocusLayer::PdbInput));
    assert!(
        c.notice
            .as_deref()
            .is_some_and(|n| n.contains("waiting for input"))
    );

    // Enter on the notebook goes back to the prompt instead of queueing a run.
    key(&mut c, KeyCode::Enter);
    assert!(matches!(c.focus.top(), FocusLayer::PdbInput));
    assert!(
        !handle
            .drain_commands()
            .iter()
            .any(|command| matches!(command, KernelCmd::Execute { .. }))
    );

    c.on_paste("c".into());
    key(&mut c, KeyCode::Enter);
    assert!(
        handle
            .drain_commands()
            .into_iter()
            .any(|command| matches!(command, KernelCmd::ReplyInput { value } if value == "c"))
    );
}
