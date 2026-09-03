//! The authority lattice — what "narrower" means, written down once (REQ-CAP-007, ADR-048).
//!
//! Delegation is the only way authority spreads, and it is legal exactly when the child is
//! **equal or narrower** than its parent. Until this module existed that phrase had no definition:
//! [`crate::spine::CapEngine::delegate`] compared the two records field by field, inline, with
//! whatever predicate was at hand — and one of those predicates was the wrong relation entirely.
//!
//! # Two relations, not one
//!
//! An action pattern denotes a SET of concrete actions, its *reach*:
//!
//! ```text
//! reach("*")     = every action
//! reach("a.*")   = { "a" } ∪ { "a.<anything>" }
//! reach("a")     = { "a" }
//! ```
//!
//! Two different questions are asked of patterns, and they are not the same question:
//!
//! * **covering** — `action_covers(pattern, action)`: is this concrete action inside the pattern's
//!   reach? This is the *authorization* test, asked at [`crate::spine::CapEngine::evaluate`].
//! * **attenuation** — `action_attenuates(parent, child)`: is the child pattern's reach a SUBSET of
//!   the parent pattern's reach? This is the *delegation* test.
//!
//! `delegate` used to ask the first question with the child's pattern in the action slot, which is
//! a category error that reads as harmless because it agrees with the second on every pattern that
//! contains no `*` other than a trailing one. It disagrees, and amplifies, as soon as one appears
//! anywhere else:
//!
//! ```text
//! parent  "q.*.*"    reach = { "q.*" } ∪ { "q.*.<anything>" }
//! child   "q.*"      reach = { "q" }   ∪ { "q.<anything>" }
//!
//! action_covers("q.*.*", "q.*")  ==  true    // the child STRING is inside the parent's reach
//! action_attenuates("q.*.*", "q.*") == false // the child's REACH is not
//! ```
//!
//! With the old test that delegation was accepted, and the child then authorized `q.delete`, which
//! its parent could never authorize. `action_attenuates` is the relation `delegate` needs; the
//! amplification is refused. Both relations live here so the pair can be proved against each other
//! (`kernel-core/tests/capalg.rs`) rather than diverging in two call sites.
//!
//! # Why it is a lattice and not a pile of comparisons
//!
//! Each of the three dimensions — action, scope, constraints — carries a partial order whose top is
//! "most authority". [`attenuates`] is their conjunction, and the properties the delegation graph
//! depends on are properties OF that order:
//!
//! * **reflexive** — `attenuates(x, x)`; delegating an exact copy is always legal, so a subject can
//!   hand on what it holds without laundering it through a wider intermediate.
//! * **transitive** — a chain of legal delegations is itself a legal delegation, which is what makes
//!   "no descendant exceeds its root" a consequence of the per-step check rather than a separate
//!   audit the engine would have to perform on every evaluate.
//! * **sound** — the property the other two exist to serve: if `attenuates(p, c)` then every request
//!   `c` authorizes, `p` would have authorized too. Delegation can never manufacture reach.
//!
//! Soundness is the one that matters and the one that is exhaustively proved: the host suite sweeps
//! the whole finite scope lattice and a generated action-pattern universe and asserts the
//! implication directly, against `evaluate`'s own covering functions, so the order cannot drift away
//! from the authorization test it is supposed to bound.
//!
//! # Deliberately incomplete, never unsound
//!
//! [`scope_attenuates`] refuses `Type(T) → Entities([…])` even though a set of entities of type `T`
//! IS narrower than "everything of type `T`". The engine cannot tell: a [`Target`] carries an id and
//! an etype independently, and `Entities([5])` authorizes `{id: 5, etype: anything}` — including
//! types the parent never reached. Deciding the case needs a store lookup the capability engine does
//! not have and must not acquire (an authority check that reads the store is an authority check that
//! can be starved). So the answer is no, and the cost is a delegation that must be re-minted from a
//! wider root rather than one that is silently over-broad.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::spine::{Constraints, EntityType, Scope, Target};

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Is `action` inside `pattern`'s reach? The **authorization** test.
///
/// `*` reaches everything. A trailing `.*` reaches its prefix and everything below it. Any other
/// pattern is literal — `*` is not a general glob, and nothing here treats it as one.
pub fn action_covers(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        // Compared in place (ADR-089): the old `format!("{}.", prefix)` allocated a `String` on
        // EVERY wildcard capability test, and the console tests one per command a human types.
        return action == prefix
            || (action.len() > prefix.len()
                && action.as_bytes()[prefix.len()] == b'.'
                && action.starts_with(prefix));
    }
    pattern == action
}

/// Is `child`'s reach a subset of `parent`'s? The **delegation** test.
///
/// Not the same relation as [`action_covers`] and not implementable in terms of it — see the module
/// docs for the pattern pair where the two disagree and the older code amplified.
pub fn action_attenuates(parent: &str, child: &str) -> bool {
    if parent == "*" {
        return true;
    }
    // A child of `*` reaches everything; only a parent of `*` (handled above) can cover that.
    if child == "*" {
        return false;
    }
    match (parent.strip_suffix(".*"), child.strip_suffix(".*")) {
        // Both wildcards: the child's prefix must sit at or below the parent's, so that every
        // action the child's subtree reaches is in the parent's subtree.
        (Some(p), Some(c)) => c == p || c.starts_with(&format!("{}.", p)),
        // Concrete child under a wildcard parent: exactly the covering question, legitimately.
        (Some(p), None) => child == p || child.starts_with(&format!("{}.", p)),
        // A wildcard child reaches at least two actions; a concrete parent reaches one.
        (None, Some(_)) => false,
        (None, None) => parent == child,
    }
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

/// Does `scope` reach `target`? The **authorization** test.
///
/// [`Scope::Entities`] matches on id alone and [`Scope::Type`] on etype alone — the two axes are
/// independent, which is exactly why [`scope_attenuates`] cannot relate them.
pub fn scope_covers(scope: &Scope, target: &Target) -> bool {
    match scope {
        Scope::All => true,
        Scope::None => false,
        Scope::Type(t) => target.etype.map(|e| e == *t).unwrap_or(false),
        Scope::Entities(set) => target.id.map(|id| set.contains(&id)).unwrap_or(false),
    }
}

/// Is `child`'s reach a subset of `parent`'s? The **delegation** test.
///
/// Conservative where the model cannot decide (see the module docs), never permissive.
pub fn scope_attenuates(parent: &Scope, child: &Scope) -> bool {
    // An empty entity set reaches nothing; it is `None` spelled differently, and a relation that
    // did not know that would refuse a delegation to the emptiest possible scope.
    if scope_is_empty(child) {
        return true;
    }
    if scope_is_empty(parent) {
        return false;
    }
    match (parent, child) {
        (Scope::All, _) => true,
        (_, Scope::All) => false,
        (Scope::Type(a), Scope::Type(b)) => a == b,
        (Scope::Entities(p), Scope::Entities(c)) => c.iter().all(|x| p.contains(x)),
        // Type ↔ Entities in either direction: undecidable without the store, so refused.
        _ => false,
    }
}

/// Reaches nothing: [`Scope::None`], or an [`Scope::Entities`] set with no members.
pub fn scope_is_empty(scope: &Scope) -> bool {
    match scope {
        Scope::None => true,
        Scope::Entities(s) => s.is_empty(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Constraints
// ---------------------------------------------------------------------------

/// Are `child`'s constraints equal or tighter than `parent`'s? The **delegation** test.
///
/// Every field is a restriction, so "tighter" means: expires no later (and a parent that expires
/// cannot bear a child that never does), never escapes `local_only`, never drops an approval
/// requirement.
pub fn constraints_attenuate(parent: &Constraints, child: &Constraints) -> bool {
    let expiry_ok = match (parent.expires_at, child.expires_at) {
        (Some(p), Some(c)) => c <= p,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let local_ok = !parent.local_only || child.local_only;
    let approval_ok = !parent.approval_required || child.approval_required;
    expiry_ok && local_ok && approval_ok
}

// ---------------------------------------------------------------------------
// The conjunction
// ---------------------------------------------------------------------------

/// The three dimensions of a capability's authority, as the lattice sees them. A view, not a
/// record: [`crate::spine`] owns the stored form and [`crate::capstore`] owns the persisted one, and
/// both hand this to [`attenuates`] so the one relation governs minting-time delegation and
/// load-time admission alike.
pub struct Authority<'a> {
    pub action: &'a str,
    pub scope: &'a Scope,
    pub constraints: &'a Constraints,
}

/// Is `child` equal or narrower than `parent` in every dimension? **The** delegation rule.
///
/// One implementation, two callers: [`crate::spine::CapEngine::delegate`] applies it when a
/// capability is created, and [`crate::capstore::load`] re-applies it to every parent/child edge in
/// a persisted registry — because a store that survives a reboot is an input, and an input that
/// widens a child is a privilege escalation that never had to pass `delegate` at all.
pub fn attenuates(parent: &Authority<'_>, child: &Authority<'_>) -> bool {
    action_attenuates(parent.action, child.action)
        && scope_attenuates(parent.scope, child.scope)
        && constraints_attenuate(parent.constraints, child.constraints)
}

/// Why a delegation was refused, in the same words the engine reports. Returning the FIRST failing
/// dimension rather than a bool keeps the caller's error message and this module's rule in one
/// place; a caller that re-derived the reason could report a dimension that in fact passed.
pub fn refusal(parent: &Authority<'_>, child: &Authority<'_>) -> Option<&'static str> {
    if !action_attenuates(parent.action, child.action) {
        return Some("delegation would amplify action");
    }
    if !scope_attenuates(parent.scope, child.scope) {
        return Some("delegation would amplify scope");
    }
    if !constraints_attenuate(parent.constraints, child.constraints) {
        return Some("delegation would loosen constraints");
    }
    None
}

// ---------------------------------------------------------------------------
// The universe the proofs sweep
// ---------------------------------------------------------------------------

/// Every [`Scope`] shape the model can express, at a fixed small entity alphabet — the scope lattice
/// is finite, so its properties are proved by EXHAUSTION rather than sampled. Lives here beside the
/// relation it exercises so a new `Scope` variant makes the sweep incomplete in one obvious place
/// instead of silently narrowing every proof that uses it.
pub fn scope_universe() -> Vec<Scope> {
    let ids: [u64; 3] = [1, 2, 3];
    let mut v = alloc::vec![Scope::All, Scope::None, Scope::Entities(Vec::new())];
    for t in [
        EntityType::Document,
        EntityType::Summary,
        EntityType::Agent,
        EntityType::Capability,
        EntityType::Event,
    ] {
        v.push(Scope::Type(t));
    }
    // Every subset of the alphabet, so subset/superset/disjoint/overlap are all present.
    for mask in 1u8..(1 << ids.len()) {
        let mut set = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            if mask & (1 << i) != 0 {
                set.push(*id);
            }
        }
        v.push(Scope::Entities(set));
    }
    v
}

/// Every [`Target`] the scope universe can be asked about, including the two half-specified forms
/// (`id` without `etype` and the reverse) that a real request can present.
pub fn target_universe() -> Vec<Target> {
    let mut v = alloc::vec![Target::default()];
    for id in [None, Some(1u64), Some(2), Some(3), Some(99)] {
        for etype in [
            None,
            Some(EntityType::Document),
            Some(EntityType::Summary),
            Some(EntityType::Agent),
            Some(EntityType::Capability),
            Some(EntityType::Event),
        ] {
            if id.is_none() && etype.is_none() {
                continue;
            }
            v.push(Target { id, etype });
        }
    }
    v
}

/// A pattern universe that deliberately includes the shapes the old relation got wrong: bare `*`,
/// a wildcard whose prefix itself ends in `*`, and concrete actions that a naive `starts_with`
/// would confuse with a sibling prefix (`entity` vs `entityx`).
pub fn action_universe() -> Vec<String> {
    [
        "*",
        "entity",
        "entityx",
        "entity.*",
        "entity.derive",
        "entity.derive.*",
        "entity.derive.summary",
        "entity.delete",
        "entity.*.*",
        "entity.*.x",
        "ipc",
        "ipc.*",
        "ipc.send",
    ]
    .iter()
    .map(|s| String::from(*s))
    .collect()
}

/// Every concrete action a pattern in [`action_universe`] could be asked to authorize. Patterns are
/// included as literals on purpose — a pattern string IS a legal action string, and the
/// covering/attenuation pair has to stay sound over that overlap rather than over a tidier alphabet.
pub fn concrete_action_universe() -> Vec<String> {
    let mut v = action_universe();
    for extra in [
        "entity.derive.summary.short",
        "entity.rename",
        "entity.*.x.y",
        "other",
        "other.thing",
    ] {
        v.push(String::from(extra));
    }
    v
}
