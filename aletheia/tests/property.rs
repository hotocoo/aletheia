//! Property-based tests for the capability engine (PRD-003 §38.3).
//!
//! The validation pyramid says unit/integration tests alone are not enough: they only prove
//! the scenarios someone thought to write. These properties assert the capability engine's
//! security invariants over *randomized* inputs, so they hold for inputs no one enumerated.
//! This is the "property-based" layer made real in code, not merely documented.
use aletheia::capabilities::{action_covers, CapEngine, Constraints, Decision, Scope, Target};
use proptest::prelude::*;

/// Dotted lowercase action tokens, e.g. "entity", "entity.read", "a.b.c".
fn action_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec("[a-z]{1,6}", 1..4).prop_map(|segs| segs.join("."))
}

proptest! {
    /// `action_covers` is reflexive: a pattern always covers itself.
    #[test]
    fn action_covers_is_reflexive(a in action_strategy()) {
        prop_assert!(action_covers(&a, &a));
    }

    /// The wildcard pattern covers every action.
    #[test]
    fn wildcard_covers_everything(a in action_strategy()) {
        prop_assert!(action_covers("*", &a));
    }

    /// Fail closed: with no capability offered, no action on any target is ever authorized.
    #[test]
    fn fail_closed_without_capability(a in action_strategy()) {
        let engine = CapEngine::new();
        let decision = engine.evaluate(&a, &Target::default(), &[]);
        prop_assert!(!matches!(decision, Decision::Allow));
    }

    /// A minted root capability authorizes exactly its action and denies any action it does
    /// not cover — authority never leaks to uncovered actions.
    #[test]
    fn mint_authorizes_exact_and_denies_uncovered(a in action_strategy(), b in action_strategy()) {
        let mut engine = CapEngine::new();
        let cap = engine.mint("subject", &a, Scope::All, Constraints::none(), "owner");
        let tokens = vec![cap.token];
        prop_assert_eq!(engine.evaluate(&a, &Target::default(), &tokens), Decision::Allow);
        if !action_covers(&a, &b) {
            prop_assert!(!matches!(engine.evaluate(&b, &Target::default(), &tokens), Decision::Allow));
        }
    }

    /// Delegation never amplifies authority: if a delegation succeeds, the parent's action
    /// must cover the child's action (no privilege escalation via delegation).
    #[test]
    fn delegation_never_amplifies_action(parent in action_strategy(), child in action_strategy()) {
        let mut engine = CapEngine::new();
        let root = engine.mint("subject", &parent, Scope::All, Constraints::none(), "owner");
        let delegated = engine.delegate(&root.token, "agent", &child, Scope::All, Constraints::none(), "subject");
        if delegated.is_ok() {
            prop_assert!(action_covers(&parent, &child));
        }
    }

    /// A revoked capability never authorizes anything afterwards.
    #[test]
    fn revoked_capability_never_authorizes(a in action_strategy()) {
        let mut engine = CapEngine::new();
        let cap = engine.mint("subject", &a, Scope::All, Constraints::none(), "owner");
        engine.revoke(&cap.token);
        prop_assert!(!matches!(engine.evaluate(&a, &Target::default(), &[cap.token]), Decision::Allow));
    }
}
