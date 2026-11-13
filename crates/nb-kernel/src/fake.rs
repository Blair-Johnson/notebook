//! Deterministic in-memory `FakeKernel` for fast headless UI and plugin tests.

use crate::client::{KernelClient, KernelError};
use crate::protocol::{KernelCmd, KernelEvent};
use std::path::Path;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

pub struct FakeKernel {
    cmd_tx: mpsc::UnboundedSender<KernelCmd>,
    event_rx: mpsc::UnboundedReceiver<KernelEvent>,
}

/// Test-side controller paired with a `FakeKernel` instance.
pub struct FakeKernelHandle {
    pub cmd_rx: mpsc::UnboundedReceiver<KernelCmd>,
    pub event_tx: mpsc::UnboundedSender<KernelEvent>,
}

impl FakeKernel {
    pub fn pair() -> (Self, FakeKernelHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Self { cmd_tx, event_rx },
            FakeKernelHandle { cmd_rx, event_tx },
        )
    }
}

impl FakeKernelHandle {
    pub fn emit(&self, event: KernelEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn drain_commands(&mut self) -> Vec<KernelCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }
}

impl KernelClient for FakeKernel {
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
        None
    }
}
