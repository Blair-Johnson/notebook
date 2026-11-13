//! Async Jupyter kernel session layer for nb.
//!
//! Architectural invariant: `nb-kernel` has zero knowledge of `ratatui`,
//! `crossterm`, keymaps, or layout. All interaction flows through `KernelClient`.

pub mod client;
pub mod discovery;
pub mod fake;
pub mod paths;
pub mod protocol;
pub mod router;
pub mod zmq_actor;

pub use client::{KernelClient, KernelError, recv_event};
pub use discovery::{KernelLaunchSpec, ResolvedKernelspec};
pub use fake::{FakeKernel, FakeKernelHandle};
pub use protocol::{
    CompletionCandidate, KernelCmd, KernelEvent, KernelSessionInfo, Lane, SubshellHeader,
    WireOutput,
};
pub use router::{ExecRouter, RingLog};
pub use zmq_actor::LocalZmqKernel;
