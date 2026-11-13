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
        self.completed_targets.remove(&exec_id);
        self.completed_order.retain(|id| id != &exec_id);
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

    pub fn mark_shell_reply(
        &mut self,
        id: &ExecId,
        ok: bool,
    ) -> Option<(TargetId, bool, Duration)> {
        if let Some(active) = self.active.get_mut(id) {
            active.got_shell_reply = true;
            active.ok &= ok;
        }
        self.finish_if_ready(id)
    }

    pub fn mark_iopub_idle(&mut self, id: &ExecId) -> Option<(TargetId, bool, Duration)> {
        if let Some(active) = self.active.get_mut(id) {
            active.got_iopub_idle = true;
        }
        self.finish_if_ready(id)
    }

    /// Retire an execution as a client-side cancellation. This is used for main-lane
    /// requests skipped by `stop_on_error` before they reach the kernel.
    pub fn cancel(&mut self, id: &ExecId, ok: bool) -> Option<(TargetId, bool, Duration)> {
        let active = self.active.remove(id)?;
        let elapsed = active.started_at.elapsed();
        let target = active.target;
        self.retire(id.clone(), target.clone());
        Some((target, active.ok && ok, elapsed))
    }

    /// Drop all active and completed state when a kernel is restarted or dies.
    pub fn clear(&mut self) {
        self.active.clear();
        self.completed_order.clear();
        self.completed_targets.clear();
    }

    fn finish_if_ready(&mut self, id: &ExecId) -> Option<(TargetId, bool, Duration)> {
        let ready = self
            .active
            .get(id)
            .is_some_and(|active| active.got_shell_reply && active.got_iopub_idle);
        if !ready {
            return None;
        }

        let active = self.active.remove(id)?;
        let elapsed = active.started_at.elapsed();
        let target = active.target;
        self.retire(id.clone(), target.clone());
        Some((target, active.ok, elapsed))
    }

    fn retire(&mut self, id: ExecId, target: TargetId) {
        self.completed_targets.remove(&id);
        self.completed_order.retain(|old| old != &id);
        self.completed_order.push_back(id.clone());
        self.completed_targets.insert(id, target);

        while self.completed_order.len() > self.completed_cap {
            if let Some(evicted) = self.completed_order.pop_front() {
                self.completed_targets.remove(&evicted);
            }
        }
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

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wb_core::{CellId, ExecId};

    fn exec(n: usize) -> ExecId {
        ExecId(format!("exec-{n}"))
    }

    fn target(n: usize) -> TargetId {
        TargetId::Cell(CellId(format!("cell-{n}")))
    }

    #[test]
    fn execution_finishes_in_either_signal_order() {
        let mut router = ExecRouter::default();
        let first = exec(1);
        router.register(first.clone(), target(1));
        assert!(router.mark_shell_reply(&first, true).is_none());
        let (resolved, ok, _) = router
            .mark_iopub_idle(&first)
            .expect("both end signals finish execution");
        assert_eq!(resolved, target(1));
        assert!(ok);
        assert!(router.active(&first).is_none());

        let second = exec(2);
        router.register(second.clone(), target(2));
        assert!(router.mark_iopub_idle(&second).is_none());
        let (_, ok, _) = router
            .mark_shell_reply(&second, false)
            .expect("idle before shell reply also finishes");
        assert!(!ok);
    }

    #[test]
    fn completed_parents_are_bounded_fifo() {
        let mut router = ExecRouter::with_capacity(4_096);
        for n in 0..=4_096 {
            let id = exec(n);
            router.register(id.clone(), target(n));
            assert!(router.mark_shell_reply(&id, true).is_none());
            assert!(router.mark_iopub_idle(&id).is_some());
        }

        assert_eq!(router.completed_capacity(), (4_096, 4_096));
        assert_eq!(
            router.resolve_output_target(Some(&exec(0))),
            (TargetId::SessionLog, true)
        );
        assert_eq!(
            router.resolve_output_target(Some(&exec(1))),
            (target(1), true)
        );
    }

    #[test]
    fn late_output_and_session_log_fallback_are_explicit() {
        let mut router = ExecRouter::default();
        let id = exec(9);
        router.register(id.clone(), target(9));
        assert_eq!(router.resolve_output_target(Some(&id)), (target(9), false));
        let _ = router.mark_shell_reply(&id, true);
        let _ = router.mark_iopub_idle(&id);
        assert_eq!(router.resolve_output_target(Some(&id)), (target(9), true));
        assert_eq!(
            router.resolve_output_target(Some(&exec(999))),
            (TargetId::SessionLog, true)
        );
        assert_eq!(
            router.resolve_output_target(None),
            (TargetId::SessionLog, true)
        );
    }

    #[test]
    fn clear_removes_active_and_completed_state() {
        let mut router = ExecRouter::default();
        let id = exec(1);
        router.register(id.clone(), target(1));
        let _ = router.mark_shell_reply(&id, true);
        let _ = router.mark_iopub_idle(&id);
        router.clear();
        assert_eq!(router.completed_capacity().0, 0);
        assert_eq!(
            router.resolve_output_target(Some(&id)),
            (TargetId::SessionLog, true)
        );
    }

    #[test]
    fn ring_log_keeps_only_the_newest_lines() {
        let mut log = RingLog::new(2);
        log.push_line("one".into());
        log.push_line("two".into());
        log.push_line("three".into());
        assert_eq!(log.tail(10), ["two", "three"]);
        assert_eq!(log.tail(1), ["three"]);
        assert_eq!(log.len(), 2);
        assert_eq!(log.capacity(), 2);
        log.clear();
        assert!(log.is_empty());
    }
}
