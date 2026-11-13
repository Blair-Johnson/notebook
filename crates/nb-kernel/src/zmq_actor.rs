//! ZeroMQ kernel process launcher, socket pumps, JEP 91 subshell setup, and comm channels.
//!
//! `jupyter-protocol` 2.0.3 exposes a closed `Header` and
//! `jupyter-zmq-client` accepts only `JupyterMessage`, so its typed sender cannot
//! put JEP 91's `subshell_id` in the wire header. We still keep a dedicated
//! inspection queue/sender and serialize [`SubshellHeader`] in one place. Until
//! the upstream client exposes a raw-header send path, the inspection id is also
//! copied into message metadata as a documented compatibility marker; ordinary
//! kernels ignore that extra metadata and the queue remains correct for kernels
//! that do not advertise subshells.

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
use serde_json::Value;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio as ProcessStdio};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use nb_core::{CommId, ExecId, MimeBundle, StreamKind};

pub struct LocalZmqKernel {
    cmd_tx: mpsc::UnboundedSender<KernelCmd>,
    event_rx: mpsc::UnboundedReceiver<KernelEvent>,
    connection_file: Option<PathBuf>,
}

impl LocalZmqKernel {
    /// Spawn or attach to a local `ipykernel` background supervisor and return immediately.
    ///
    /// Startup progress and readiness arrive asynchronously via `KernelEvent::Ready`
    /// or `KernelEvent::LaunchFailed`.
    pub fn spawn(spec: KernelLaunchSpec) -> Self {
        let connection_file = spec.existing_connection_file.clone().or_else(|| {
            Some(
                jupyter_zmq_client::runtime_dir()
                    .join(format!("nb-{}.json", uuid::Uuid::new_v4())),
            )
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let supervisor_connection_file = connection_file.clone();
        tokio::spawn(async move {
            supervise(spec, supervisor_connection_file, cmd_rx, event_tx).await;
        });
        Self {
            cmd_tx,
            event_rx,
            connection_file,
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
    ShellFailed {
        lane: Lane,
        reason: String,
    },
    Stdin(JupyterMessage),
    ProcessLog {
        stream: StreamKind,
        line: String,
    },
    ProcessExited {
        status: String,
        expected: bool,
    },
}

struct PreparedKernel {
    connection_info: ConnectionInfo,
    resolved: ResolvedKernelspec,
    child: Option<Child>,
    owns_process: bool,
    pid: Option<u32>,
}

async fn supervise(
    spec: KernelLaunchSpec,
    connection_file: Option<PathBuf>,
    mut cmd_rx: mpsc::UnboundedReceiver<KernelCmd>,
    event_tx: mpsc::UnboundedSender<KernelEvent>,
) {
    let generation = 0_u64;
    let Some(connection_file) = connection_file else {
        let _ = event_tx.send(KernelEvent::LaunchFailed {
            generation,
            reason: "no connection file path available".to_owned(),
        });
        return;
    };

    let prepared = match prepare_kernel(&spec, &connection_file).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = event_tx.send(KernelEvent::LaunchFailed {
                generation,
                reason: format!("{error:#}"),
            });
            return;
        }
    };

    let PreparedKernel {
        connection_info,
        resolved,
        mut child,
        owns_process,
        pid,
    } = prepared;
    let stderr_ring = Arc::new(Mutex::new(RingLog::new(DEFAULT_PROCESS_LOG_CAP)));
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut kill_tx = None;

    if let Some(mut process) = child.take() {
        if let Some(stdout) = process.stdout.take() {
            spawn_process_stream(
                stdout,
                StreamKind::Stdout,
                internal_tx.clone(),
                Arc::new(Mutex::new(RingLog::new(DEFAULT_PROCESS_LOG_CAP))),
            );
        }
        if let Some(stderr) = process.stderr.take() {
            spawn_process_stream(
                stderr,
                StreamKind::Stderr,
                internal_tx.clone(),
                stderr_ring.clone(),
            );
        }
        let (terminate_tx, terminate_rx) = oneshot::channel();
        kill_tx = Some(terminate_tx);
        spawn_process_monitor(process, terminate_rx, internal_tx.clone());
    }

    let session = uuid::Uuid::new_v4().to_string();
    let iopub =
        match jupyter_zmq_client::create_client_iopub_connection(&connection_info, "", &session)
            .await
        {
            Ok(connection) => connection,
            Err(error) => {
                launch_failed(
                    &event_tx,
                    generation,
                    format!("iopub connect: {error}"),
                    owns_process,
                    &connection_file,
                    kill_tx,
                )
                .await;
                return;
            }
        };

    let identity = match jupyter_zmq_client::peer_identity_for_session(&session) {
        Ok(identity) => identity,
        Err(error) => {
            launch_failed(
                &event_tx,
                generation,
                format!("shell identity: {error}"),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
    };
    let mut shell = match jupyter_zmq_client::create_client_shell_connection_with_identity(
        &connection_info,
        &session,
        identity.clone(),
    )
    .await
    {
        Ok(connection) => connection,
        Err(error) => {
            launch_failed(
                &event_tx,
                generation,
                format!("shell connect: {error}"),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
    };
    let stdin = match jupyter_zmq_client::create_client_stdin_connection_with_identity(
        &connection_info,
        &session,
        identity,
    )
    .await
    {
        Ok(connection) => connection,
        Err(error) => {
            launch_failed(
                &event_tx,
                generation,
                format!("stdin connect: {error}"),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
    };
    let mut control = match jupyter_zmq_client::create_client_control_connection(
        &connection_info,
        &session,
    )
    .await
    {
        Ok(connection) => connection,
        Err(error) => {
            launch_failed(
                &event_tx,
                generation,
                format!("control connect: {error}"),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
    };

    spawn_iopub_pump(iopub, internal_tx.clone());

    let kernel_info_request: JupyterMessage = KernelInfoRequest::default().into();
    let kernel_info_id = kernel_info_request.header.msg_id.clone();
    if let Err(error) = shell.send(kernel_info_request).await {
        launch_failed(
            &event_tx,
            generation,
            format!("kernel_info send: {error}"),
            owns_process,
            &connection_file,
            kill_tx,
        )
        .await;
        return;
    }
    let (kernel_info, kernel_info_metadata) = match tokio::time::timeout(
        Duration::from_secs(30),
        read_kernel_info(&mut shell, &kernel_info_id),
    )
    .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => {
            launch_failed(
                &event_tx,
                generation,
                format!("kernel_info read: {error}"),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
        Err(_) => {
            launch_failed(
                &event_tx,
                generation,
                "timed out waiting for kernel_info_reply".to_owned(),
                owns_process,
                &connection_file,
                kill_tx,
            )
            .await;
            return;
        }
    };

    let features = supported_features(&kernel_info_metadata);
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
                    &connection_info,
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

    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
    spawn_stdin_pump(stdin, stdin_rx, internal_tx.clone());
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    spawn_control_pump(control, control_rx);

    let (main_tx, main_rx) = mpsc::unbounded_channel();
    spawn_shell_worker(shell, Lane::Main, main_rx, internal_tx.clone());
    let (inspect_tx, inspect_rx) = mpsc::unbounded_channel();
    if let Some(connection) = inspect_shell {
        spawn_shell_worker(connection, Lane::Inspect, inspect_rx, internal_tx.clone());
    }

    let info = KernelSessionInfo {
        generation,
        kernel_name: resolved.kernel_name.clone(),
        display_name: resolved.display_name.clone(),
        spec_dir: resolved.spec_dir.clone(),
        connection_file: connection_file.clone(),
        pid,
        supports_subshells: subshell_id.is_some(),
        supports_debugger,
    };
    if event_tx.send(KernelEvent::Ready { info }).is_err() {
        cleanup(owns_process, &connection_file, kill_tx, true).await;
        return;
    }

    let mut router = ExecRouter::default();
    let mut main_queue = VecDeque::new();
    let mut inspect_queue = VecDeque::new();
    let mut main_inflight = false;
    let mut inspect_inflight = false;
    let mut pending_input: Option<JupyterMessage> = None;
    let mut current_generation = generation;
    let mut shutdown_force = true;

    loop {
        schedule_shell(&mut main_queue, &main_tx, &mut main_inflight);
        if subshell_id.is_some() {
            schedule_shell(&mut inspect_queue, &inspect_tx, &mut inspect_inflight);
        }

        tokio::select! {
            command = cmd_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    KernelCmd::Execute { target, lane, code, silent, store_history, stop_on_error } => {
                        let mut message = execute_message(code, silent, store_history, stop_on_error);
                        if lane == Lane::Inspect && let Some(id) = subshell_id.as_deref() {
                            message = decorate_subshell_message(message, id);
                        }
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
                        main_queue.push_back(ShellRequest {
                            message: complete_message(code, cursor_char_pos),
                            kind: ShellKind::Complete,
                            stop_on_error: false,
                            request_id: Some(req_id),
                        });
                    }
                    KernelCmd::Inspect { req_id, code, cursor_char_pos, detail_level } => {
                        let mut message = inspect_message(code, cursor_char_pos, detail_level);
                        if subshell_id.is_some() {
                            if let Some(id) = subshell_id.as_deref() {
                                message = decorate_subshell_message(message, id);
                            }
                            inspect_queue.push_back(ShellRequest {
                                message,
                                kind: ShellKind::Inspect,
                                stop_on_error: false,
                                request_id: Some(req_id),
                            });
                        } else {
                            main_queue.push_back(ShellRequest {
                                message,
                                kind: ShellKind::Inspect,
                                stop_on_error: false,
                                request_id: Some(req_id),
                            });
                        }
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
                            send_sigint(process_id);
                        }
                    }
                    KernelCmd::Restart => {
                        current_generation = current_generation.wrapping_add(1);
                        router.clear();
                        let _ = control_tx.send(ShutdownRequest { restart: true }.into());
                    }
                    KernelCmd::Shutdown { kill_process } => {
                        shutdown_force = kill_process;
                        if owns_process || kill_process {
                            let _ = control_tx.send(ShutdownRequest { restart: false }.into());
                        }
                        break;
                    }
                }
            }
            message = internal_rx.recv() => {
                let Some(message) = message else { break };
                match message {
                    SupervisorMessage::IoPub(message) => {
                        handle_iopub(message, &mut router, &event_tx);
                    }
                    SupervisorMessage::Shell { message, request_id } => {
                        handle_shell_message(message, request_id, &mut router, &event_tx);
                    }
                    SupervisorMessage::ShellFinished { lane, failed, stop_on_error } => {
                        match lane {
                            Lane::Main => {
                                main_inflight = false;
                                if failed && stop_on_error {
                                    cancel_queued_executes(&mut main_queue, &mut router, &event_tx);
                                }
                            }
                            Lane::Inspect => inspect_inflight = false,
                        }
                    }
                    SupervisorMessage::ShellFailed { lane, reason } => {
                        let _ = event_tx.send(KernelEvent::Died {
                            generation: current_generation,
                            exit_status: format!("{lane:?} shell connection failed: {reason}"),
                            stderr_tail: ring_tail(&stderr_ring),
                        });
                        break;
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
                    SupervisorMessage::ProcessExited { status, expected } => {
                        if !expected {
                            router.clear();
                            let _ = event_tx.send(KernelEvent::Died {
                                generation: current_generation,
                                exit_status: status,
                                stderr_tail: ring_tail(&stderr_ring),
                            });
                        }
                        break;
                    }
                }
            }
        }
    }

    cleanup(
        owns_process,
        &connection_file,
        kill_tx.take(),
        shutdown_force,
    )
    .await;
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
        let resolved = resolve_kernelspec(&spec.requested_name, &spec.working_dir)
            .await
            .unwrap_or_else(|_| ResolvedKernelspec {
                kernel_name: connection_info
                    .kernel_name
                    .clone()
                    .unwrap_or_else(|| spec.requested_name.clone()),
                display_name: connection_info
                    .kernel_name
                    .clone()
                    .unwrap_or_else(|| spec.requested_name.clone()),
                language: "python".to_owned(),
                spec_dir: spec.working_dir.clone(),
                argv: Vec::new(),
                interrupt_via_message: false,
            });
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
    if let Some(parent) = connection_file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating runtime directory {}", parent.display()))?;
    }
    tokio::fs::write(
        connection_file,
        serde_json::to_vec(&connection_info).context("serializing connection info")?,
    )
    .await
    .with_context(|| format!("writing connection file {}", connection_file.display()))?;

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

async fn launch_failed(
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
    generation: u64,
    reason: String,
    owns_process: bool,
    connection_file: &Path,
    kill_tx: Option<oneshot::Sender<bool>>,
) {
    cleanup(owns_process, connection_file, kill_tx, true).await;
    let _ = event_tx.send(KernelEvent::LaunchFailed { generation, reason });
}

async fn cleanup(
    owns_process: bool,
    connection_file: &Path,
    kill_tx: Option<oneshot::Sender<bool>>,
    force_kill: bool,
) {
    if let Some(terminate) = kill_tx {
        let _ = terminate.send(force_kill);
    }
    if owns_process {
        let _ = tokio::fs::remove_file(connection_file).await;
    }
}

fn spawn_process_monitor(
    mut child: Child,
    mut terminate_rx: oneshot::Receiver<bool>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) {
    tokio::spawn(async move {
        tokio::select! {
            result = child.wait() => {
                let status = format_exit_status(result);
                let _ = internal_tx.send(SupervisorMessage::ProcessExited { status, expected: false });
            }
            requested = &mut terminate_rx => {
                if requested.unwrap_or(true) {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                } else {
                    let status = format_exit_status(child.wait().await);
                    let _ = internal_tx.send(SupervisorMessage::ProcessExited { status, expected: true });
                }
            }
        }
    });
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
) where
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
    });
}

fn spawn_iopub_pump(
    mut connection: jupyter_zmq_client::ClientIoPubConnection,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) {
    tokio::spawn(async move {
        while let Ok(message) = connection.read().await {
            if internal_tx.send(SupervisorMessage::IoPub(message)).is_err() {
                break;
            }
        }
    });
}

fn spawn_stdin_pump(
    mut connection: jupyter_zmq_client::ClientStdinConnection,
    mut outgoing: mpsc::UnboundedReceiver<JupyterMessage>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break };
                    if connection.send(message).await.is_err() { break; }
                }
                message = connection.read() => {
                    let Ok(message) = message else { break };
                    if internal_tx.send(SupervisorMessage::Stdin(message)).is_err() { break; }
                }
            }
        }
    });
}

fn spawn_control_pump(
    mut connection: jupyter_zmq_client::ClientControlConnection,
    mut outgoing: mpsc::UnboundedReceiver<JupyterMessage>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break };
                    if connection.send(message).await.is_err() { break; }
                }
                result = connection.read() => {
                    if result.is_err() { break; }
                }
            }
        }
    });
}

fn spawn_shell_worker(
    mut connection: jupyter_zmq_client::ClientShellConnection,
    lane: Lane,
    mut requests: mpsc::UnboundedReceiver<ShellRequest>,
    internal_tx: mpsc::UnboundedSender<SupervisorMessage>,
) {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let id = ExecId(request.message.header.msg_id.clone());
            let kind = request.kind;
            let stop_on_error = request.stop_on_error;
            let request_id = request.request_id;
            if let Err(error) = connection.send(request.message).await {
                let _ = internal_tx.send(SupervisorMessage::ShellFailed {
                    lane,
                    reason: error.to_string(),
                });
                break;
            }
            if kind == ShellKind::NoReply {
                let _ = internal_tx.send(SupervisorMessage::ShellFinished {
                    lane,
                    failed: false,
                    stop_on_error,
                });
                continue;
            }

            let mut failed = false;
            loop {
                let message = match connection.read().await {
                    Ok(message) => message,
                    Err(error) => {
                        let _ = internal_tx.send(SupervisorMessage::ShellFailed {
                            lane,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                let terminal = shell_reply_is_terminal(&message, &id, kind);
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
    });
}

fn shell_reply_is_terminal(message: &JupyterMessage, id: &ExecId, kind: ShellKind) -> bool {
    let Some(parent) = message.parent_header.as_ref() else {
        return false;
    };
    if parent.msg_id != id.0 {
        return false;
    }
    match kind {
        ShellKind::Execute => matches!(message.content, JupyterMessageContent::ExecuteReply(_)),
        ShellKind::Complete => matches!(message.content, JupyterMessageContent::CompleteReply(_)),
        ShellKind::Inspect => matches!(message.content, JupyterMessageContent::InspectReply(_)),
        ShellKind::NoReply => true,
    }
}

async fn read_kernel_info(
    connection: &mut jupyter_zmq_client::ClientShellConnection,
    request_id: &str,
) -> Result<(Box<KernelInfoReply>, Value), String> {
    loop {
        let message = connection.read().await.map_err(|error| error.to_string())?;
        if message
            .parent_header
            .as_ref()
            .is_some_and(|parent| parent.msg_id == request_id)
            && let JupyterMessageContent::KernelInfoReply(reply) = message.content
        {
            return Ok((reply, message.metadata));
        }
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

fn supported_features(metadata: &Value) -> Vec<String> {
    metadata
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

fn schedule_shell(
    queue: &mut VecDeque<ShellRequest>,
    sender: &mpsc::UnboundedSender<ShellRequest>,
    inflight: &mut bool,
) {
    if *inflight {
        return;
    }
    let Some(request) = queue.pop_front() else {
        return;
    };
    if sender.send(request).is_ok() {
        *inflight = true;
    }
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

fn handle_iopub(
    message: JupyterMessage,
    router: &mut ExecRouter,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
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
            let busy = matches!(status.execution_state, ExecutionState::Busy);
            let _ = event_tx.send(KernelEvent::BusyStateChanged { busy });
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
}

fn handle_shell_message(
    message: JupyterMessage,
    request_id: Option<u64>,
    router: &mut ExecRouter,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) {
    let parent = parent_exec_id(&message);
    match message.content {
        JupyterMessageContent::ExecuteReply(reply) => {
            if let Some(exec_id) = parent
                && let Some((target, ok, elapsed)) =
                    router.mark_shell_reply(&exec_id, reply.status == ReplyStatus::Ok)
            {
                let _ = event_tx.send(KernelEvent::ExecutionFinished {
                    exec_id,
                    target,
                    ok,
                    elapsed,
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

fn serialize_subshell_header(header: &SubshellHeader) -> serde_json::Result<Value> {
    serde_json::to_value(header)
}

fn decorate_subshell_message(mut message: JupyterMessage, subshell_id: &str) -> JupyterMessage {
    let header = SubshellHeader {
        inner: message.header.clone(),
        subshell_id: Some(subshell_id.to_owned()),
    };
    if let Ok(Value::Object(serialized)) = serialize_subshell_header(&header)
        && let Some(id) = serialized.get("subshell_id").cloned()
    {
        if let Value::Object(metadata) = &mut message.metadata {
            metadata.insert("subshell_id".to_owned(), id);
        } else {
            message.metadata = serde_json::json!({ "subshell_id": id });
        }
    }
    message
}

#[allow(unsafe_code)]
fn send_sigint(pid: u32) {
    // Signal mode is the Jupyter default for ipykernel. This is intentionally
    // the only unsafe operation in nb-kernel; all other lifecycle paths use
    // Tokio process handles or protocol messages.
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGINT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subshell_header_serializes_the_extension_without_losing_header_fields() {
        let request: JupyterMessage = KernelInfoRequest::default().into();
        let original_id = request.header.msg_id.clone();
        let header = SubshellHeader {
            inner: request.header,
            subshell_id: Some("inspect-123".to_owned()),
        };
        let value = serialize_subshell_header(&header).expect("serialize header");
        assert_eq!(value["msg_id"], original_id);
        assert_eq!(value["subshell_id"], "inspect-123");
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
