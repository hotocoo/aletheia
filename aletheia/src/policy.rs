//! Policy & approval engine (ADR-015; SAD §10 `approve()`).
//!
//! SEPARATION OF CONCERNS — the discriminating design decision of Aletheia's authority model:
//!   * The **capability engine** (`capabilities`) answers *authority*: is this subject permitted to
//!     perform this action on this target at all? → `Allow | Deny | RequireApproval`.
//!   * The **policy engine** (this module) answers *governance*: even when an action is authorized,
//!     must a human approve it before it takes effect?
//!
//! The two axes are independent and neither can be bypassed by the other. A subject with full
//! authority over an entity still cannot delete it without approval (destructive risk). A subject
//! whose capability carries an approval-required constraint needs approval even for a safe read.
//! The AI never participates in either decision — it only proposes the plan that these engines then
//! judge (PRD-002 §21, INV-014).
use crate::capabilities::Decision;
use crate::domain::{now, Id};
use crate::intent_action::Intent;
use crate::tools::Risk;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Default lifetime of a pending approval before it expires (24h). An expired approval can no
/// longer be granted — the requester must re-submit the intent.
pub const APPROVAL_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The policy decision for a single authorized action. Deterministic and inspectable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalVerdict {
    NotRequired,
    Required { reason: String },
}

/// Governs which authorized actions require human approval. Stateless and pure: the same inputs
/// always yield the same verdict, so the decision is fully explainable and testable.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    /// Destructive-risk operations always require approval regardless of authority.
    approve_destructive: bool,
}

impl PolicyEngine {
    pub fn new() -> Self {
        PolicyEngine { approve_destructive: true }
    }

    /// Decide whether an authorized action needs human approval. Preserves BOTH historical
    /// triggers, now unified in one place: a destructive-risk operation, OR a capability whose
    /// constraint demands approval. `cap_decision` is the capability engine's authority answer;
    /// this function never overrides a `Deny` (callers stop before reaching policy on a deny).
    pub fn evaluate(&self, cap_decision: &Decision, risk: Risk) -> ApprovalVerdict {
        if self.approve_destructive && risk == Risk::Destructive {
            return ApprovalVerdict::Required { reason: "destructive operation".into() };
        }
        if matches!(cap_decision, Decision::RequireApproval) {
            return ApprovalVerdict::Required { reason: "capability constrained: approval required".into() };
        }
        ApprovalVerdict::NotRequired
    }
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalState {
    Pending,
    Granted,
    Denied,
    Expired,
}

/// A request for human approval, bound to the EXACT action it will authorize (SAD §10: approvals
/// are "bound to exact action/scope/expiry"). Granting this record authorizes only this intent —
/// approval confers no capability; the subject's capabilities are still re-evaluated on execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub id: Id,
    pub subject: String,
    pub intent: Intent,
    pub reason: String,
    pub requested_at: u64,
    pub expires_at: u64,
    pub state: ApprovalState,
}

impl PendingApproval {
    pub fn new(subject: &str, intent: Intent, reason: &str) -> Self {
        let at = now();
        PendingApproval {
            id: crate::domain::new_id(),
            subject: subject.to_string(),
            intent,
            reason: reason.to_string(),
            requested_at: at,
            expires_at: at + APPROVAL_TTL_MS,
            state: ApprovalState::Pending,
        }
    }
    pub fn is_expired(&self, at: u64) -> bool {
        at > self.expires_at
    }
}

/// In-memory registry of approvals. The durable source of truth is the store's immutable event log
/// (`ApprovalRequested` / `ApprovalResolved`), which `SysCore` replays into a fresh registry on
/// open — so pending approvals survive restart without the storage layer knowing about policy.
#[derive(Debug, Default)]
pub struct ApprovalStore {
    approvals: HashMap<Id, PendingApproval>,
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, a: PendingApproval) {
        self.approvals.insert(a.id.clone(), a);
    }

    pub fn get(&self, id: &Id) -> Option<&PendingApproval> {
        self.approvals.get(id)
    }

    /// All approvals still awaiting a human decision (freshest first), skipping ones that have
    /// passed their expiry.
    pub fn list_pending(&self, at: u64) -> Vec<PendingApproval> {
        let mut v: Vec<PendingApproval> = self
            .approvals
            .values()
            .filter(|a| a.state == ApprovalState::Pending && !a.is_expired(at))
            .cloned()
            .collect();
        v.sort_by(|a, b| b.requested_at.cmp(&a.requested_at));
        v
    }

    /// Transition an approval's state. Idempotent per record — an already-resolved approval is not
    /// re-transitioned. Returns true only when a `Pending` record actually changed state.
    pub fn mark_state(&mut self, id: &Id, state: ApprovalState) -> bool {
        match self.approvals.get_mut(id) {
            Some(a) if a.state == ApprovalState::Pending => {
                a.state = state;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::Decision;
    use crate::intent_action::{Intent, Verb};
    use crate::tools::Risk;

    #[test]
    fn destructive_always_requires_approval_even_when_fully_authorized() {
        let p = PolicyEngine::new();
        // Authority says ALLOW, but policy still demands approval for a destructive op.
        assert_eq!(
            p.evaluate(&Decision::Allow, Risk::Destructive),
            ApprovalVerdict::Required { reason: "destructive operation".into() }
        );
    }

    #[test]
    fn safe_op_with_approval_constrained_capability_requires_approval() {
        let p = PolicyEngine::new();
        assert!(matches!(
            p.evaluate(&Decision::RequireApproval, Risk::Safe),
            ApprovalVerdict::Required { .. }
        ));
    }

    #[test]
    fn safe_op_with_full_authority_needs_no_approval() {
        let p = PolicyEngine::new();
        assert_eq!(p.evaluate(&Decision::Allow, Risk::Safe), ApprovalVerdict::NotRequired);
    }

    #[test]
    fn approval_store_lists_and_resolves() {
        let mut store = ApprovalStore::new();
        let pa = PendingApproval::new(
            "human:owner",
            Intent { subject: "human:owner".into(), verb: Verb::Delete { id: "e1".into() } },
            "destructive operation",
        );
        let id = pa.id.clone();
        store.insert(pa);
        assert_eq!(store.list_pending(now()).len(), 1);
        assert!(store.mark_state(&id, ApprovalState::Granted));
        assert_eq!(store.list_pending(now()).len(), 0);
        // Idempotent: cannot re-resolve.
        assert!(!store.mark_state(&id, ApprovalState::Denied));
    }
}
