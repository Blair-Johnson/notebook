//! Runtime event-batch helpers.

use nb_kernel::{KernelEvent, WireOutput};

/// Append a kernel event while retaining only the newest update for each
/// display id, and merging a stream chunk into the chunk right before it
/// when both go to the same stream of the same target. Other events retain
/// their relative ordering, including `ClearOutput` barriers around later
/// updates.
pub fn queue_kernel_event(queued: &mut Vec<KernelEvent>, event: KernelEvent) {
    if let KernelEvent::Output {
        exec_id,
        target,
        late,
        payload: WireOutput::Stream { stream, text },
    } = &event
        && let Some(KernelEvent::Output {
            exec_id: last_exec_id,
            target: last_target,
            late: last_late,
            payload:
                WireOutput::Stream {
                    stream: last_stream,
                    text: last_text,
                },
        }) = queued.last_mut()
        && last_exec_id == exec_id
        && last_target == target
        && last_late == late
        && last_stream == stream
    {
        last_text.push_str(text);
        return;
    }
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
    use nb_core::{StreamKind, TargetId};
    use nb_kernel::{KernelEvent, WireOutput};

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

    fn stream(stream: StreamKind, text: &str) -> KernelEvent {
        KernelEvent::Output {
            exec_id: None,
            target: TargetId::SessionLog,
            late: false,
            payload: WireOutput::Stream {
                stream,
                text: text.to_string(),
            },
        }
    }

    #[test]
    fn adjacent_chunks_of_one_stream_merge() {
        let mut queued = Vec::new();
        queue_kernel_event(&mut queued, stream(StreamKind::Stdout, "a\n"));
        queue_kernel_event(&mut queued, stream(StreamKind::Stdout, "b\r"));
        queue_kernel_event(&mut queued, stream(StreamKind::Stderr, "warn\n"));
        queue_kernel_event(&mut queued, stream(StreamKind::Stdout, "c\n"));

        let texts: Vec<&str> = queued
            .iter()
            .map(|event| match event {
                KernelEvent::Output {
                    payload: WireOutput::Stream { text, .. },
                    ..
                } => text.as_str(),
                _ => "other",
            })
            .collect();
        assert_eq!(texts, ["a\nb\r", "warn\n", "c\n"]);
    }
}
