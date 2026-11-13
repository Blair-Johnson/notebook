use std::time::Duration;

use jupyter_protocol::messaging::{JupyterMessage, KernelInfoRequest};
use nb_core::{CellId, ExecId, TargetId};
use nb_kernel::{
    ExecRouter, FakeKernel, KernelClient, KernelCmd, KernelEvent, KernelLaunchSpec, Lane,
    LocalZmqKernel, RingLog, SubshellHeader, WireOutput, recv_event,
};

#[tokio::test]
async fn fake_kernel_round_trip_preserves_commands_and_events() {
    let (mut kernel, mut handle) = FakeKernel::pair();
    let target = TargetId::Cell(CellId("fake-cell".to_owned()));
    kernel
        .send(KernelCmd::Execute {
            target: target.clone(),
            lane: Lane::Main,
            code: "1 + 1".to_owned(),
            silent: false,
            store_history: true,
            stop_on_error: true,
        })
        .expect("fake command channel stays open");

    let commands = handle.drain_commands();
    assert_eq!(commands.len(), 1);
    assert!(matches!(
        &commands[0],
        KernelCmd::Execute {
            target: command_target,
            code,
            stop_on_error: true,
            ..
        } if command_target == &target && code == "1 + 1"
    ));

    handle.emit(KernelEvent::BusyStateChanged { busy: true });
    assert!(matches!(
        recv_event(&mut kernel).await,
        Some(KernelEvent::BusyStateChanged { busy: true })
    ));
}

#[test]
fn subshell_header_serializes_without_losing_base_header() {
    let message: JupyterMessage = KernelInfoRequest::default().into();
    let original_id = message.header.msg_id.clone();
    let header = SubshellHeader {
        inner: message.header,
        subshell_id: Some("inspect-123".to_owned()),
    };
    let value = serde_json::to_value(header).expect("serialize subshell header");
    assert_eq!(value["msg_id"], original_id);
    assert_eq!(value["subshell_id"], "inspect-123");
}

#[test]
fn router_and_process_log_ring_keep_late_output_deterministic() {
    let mut router = ExecRouter::with_capacity(1);
    let exec = ExecId("exec-1".to_owned());
    let target = TargetId::Cell(CellId("cell-1".to_owned()));
    router.register(exec.clone(), target.clone());
    assert_eq!(router.mark_iopub_idle(&exec), None);
    let (finished_target, ok, _) = router
        .mark_shell_reply(&exec, true)
        .expect("both signals finish execution");
    assert_eq!(finished_target, target);
    assert!(ok);
    assert_eq!(
        router.resolve_output_target(Some(&exec)),
        (finished_target, true)
    );
    assert_eq!(
        router.resolve_output_target(Some(&ExecId("unknown".to_owned()))),
        (TargetId::SessionLog, true)
    );

    let mut log = RingLog::new(2);
    log.push_line("one".to_owned());
    log.push_line("two".to_owned());
    log.push_line("three".to_owned());
    assert_eq!(log.tail(2), ["two", "three"]);
}

#[tokio::test]
#[ignore = "requires an installed ipykernel and a usable local Jupyter runtime"]
async fn live_ipykernel_round_trip() {
    let workdir = std::env::temp_dir().join(format!("nb-kernel-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workdir).expect("create live test directory");
    let mut kernel = LocalZmqKernel::spawn(KernelLaunchSpec {
        requested_name: "python3".to_owned(),
        working_dir: workdir.clone(),
        existing_connection_file: None,
    });

    let mut ready = false;
    for _ in 0..120 {
        match tokio::time::timeout(Duration::from_secs(1), recv_event(&mut kernel)).await {
            Ok(Some(KernelEvent::Ready { .. })) => {
                ready = true;
                break;
            }
            Ok(Some(KernelEvent::LaunchFailed { reason, .. })) => panic!("launch failed: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) => panic!("kernel event channel closed during startup"),
            Err(_) => continue,
        }
    }
    assert!(ready, "kernel did not become ready");

    let target = TargetId::Cell(CellId("live-cell".to_owned()));
    kernel
        .send(KernelCmd::Execute {
            target,
            lane: Lane::Main,
            code: "print('round-trip')".to_owned(),
            silent: false,
            store_history: true,
            stop_on_error: true,
        })
        .expect("send execute");

    let mut exec_id = None;
    let mut saw_output = false;
    let mut finished = false;
    for _ in 0..120 {
        match tokio::time::timeout(Duration::from_secs(1), recv_event(&mut kernel)).await {
            Ok(Some(KernelEvent::ExecutionStarted { exec_id: id, .. })) => exec_id = Some(id),
            Ok(Some(KernelEvent::Output {
                payload: WireOutput::Stream { text, .. },
                ..
            })) if text.contains("round-trip") => saw_output = true,
            Ok(Some(KernelEvent::ExecutionFinished { ok, .. })) => {
                assert!(ok, "live execute failed");
                finished = true;
                break;
            }
            Ok(Some(KernelEvent::Died { exit_status, .. })) => panic!("kernel died: {exit_status}"),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert!(exec_id.is_some());
    assert!(finished);
    assert!(saw_output);
    let _ = kernel.send(KernelCmd::Shutdown { kill_process: true });
    let _ = std::fs::remove_dir_all(workdir);
}
