//! An empty completion reply to a request a newer one replaced opens no
//! popup; the newer reply decides.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nb_core::{Cell, Document, FormatBacking};
use nb_kernel::{CompletionCandidate, FakeKernel, KernelCmd, KernelEvent};
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};

fn press(c: &mut Coordinator, code: KeyCode, modifiers: KeyModifiers) {
    c.on_key(KeyEvent::new(code, modifiers), Instant::now());
}

#[test]
fn superseded_empty_reply_opens_no_fallback_popup() {
    let mut cell = Cell::new_code();
    cell.buffer.replace_all("value = 1\nval".to_string());
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, mut handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::from_cells(vec![cell], FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    );
    // Into the cell, insert mode at the end, then ask twice.
    press(&mut c, KeyCode::Char('A'), KeyModifiers::NONE);
    press(&mut c, KeyCode::Char(' '), KeyModifiers::CONTROL);
    press(&mut c, KeyCode::Char(' '), KeyModifiers::CONTROL);
    let ids: Vec<u64> = handle
        .drain_commands()
        .into_iter()
        .filter_map(|command| match command {
            KernelCmd::Complete { req_id, .. } => Some(req_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2, "two completion requests");

    c.apply_kernel_event(KernelEvent::CompleteReply {
        req_id: ids[0],
        start_char: 0,
        items: Vec::new(),
    });
    assert!(c.completion.is_none(), "superseded empty reply is ignored");

    c.apply_kernel_event(KernelEvent::CompleteReply {
        req_id: ids[1],
        start_char: 10,
        items: vec![CompletionCandidate {
            replace_text: "value".into(),
            kind: "instance".into(),
            detail: String::new(),
        }],
    });
    assert!(c.completion.is_some(), "the newer reply opens the popup");
}
