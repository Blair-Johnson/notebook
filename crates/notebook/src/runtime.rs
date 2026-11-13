//! Runtime event-batch helpers.

use nb_kernel::KernelEvent;

/// Append a kernel event while retaining only the newest update for each
/// display id. Non-display events retain their relative ordering, including
/// `ClearOutput` barriers around later updates.
pub fn queue_kernel_event(queued: &mut Vec<KernelEvent>, event: KernelEvent) {
    if let KernelEvent::UpdateDisplay { display_id, .. } = &event {
        queued.retain(|queued_event| {
            !matches!(
                queued_event,
                KernelEvent::UpdateDisplay {
                    display_id: queued_display_id,
                    ..
                } if queued_display_id == display_id
            )
        });
    }
    queued.push(event);
}

#[cfg(test)]
mod tests {
    use super::queue_kernel_event;
    use nb_core::TargetId;
    use nb_kernel::KernelEvent;

    fn update(display_id: &str) -> KernelEvent {
        KernelEvent::UpdateDisplay {
            display_id: display_id.to_string(),
            bundle: Default::default(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn latest_display_update_collapses_without_crossing_clear_output() {
        let mut queued = Vec::new();
        queue_kernel_event(&mut queued, update("a"));
        queue_kernel_event(
            &mut queued,
            KernelEvent::ClearOutput {
                target: TargetId::SessionLog,
                wait: false,
            },
        );
        queue_kernel_event(&mut queued, update("b"));
        queue_kernel_event(&mut queued, update("a"));

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
}
