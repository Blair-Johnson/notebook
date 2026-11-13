//! Object-safe `KernelClient` boundary between `nb-tui` and kernel backends.

use crate::protocol::{KernelCmd, KernelEvent};
use std::path::Path;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("kernel channel closed")]
    Closed,
    #[error("kernelspec error: {0}")]
    Kernelspec(String),
    #[error("kernel launch failed: {0}")]
    Launch(String),
}

/// Object-safe async boundary implemented by `LocalZmqKernel` and `FakeKernel`.
pub trait KernelClient: Send {
    fn send(&self, cmd: KernelCmd) -> Result<(), KernelError>;
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<KernelEvent>>;
    fn try_recv(&mut self) -> Option<KernelEvent>;
    fn connection_file(&self) -> Option<&Path>;
}

/// Await the next `KernelEvent` from any `KernelClient` without heap-boxing futures.
pub async fn recv_event(client: &mut (impl KernelClient + ?Sized)) -> Option<KernelEvent> {
    std::future::poll_fn(|cx| client.poll_recv(cx)).await
}
