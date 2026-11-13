//! Socket-level liveness tests. A scripted in-process kernel speaks the Jupyter
//! wire protocol, so deadlines, pumps, heartbeats and restarts run against real
//! ZeroMQ sockets. Liveness bounds come from `fast()`; the only wall-clock
//! assumption is that each awaited event arrives within `WAIT`.

use super::*;
use nb_core::{CellId, TargetId};
use serde_json::json;
use tokio::time::Instant;

/// Upper bound for any single expected event.
const WAIT: Duration = Duration::from_secs(10);

/// macOS marks new sockets close-on-exec only after creating them, so a kernel
/// process spawned by one test can inherit a listener another test thread just
/// opened, then reset connections to that port when it exits. Tests that spawn
/// processes hold this exclusively; socket-only tests share it.
static SPAWN_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

fn fast() -> Liveness {
    Liveness {
        query_deadline: Duration::from_millis(300),
        idle_grace: Duration::from_millis(300),
        heartbeat_interval: Duration::from_millis(100),
        heartbeat_timeout: Duration::from_millis(500),
        heartbeat_misses: 2,
        shutdown_grace: Duration::from_millis(300),
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nb-kernel-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
}

fn message(msg_type: &str, content: Value, parent: Option<&JupyterMessage>) -> JupyterMessage {
    let content = JupyterMessageContent::from_type_and_content(msg_type, content)
        .expect("valid test message content");
    JupyterMessage::new(content, parent)
}

fn kernel_info_reply(parent: &JupyterMessage) -> JupyterMessage {
    message(
        "kernel_info_reply",
        json!({
            "status": "ok",
            "protocol_version": "5.3",
            "implementation": "wire",
            "implementation_version": "0",
            "language_info": {"name": "python", "version": "3"},
            "banner": "",
            "help_links": [],
            "debugger": false,
        }),
        Some(parent),
    )
}

fn execute_reply(parent: &JupyterMessage) -> JupyterMessage {
    message(
        "execute_reply",
        json!({"status": "ok", "execution_count": 1, "payload": [], "user_expressions": {}}),
        Some(parent),
    )
}

fn idle(parent: &JupyterMessage) -> JupyterMessage {
    message("status", json!({"execution_state": "idle"}), Some(parent))
}

fn stdout(text: &str) -> JupyterMessage {
    message("stream", json!({"name": "stdout", "text": text}), None)
}

fn execute(code: &str) -> KernelCmd {
    KernelCmd::Execute {
        target: TargetId::Cell(CellId("cell".to_owned())),
        lane: Lane::Main,
        code: code.to_owned(),
        silent: false,
        store_history: true,
        stop_on_error: true,
    }
}

fn is_stream(event: &KernelEvent, expected: &str) -> bool {
    matches!(
        event,
        KernelEvent::Output { payload: WireOutput::Stream { text, .. }, .. } if text == expected
    )
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(ProcessStdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Bind, retrying while a just-released port is still held by its previous owner.
async fn bind<T, F, Fut>(mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = jupyter_zmq_client::Result<T>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        match attempt().await {
            Ok(socket) => return socket,
            Err(error) if Instant::now() >= deadline => panic!("bind failed: {error}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// An in-process kernel that answers `kernel_info_request` and heartbeats by
/// itself and hands every other shell and control request to the test.
struct WireKernel {
    info: ConnectionInfo,
    shell_requests: mpsc::UnboundedReceiver<JupyterMessage>,
    control_requests: mpsc::UnboundedReceiver<JupyterMessage>,
    shell_replies: mpsc::UnboundedSender<JupyterMessage>,
    /// Messages to publish, and whether to sign them.
    iopub: mpsc::UnboundedSender<(JupyterMessage, bool)>,
    heartbeat: JoinHandle<()>,
    _tasks: TaskSet,
}

impl WireKernel {
    async fn ephemeral() -> Self {
        let (ports, listeners) =
            jupyter_zmq_client::peek_ports_with_listeners(IpAddr::V4(Ipv4Addr::LOCALHOST), 5)
                .await
                .expect("allocate ports");
        drop(listeners);
        Self::start(ConnectionInfo {
            ip: Ipv4Addr::LOCALHOST.to_string(),
            transport: Transport::TCP,
            shell_port: ports[0],
            iopub_port: ports[1],
            stdin_port: ports[2],
            control_port: ports[3],
            hb_port: ports[4],
            key: uuid::Uuid::new_v4().to_string(),
            signature_scheme: "hmac-sha256".to_owned(),
            kernel_name: Some("wire".to_owned()),
        })
        .await
    }

    async fn start(info: ConnectionInfo) -> Self {
        let session = uuid::Uuid::new_v4().to_string();
        let mut heartbeat =
            bind(|| jupyter_zmq_client::create_kernel_heartbeat_connection(&info)).await;
        let shell =
            bind(|| jupyter_zmq_client::create_kernel_shell_connection(&info, &session)).await;
        let mut control =
            bind(|| jupyter_zmq_client::create_kernel_control_connection(&info, &session)).await;
        let stdin =
            bind(|| jupyter_zmq_client::create_kernel_stdin_connection(&info, &session)).await;
        let mut iopub =
            bind(|| jupyter_zmq_client::create_kernel_iopub_connection(&info, &session)).await;
        let mut tasks = TaskSet::default();

        let (mut shell_out, mut shell_in) = shell.split();
        let (shell_replies, mut replies) = mpsc::unbounded_channel::<JupyterMessage>();
        let (requests_tx, shell_requests) = mpsc::unbounded_channel();
        let kernel_info_tx = shell_replies.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok(request) = shell_in.read().await {
                if matches!(request.content, JupyterMessageContent::KernelInfoRequest(_)) {
                    let _ = kernel_info_tx.send(kernel_info_reply(&request));
                } else if requests_tx.send(request).is_err() {
                    break;
                }
            }
        }));
        tasks.push(tokio::spawn(async move {
            while let Some(reply) = replies.recv().await {
                if shell_out.send(reply).await.is_err() {
                    break;
                }
            }
        }));

        let (control_tx, control_requests) = mpsc::unbounded_channel();
        tasks.push(tokio::spawn(async move {
            let _stdin = stdin;
            while let Ok(request) = control.read().await {
                if control_tx.send(request).is_err() {
                    break;
                }
            }
        }));

        let (iopub_tx, mut published) = mpsc::unbounded_channel::<(JupyterMessage, bool)>();
        tasks.push(tokio::spawn(async move {
            while let Some((message, signed)) = published.recv().await {
                // Without the key the signature is empty and fails the client's HMAC check.
                let key = if signed { None } else { iopub.mac.take() };
                let _ = iopub.send(message).await;
                if key.is_some() {
                    iopub.mac = key;
                }
            }
        }));

        let heartbeat = tokio::spawn(async move {
            loop {
                if heartbeat.single_heartbeat().await.is_err() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });

        Self {
            info,
            shell_requests,
            control_requests,
            shell_replies,
            iopub: iopub_tx,
            heartbeat,
            _tasks: tasks,
        }
    }

    async fn next_shell(&mut self) -> JupyterMessage {
        tokio::time::timeout(WAIT, self.shell_requests.recv())
            .await
            .expect("timed out waiting for a shell request")
            .expect("shell reader stopped")
    }

    async fn next_control(&mut self) -> JupyterMessage {
        tokio::time::timeout(WAIT, self.control_requests.recv())
            .await
            .expect("timed out waiting for a control request")
            .expect("control reader stopped")
    }

    fn reply(&self, message: JupyterMessage) {
        let _ = self.shell_replies.send(message);
    }

    fn publish(&self, message: JupyterMessage) {
        let _ = self.iopub.send((message, true));
    }

    fn publish_unsigned(&self, message: JupyterMessage) {
        let _ = self.iopub.send((message, false));
    }
}

impl Drop for WireKernel {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

/// A supervisor driven directly, as `LocalZmqKernel` drives it.
struct Session {
    commands: mpsc::UnboundedSender<KernelCmd>,
    events: mpsc::UnboundedReceiver<KernelEvent>,
    dir: PathBuf,
}

impl Session {
    fn start(spec: KernelLaunchSpec, connection_file: PathBuf, dir: PathBuf) -> Self {
        let (commands, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(supervise(
            spec,
            Some(connection_file),
            cmd_rx,
            event_tx,
            fast(),
        ));
        Self {
            commands,
            events,
            dir,
        }
    }

    fn attach(info: &ConnectionInfo) -> Self {
        let dir = temp_dir("attach");
        let file = dir.join("connection.json");
        std::fs::write(
            &file,
            serde_json::to_vec(info).expect("serialize connection info"),
        )
        .expect("write connection file");
        let spec = KernelLaunchSpec {
            requested_name: "wire".to_owned(),
            working_dir: dir.clone(),
            existing_connection_file: Some(file.clone()),
        };
        Self::start(spec, file, dir)
    }

    /// Launch an owned "kernel" that is really `/bin/sh -c script`, found through
    /// the project `.venv`. Returns the connection file it is launched with.
    fn launch(script: &str) -> (Self, PathBuf) {
        let dir = temp_dir("owned");
        let name = format!("nb-test-{}", uuid::Uuid::new_v4());
        let spec_dir = dir.join(".venv/share/jupyter/kernels").join(&name);
        std::fs::create_dir_all(&spec_dir).expect("create kernelspec directory");
        let kernelspec = json!({
            "argv": ["/bin/sh", "-c", script, "nb-test-kernel", "{connection_file}"],
            "display_name": "test",
            "language": "python",
        });
        std::fs::write(
            spec_dir.join("kernel.json"),
            serde_json::to_vec(&kernelspec).expect("serialize kernelspec"),
        )
        .expect("write kernelspec");
        let file = dir.join("connection.json");
        let spec = KernelLaunchSpec {
            requested_name: name,
            working_dir: dir.clone(),
            existing_connection_file: None,
        };
        (Self::start(spec, file.clone(), dir), file)
    }

    fn send(&self, command: KernelCmd) {
        self.commands.send(command).expect("supervisor is running");
    }

    /// Events up to and including the first one matching `done`.
    async fn until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&KernelEvent) -> bool,
    ) -> Vec<KernelEvent> {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => {
                    let finished = done(&event);
                    seen.push(event);
                    if finished {
                        return seen;
                    }
                }
                Ok(None) => panic!("event channel closed while waiting for {what}: {seen:?}"),
                Err(_) => panic!("timed out waiting for {what}: {seen:?}"),
            }
        }
    }

    async fn ready(&mut self) -> KernelSessionInfo {
        match self
            .until("Ready", |event| matches!(event, KernelEvent::Ready { .. }))
            .await
            .pop()
        {
            Some(KernelEvent::Ready { info }) => info,
            other => unreachable!("{other:?}"),
        }
    }

    /// PUB drops messages until the client's subscription lands, so publish
    /// probes until one arrives before asserting on iopub traffic.
    async fn subscribe(&mut self, kernel: &WireKernel) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            kernel.publish(stdout("probe"));
            let probe = tokio::time::timeout(Duration::from_millis(100), async {
                while let Some(event) = self.events.recv().await {
                    if is_stream(&event, "probe") {
                        return;
                    }
                }
            })
            .await;
            if probe.is_ok() {
                return;
            }
        }
        panic!("iopub subscription never established");
    }

    /// Stop the supervisor and wait until it has cleaned up its kernel.
    async fn shutdown(mut self) {
        let _ = self
            .commands
            .send(KernelCmd::Shutdown { kill_process: true });
        let _ =
            tokio::time::timeout(WAIT, async { while self.events.recv().await.is_some() {} }).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self
            .commands
            .send(KernelCmd::Shutdown { kill_process: true });
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Session {
    /// The connection info an owned launch wrote, once its key differs from `previous`.
    async fn written_connection(&mut self, file: &Path, previous: Option<&str>) -> ConnectionInfo {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            if let Ok(bytes) = tokio::fs::read(file).await
                && let Ok(info) = serde_json::from_slice::<ConnectionInfo>(&bytes)
                && Some(info.key.as_str()) != previous
            {
                return info;
            }
            while let Ok(event) = self.events.try_recv() {
                seen.push(event);
            }
            assert!(
                Instant::now() < deadline,
                "no connection file at {}: {seen:?}",
                file.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn an_unanswered_completion_times_out_and_frees_the_lane() {
    let _shared = SPAWN_LOCK.read().await;
    let mut kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;

    session.send(KernelCmd::Complete {
        req_id: 1,
        code: "x.".to_owned(),
        cursor_char_pos: 2,
    });
    let complete = kernel.next_shell().await;
    let events = session
        .until("the timed-out completion", |event| {
            matches!(event, KernelEvent::CompleteReply { req_id: 1, .. })
        })
        .await;
    assert!(
        matches!(events.last(), Some(KernelEvent::CompleteReply { items, .. }) if items.is_empty())
    );

    // The lane is free again, and a late reply to the abandoned request is stale.
    session.send(execute("1"));
    let request = kernel.next_shell().await;
    assert_eq!(request.header.msg_type, "execute_request");
    kernel.reply(message(
        "complete_reply",
        json!({"status": "ok", "matches": ["late"], "cursor_start": 0, "cursor_end": 2, "metadata": {}}),
        Some(&complete),
    ));
    kernel.reply(execute_reply(&request));
    let events = session
        .until("the execution to finish", |event| {
            matches!(event, KernelEvent::ExecutionFinished { .. })
        })
        .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, KernelEvent::CompleteReply { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn queued_queries_coalesce_and_replaced_ones_are_answered_empty() {
    let _shared = SPAWN_LOCK.read().await;
    let mut kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;
    session.send(execute("busy"));
    let busy = kernel.next_shell().await;

    for (req_id, code) in [(1, "a"), (2, "ab")] {
        session.send(KernelCmd::Complete {
            req_id,
            code: code.to_owned(),
            cursor_char_pos: code.len(),
        });
    }
    for (req_id, code) in [(3, "f("), (4, "g(")] {
        session.send(KernelCmd::Inspect {
            req_id,
            code: code.to_owned(),
            cursor_char_pos: code.len(),
            detail_level: 0,
        });
    }
    let events = session
        .until("the replaced inspection", |event| {
            matches!(event, KernelEvent::InspectReply { req_id: 3, .. })
        })
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        KernelEvent::CompleteReply { req_id: 1, items, .. } if items.is_empty()
    )));
    assert!(matches!(
        events.last(),
        Some(KernelEvent::InspectReply { doc: None, .. })
    ));

    kernel.reply(execute_reply(&busy));
    kernel.publish(idle(&busy));
    let complete = kernel.next_shell().await;
    assert!(matches!(
        &complete.content,
        JupyterMessageContent::CompleteRequest(request) if request.code == "ab"
    ));
    kernel.reply(message(
        "complete_reply",
        json!({"status": "ok", "matches": ["abc"], "cursor_start": 0, "cursor_end": 2, "metadata": {}}),
        Some(&complete),
    ));
    let events = session
        .until("the surviving completion", |event| {
            matches!(event, KernelEvent::CompleteReply { req_id: 2, .. })
        })
        .await;
    assert!(matches!(
        events.last(),
        Some(KernelEvent::CompleteReply { items, .. })
            if items.len() == 1 && items[0].replace_text == "abc"
    ));

    let inspect = kernel.next_shell().await;
    assert!(matches!(
        &inspect.content,
        JupyterMessageContent::InspectRequest(request) if request.code == "g("
    ));
    kernel.reply(message(
        "inspect_reply",
        json!({"status": "ok", "found": true, "data": {"text/plain": "g doc"}, "metadata": {}}),
        Some(&inspect),
    ));
    let events = session
        .until("the surviving inspection", |event| {
            matches!(event, KernelEvent::InspectReply { req_id: 4, .. })
        })
        .await;
    assert!(matches!(
        events.last(),
        Some(KernelEvent::InspectReply { doc: Some(doc), .. }) if doc == "g doc"
    ));
}

#[tokio::test]
async fn an_execution_whose_idle_is_lost_still_finishes() {
    let _shared = SPAWN_LOCK.read().await;
    let mut kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;

    session.send(execute("x"));
    let request = kernel.next_shell().await;
    kernel.reply(execute_reply(&request));
    let events = session
        .until("the execution to finish", |event| {
            matches!(event, KernelEvent::ExecutionFinished { .. })
        })
        .await;
    assert!(matches!(
        events.last(),
        Some(KernelEvent::ExecutionFinished { ok: true, .. })
    ));
}

#[tokio::test]
async fn malformed_iopub_messages_are_logged_and_skipped() {
    let _shared = SPAWN_LOCK.read().await;
    let kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;
    session.subscribe(&kernel).await;

    kernel.publish_unsigned(stdout("forged"));
    kernel.publish(stdout("after"));
    let events = session
        .until("output after the bad message", |event| {
            is_stream(event, "after")
        })
        .await;
    assert!(
        events.iter().any(
            |event| matches!(event, KernelEvent::ProcessLog { line, .. } if line.contains("iopub"))
        ),
        "{events:?}"
    );
    assert!(!events.iter().any(|event| is_stream(event, "forged")));
}

#[tokio::test]
async fn a_dead_status_on_iopub_is_reported_as_death() {
    let _shared = SPAWN_LOCK.read().await;
    let kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;
    session.subscribe(&kernel).await;

    kernel.publish(message("status", json!({"execution_state": "dead"}), None));
    let events = session
        .until("Died", |event| matches!(event, KernelEvent::Died { .. }))
        .await;
    assert!(matches!(
        events.last(),
        Some(KernelEvent::Died { exit_status, .. }) if exit_status.contains("dead")
    ));
}

#[tokio::test]
async fn a_silent_heartbeat_reports_death_and_restart_reattaches() {
    let _shared = SPAWN_LOCK.read().await;
    let kernel = WireKernel::ephemeral().await;
    let info = kernel.info.clone();
    let mut session = Session::attach(&info);
    assert_eq!(session.ready().await.generation, 0);

    kernel.heartbeat.abort();
    let events = session
        .until("Died", |event| matches!(event, KernelEvent::Died { .. }))
        .await;
    assert!(
        matches!(
            events.last(),
            Some(KernelEvent::Died { exit_status, .. }) if exit_status.contains("heartbeat")
        ),
        "{events:?}"
    );

    // Its manager brought the kernel back on the same ports; Restart reattaches.
    drop(kernel);
    let _kernel = WireKernel::start(info).await;
    session.send(KernelCmd::Restart);
    assert_eq!(session.ready().await.generation, 1);
}

#[tokio::test]
async fn restart_reconnects_to_an_attached_kernel_without_shutting_it_down() {
    let _shared = SPAWN_LOCK.read().await;
    let mut kernel = WireKernel::ephemeral().await;
    let mut session = Session::attach(&kernel.info);
    session.ready().await;

    session.send(KernelCmd::Restart);
    let events = session
        .until("Ready again", |event| {
            matches!(event, KernelEvent::Ready { .. })
        })
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        KernelEvent::ProcessLog { line, .. } if line.contains("reconnecting")
    )));
    assert!(matches!(
        events.last(),
        Some(KernelEvent::Ready { info }) if info.generation == 1
    ));
    assert!(
        kernel.control_requests.try_recv().is_err(),
        "an attached kernel must not be sent shutdown_request"
    );

    // The new sockets drive the same kernel.
    session.send(execute("after"));
    let request = kernel.next_shell().await;
    kernel.reply(execute_reply(&request));
    kernel.publish(idle(&request));
    session
        .until("the execution to finish", |event| {
            matches!(event, KernelEvent::ExecutionFinished { ok: true, .. })
        })
        .await;
}

#[tokio::test]
async fn a_kernel_that_exits_during_startup_fails_the_launch_with_its_stderr() {
    let _exclusive = SPAWN_LOCK.write().await;
    let (mut session, _) = Session::launch("echo 'No module named ipykernel_launcher' >&2; exit 3");
    let events = session
        .until("LaunchFailed", |event| {
            matches!(event, KernelEvent::LaunchFailed { .. })
        })
        .await;
    let Some(KernelEvent::LaunchFailed { reason, .. }) = events.last() else {
        unreachable!()
    };
    assert!(reason.contains("exited during startup"), "{reason}");
    assert!(reason.contains("ipykernel_launcher"), "{reason}");
    assert!(events.iter().any(|event| matches!(
        event,
        KernelEvent::ProcessLog { line, .. } if line.contains("ipykernel_launcher")
    )));

    // A failed launch is not terminal: Restart tries again as the next generation.
    session.send(KernelCmd::Restart);
    let events = session
        .until("LaunchFailed again", |event| {
            matches!(event, KernelEvent::LaunchFailed { .. })
        })
        .await;
    assert!(matches!(
        events.last(),
        Some(KernelEvent::LaunchFailed { generation: 1, .. })
    ));
    session.shutdown().await;
}

#[tokio::test]
async fn restart_relaunches_an_owned_kernel_as_the_next_generation() {
    // The stand-in ignores shutdown_request, so restart must escalate to SIGKILL.
    let _exclusive = SPAWN_LOCK.write().await;
    let (mut session, file) = Session::launch("exec sleep 60");
    let first_info = session.written_connection(&file, None).await;
    let mut first = WireKernel::start(first_info.clone()).await;
    let info = session.ready().await;
    assert_eq!(info.generation, 0);
    let first_pid = info.pid.expect("owned kernel pid");

    session.send(KernelCmd::Restart);
    let shutdown = first.next_control().await;
    assert!(matches!(
        &shutdown.content,
        JupyterMessageContent::ShutdownRequest(request) if request.restart
    ));
    drop(first);

    let second_info = session
        .written_connection(&file, Some(&first_info.key))
        .await;
    let _second = WireKernel::start(second_info).await;
    let info = session.ready().await;
    assert_eq!(info.generation, 1);
    assert_ne!(info.pid, Some(first_pid));
    assert!(
        !process_alive(first_pid),
        "the first kernel survived restart"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn interrupt_reaches_the_children_of_a_wrapper_launcher() {
    // The outer shell stands in for `conda run`, which does not forward SIGINT.
    let _exclusive = SPAWN_LOCK.write().await;
    let (mut session, file) = Session::launch(
        "trap : INT; \
         sh -c 'trap \"echo child-interrupted >&2; exit 0\" INT; while :; do sleep 0.05; done'; \
         exec sleep 60",
    );
    let _kernel = WireKernel::start(session.written_connection(&file, None).await).await;
    session.ready().await;

    session.send(KernelCmd::Interrupt);
    session
        .until("the child's SIGINT trap", |event| {
            matches!(event, KernelEvent::ProcessLog { line, .. } if line.contains("child-interrupted"))
        })
        .await;
    session.shutdown().await;
}
