//! Smoke test verifying cross-crate wiring (`nb-core` + `nb-kernel` + `nb-sdk` + `nb-tui`).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use nb_core::{Document, FormatBacking};
use nb_kernel::{FakeKernel, KernelCmd};
use nb_sdk::ExtensionRegistry;
use nb_tui::layout::FenwickTree;
use nb_tui::{BuiltinsPlugin, Coordinator};

#[test]
fn headless_coordinator_wires_plugins_fake_kernel_and_test_backend() {
    let mut reg = ExtensionRegistry::default();
    reg.install(BuiltinsPlugin);
    assert!(reg.output_renderers.len() >= 3);

    let (kernel, mut handle) = FakeKernel::pair();
    let doc = Document::new_empty(FormatBacking::PercentPy { header: None });
    let mut coord = Coordinator::headless(doc, reg, Box::new(kernel));

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal.draw(|frame| coord.draw(frame)).expect("draw frame");
    assert_eq!(coord.last_geometry.viewport_rect.width, 120);
    assert_eq!(coord.last_geometry.viewport_rect.height, 38);

    coord.kernel.send(KernelCmd::Interrupt).expect("send interrupt");
    assert_eq!(handle.drain_commands().len(), 1);
}

#[test]
fn fenwick_tree_resolves_scroll_rows_in_log_time() {
    let mut tree = FenwickTree::with_len(4);
    tree.set(0, 5);
    tree.set(1, 0);
    tree.set(2, 8);
    tree.set(3, 3);
    assert_eq!(tree.total_height(), 16);
    assert_eq!(tree.find_row(0), Some((0, 0)));
    assert_eq!(tree.find_row(4), Some((0, 4)));
    assert_eq!(tree.find_row(5), Some((2, 0)));
    assert_eq!(tree.find_row(13), Some((3, 0)));
}
