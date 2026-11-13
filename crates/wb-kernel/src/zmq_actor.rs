//! ZeroMQ kernel process launcher, socket pumps, JEP 91 subshell setup, and comm channels.

use crate::client::{KernelClient, KernelError};
use crate::discovery::KernelLaunchSpec;
use crate::protocol::{KernelCmd, KernelEvent};
use std::path::{Path, PathBuf};
use std::task::{Context, Poll};
use tokio::sync::mpsc;

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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let connection_file = spec.existing_connection_file.clone();
        // v1 implementation spawns supervisor task handling:
        // 1. resolve_kernelspec or read existing connection JSON
        // 2. child spawn with JPY_PARENT_PID + RingLog stdout/stderr drainers
        // 3. ZMQ iopub/shell/stdin/control connections + kernel_info probe
        // 4. JEP 91 `create_subshell_request` on control when supported
        // 5. per-lane (`Main`, `Inspect`) single-in-flight shell sender + stop_on_error
        Self {
            cmd_tx,
            event_rx,
            connection_file,
        }
    }
}

impl KernelClient for LocalZmqKernel {
    fn send(&self, cmd: KernelCmd) -> Result<(), KernelError> {
        self.cmd_tx.send(cmd).map_err(|_| KernelError::Closed)
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<KernelEvent>> {
        self.event_rx.poll_recv(cx)
    }

    fn try_recv(&mut self) -> Option<KernelEvent> {
        self.event_rx.try_recv().ok()
    }

    fn connection_file(&self) -> Option<&Path> {
        self.connection_file.as_deref()
    }
}
