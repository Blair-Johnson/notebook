//! Runtime helper coverage for editor handoff and kernel batching.

#[path = "../src/editor.rs"]
#[allow(dead_code)]
mod editor;
#[path = "../src/runtime.rs"]
mod runtime;

use std::time::{SystemTime, UNIX_EPOCH};
use nb_core::TargetId;
use nb_kernel::KernelEvent;

fn unique_path(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    ))
}

fn update(display_id: &str) -> KernelEvent {
    KernelEvent::UpdateDisplay {
        display_id: display_id.to_string(),
        bundle: Default::default(),
        metadata: Default::default(),
    }
}

#[test]
fn editor_command_splits_visual_program_and_appends_file() {
    let path = unique_path("nb-editor-args");
    std::fs::write(&path, "original").expect("write test file");
    let status = editor::editor_command("cp /dev/null", &path)
        .status()
        .expect("run test editor");
    let contents = std::fs::read_to_string(&path).expect("read test file");
    let _ = std::fs::remove_file(&path);

    assert!(status.success());
    assert!(contents.is_empty());
}

#[test]
fn queue_kernel_event_collapses_duplicate_display_updates_in_order() {
    let mut queued = Vec::new();
    runtime::queue_kernel_event(&mut queued, update("a"));
    runtime::queue_kernel_event(
        &mut queued,
        KernelEvent::ClearOutput {
            target: TargetId::SessionLog,
            wait: false,
        },
    );
    runtime::queue_kernel_event(&mut queued, update("b"));
    runtime::queue_kernel_event(&mut queued, update("a"));

    let shape: Vec<&'static str> = queued
        .iter()
        .map(|event| match event {
            KernelEvent::UpdateDisplay { display_id, .. } if display_id == "a" => "a",
            KernelEvent::UpdateDisplay { display_id, .. } if display_id == "b" => "b",
            KernelEvent::ClearOutput { .. } => "clear",
            _ => "other",
        })
        .collect();
    assert_eq!(shape, ["clear", "b", "a"]);
}
