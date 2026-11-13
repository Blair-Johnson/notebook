//! Object-safe `KernelClient` boundary between `nb-tui` and kernel backends.

use crate::protocol::{KernelCmd, KernelEvent};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
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

    /// End the session for good and wait, at most `timeout`, until that is done:
    /// a kernel this client launched is asked to exit, killed if it does not, and
    /// its connection file removed; an attached kernel is only disconnected. Call
    /// it before the async runtime stops, which would kill the kernel abruptly.
    /// The default returns at once, for backends with nothing to wait for.
    fn shutdown(&mut self, timeout: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let _ = timeout;
        Box::pin(std::future::ready(()))
    }
}

/// Await the next `KernelEvent` from any `KernelClient` without heap-boxing futures.
pub async fn recv_event(client: &mut (impl KernelClient + ?Sized)) -> Option<KernelEvent> {
    std::future::poll_fn(|cx| client.poll_recv(cx)).await
}
