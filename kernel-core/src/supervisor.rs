//! Kill the task, keep the system (REQ-REL-002, ADR-042).
//!
//! `faultclass` decides that a fault is the *task's* fault and returns [`FaultVerdict::KillTask`] — and
//! until now that verdict had nowhere to go. Every target's handler ended the boot, because nothing could
//! remove one task and let the rest continue. `docs/MATURITY.md` lists this first among the things
//! production would additionally require: it is the difference between "the kernel detects a bad access"
//! and "the kernel survives one".
//!
//! This module is the policy and the bookkeeping, deliberately separate from any target's trap assembly:
//!
//! * **[`Supervisor::on_fault`] turns a verdict into an action.** A user fault terminates the task; a
//!   kernel fault, a corrupt translation or an unknown report escalates — the kernel cannot sensibly "kill
//!   a task" for its own bad access.
//! * **A terminated task is terminated forever.** [`Supervisor::may_run`] is the question a scheduler must
//!   ask before dispatch, and it never says yes again. Termination is idempotent and records the *reason*,
//!   so a boot log can say why a task died rather than only that it did.
//! * **Terminating one task never touches another.** That is the whole claim: containment. The counters
//!   make it auditable, and an escalated fault is counted separately from a contained one — a system that
//!   quietly turned kernel bugs into task deaths would look healthier than it is.
//!
//! Not claimed: this does not free the dead task's memory — that is `teardown::destroy_address_space`
//! (ADR-032), which the caller invokes with the task's root — and it does not restart anything, because a
//! restart policy needs a supervision tree (REQ-REL-001), not a flag.
use crate::faultclass::{FaultKind, FaultVerdict};
use crate::sched::TaskId;
use alloc::vec::Vec;

/// Why a task was terminated. A reason rather than a bool, so a log or an audit can say what happened
/// without re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationReason {
    /// A fault the classifier attributed to the task (a bad access from user privilege).
    Fault(FaultKind),
    /// The task asked to exit.
    Exited,
    /// Policy terminated it (a quota, a supervisor decision).
    Policy,
}

/// What the kernel should do about a fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    /// The task is gone; the system continues. The caller abandons its context and reschedules.
    TaskTerminated(TerminationReason),
    /// Not survivable: the kernel itself, or its memory model, is untrustworthy. Stop.
    Escalate(FaultKind),
}

/// Tracks which tasks are dead, and why.
pub struct Supervisor {
    dead: Vec<(TaskId, TerminationReason)>,
    escalations: usize,
}

impl Supervisor {
    pub const fn new() -> Self {
        Supervisor {
            dead: Vec::new(),
            escalations: 0,
        }
    }

    /// Decide what a fault means for `task`, and record the outcome.
    ///
    /// A [`FaultVerdict::KillTask`] with no task to blame is NOT survivable: something faulted at user
    /// privilege while the kernel believed nothing was running, which is a kernel bug wearing a user
    /// fault's clothes.
    pub fn on_fault(
        &mut self,
        task: Option<TaskId>,
        kind: FaultKind,
        verdict: FaultVerdict,
    ) -> SupervisorAction {
        match (verdict, task) {
            (FaultVerdict::KillTask, Some(id)) => {
                let reason = TerminationReason::Fault(kind);
                self.terminate(id, reason);
                SupervisorAction::TaskTerminated(reason)
            }
            (FaultVerdict::KillTask, None) | (FaultVerdict::Panic, _) => {
                self.escalations += 1;
                SupervisorAction::Escalate(kind)
            }
        }
    }

    /// Terminate `task` for `reason`. Idempotent, and the FIRST reason is kept — that is the one that
    /// explains the death; a later policy sweep must not overwrite the fault that actually killed it.
    pub fn terminate(&mut self, task: TaskId, reason: TerminationReason) {
        if self.reason(task).is_none() {
            self.dead.push((task, reason));
        }
    }

    /// The question a scheduler must ask before dispatching. Never says yes about a dead task again.
    pub fn may_run(&self, task: TaskId) -> bool {
        self.reason(task).is_none()
    }

    /// Why `task` died, if it did.
    pub fn reason(&self, task: TaskId) -> Option<TerminationReason> {
        self.dead.iter().find(|(t, _)| *t == task).map(|(_, r)| *r)
    }

    /// How many tasks have been terminated.
    pub fn terminated(&self) -> usize {
        self.dead.len()
    }

    /// How many faults were escalated instead of contained. Separate on purpose: see the module docs.
    pub fn escalations(&self) -> usize {
        self.escalations
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faultclass::{classify, from_x86_error_code, verdict};

    fn user_write_fault() -> (FaultKind, FaultVerdict) {
        let f = from_x86_error_code(0b111); // present + write + user
        let k = classify(&f);
        (k, verdict(k))
    }

    #[test]
    fn a_user_fault_terminates_only_that_task_and_the_rest_still_run() {
        let mut s = Supervisor::new();
        let (kind, v) = user_write_fault();
        assert_eq!(
            s.on_fault(Some(TaskId(7)), kind, v),
            SupervisorAction::TaskTerminated(TerminationReason::Fault(kind))
        );
        assert!(!s.may_run(TaskId(7)));
        for other in [1u64, 2, 6, 8, 99] {
            assert!(
                s.may_run(TaskId(other)),
                "task {other} lost the right to run"
            );
        }
        assert_eq!(s.terminated(), 1);
        assert_eq!(s.escalations(), 0);
    }

    #[test]
    fn a_kernel_fault_escalates_and_kills_no_task() {
        let mut s = Supervisor::new();
        let f = from_x86_error_code(0b011); // present + write, NOT user ⇒ kernel
        let k = classify(&f);
        assert_eq!(
            s.on_fault(Some(TaskId(1)), k, verdict(k)),
            SupervisorAction::Escalate(k)
        );
        assert!(s.may_run(TaskId(1)), "a kernel fault must not kill a task");
        assert_eq!(s.terminated(), 0);
        assert_eq!(s.escalations(), 1);
    }

    #[test]
    fn a_user_verdict_with_no_task_escalates_rather_than_pretending() {
        let mut s = Supervisor::new();
        let (kind, v) = user_write_fault();
        assert_eq!(s.on_fault(None, kind, v), SupervisorAction::Escalate(kind));
        assert_eq!(s.terminated(), 0);
        assert_eq!(s.escalations(), 1);
    }

    #[test]
    fn termination_is_idempotent_and_keeps_the_first_reason() {
        let mut s = Supervisor::new();
        let (kind, _) = user_write_fault();
        s.terminate(TaskId(3), TerminationReason::Fault(kind));
        s.terminate(TaskId(3), TerminationReason::Policy);
        assert_eq!(s.reason(TaskId(3)), Some(TerminationReason::Fault(kind)));
        assert_eq!(
            s.terminated(),
            1,
            "a repeat termination created a second record"
        );
    }
}
