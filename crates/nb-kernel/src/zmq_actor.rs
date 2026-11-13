//! ZeroMQ kernel process launcher, socket pumps, JEP 91 subshell setup, and comm channels.
//!
//! `jupyter-protocol` 2.0.3 has a closed `Header` and drops unknown reply fields,
//! so two JEP 91 details bypass its typed messages: requests for the inspection
//! subshell are framed and signed here with [`SubshellHeader`] (ipykernel 7 routes
//! on `header.subshell_id`), and `kernel_info_reply` is read raw because ipykernel
//! reports `supported_features` in its content.

use crate::client::{KernelClient, KernelError};
use crate::discovery::{KernelLaunchSpec, ResolvedKernelspec, resolve_kernelspec};
use crate::protocol::{
    CompletionCandidate, KernelCmd, KernelEvent, KernelSessionInfo, Lane, SubshellHeader,
    WireOutput,
};
use crate::router::{DEFAULT_PROCESS_LOG_CAP, ExecRouter, RingLog};
use anyhow::{Context, anyhow};
use jupyter_protocol::media::{Media, MediaType};
use jupyter_protocol::messaging::{
    CommClose as JupyterCommClose, CommMsg as JupyterCommMsg, CommOpen as JupyterCommOpen,
    CompleteReply, ExecuteRequest, ExecutionState, InputReply, InspectRequest, InterruptRequest,
    JupyterMessage, JupyterMessageContent, KernelInfoReply, KernelInfoRequest, ReplyStatus,
    ShutdownRequest, Stdio, UnknownMessage,
};
use jupyter_protocol::{ConnectionInfo, Transport};
use jupyter_zmq_client::{RawMessage, RuntimeError};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio as ProcessStdio};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zeromq::{SocketRecv, SocketSend, ZmqMessage};
use nb_core::{CommId, ExecId, MimeBundle, StreamKind};

pub struct LocalZmqKernel {
    cmd_tx: mpsc::UnboundedSender<KernelCmd>,
    event_rx: mpsc::UnboundedReceiver<KernelEvent>,
    connection_file: Option<PathBuf>,
    supervisor: Option<JoinHandle<()>>,
}

impl LocalZmqKernel {
    /// Spawn or attach to a local `ipykernel` background supervisor and return immediately.
    ///
    /// Startup progress and readiness arrive asynchronously via `KernelEvent::Ready`
    /// or `KernelEvent::LaunchFailed`.
    pub fn spawn(spec: KernelLaunchSpec) -> Self {
        let connection_file = spec.existing_connection_file.clone().or_else(|| {
            Some(
                crate::paths::runtime_dir().join(format!("nb-{}.json", uuid::Uuid::new_v4())),
            )
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let supervisor_connection_file = connection_file.clone();
        let supervisor = tokio::spawn(async move {
            supervise(
                spec,
                supervisor_connection_file,
                cmd_rx,
                event_tx,
                Liveness::default(),
            )
            .await;
        });
        Self {
            cmd_tx,
            event_rx,
            connection_file,
            supervisor: Some(supervisor),
        }
    }
}

impl Drop for LocalZmqKernel {
    fn drop(&mut self) {
        // A closed command channel is also handled by the supervisor. Sending a
        // final non-destructive command lets it distinguish a normal handle drop
        // from an explicit kill request for an attached external process.
        let _ = self.cmd_tx.send(KernelCmd::Shutdown {
            kill_process: false,
        });
    }
}

impl KernelClient for LocalZmqKernel {
    fn send(&self, cmd: KernelCmd) -> Result<(), KernelError> {
        self.cmd_tx.send(cmd).map_err(|_| KernelError::Closed)
    }

    fn poll_recv(&mut self, cx: &mut TaskContext<'_>) -> Poll<Option<KernelEvent>> {
        self.event_rx.poll_recv(cx)
    }

    fn try_recv(&mut self) -> Option<KernelEvent> {
        self.event_rx.try_recv().ok()
    }

    fn connection_file(&self) -> Option<&Path> {
        self.connection_file.as_deref()
    }

    fn shutdown(&mut self, timeout: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // The supervisor asks an owned kernel to exit, kills it after
            // `quit_grace`, and removes its connection file; an attached kernel
            // is only disconnected.
            let _ = self.cmd_tx.send(KernelCmd::Shutdown {
                kill_process: false,
            });
            if let Some(mut supervisor) = self.supervisor.take()
                && tokio::time::timeout(timeout, &mut supervisor)
                    .await
                    .is_err()
            {
                // Dropping its process handle makes the monitor kill the kernel.
                supervisor.abort();
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Execute,
    Complete,
    Inspect,
    NoReply,
}

struct ShellRequest {
    message: JupyterMessage,
    kind: ShellKind,
    stop_on_error: bool,
    request_id: Option<u64>,
}

/// Time bounds that keep a silent, wedged or vanished kernel from hanging the session.
#[derive(Clone, Copy, Debug)]
struct Liveness {
    /// Deadline for a `complete_request`/`inspect_request` reply. Executes never
    /// time out: cells may legitimately run for days.
    query_deadline: Duration,
    /// How long an execution that has its `execute_reply` waits for the iopub
    /// `idle`, which a PUB socket may drop under load, before finishing anyway.
    idle_grace: Duration,
    /// Heartbeat cadence for attached kernels, which have no child process to watch.
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
    heartbeat_misses: u32,
    /// How long a kernel may take to exit after `shutdown_request` before SIGKILL.
    shutdown_grace: Duration,
    /// The same, when nb is quitting and should not linger.
    quit_grace: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            query_deadline: Duration::from_secs(10),
            idle_grace: Duration::from_secs(2),
            heartbeat_interval: Duration::from_secs(3),
            heartbeat_timeout: Duration::from_secs(10),
            heartbeat_misses: 3,
            shutdown_grace: Duration::from_secs(5),
            quit_grace: Duration::from_millis(1500),
        }
    }
}

/// How long a dead kernel's stderr reader may take to reach EOF.
const STDERR_DRAIN: Duration = Duration::from_millis(500);
/// How long a final `shutdown_request` may take to reach the kernel.
const CONTROL_FLUSH: Duration = Duration::from_millis(500);
/// Pause after an iopub transport error while the SUB socket reconnects.
const IOPUB_RETRY: Duration = Duration::from_millis(100);

enum SupervisorMessage {
    IoPub(JupyterMessage),
    Shell {
        message: JupyterMessage,
        request_id: Option<u64>,
    },
    ShellFinished {
        lane: Lane,
        failed: bool,
        stop_on_error: bool,
    },
    /// A completion or inspection got no reply before its deadline.
    QueryTimedOut {
        kind: ShellKind,
        request_id: Option<u64>,
    },
    /// A shell, stdin or control socket failed; the kernel can no longer be driven.
    ChannelFailed {
        channel: &'static str,
        reason: String,
    },
    Stdin(JupyterMessage),
    ProcessLog {
        stream: StreamKind,
        line: String,
    },
    ProcessExited {
        status: String,
    },
    /// An `execute_reply` waited `idle_grace` without its iopub `idle`.
    IdleGrace(ExecId),
    HeartbeatLost {
        misses: u32,
    },
}

struct PreparedKernel {
    connection_info: ConnectionInfo,
    resolved: ResolvedKernelspec,
    child: Option<Child>,
    owns_process: bool,
    pid: Option<u32>,
}

/// Handles for a kernel process this supervisor launched.
struct OwnedProcess {
    terminate: Option<oneshot::Sender<Duration>>,
    monitor: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

impl OwnedProcess {
    /// Kill the process after `grace` (at once when zero) and wait until it is reaped.
    async fn stop(&mut self, grace: Duration) {
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(grace);
        }
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.await;
        }
    }

    /// Give the stderr reader a moment to reach EOF so the ring holds the final lines.
    async fn drain_stderr(&mut self) {
        if let Some(mut reader) = self.stderr.take() {
            let _ = tokio::time::timeout(STDERR_DRAIN, &mut reader).await;
        }
    }
}

/// Background tasks owned by one kernel session. They are aborted when it ends,
/// because a DEALER socket whose peer vanished blocks in `recv` forever.
#[derive(Default)]
struct TaskSet(Vec<JoinHandle<()>>);

impl TaskSet {
    fn push(&mut self, task: JoinHandle<()>) {
        self.0.push(task);
    }
}

impl Drop for TaskSet {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

struct Connected {
    shell: jupyter_zmq_client::ClientShellConnection,
    stdin: jupyter_zmq_client::ClientStdinConnection,
    control: jupyter_zmq_client::ClientControlConnection,
    inspect_shell: Option<jupyter_zmq_client::ClientShellConnection>,
    subshell_id: Option<String>,
    supports_debugger: bool,
}

/// Why a kernel failed to start.
enum StartupFailure {
    /// Connecting or the kernel_info handshake failed.
    Connect(String),
    /// The kernel process exited first.
    Exited(String),
    /// `Shutdown` arrived (or the handle was dropped) first.
    Shutdown,
}

/// Why one kernel session ended.
enum Outcome {
    /// Shut down on request or because the handle was dropped.
    Stop { kill: bool },
    /// Relaunch as the next generation.
    Restart,
    /// The kernel died or failed to launch.
    Dead,
}

async fn supervise(
    spec: KernelLaunchSpec,
    connection_file: Option<PathBuf>,
    mut cmd_rx: mpsc::UnboundedReceiver<KernelCmd>,
    event_tx: mpsc::UnboundedSender<KernelEvent>,
    liveness: Liveness,
) {
    let mut generation = 0_u64;
    let Some(connection_file) = connection_file else {
        let _ = event_tx.send(KernelEvent::LaunchFailed {
            generation,
            reason: "no connection file path available".to_owned(),
        });
        return;
    };

    // Commands that arrived while a kernel was starting, replayed in order.
    let mut pending = VecDeque::new();
    loop {
        let outcome = run_kernel(
            &spec,
            &connection_file,
            generation,
            &mut cmd_rx,
            &mut pending,
            &event_tx,
            liveness,
        )
        .await;
        match outcome {
            Outcome::Stop { .. } => return,
            Outcome::Restart => {}
            // A dead kernel does not end the session: `Restart` relaunches it.
            Outcome::Dead => {
                if !wait_for_restart(&mut cmd_rx, &mut pending, &event_tx).await {
                    return;
                }
            }
        }
        generation = generation.wrapping_add(1);
    }
}

/// Launch or attach one kernel generation and drive it until it stops, dies or restarts.
async fn run_kernel(
    spec: &KernelLaunchSpec,
    connection_file: &Path,
    generation: u64,
    cmd_rx: &mut mpsc::UnboundedReceiver<KernelCmd>,
    pending: &mut VecDeque<KernelCmd>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
    liveness: Liveness,
) -> Outcome {
    let prepared = match prepare_kernel(spec, connection_file).await {
        Ok(prepared) => prepared,
        Err(error) => {
            if spec.existing_connection_file.is_none() {
                let _ = tokio::fs::remove_file(connection_file).await;
            }
            let _ = event_tx.send(KernelEvent::LaunchFailed {
                generation,
                reason: format!("{error:#}"),
            });
            return Outcome::Dead;
        }
    };

    let PreparedKernel {
        connection_info,
        resolved,
        child,
        owns_process,
        pid,
    } = prepared;
    let stderr_ring = Arc::new(Mutex::new(RingLog::new(DEFAULT_PROCESS_LOG_CAP)));
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut process =
        child.map(|child| spawn_owned_process(child, pid, &stderr_ring, &internal_tx));
    let mut tasks = TaskSet::default();
    // Supervisor messages that arrive while connecting, replayed by the session loop.
    let mut early = VecDeque::new();

    // zeromq retries a refused connect for 30 s, so race startup against the
    // kernel exiting and against the UI shutting down.
    let startup = tokio::select! {
        result = connect_kernel(&connection_info, &internal_tx, &mut tasks) => {
            result.map_err(StartupFailure::Connect)
        }
        status = startup_exit(&mut internal_rx, &mut early) => {
            Err(StartupFailure::Exited(status))
        }
        () = startup_shutdown(cmd_rx, pending) => Err(StartupFailure::Shutdown),
    };
    // `Err(Some(reason))` failed the launch; `Err(None)` was asked to shut down.
    let startup = match startup {
        Ok(connected) => Ok(connected),
        Err(StartupFailure::Connect(error)) if process.is_some() => {
            // A kernel that crashes on startup usually surfaces first as a socket
            // error; its exit status and stderr say far more about why.
            match tokio::time::timeout(STDERR_DRAIN, startup_exit(&mut internal_rx, &mut early))
                .await
            {
                Ok(status) => Err(Some(format!(
                    "kernel process exited during startup ({status})"
                ))),
                Err(_) => Err(Some(error)),
            }
        }
        Err(StartupFailure::Connect(error)) => Err(Some(error)),
        Err(StartupFailure::Exited(status)) => Err(Some(format!(
            "kernel process exited during startup ({status})"
        ))),
        Err(StartupFailure::Shutdown) => Err(None),
    };
    let connected = match startup {
        Ok(connected) => connected,
        Err(Some(reason)) => {
            drop(tasks);
            let reason = match process.as_mut() {
                Some(process) => {
                    process.stop(Duration::ZERO).await;
                    process.drain_stderr().await;
                    with_stderr_hint(reason, &stderr_ring)
                }
                None => reason,
            };
            // The session loop that would forward the kernel's output never runs.
            early.extend(std::iter::from_fn(|| internal_rx.try_recv().ok()));
            for message in early {
                if let SupervisorMessage::ProcessLog { stream, line } = message {
                    let _ = event_tx.send(KernelEvent::ProcessLog { stream, line });
                }
            }
            if owns_process {
                let _ = tokio::fs::remove_file(connection_file).await;
            }
            let _ = event_tx.send(KernelEvent::LaunchFailed { generation, reason });
            return Outcome::Dead;
        }
        Err(None) => {
            drop(tasks);
            if let Some(process) = process.as_mut() {
                process.stop(Duration::ZERO).await;
            }
            if owns_process {
                let _ = tokio::fs::remove_file(connection_file).await;
            }
            return Outcome::Stop { kill: true };
        }
    };

    let Connected {
        shell,
        stdin,
        control,
        inspect_shell,
        subshell_id,
        supports_debugger,
    } = connected;
    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
    tasks.push(spawn_stdin_pump(stdin, stdin_rx, internal_tx.clone()));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let mut control_task = spawn_control_pump(control, control_rx, internal_tx.clone());

    let (main_tx, main_rx) = mpsc::unbounded_channel();
    tasks.push(spawn_shell_worker(
        shell,
        Lane::Main,
        None,
        main_rx,
        internal_tx.clone(),
        liveness.query_deadline,
    ));
    let (inspect_tx, inspect_rx) = mpsc::unbounded_channel();
    if let Some(connection) = inspect_shell {
        tasks.push(spawn_shell_worker(
            connection,
            Lane::Inspect,
            subshell_id.clone(),
            inspect_rx,
            internal_tx.clone(),
            liveness.query_deadline,
        ));
    }
    if !owns_process {
        // An attached kernel has no child process to watch, so the heartbeat is
        // the only way to notice that it died or was restarted elsewhere.
        tasks.push(spawn_heartbeat(
            connection_info,
            liveness,
            internal_tx.clone(),
        ));
    }

    let info = KernelSessionInfo {
        generation,
        kernel_name: resolved.kernel_name.clone(),
        display_name: resolved.display_name.clone(),
        spec_dir: resolved.spec_dir.clone(),
        connection_file: connection_file.to_path_buf(),
        pid,
        supports_subshells: subshell_id.is_some(),
        supports_debugger,
    };

    let mut router = ExecRouter::default();
    let mut main_queue = VecDeque::new();
    let mut inspect_queue = VecDeque::new();
    let mut main_inflight = None;
    let mut inspect_inflight = None;
    // Requests sent to the subshell; their busy/idle says nothing about the main shell.
    let mut subshell_requests = HashSet::new();
    let mut pending_input: Option<JupyterMessage> = None;

    let outcome = 'session: {
        if event_tx.send(KernelEvent::Ready { info }).is_err() {
            break 'session Outcome::Stop { kill: true };
        }

        loop {
            schedule_shell(&mut main_queue, &main_tx, &mut main_inflight);
            if subshell_id.is_some()
                && let Some(msg_id) =
                    schedule_shell(&mut inspect_queue, &inspect_tx, &mut inspect_inflight)
            {
                subshell_requests.insert(msg_id);
            }

            tokio::select! {
                command = next_buffered(pending, cmd_rx) => {
                    let Some(command) = command else { break Outcome::Stop { kill: true } };
                    match command {
                        KernelCmd::Execute { target, lane, code, silent, store_history, stop_on_error } => {
                            let message = execute_message(code, silent, store_history, stop_on_error);
                            let exec_id = ExecId(message.header.msg_id.clone());
                            let execution_count = None;
                            router.register(exec_id.clone(), target.clone());
                            let _ = event_tx.send(KernelEvent::ExecutionStarted {
                                exec_id: exec_id.clone(),
                                target: target.clone(),
                                execution_count,
                            });
                            let request = ShellRequest {
                                message,
                                kind: ShellKind::Execute,
                                stop_on_error,
                                request_id: None,
                            };
                            if lane == Lane::Inspect && subshell_id.is_some() {
                                inspect_queue.push_back(request);
                            } else {
                                main_queue.push_back(request);
                            }
                        }
                        KernelCmd::Complete { req_id, code, cursor_char_pos } => {
                            submit_query(
                                ShellRequest {
                                    message: complete_message(code, cursor_char_pos),
                                    kind: ShellKind::Complete,
                                    stop_on_error: false,
                                    request_id: Some(req_id),
                                },
                                subshell_id.is_some().then_some(&mut inspect_queue),
                                &mut main_queue,
                                main_inflight,
                                event_tx,
                            );
                        }
                        KernelCmd::Inspect { req_id, code, cursor_char_pos, detail_level } => {
                            submit_query(
                                ShellRequest {
                                    message: inspect_message(code, cursor_char_pos, detail_level),
                                    kind: ShellKind::Inspect,
                                    stop_on_error: false,
                                    request_id: Some(req_id),
                                },
                                subshell_id.is_some().then_some(&mut inspect_queue),
                                &mut main_queue,
                                main_inflight,
                                event_tx,
                            );
                        }
                        KernelCmd::ReplyInput { value } => {
                            if let Some(parent) = pending_input.take() {
                                let reply: JupyterMessage = InputReply {
                                    value,
                                    ..InputReply::default()
                                }
                                .as_child_of(&parent);
                                let _ = stdin_tx.send(reply);
                            }
                        }
                        KernelCmd::CommOpen { comm_id, target_name, data } => {
                            main_queue.push_back(ShellRequest {
                                message: JupyterCommOpen {
                                    comm_id: jupyter_protocol::CommId(comm_id.0),
                                    target_name,
                                    data,
                                    target_module: None,
                                }.into(),
                                kind: ShellKind::NoReply,
                                stop_on_error: false,
                                request_id: None,
                            });
                        }
                        KernelCmd::CommMsg { comm_id, data } => {
                            main_queue.push_back(ShellRequest {
                                message: JupyterCommMsg {
                                    comm_id: jupyter_protocol::CommId(comm_id.0),
                                    data,
                                }.into(),
                                kind: ShellKind::NoReply,
                                stop_on_error: false,
                                request_id: None,
                            });
                        }
                        KernelCmd::CommClose { comm_id, data } => {
                            main_queue.push_back(ShellRequest {
                                message: JupyterCommClose {
                                    comm_id: jupyter_protocol::CommId(comm_id.0),
                                    data,
                                }.into(),
                                kind: ShellKind::NoReply,
                                stop_on_error: false,
                                request_id: None,
                            });
                        }
                        KernelCmd::Interrupt => {
                            if resolved.interrupt_via_message || pid.is_none() {
                                let _ = control_tx.send(InterruptRequest::default().into());
                            } else if let Some(process_id) = pid {
                                signal_process_group(process_id, libc::SIGINT);
                            }
                        }
                        KernelCmd::Restart => {
                            if owns_process {
                                let _ = control_tx.send(ShutdownRequest { restart: true }.into());
                            } else {
                                // nb cannot relaunch a kernel it did not start, and shutting
                                // it down would strand whoever owns it. Reconnect instead:
                                // DEALER sockets never reconnect by themselves, so this
                                // also recovers from the kernel being restarted elsewhere.
                                let _ = event_tx.send(KernelEvent::ProcessLog {
                                    stream: StreamKind::Stderr,
                                    line: "nb did not launch this kernel; reconnecting \
                                           instead of restarting it"
                                        .to_owned(),
                                });
                            }
                            break Outcome::Restart;
                        }
                        KernelCmd::Shutdown { kill_process } => {
                            if owns_process || kill_process {
                                let _ = control_tx.send(ShutdownRequest { restart: false }.into());
                            }
                            break Outcome::Stop { kill: kill_process };
                        }
                    }
                }
                message = next_buffered(&mut early, &mut internal_rx) => {
                    let Some(message) = message else { break Outcome::Stop { kill: true } };
                    match message {
                        SupervisorMessage::IoPub(message) => {
                            if handle_iopub(message, &mut router, &mut subshell_requests, event_tx) {
                                send_died(
                                    event_tx,
                                    generation,
                                    "kernel reported execution state 'dead'".to_owned(),
                                    &stderr_ring,
                                );
                                break Outcome::Dead;
                            }
                        }
                        SupervisorMessage::Shell { message, request_id } => {
                            handle_shell_message(
                                message,
                                request_id,
                                &mut router,
                                event_tx,
                                &internal_tx,
                                liveness.idle_grace,
                            );
                        }
                        SupervisorMessage::ShellFinished { lane, failed, stop_on_error } => {
                            match lane {
                                Lane::Main => {
                                    main_inflight = None;
                                    if failed && stop_on_error {
                                        cancel_queued_executes(&mut main_queue, &mut router, event_tx);
                                    }
                                }
                                Lane::Inspect => inspect_inflight = None,
                            }
                        }
                        SupervisorMessage::QueryTimedOut { kind, request_id } => {
                            send_empty_reply(kind, request_id, event_tx);
                        }
                        SupervisorMessage::ChannelFailed { channel, reason } => {
                            send_died(
                                event_tx,
                                generation,
                                format!("{channel} connection failed: {reason}"),
                                &stderr_ring,
                            );
                            break Outcome::Dead;
                        }
                        SupervisorMessage::Stdin(message) => {
                            if let JupyterMessageContent::InputRequest(request) = &message.content {
                                let prompt = request.prompt.clone();
                                let password = request.password;
                                pending_input = Some(message);
                                let _ = event_tx.send(KernelEvent::InputRequested { prompt, password });
                            }
                        }
                        SupervisorMessage::ProcessLog { stream, line } => {
                            let _ = event_tx.send(KernelEvent::ProcessLog { stream, line });
                        }
                        SupervisorMessage::ProcessExited { status } => {
                            if let Some(process) = process.as_mut() {
                                process.drain_stderr().await;
                            }
                            send_died(event_tx, generation, status, &stderr_ring);
                            break Outcome::Dead;
                        }
                        SupervisorMessage::IdleGrace(exec_id) => {
                            if let Some((target, ok, elapsed)) = router.mark_iopub_idle(&exec_id) {
                                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                                    exec_id,
                                    target,
                                    ok,
                                    elapsed,
                                });
                            }
                        }
                        SupervisorMessage::HeartbeatLost { misses } => {
                            send_died(
                                event_tx,
                                generation,
                                format!("kernel stopped answering heartbeats ({misses} missed)"),
                                &stderr_ring,
                            );
                            break Outcome::Dead;
                        }
                    }
                }
            }
        }
    };

    answer_queued_queries(&mut main_queue, event_tx);
    answer_queued_queries(&mut inspect_queue, event_tx);
    // Closing the channel lets the control pump deliver a final shutdown_request
    // before it exits.
    drop(control_tx);
    if tokio::time::timeout(CONTROL_FLUSH, &mut control_task)
        .await
        .is_err()
    {
        control_task.abort();
    }
    drop(tasks);
    if let Some(process) = process.as_mut() {
        let grace = match outcome {
            Outcome::Restart => liveness.shutdown_grace,
            Outcome::Stop { kill: false } => liveness.quit_grace,
            Outcome::Stop { kill: true } | Outcome::Dead => Duration::ZERO,
        };
        process.stop(grace).await;
    }
    if owns_process {
        let _ = tokio::fs::remove_file(connection_file).await;
    }
    outcome
}

/// Connect every channel and complete the kernel_info handshake. The iopub pump
/// starts first so status published during startup is not lost.
async fn connect_kernel(
    connection_info: &ConnectionInfo,
    internal_tx: &mpsc::UnboundedSender<SupervisorMessage>,
    tasks: &mut TaskSet,
) -> Result<Connected, String> {
    let session = uuid::Uuid::new_v4().to_string();
    let iopub = jupyter_zmq_client::create_client_iopub_connection(connection_info, "", &session)
        .await
        .map_err(|error| format!("iopub connect: {error}"))?;
    let identity = jupyter_zmq_client::peer_identity_for_session(&session)
        .map_err(|error| format!("shell identity: {error}"))?;
    let mut shell = jupyter_zmq_client::create_client_shell_connection_with_identity(
        connection_info,
        &session,
        identity.clone(),
    )
    .await
    .map_err(|error| format!("shell connect: {error}"))?;
    let stdin = jupyter_zmq_client::create_client_stdin_connection_with_identity(
        connection_info,
        &session,
        identity,
    )
    .await
    .map_err(|error| format!("stdin connect: {error}"))?;
    let mut control =
        jupyter_zmq_client::create_client_control_connection(connection_info, &session)
            .await
            .map_err(|error| format!("control connect: {error}"))?;

    tasks.push(spawn_iopub_pump(iopub, internal_tx.clone()));

    let kernel_info_request: JupyterMessage = KernelInfoRequest::default().into();
    let kernel_info_id = kernel_info_request.header.msg_id.clone();
    shell
        .send(kernel_info_request)
        .await
        .map_err(|error| format!("kernel_info send: {error}"))?;
    let (kernel_info, features) = match tokio::time::timeout(
        Duration::from_secs(30),
        read_kernel_info(&mut shell, &kernel_info_id),
    )
    .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return Err(format!("kernel_info read: {error}")),
        Err(_) => return Err("timed out waiting for kernel_info_reply".to_owned()),
    };

    let supports_debugger = kernel_info.debugger || features.iter().any(|f| f == "debugger");
    let mut subshell_id = None;
    if features.iter().any(|feature| feature == "kernel subshells") {
        let request: JupyterMessage = UnknownMessage {
            msg_type: "create_subshell_request".to_owned(),
            content: serde_json::json!({}),
        }
        .into();
        let request_id = request.header.msg_id.clone();
        if control.send(request).await.is_ok()
            && let Ok(Ok(reply)) = tokio::time::timeout(
                Duration::from_secs(2),
                read_subshell_reply(&mut control, &request_id),
            )
            .await
        {
            subshell_id = reply;
        }
    }

    let mut inspect_shell = None;
    if subshell_id.is_some() {
        let inspect_session = format!("{session}-inspect");
        if let Ok(inspect_identity) =
            jupyter_zmq_client::peer_identity_for_session(&inspect_session)
            && let Ok(connection) =
                jupyter_zmq_client::create_client_shell_connection_with_identity(
                    connection_info,
                    &inspect_session,
                    inspect_identity,
                )
                .await
        {
            inspect_shell = Some(connection);
        } else {
            subshell_id = None;
        }
    }

    Ok(Connected {
        shell,
        stdin,
        control,
        inspect_shell,
        subshell_id,
        supports_debugger,
    })
}

/// Resolve when the kernel process exits during startup, buffering every other
/// supervisor message for the session loop.
async fn startup_exit(
    internal_rx: &mut mpsc::UnboundedReceiver<SupervisorMessage>,
    early: &mut VecDeque<SupervisorMessage>,
) -> String {
    while let Some(message) = internal_rx.recv().await {
        match message {
            SupervisorMessage::ProcessExited { status } => return status,
            other => early.push_back(other),
        }
    }
    std::future::pending().await
}

/// Resolve when `Shutdown` arrives (or the handle is dropped) during startup,
/// buffering every other command for the session loop.
async fn startup_shutdown(
    cmd_rx: &mut mpsc::UnboundedReceiver<KernelCmd>,
    pending: &mut VecDeque<KernelCmd>,
) {
    loop {
        match cmd_rx.recv().await {
            None | Some(KernelCmd::Shutdown { .. }) => return,
            Some(command) => pending.push_back(command),
        }
    }
}

/// Park a dead session until the UI asks for a restart (`true`) or shuts it down.
async fn wait_for_restart(
    cmd_rx: &mut mpsc::UnboundedReceiver<KernelCmd>,
    pending: &mut VecDeque<KernelCmd>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) -> bool {
    while let Some(command) = next_buffered(pending, cmd_rx).await {
        match command {
            KernelCmd::Restart => return true,
            KernelCmd::Shutdown { .. } => return false,
            KernelCmd::Execute { target, .. } => {
                // Nothing can run; finish at once so the request does not hang.
                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                    exec_id: ExecId(uuid::Uuid::new_v4().to_string()),
                    target,
                    ok: false,
                    elapsed: Duration::ZERO,
                });
            }
            KernelCmd::Complete { req_id, .. } => {
                send_empty_reply(ShellKind::Complete, Some(req_id), event_tx);
            }
            KernelCmd::Inspect { req_id, .. } => {
                send_empty_reply(ShellKind::Inspect, Some(req_id), event_tx);
            }
            _ => {}
        }
    }
    false
}

/// Take the next buffered item before waiting on the channel.
async fn next_buffered<T>(
    buffered: &mut VecDeque<T>,
    receiver: &mut mpsc::UnboundedReceiver<T>,
) -> Option<T> {
    match buffered.pop_front() {
        Some(item) => Some(item),
        None => receiver.recv().await,
    }
}

fn send_died(
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
    generation: u64,
    exit_status: String,
    stderr_ring: &Arc<Mutex<RingLog>>,
) {
    let _ = event_tx.send(KernelEvent::Died {
        generation,
        exit_status,
        stderr_tail: ring_tail(stderr_ring),
    });
}

/// Append the kernel's last stderr line, usually the real error (for example
/// `No module named ipykernel_launcher`), to a launch failure reason.
fn with_stderr_hint(reason: String, stderr_ring: &Arc<Mutex<RingLog>>) -> String {
    let tail = ring_tail(stderr_ring);
    match tail.iter().rev().find(|line| !line.trim().is_empty()) {
        Some(line) => format!("{reason}: {}", line.trim()),
        None => reason,
    }
}

async fn prepare_kernel(
    spec: &KernelLaunchSpec,
    connection_file: &Path,
) -> anyhow::Result<PreparedKernel> {
    if let Some(existing) = &spec.existing_connection_file {
        let bytes = tokio::fs::read(existing)
            .await
            .with_context(|| format!("reading connection file {}", existing.display()))?;
        let connection_info: ConnectionInfo = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing connection file {}", existing.display()))?;
        // The kernel is already running, so a kernelspec would only supply names;
        // skip discovery, which may have to spawn `jupyter --paths`.
        let kernel_name = connection_info
            .kernel_name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| spec.requested_name.clone());
        let resolved = ResolvedKernelspec {
            kernel_name: kernel_name.clone(),
            display_name: kernel_name,
            language: "python".to_owned(),
            spec_dir: spec.working_dir.clone(),
            argv: Vec::new(),
            interrupt_via_message: false,
        };
        return Ok(PreparedKernel {
            connection_info,
            resolved,
            child: None,
            owns_process: false,
            pid: None,
        });
    }

    let resolved = resolve_kernelspec(&spec.requested_name, &spec.working_dir)
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    let (ports, _port_listeners) =
        jupyter_zmq_client::peek_ports_with_listeners(IpAddr::V4(Ipv4Addr::LOCALHOST), 5)
            .await
            .context("allocating kernel ports")?;
    if ports.len() < 5 {
        return Err(anyhow!(
            "port allocator returned only {} ports",
            ports.len()
        ));
    }
    let connection_info = ConnectionInfo {
        ip: Ipv4Addr::LOCALHOST.to_string(),
        transport: Transport::TCP,
        stdin_port: ports[0],
        control_port: ports[1],
        hb_port: ports[2],
        shell_port: ports[3],
        iopub_port: ports[4],
        key: uuid::Uuid::new_v4().to_string(),
        signature_scheme: "hmac-sha256".to_owned(),
        kernel_name: Some(resolved.kernel_name.clone()),
    };
    let bytes = serde_json::to_vec(&connection_info).context("serializing connection info")?;
    write_private(connection_file, &bytes).await?;

    let Some(argv0) = resolved.argv.first() else {
        return Err(anyhow!("kernelspec has an empty argv"));
    };
    let mut command = Command::new(argv0);
    for arg in resolved.argv.iter().skip(1) {
        if arg == "{connection_file}" {
            command.arg(connection_file);
        } else {
            command.arg(arg);
        }
    }
    command
        .current_dir(&spec.working_dir)
        .env("JPY_PARENT_PID", std::process::id().to_string())
        .stdin(ProcessStdio::null())
        .stdout(ProcessStdio::piped())
        .stderr(ProcessStdio::piped())
        // Lead a process group so signals reach the kernel through wrapper
        // launchers (`conda run`, `pixi run`, scripts), and so terminal signals
        // aimed at nb do not reach the kernel.
        .process_group(0)
        .kill_on_drop(true);
    let child = command.spawn().with_context(|| {
        format!(
            "spawning kernelspec '{}' with {:?}",
            resolved.kernel_name, resolved.argv
        )
    })?;
    let pid = child.id();
    Ok(PreparedKernel {
        connection_info,
        resolved,
        child: Some(child),
        owns_process: true,
        pid,
    })
}

fn spawn_owned_process(
    mut child: Child,
    pid: Option<u32>,
    stderr_ring: &Arc<Mutex<RingLog>>,
    internal_tx: &mpsc::UnboundedSender<SupervisorMessage>,
) -> OwnedProcess {
    if let Some(stdout) = child.stdout.take() {
        spawn_process_stream(
            stdout,
            StreamKind::Stdout,
            internal_tx.clone(),
            Arc::new(Mutex::new(RingLog::new(DEFAULT_PROCESS_LOG_CAP))),
        );
    }
    let stderr = child.stderr.take().map(|stderr| {
        spawn_process_stream(
            stderr,
            StreamKind::Stderr,
            internal_tx.clone(),
            stderr_ring.clone(),
        )
    });
    let (terminate_tx, terminate_rx) = oneshot::channel();
    let monitor = spawn_process_monitor(child, pid, terminate_rx, internal_tx.clone());
    OwnedProcess {
        terminate: Some(terminate_tx),
        monitor: Some(monitor),
        stderr,
    }
}

fn spawn_process_monitor(
    mut child: Child,
    pid: Option<u32>,
    terminate_rx: oneshot::Receiver<Duration>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            result = child.wait() => {
                let status = format_exit_status(result);
                let _ = internal_tx.send(SupervisorMessage::ProcessExited { status });
            }
            grace = terminate_rx => {
                // A dropped sender means the supervisor is gone: kill at once.
                let grace = grace.unwrap_or(Duration::ZERO);
                if tokio::time::timeout(grace, child.wait()).await.is_err() {
                    // The group leader is not reaped yet, so its pid still names
                    // this group; kill wrapper launchers' children too.
                    if let Some(pid) = pid {
                        signal_process_group(pid, libc::SIGKILL);
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        }
    })
}

fn format_exit_status(result: std::io::Result<ExitStatus>) -> String {
    match result {
        Ok(status) => status.to_string(),
        Err(error) => error.to_string(),
    }
}

fn spawn_process_stream<R>(
    reader: R,
    stream: StreamKind,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
    ring: Arc<Mutex<RingLog>>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut log) = ring.lock() {
                log.push_line(line.clone());
            }
            if internal_tx
                .send(SupervisorMessage::ProcessLog { stream, line })
                .is_err()
            {
                break;
            }
        }
    })
}

/// Whether `error` only affects one message (bad signature, framing or JSON), so
/// the socket can keep reading. Anything else is a transport failure.
fn is_message_error(error: &RuntimeError) -> bool {
    !matches!(error, RuntimeError::ZmqError(_) | RuntimeError::IoError(_))
}

/// Report a dropped or failed message in the process log instead of losing it
/// silently. Returns `false` once the supervisor is gone.
fn log_channel_error(
    internal_tx: &mpsc::UnboundedSender<SupervisorMessage>,
    channel: &str,
    error: &RuntimeError,
) -> bool {
    internal_tx
        .send(SupervisorMessage::ProcessLog {
            stream: StreamKind::Stderr,
            line: format!("nb-kernel: {channel} message dropped: {error}"),
        })
        .is_ok()
}

fn spawn_iopub_pump(
    mut connection: jupyter_zmq_client::ClientIoPubConnection,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let message = match connection.read().await {
                Ok(message) => message,
                Err(error) => {
                    if !log_channel_error(&internal_tx, "iopub", &error) {
                        break;
                    }
                    if !is_message_error(&error) {
                        // SUB sockets reconnect by themselves; pause so a
                        // persistent transport error cannot spin this loop.
                        tokio::time::sleep(IOPUB_RETRY).await;
                    }
                    continue;
                }
            };
            if internal_tx.send(SupervisorMessage::IoPub(message)).is_err() {
                break;
            }
        }
    })
}

fn spawn_stdin_pump(
    mut connection: jupyter_zmq_client::ClientStdinConnection,
    mut outgoing: mpsc::UnboundedReceiver<JupyterMessage>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break };
                    connection.send(message).await.map(|()| None)
                }
                message = connection.read() => message.map(Some),
            };
            match result {
                Ok(Some(message)) => {
                    if internal_tx.send(SupervisorMessage::Stdin(message)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) if is_message_error(&error) => {
                    if !log_channel_error(&internal_tx, "stdin", &error) {
                        break;
                    }
                }
                Err(error) => {
                    let _ = internal_tx.send(SupervisorMessage::ChannelFailed {
                        channel: "stdin",
                        reason: error.to_string(),
                    });
                    break;
                }
            }
        }
    })
}

fn spawn_control_pump(
    mut connection: jupyter_zmq_client::ClientControlConnection,
    mut outgoing: mpsc::UnboundedReceiver<JupyterMessage>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break };
                    connection.send(message).await
                }
                // Control replies carry nothing the session needs.
                message = connection.read() => message.map(|_| ()),
            };
            match result {
                Ok(()) => {}
                Err(error) if is_message_error(&error) => {
                    if !log_channel_error(&internal_tx, "control", &error) {
                        break;
                    }
                }
                Err(error) => {
                    let _ = internal_tx.send(SupervisorMessage::ChannelFailed {
                        channel: "control",
                        reason: error.to_string(),
                    });
                    break;
                }
            }
        }
    })
}

fn spawn_heartbeat(
    connection_info: ConnectionInfo,
    liveness: Liveness,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut misses = 0;
        loop {
            tokio::time::sleep(liveness.heartbeat_interval).await;
            // A REQ socket is unusable after a lost reply, so each ping gets a fresh one.
            let ping = async {
                let mut heartbeat =
                    jupyter_zmq_client::create_client_heartbeat_connection(&connection_info)
                        .await?;
                heartbeat.single_heartbeat().await
            };
            match tokio::time::timeout(liveness.heartbeat_timeout, ping).await {
                Ok(Ok(())) => misses = 0,
                Ok(Err(_)) | Err(_) => misses += 1,
            }
            if misses >= liveness.heartbeat_misses {
                let _ = internal_tx.send(SupervisorMessage::HeartbeatLost { misses });
                return;
            }
        }
    })
}

fn lane_channel(lane: Lane) -> &'static str {
    match lane {
        Lane::Main => "main shell",
        Lane::Inspect => "inspect shell",
    }
}

/// Drive one shell lane. With `subshell_id`, every request is addressed to that
/// JEP 91 subshell, which the kernel serves even while its main shell is busy.
fn spawn_shell_worker(
    mut connection: jupyter_zmq_client::ClientShellConnection,
    lane: Lane,
    subshell_id: Option<String>,
    mut requests: mpsc::UnboundedReceiver<ShellRequest>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
    query_deadline: Duration,
) -> JoinHandle<()> {
    let channel = lane_channel(lane);
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let id = ExecId(request.message.header.msg_id.clone());
            let kind = request.kind;
            let stop_on_error = request.stop_on_error;
            let request_id = request.request_id;
            let sent = match subshell_id.as_deref() {
                Some(subshell_id) => {
                    send_to_subshell(&mut connection, request.message, subshell_id).await
                }
                None => connection.send(request.message).await,
            };
            if let Err(error) = sent {
                let _ = internal_tx.send(SupervisorMessage::ChannelFailed {
                    channel,
                    reason: error.to_string(),
                });
                return;
            }
            if kind == ShellKind::NoReply {
                let _ = internal_tx.send(SupervisorMessage::ShellFinished {
                    lane,
                    failed: false,
                    stop_on_error,
                });
                continue;
            }

            // Only completions and inspections get a deadline: a kernel that never
            // answers one (an unsupported request, a handler that raised) must not
            // wedge the lane, but an execute may run for days.
            let deadline =
                (kind != ShellKind::Execute).then(|| tokio::time::Instant::now() + query_deadline);
            let mut failed = false;
            loop {
                let read = match deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, connection.read()).await,
                    None => Ok(connection.read().await),
                };
                let message = match read {
                    Ok(Ok(message)) => message,
                    Ok(Err(error)) if is_message_error(&error) => {
                        if !log_channel_error(&internal_tx, channel, &error) {
                            return;
                        }
                        continue;
                    }
                    Ok(Err(error)) => {
                        let _ = internal_tx.send(SupervisorMessage::ChannelFailed {
                            channel,
                            reason: error.to_string(),
                        });
                        return;
                    }
                    Err(_) => {
                        let _ =
                            internal_tx.send(SupervisorMessage::QueryTimedOut { kind, request_id });
                        break;
                    }
                };
                // Requests are serialized per lane, so a reply to anything else is
                // a late answer to a request that already timed out.
                if !is_reply_to(&message, &id) {
                    continue;
                }
                let terminal = shell_reply_is_terminal(&message, kind);
                if let JupyterMessageContent::ExecuteReply(reply) = &message.content {
                    failed = reply.status != ReplyStatus::Ok;
                }
                if internal_tx
                    .send(SupervisorMessage::Shell {
                        message,
                        request_id,
                    })
                    .is_err()
                {
                    return;
                }
                if terminal {
                    break;
                }
            }
            let _ = internal_tx.send(SupervisorMessage::ShellFinished {
                lane,
                failed,
                stop_on_error,
            });
        }
    })
}

fn is_reply_to(message: &JupyterMessage, id: &ExecId) -> bool {
    message
        .parent_header
        .as_ref()
        .is_some_and(|parent| parent.msg_id == id.0)
}

fn shell_reply_is_terminal(message: &JupyterMessage, kind: ShellKind) -> bool {
    match kind {
        ShellKind::Execute => matches!(message.content, JupyterMessageContent::ExecuteReply(_)),
        ShellKind::Complete => matches!(message.content, JupyterMessageContent::CompleteReply(_)),
        ShellKind::Inspect => matches!(message.content, JupyterMessageContent::InspectReply(_)),
        ShellKind::NoReply => true,
    }
}

/// Read the `kernel_info_reply` and its supported features. The typed reply drops
/// `supported_features`, which ipykernel 7 sends in the content (and some kernels
/// in the metadata), so the frames are parsed here.
async fn read_kernel_info(
    connection: &mut jupyter_zmq_client::ClientShellConnection,
    request_id: &str,
) -> Result<(KernelInfoReply, Vec<String>), String> {
    loop {
        let frames = connection
            .socket
            .recv()
            .await
            .map_err(|error| error.to_string())?;
        let raw = RawMessage::from_multipart(frames, &connection.mac)
            .map_err(|error| error.to_string())?;
        let [header, parent, metadata, content, ..] = raw.jparts.as_slice() else {
            continue;
        };
        let field = |frame: &[u8], key: &str| {
            serde_json::from_slice::<Value>(frame)
                .ok()
                .and_then(|value| value.get(key)?.as_str().map(str::to_owned))
        };
        if field(parent, "msg_id").as_deref() != Some(request_id)
            || field(header, "msg_type").as_deref() != Some("kernel_info_reply")
        {
            continue;
        }
        let content: Value = serde_json::from_slice(content).map_err(|error| error.to_string())?;
        let metadata: Value = serde_json::from_slice(metadata).unwrap_or_default();
        let mut features = supported_features(&content);
        features.extend(supported_features(&metadata));
        let reply = serde_json::from_value(content).map_err(|error| error.to_string())?;
        return Ok((reply, features));
    }
}

async fn read_subshell_reply(
    connection: &mut jupyter_zmq_client::ClientControlConnection,
    request_id: &str,
) -> Result<Option<String>, String> {
    loop {
        let message = connection.read().await.map_err(|error| error.to_string())?;
        if !message
            .parent_header
            .as_ref()
            .is_some_and(|parent| parent.msg_id == request_id)
        {
            continue;
        }
        if let JupyterMessageContent::UnknownMessage(unknown) = message.content {
            let id = unknown
                .content
                .get("subshell_id")
                .and_then(Value::as_str)
                .or_else(|| {
                    unknown
                        .content
                        .get("subshell")
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_str)
                });
            return Ok(id.map(str::to_owned));
        }
        return Ok(None);
    }
}

fn supported_features(content_or_metadata: &Value) -> Vec<String> {
    content_or_metadata
        .get("supported_features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn execute_message(
    code: String,
    silent: bool,
    store_history: bool,
    stop_on_error: bool,
) -> JupyterMessage {
    ExecuteRequest {
        code,
        silent,
        store_history,
        user_expressions: None,
        allow_stdin: !silent,
        stop_on_error,
    }
    .into()
}

fn complete_message(code: String, cursor_pos: usize) -> JupyterMessage {
    jupyter_protocol::CompleteRequest { code, cursor_pos }.into()
}

fn inspect_message(code: String, cursor_pos: usize, detail_level: u8) -> JupyterMessage {
    InspectRequest {
        code,
        cursor_pos,
        detail_level: Some(detail_level as usize),
    }
    .into()
}

/// Send the next queued request if the lane is free, returning its message id.
fn schedule_shell(
    queue: &mut VecDeque<ShellRequest>,
    sender: &mpsc::UnboundedSender<ShellRequest>,
    inflight: &mut Option<ShellKind>,
) -> Option<String> {
    if inflight.is_some() {
        return None;
    }
    let request = queue.pop_front()?;
    let kind = request.kind;
    let msg_id = request.message.header.msg_id.clone();
    sender.send(request).ok()?;
    *inflight = Some(kind);
    Some(msg_id)
}

fn cancel_queued_executes(
    queue: &mut VecDeque<ShellRequest>,
    router: &mut ExecRouter,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    let mut retained = VecDeque::new();
    while let Some(request) = queue.pop_front() {
        if request.kind == ShellKind::Execute {
            let id = ExecId(request.message.header.msg_id.clone());
            if let Some((target, _, elapsed)) = router.cancel(&id, false) {
                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                    exec_id: id,
                    target,
                    ok: false,
                    elapsed,
                });
            }
        } else {
            retained.push_back(request);
        }
    }
    *queue = retained;
}

/// Route a completion or inspection where it is answered without waiting for a
/// cell: to the subshell lane when the kernel has one, otherwise to the main lane
/// unless an execute is running or queued there. Then it would wait for the cell,
/// so it is answered empty at once instead.
fn submit_query(
    request: ShellRequest,
    subshell_queue: Option<&mut VecDeque<ShellRequest>>,
    main_queue: &mut VecDeque<ShellRequest>,
    main_inflight: Option<ShellKind>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    if let Some(queue) = subshell_queue {
        enqueue_query(queue, request, event_tx);
    } else if main_inflight == Some(ShellKind::Execute)
        || main_queue
            .iter()
            .any(|queued| queued.kind == ShellKind::Execute)
    {
        send_empty_reply(request.kind, request.request_id, event_tx);
    } else {
        enqueue_query(main_queue, request, event_tx);
    }
}

/// Queue a completion or inspection, keeping at most one of each kind waiting so
/// keystrokes cannot pile requests up behind a running cell. The request it
/// replaces is answered empty at once so the UI stops waiting for it.
fn enqueue_query(
    queue: &mut VecDeque<ShellRequest>,
    request: ShellRequest,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    if let Some(queued) = queue.iter_mut().find(|queued| queued.kind == request.kind) {
        let replaced = std::mem::replace(queued, request);
        send_empty_reply(replaced.kind, replaced.request_id, event_tx);
    } else {
        queue.push_back(request);
    }
}

/// Answer every queued completion and inspection when a session ends.
fn answer_queued_queries(
    queue: &mut VecDeque<ShellRequest>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    for request in queue.drain(..) {
        send_empty_reply(request.kind, request.request_id, event_tx);
    }
}

fn send_empty_reply(
    kind: ShellKind,
    request_id: Option<u64>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    let Some(req_id) = request_id else {
        return;
    };
    let event = match kind {
        ShellKind::Complete => KernelEvent::CompleteReply {
            req_id,
            start_char: 0,
            items: Vec::new(),
        },
        ShellKind::Inspect => KernelEvent::InspectReply { req_id, doc: None },
        ShellKind::Execute | ShellKind::NoReply => return,
    };
    let _ = event_tx.send(event);
}

/// Route one iopub message. Returns `true` when the kernel announced its own death.
fn handle_iopub(
    message: JupyterMessage,
    router: &mut ExecRouter,
    subshell_requests: &mut HashSet<String>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) -> bool {
    let parent = parent_exec_id(&message);
    match message.content {
        JupyterMessageContent::StreamContent(stream) => {
            let kind = match stream.name {
                Stdio::Stdout => StreamKind::Stdout,
                Stdio::Stderr => StreamKind::Stderr,
            };
            route_output(
                parent,
                WireOutput::Stream {
                    stream: kind,
                    text: stream.text,
                },
                router,
                event_tx,
            );
        }
        JupyterMessageContent::ExecuteResult(result) => {
            route_output(
                parent,
                WireOutput::Rich {
                    bundle: media_to_bundle(&result.data),
                    metadata: result.metadata,
                    display_id: result.transient.and_then(|transient| transient.display_id),
                    is_execute_result: true,
                    execution_count: i64::try_from(result.execution_count.0).ok(),
                },
                router,
                event_tx,
            );
        }
        JupyterMessageContent::DisplayData(data) => {
            route_output(
                parent,
                WireOutput::Rich {
                    bundle: media_to_bundle(&data.data),
                    metadata: data.metadata,
                    display_id: data.transient.and_then(|transient| transient.display_id),
                    is_execute_result: false,
                    execution_count: None,
                },
                router,
                event_tx,
            );
        }
        JupyterMessageContent::UpdateDisplayData(data) => {
            if let Some(display_id) = data.transient.display_id {
                let _ = event_tx.send(KernelEvent::UpdateDisplay {
                    display_id,
                    bundle: media_to_bundle(&data.data),
                    metadata: data.metadata,
                });
            }
        }
        JupyterMessageContent::ErrorOutput(error) => {
            route_output(
                parent,
                WireOutput::Error {
                    ename: error.ename,
                    evalue: error.evalue,
                    traceback: error.traceback,
                },
                router,
                event_tx,
            );
        }
        JupyterMessageContent::ClearOutput(clear) => {
            let (target, _) = router.resolve_output_target(parent.as_ref());
            let _ = event_tx.send(KernelEvent::ClearOutput {
                target,
                wait: clear.wait,
            });
        }
        JupyterMessageContent::ExecuteInput(input) => {
            if let Some(exec_id) = parent {
                router.mark_started(&exec_id);
                let (target, _) = router.resolve_output_target(Some(&exec_id));
                let _ = event_tx.send(KernelEvent::ExecutionStarted {
                    exec_id,
                    target,
                    execution_count: i64::try_from(input.execution_count.0).ok(),
                });
            }
        }
        JupyterMessageContent::Status(status) => {
            if status.execution_state == ExecutionState::Dead {
                return true;
            }
            let from_subshell = parent.as_ref().is_some_and(|id| {
                if status.execution_state == ExecutionState::Idle {
                    subshell_requests.remove(&id.0)
                } else {
                    subshell_requests.contains(&id.0)
                }
            });
            if !from_subshell {
                let busy = matches!(status.execution_state, ExecutionState::Busy);
                let _ = event_tx.send(KernelEvent::BusyStateChanged { busy });
            }
            if status.execution_state == ExecutionState::Idle
                && let Some(exec_id) = parent
                && let Some((target, ok, elapsed)) = router.mark_iopub_idle(&exec_id)
            {
                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                    exec_id,
                    target,
                    ok,
                    elapsed,
                });
            }
        }
        JupyterMessageContent::CommOpen(comm) => {
            let (target, _) = router.resolve_output_target(parent.as_ref());
            let _ = event_tx.send(KernelEvent::CommOpened {
                comm_id: CommId(comm.comm_id.0),
                target_name: comm.target_name,
                data: comm.data,
                parent_target: Some(target),
            });
        }
        JupyterMessageContent::CommMsg(comm) => {
            let _ = event_tx.send(KernelEvent::CommMessage {
                comm_id: CommId(comm.comm_id.0),
                data: comm.data,
            });
        }
        JupyterMessageContent::CommClose(comm) => {
            let _ = event_tx.send(KernelEvent::CommClosed {
                comm_id: CommId(comm.comm_id.0),
            });
        }
        _ => {}
    }
    false
}

fn handle_shell_message(
    message: JupyterMessage,
    request_id: Option<u64>,
    router: &mut ExecRouter,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
    internal_tx: &mpsc::UnboundedSender<SupervisorMessage>,
    idle_grace: Duration,
) {
    let parent = parent_exec_id(&message);
    match message.content {
        JupyterMessageContent::ExecuteReply(reply) => {
            let Some(exec_id) = parent else {
                return;
            };
            if let Some((target, ok, elapsed)) =
                router.mark_shell_reply(&exec_id, reply.status == ReplyStatus::Ok)
            {
                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                    exec_id,
                    target,
                    ok,
                    elapsed,
                });
            } else if router.active(&exec_id).is_some() {
                // The iopub `idle` normally follows within milliseconds. PUB drops
                // messages past its high-water mark, so finish without it after a
                // grace period rather than leave the cell running forever.
                let internal_tx = internal_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(idle_grace).await;
                    let _ = internal_tx.send(SupervisorMessage::IdleGrace(exec_id));
                });
            }
        }
        JupyterMessageContent::CompleteReply(reply) => {
            if let Some(exec_id) = parent {
                let _ = event_tx.send(KernelEvent::CompleteReply {
                    req_id: request_id.unwrap_or_else(|| request_id_as_u64(&exec_id)),
                    start_char: reply.cursor_start,
                    items: completion_candidates(&reply),
                });
            }
        }
        JupyterMessageContent::InspectReply(reply) => {
            if let Some(exec_id) = parent {
                let _ = event_tx.send(KernelEvent::InspectReply {
                    req_id: request_id.unwrap_or_else(|| request_id_as_u64(&exec_id)),
                    doc: reply.found.then(|| media_text(&reply.data)).flatten(),
                });
            }
        }
        _ => {}
    }
}

fn parent_exec_id(message: &JupyterMessage) -> Option<ExecId> {
    message
        .parent_header
        .as_ref()
        .map(|parent| ExecId(parent.msg_id.clone()))
}

fn route_output(
    parent: Option<ExecId>,
    payload: WireOutput,
    router: &ExecRouter,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    let (target, late) = router.resolve_output_target(parent.as_ref());
    let _ = event_tx.send(KernelEvent::Output {
        exec_id: parent,
        target,
        late,
        payload,
    });
}

fn media_to_bundle(media: &Media) -> MimeBundle {
    serde_json::to_value(media)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|object| object.into_iter().collect())
        .unwrap_or_default()
}

fn media_text(media: &Media) -> Option<String> {
    media.content.iter().find_map(|item| match item {
        MediaType::Plain(text) => Some(text.clone()),
        _ => None,
    })
}

fn completion_candidates(reply: &CompleteReply) -> Vec<CompletionCandidate> {
    let types = reply
        .metadata
        .get("_jupyter_types_experimental")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item.get("text")?.as_str()?.to_owned(),
                item.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item.get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ))
        })
        .collect::<Vec<_>>();
    reply
        .matches
        .iter()
        .map(|replace_text| {
            let (_, kind, detail) = types
                .iter()
                .find(|(text, _, _)| text == replace_text)
                .cloned()
                .unwrap_or_else(|| (replace_text.clone(), String::new(), String::new()));
            CompletionCandidate {
                replace_text: replace_text.clone(),
                kind,
                detail,
            }
        })
        .collect()
}

fn request_id_as_u64(id: &ExecId) -> u64 {
    let mut hash = 14695981039346656037_u64;
    for byte in id.0.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

fn ring_tail(ring: &Arc<Mutex<RingLog>>) -> Vec<String> {
    ring.lock()
        .map(|log| log.tail(DEFAULT_PROCESS_LOG_CAP))
        .unwrap_or_default()
}

async fn send_to_subshell(
    connection: &mut jupyter_zmq_client::ClientShellConnection,
    message: JupyterMessage,
    subshell_id: &str,
) -> Result<(), RuntimeError> {
    let frames = subshell_frames(
        message,
        &connection.session_id,
        subshell_id,
        connection.mac.as_ref(),
    )?;
    connection.socket.send(frames).await?;
    Ok(())
}

/// Frame and sign `message` as `Connection::send` does, but with JEP 91's
/// `subshell_id` in the header, which `jupyter-protocol`'s `Header` cannot carry.
fn subshell_frames(
    message: JupyterMessage,
    session: &str,
    subshell_id: &str,
    key: Option<&ring::hmac::Key>,
) -> Result<ZmqMessage, RuntimeError> {
    let message = message.with_session(session);
    let header = SubshellHeader {
        inner: message.header,
        subshell_id: Some(subshell_id.to_owned()),
    };
    let parts = [
        serde_json::to_vec(&header)?,
        match &message.parent_header {
            Some(parent) => serde_json::to_vec(parent)?,
            None => b"{}".to_vec(),
        },
        serde_json::to_vec(&message.metadata)?,
        serde_json::to_vec(&message.content)?,
    ];
    // The signature covers header, parent header, metadata and content, not buffers.
    let signature = key
        .map(|key| {
            ring::hmac::sign(key, &parts.concat())
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .unwrap_or_default();
    let mut frames = ZmqMessage::from(b"<IDS|MSG>".to_vec());
    frames.push_back(signature.into_bytes().into());
    for part in parts {
        frames.push_back(part.into());
    }
    for buffer in message.buffers {
        frames.push_back(buffer);
    }
    Ok(frames)
}

/// Write a connection file readable only by its owner, as Jupyter does: it
/// holds the key that signs messages, so anyone who can read it can run
/// code in the kernel. Missing directories are created owner-only too.
async fn write_private(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let hint = "set JUPYTER_RUNTIME_DIR to a writable directory";
    if let Some(parent) = path.parent() {
        let mut dirs = tokio::fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        dirs.mode(0o700);
        dirs.create(parent)
            .await
            .with_context(|| format!("creating runtime directory {} ({hint})", parent.display()))?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("writing connection file {} ({hint})", path.display()))?;
    // `mode` only applies to new files; a restart rewrites the same path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).await?;
    }
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

#[allow(unsafe_code)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    // Owned kernels lead their own process group (see `prepare_kernel`), so the
    // signal also reaches a kernel started through a wrapper launcher. This is
    // intentionally the only unsafe operation in nb-kernel; all other lifecycle
    // paths use Tokio process handles or protocol messages.
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    unsafe {
        if libc::killpg(pid, signal) != 0 {
            let _ = libc::kill(pid, signal);
        }
    }
}

#[cfg(test)]
mod liveness_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subshell_frames_carry_the_id_in_a_signed_header() {
        let request = complete_message("impo".to_owned(), 4);
        let original_id = request.header.msg_id.clone();
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"secret");
        let frames = subshell_frames(request, "session-1", "inspect-123", Some(&key))
            .expect("frame message");

        // The upstream reader checks the signature, so these frames pass as signed.
        let raw = RawMessage::from_multipart(frames, &Some(key)).expect("valid signature");
        let header: Value = serde_json::from_slice(&raw.jparts[0]).expect("header json");
        assert_eq!(header["msg_id"], original_id);
        assert_eq!(header["msg_type"], "complete_request");
        assert_eq!(header["session"], "session-1");
        assert_eq!(header["subshell_id"], "inspect-123");
        assert_eq!(&raw.jparts[1][..], b"{}");
        let content: Value = serde_json::from_slice(&raw.jparts[3]).expect("content json");
        assert_eq!(content["code"], "impo");
    }

    #[test]
    fn completion_metadata_is_joined_to_candidates() {
        let reply = CompleteReply {
            matches: vec!["alpha".to_owned(), "beta".to_owned()],
            cursor_start: 0,
            cursor_end: 5,
            metadata: serde_json::Map::from_iter([(
                "_jupyter_types_experimental".to_owned(),
                serde_json::json!([
                    {"text": "alpha", "type": "function", "signature": "(x)"}
                ]),
            )]),
            ..CompleteReply::default()
        };
        let candidates = completion_candidates(&reply);
        assert_eq!(candidates[0].kind, "function");
        assert_eq!(candidates[0].detail, "(x)");
        assert_eq!(candidates[1].kind, "");
    }
}
