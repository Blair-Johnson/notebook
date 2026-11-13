//! Execution lifecycle tracking, dual-end-signal completion, late-output LRU, and stderr ring.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use wb_core::{ExecId, TargetId};

pub const COMPLETED_PARENTS_CAP: usize = 4_096;
pub const DEFAULT_PROCESS_LOG_CAP: usize = 2_000;

#[derive(Debug)]
pub struct ActiveExec {
    pub target: TargetId,
    pub started_at: Instant,
    pub got_shell_reply: bool,
    pub got_iopub_idle: bool,
    pub ok: bool,
}

/// Routes `iopub` and `shell` replies to their `TargetId`, enforces dual end signals
/// (`execute_reply` + `iopub::idle`), and retains completed `ExecId`s in a bounded LRU
/// so post-idle thread and fd output routes as `late = true` instead of being dropped.
#[derive(Debug)]
pub struct ExecRouter {
    active: HashMap<ExecId, ActiveExec>,
    completed_order: VecDeque<ExecId>,
    completed_targets: HashMap<ExecId, TargetId>,
    completed_cap: usize,
}

impl Default for ExecRouter {
    fn default() -> Self {
        Self::with_capacity(COMPLETED_PARENTS_CAP)
    }
}

impl ExecRouter {
    pub fn with_capacity(completed_cap: usize) -> Self {
        Self {
            active: HashMap::new(),
            completed_order: VecDeque::new(),
            completed_targets: HashMap::new(),
            completed_cap: completed_cap.max(1),
        }
    }

    pub fn register(&mut self, exec_id: ExecId, target: TargetId) {
        self.active.insert(
            exec_id,
            ActiveExec {
                target,
                started_at: Instant::now(),
                got_shell_reply: false,
                got_iopub_idle: false,
                ok: true,
            },
        );
    }

    pub fn active(&self, id: &ExecId) -> Option<&ActiveExec> {
        self.active.get(id)
    }

    pub fn completed_capacity(&self) -> (usize, usize) {
        (self.completed_order.len(), self.completed_cap)
    }

    /// Resolve `parent` to `(TargetId, is_late)`, falling back to `TargetId::SessionLog`.
    pub fn resolve_output_target(&self, parent: Option<&ExecId>) -> (TargetId, bool) {
        let Some(id) = parent else {
            return (TargetId::SessionLog, true);
        };
        if let Some(active) = self.active.get(id) {
            return (active.target.clone(), false);
        }
        if let Some(done_target) = self.completed_targets.get(id) {
            return (done_target.clone(), true);
        }
        (TargetId::SessionLog, true)
    }

    pub fn mark_shell_reply(&mut self, _id: &ExecId, _ok: bool) -> Option<(TargetId, bool, Duration)> {
        todo!("wb-kernel::router: record shell reply; retire to completed LRU once idle also seen")
    }

    pub fn mark_iopub_idle(&mut self, _id: &ExecId) -> Option<(TargetId, bool, Duration)> {
        todo!("wb-kernel::router: record iopub idle; retire to completed LRU once reply also seen")
    }
}

/// Bounded ring buffer retaining raw kernel process stdout/stderr lines for crash diagnostics.
#[derive(Debug)]
pub struct RingLog {
    lines: VecDeque<String>,
    cap: usize,
}

impl RingLog {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    pub fn push_line(&mut self, line: String) {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    pub fn tail(&self, max_lines: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(max_lines);
        self.lines.iter().skip(skip).cloned().collect()
    }
}
