use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jupyter_protocol::messaging::{JupyterMessage, KernelInfoRequest};
use nb_core::{CellId, ExecId, TargetId};
use nb_kernel::{
    ExecRouter, FakeKernel, KernelClient, KernelCmd, KernelEvent, KernelLaunchSpec,
    KernelSessionInfo, Lane, LocalZmqKernel, RingLog, SubshellHeader, WireOutput, recv_event,
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
    kernel.shutdown(Duration::from_secs(3)).await;
    let _ = std::fs::remove_dir_all(workdir);
}

// Live tests against a real ipykernel 7, which supports subshells. Run with
// `pixi run cargo test -p nb-kernel --test kernel -- --ignored` in an environment
// that has it.

fn live_kernel() -> (LocalZmqKernel, PathBuf) {
    let workdir = std::env::temp_dir().join(format!("nb-kernel-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workdir).expect("create live test directory");
    let kernel = LocalZmqKernel::spawn(KernelLaunchSpec {
        requested_name: "python3".to_owned(),
        working_dir: workdir.clone(),
        existing_connection_file: None,
    });
    (kernel, workdir)
}

/// The first event matching `done` within `limit`, panicking on `Died`.
async fn wait_for(
    kernel: &mut (impl KernelClient + ?Sized),
    limit: Duration,
    mut done: impl FnMut(&KernelEvent) -> bool,
) -> Option<KernelEvent> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match tokio::time::timeout_at(deadline, recv_event(kernel)).await {
            Ok(Some(event)) if done(&event) => return Some(event),
            Ok(Some(KernelEvent::Died { exit_status, .. })) => panic!("kernel died: {exit_status}"),
            Ok(Some(KernelEvent::LaunchFailed { reason, .. })) => panic!("launch failed: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
}

async fn wait_ready(kernel: &mut LocalZmqKernel) -> KernelSessionInfo {
    match wait_for(kernel, Duration::from_secs(60), |event| {
        matches!(event, KernelEvent::Ready { .. })
    })
    .await
    {
        Some(KernelEvent::Ready { info }) => info,
        _ => panic!("kernel did not become ready"),
    }
}

/// Start `code` on the main shell and wait until the kernel is running it.
async fn start_cell(kernel: &mut LocalZmqKernel, code: &str) {
    kernel
        .send(KernelCmd::Execute {
            target: TargetId::Cell(CellId("live-cell".to_owned())),
            lane: Lane::Main,
            code: code.to_owned(),
            silent: false,
            store_history: true,
            stop_on_error: true,
        })
        .expect("send execute");
    let running = wait_for(kernel, Duration::from_secs(10), |event| {
        matches!(
            event,
            KernelEvent::ExecutionStarted {
                execution_count: Some(_),
                ..
            }
        )
    })
    .await;
    assert!(running.is_some(), "the cell never started");
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Shut `kernel` down as nb does at quit and check that it is gone without a trace.
async fn assert_quits_cleanly(mut kernel: LocalZmqKernel, pid: u32, connection_file: &Path) {
    assert!(connection_file.exists());
    let started = Instant::now();
    kernel.shutdown(Duration::from_secs(3)).await;
    let elapsed = started.elapsed();
    eprintln!("shutdown took {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?}"
    );
    assert!(!process_alive(pid), "kernel {pid} survived shutdown");
    assert!(
        !connection_file.exists(),
        "{} was left behind",
        connection_file.display()
    );
}

#[tokio::test]
#[ignore = "requires an installed ipykernel 7 and a usable local Jupyter runtime"]
async fn live_completion_is_answered_while_a_cell_sleeps() {
    let (mut kernel, workdir) = live_kernel();
    let info = wait_ready(&mut kernel).await;
    assert!(info.supports_subshells, "ipykernel 7 offers subshells");
    let complete = |req_id| KernelCmd::Complete {
        req_id,
        code: "impo".to_owned(),
        cursor_char_pos: 4,
    };

    // Warm up the completer so the timed request measures routing, not imports.
    kernel.send(complete(1)).expect("send complete");
    let warm = wait_for(&mut kernel, Duration::from_secs(30), |event| {
        matches!(event, KernelEvent::CompleteReply { req_id: 1, .. })
    })
    .await;
    assert!(warm.is_some(), "no completion on an idle kernel");

    start_cell(&mut kernel, "import time; time.sleep(10)").await;
    let started = Instant::now();
    kernel.send(complete(2)).expect("send complete");
    let reply = wait_for(&mut kernel, Duration::from_secs(2), |event| {
        matches!(event, KernelEvent::CompleteReply { req_id: 2, .. })
    })
    .await;
    eprintln!("completion during sleep took {:?}", started.elapsed());
    match reply {
        Some(KernelEvent::CompleteReply { items, .. }) => assert!(
            items.iter().any(|item| item.replace_text == "import"),
            "{items:?}"
        ),
        _ => panic!("no completion within 2 s while the cell sleeps"),
    }

    let pid = info.pid.expect("owned kernel pid");
    assert_quits_cleanly(kernel, pid, &info.connection_file).await;
    let _ = std::fs::remove_dir_all(workdir);
}

#[tokio::test]
#[ignore = "requires an installed ipykernel and a usable local Jupyter runtime"]
async fn live_shutdown_stops_the_kernel_and_removes_its_connection_file() {
    let (mut kernel, workdir) = live_kernel();
    let info = wait_ready(&mut kernel).await;
    let pid = info.pid.expect("owned kernel pid");
    assert_quits_cleanly(kernel, pid, &info.connection_file).await;
    let _ = std::fs::remove_dir_all(workdir);
}

#[tokio::test]
#[ignore = "requires an installed ipykernel and a usable local Jupyter runtime"]
async fn live_shutdown_is_bounded_when_the_kernel_ignores_it() {
    let (mut kernel, workdir) = live_kernel();
    let info = wait_ready(&mut kernel).await;
    // `sum` over a range loops in C holding the GIL, so no kernel thread can act
    // on `shutdown_request` until it returns.
    start_cell(&mut kernel, "sum(range(10**12))").await;
    let pid = info.pid.expect("owned kernel pid");
    assert_quits_cleanly(kernel, pid, &info.connection_file).await;
    let _ = std::fs::remove_dir_all(workdir);
}

#[tokio::test]
#[ignore = "requires an installed ipykernel and a usable local Jupyter runtime"]
async fn live_shutdown_only_disconnects_an_attached_kernel() {
    let (mut owner, workdir) = live_kernel();
    let info = wait_ready(&mut owner).await;
    let pid = info.pid.expect("owned kernel pid");

    let mut attached = LocalZmqKernel::spawn(KernelLaunchSpec {
        requested_name: "python3".to_owned(),
        working_dir: workdir.clone(),
        existing_connection_file: Some(info.connection_file.clone()),
    });
    let attached_info = wait_ready(&mut attached).await;
    assert_eq!(attached_info.pid, None);
    let started = Instant::now();
    attached.shutdown(Duration::from_secs(3)).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(attached);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(process_alive(pid), "detaching shut the kernel down");
    assert!(
        info.connection_file.exists(),
        "detaching removed the connection file"
    );

    // The owner still drives the kernel.
    start_cell(&mut owner, "1 + 1").await;
    assert_quits_cleanly(owner, pid, &info.connection_file).await;
    let _ = std::fs::remove_dir_all(workdir);
}
