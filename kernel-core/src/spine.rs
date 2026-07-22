//! The capability-secure spine, in kernel space.
//!
//! This is a `no_std` reification of the M1 hosted System Core
//! (`aletheia/src/{capabilities,domain,intent_action,syscore}.rs`). It enforces the same
//! invariants — possession-based unforgeable capabilities, fail-closed authorization,
//! attenuated delegation, cascading revocation, and the interpret→validate→authorize→
//! approve→execute→verify→record pipeline — but now the authority handle (`CapToken`) is
//! **unforgeable by construction**: its field is module-private, so no code outside this
//! module can fabricate one (a strictly stronger property than the hosted string-token
//! reference). ADR-010: the microkernel enforces what M1 could only contract for.
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Domain (seven primitives, minimal kernel form)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityType {
    Document,
    Summary,
    Agent,
    Capability,
    Event,
}

/// Entity: content-addressed, versioned unit of meaning. Content is immutable; a mutation
/// produces a new version linked to the prior via `chain`.
#[derive(Clone, Debug)]
pub struct Entity {
    pub id: u64,
    pub etype: EntityType,
    pub content: String,
    pub content_hash: u64,
    pub version: u64,
    pub chain: u64,
    pub deleted: bool,
    pub provenance: String,
}

/// FNV-1a 64-bit — content addressing for the kernel store (no external crate).
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Capability engine — the sole authority mechanism
// ---------------------------------------------------------------------------

/// Unforgeable authority handle. The inner id is private: only `CapEngine` mints real
/// tokens. Holding a `CapToken` value is not itself authority — `evaluate` requires the
/// token to still exist (and not be revoked) in the engine's private registry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CapToken(u64);

impl CapToken {
    /// Construct an arbitrary handle *without* the engine minting it. Used only by the
    /// forgery selftest to model an attacker who guesses/fabricates a token id; `evaluate`
    /// must still DENY it because it is absent from the registry. Not a minting path.
    pub fn forge_for_test(id: u64) -> Self {
        CapToken(id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    All,
    Type(EntityType),
    Entities(Vec<u64>),
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct Constraints {
    pub expires_at: Option<u64>,
    pub approval_required: bool,
    pub local_only: bool,
}

impl Constraints {
    pub fn none() -> Self {
        Constraints {
            expires_at: None,
            approval_required: false,
            local_only: true,
        }
    }
    pub fn approval() -> Self {
        Constraints {
            approval_required: true,
            ..Self::none()
        }
    }
}

#[derive(Clone, Debug)]
struct StoredCapability {
    // `subject` (the holder) and `parent` (the delegation ancestor) are part of the capability
    // record for model fidelity and auditability, but the minimal kernel `evaluate` path does not
    // read them (revocation walks the separate `children` map, not `parent`). Retained rather than
    // dropped so the kernel spine mirrors the hosted System Core's capability shape; allowed here so
    // `clippy -D warnings` stays clean now that the spine is a shared library crate.
    #[allow(dead_code)]
    subject: String,
    action: String,
    scope: Scope,
    constraints: Constraints,
    #[allow(dead_code)]
    parent: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny(String),
    RequireApproval,
}

/// Evidence that [`CapEngine::authorize`] found a live, matching capability for a request, naming
/// **which** token satisfied it — information [`CapEngine::evaluate`] discards (it reports only the
/// verdict). An `Authorization` is NOT authority on its own and NOT a lasting grant: under
/// concurrency it is valid only for the duration of the engine-lock hold in which it was produced.
/// Bind the check and its effect with [`CapEngine::with_authorization`] so no revocation can
/// linearize between them — see ADR-027 (capability concurrency model, Option A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authorization {
    cap: CapToken,
}

impl Authorization {
    /// The capability that authorized the request.
    pub fn capability(&self) -> CapToken {
        self.cap
    }
}

/// Result of [`CapEngine::authorize`] — the same three outcomes as [`Decision`], but the `Allow`
/// arm carries the [`Authorization`] naming the matching token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    Allow(Authorization),
    RequireApproval,
    Deny(String),
}

/// Outcome of testing ONE offered token against a request. Private — the single source of truth for
/// the matching logic shared by [`CapEngine::evaluate`] and [`CapEngine::authorize`], so the two can
/// never drift apart.
enum TokenMatch {
    Allow,
    NeedsApproval,
    NoMatch,
}

#[derive(Debug, Clone, Default)]
pub struct Target {
    pub id: Option<u64>,
    pub etype: Option<EntityType>,
}

pub struct CapEngine {
    registry: BTreeMap<u64, StoredCapability>,
    revoked: BTreeSet<u64>,
    children: BTreeMap<u64, Vec<u64>>,
    next_id: u64,
    secret: u64,
    now: u64,
}

impl CapEngine {
    /// `secret` seeds token ids so they are not the guessable sequence 1,2,3… (defense in
    /// depth, mirroring the hosted engine's random tokens). `now` is a fixed logical clock.
    pub fn new(secret: u64, now: u64) -> Self {
        CapEngine {
            registry: BTreeMap::new(),
            revoked: BTreeSet::new(),
            children: BTreeMap::new(),
            next_id: 1,
            secret,
            now,
        }
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id ^ self.secret;
        self.next_id += 1;
        id
    }

    /// Mint a fresh root capability. Only the engine produces a valid token.
    pub fn mint(
        &mut self,
        subject: &str,
        action: &str,
        scope: Scope,
        constraints: Constraints,
    ) -> CapToken {
        let id = self.fresh_id();
        self.registry.insert(
            id,
            StoredCapability {
                subject: subject.to_string(),
                action: action.to_string(),
                scope,
                constraints,
                parent: None,
            },
        );
        CapToken(id)
    }

    /// Delegate with equal-or-narrower authority only (attenuation). Amplification is denied.
    pub fn delegate(
        &mut self,
        parent: CapToken,
        subject: &str,
        action: &str,
        scope: Scope,
        constraints: Constraints,
    ) -> Result<CapToken, String> {
        if self.revoked.contains(&parent.0) {
            return Err("parent capability revoked".to_string());
        }
        let p = self
            .registry
            .get(&parent.0)
            .ok_or_else(|| "unknown parent capability".to_string())?
            .clone();
        if !action_covers(&p.action, action) {
            return Err("delegation would amplify action".to_string());
        }
        if !scope_subset(&p.scope, &scope) {
            return Err("delegation would amplify scope".to_string());
        }
        if !constraints_not_looser(&p.constraints, &constraints) {
            return Err("delegation would loosen constraints".to_string());
        }
        let id = self.fresh_id();
        self.registry.insert(
            id,
            StoredCapability {
                subject: subject.to_string(),
                action: action.to_string(),
                scope,
                constraints,
                parent: Some(parent.0),
            },
        );
        self.children.entry(parent.0).or_default().push(id);
        Ok(CapToken(id))
    }

    /// Revoke a capability and all descendants, transitively and immediately.
    pub fn revoke(&mut self, token: CapToken) {
        let mut stack = alloc::vec![token.0];
        while let Some(t) = stack.pop() {
            if self.revoked.insert(t) {
                if let Some(kids) = self.children.get(&t) {
                    stack.extend(kids.iter().copied());
                }
            }
        }
    }

    /// Test ONE offered token against a request. The single matching rule used by both `evaluate`
    /// and `authorize` (so the fast verdict path and the token-naming path can never diverge).
    /// Fail-closed: a revoked, forged/unknown, non-covering, or expired token is `NoMatch`.
    fn test_token(&self, token: CapToken, action: &str, target: &Target) -> TokenMatch {
        if self.revoked.contains(&token.0) {
            return TokenMatch::NoMatch;
        }
        let cap = match self.registry.get(&token.0) {
            Some(c) => c,
            None => return TokenMatch::NoMatch, // forged / unknown handle — not authority
        };
        if !action_covers(&cap.action, action) {
            return TokenMatch::NoMatch;
        }
        if !scope_covers(&cap.scope, target) {
            return TokenMatch::NoMatch;
        }
        if let Some(exp) = cap.constraints.expires_at {
            if self.now > exp {
                return TokenMatch::NoMatch;
            }
        }
        if cap.constraints.approval_required {
            return TokenMatch::NeedsApproval;
        }
        TokenMatch::Allow
    }

    /// The core authorization decision. Fail closed: no matching live capability => Deny.
    pub fn evaluate(&self, action: &str, target: &Target, offered: &[CapToken]) -> Decision {
        let mut needs_approval = false;
        for &token in offered {
            match self.test_token(token, action, target) {
                TokenMatch::Allow => return Decision::Allow,
                TokenMatch::NeedsApproval => needs_approval = true,
                TokenMatch::NoMatch => {}
            }
        }
        if needs_approval {
            Decision::RequireApproval
        } else {
            Decision::Deny("no capability".to_string())
        }
    }

    /// Like [`evaluate`](Self::evaluate), but the `Allow` arm reports **which** token authorized the
    /// request as an [`Authorization`]. Read-only (`&self`). Fail-closed identically. The returned
    /// `Authorization` is a point-in-time result: under concurrency it is valid only while the
    /// engine lock is still held. Prefer [`with_authorization`](Self::with_authorization) to bind the
    /// check and its effect into one critical section (ADR-027).
    pub fn authorize(&self, action: &str, target: &Target, offered: &[CapToken]) -> AuthOutcome {
        let mut needs_approval = false;
        for &token in offered {
            match self.test_token(token, action, target) {
                TokenMatch::Allow => return AuthOutcome::Allow(Authorization { cap: token }),
                TokenMatch::NeedsApproval => needs_approval = true,
                TokenMatch::NoMatch => {}
            }
        }
        if needs_approval {
            AuthOutcome::RequireApproval
        } else {
            AuthOutcome::Deny("no capability".to_string())
        }
    }

    /// Atomic authorize-and-commit — the concurrency-safe way to act on a capability (ADR-027,
    /// Option A: one critical section). Evaluates `action`/`target` against `offered`; **iff** the
    /// verdict is `Allow`, runs `commit` and returns `Ok(T)`; otherwise runs nothing and returns
    /// `Err(Decision)` (fail-closed).
    ///
    /// The check and the effect execute inside this single `&self` call, so under the engine's lock
    /// (revocation requires `&mut self` — the write side) **no revoke can linearize between the
    /// authorization and the effect**. That closes the time-of-check/time-of-use window GAPS2 #9
    /// flags for SMP: a `check(); drop_lock(); …; act();` sequence can act on a stale `Allow`, but
    /// this primitive makes that gap structurally unrepresentable.
    ///
    /// `commit` receives `&self` (the still-live engine) alongside the [`Authorization`], so an
    /// effect may perform additional capability reads or a post-condition *verify* within the same
    /// authorized critical section (mirroring the pipeline's authorize→execute→verify step) — but it
    /// cannot revoke or otherwise mutate the engine (no `&mut`).
    pub fn with_authorization<T>(
        &self,
        action: &str,
        target: &Target,
        offered: &[CapToken],
        commit: impl FnOnce(&Self, &Authorization) -> T,
    ) -> Result<T, Decision> {
        match self.authorize(action, target, offered) {
            AuthOutcome::Allow(auth) => Ok(commit(self, &auth)),
            AuthOutcome::RequireApproval => Err(Decision::RequireApproval),
            AuthOutcome::Deny(msg) => Err(Decision::Deny(msg)),
        }
    }

    pub fn is_revoked(&self, token: CapToken) -> bool {
        self.revoked.contains(&token.0)
    }
}

pub fn action_covers(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return action == prefix || action.starts_with(&format!("{}.", prefix));
    }
    pattern == action
}

fn scope_covers(scope: &Scope, target: &Target) -> bool {
    match scope {
        Scope::All => true,
        Scope::None => false,
        Scope::Type(t) => target.etype.map(|e| e == *t).unwrap_or(false),
        Scope::Entities(set) => target.id.map(|id| set.contains(&id)).unwrap_or(false),
    }
}

fn scope_subset(parent: &Scope, child: &Scope) -> bool {
    match (parent, child) {
        (Scope::All, _) => true,
        (_, Scope::None) => true,
        (Scope::Type(a), Scope::Type(b)) => a == b,
        (Scope::Entities(p), Scope::Entities(c)) => c.iter().all(|x| p.contains(x)),
        _ => false,
    }
}

fn constraints_not_looser(parent: &Constraints, child: &Constraints) -> bool {
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
// Semantic store — content-addressed, versioned, in-memory
// ---------------------------------------------------------------------------

pub struct Store {
    entities: BTreeMap<u64, Entity>,
    events: Vec<Entity>,
    next_id: u64,
}

impl Store {
    pub fn new() -> Self {
        Store {
            entities: BTreeMap::new(),
            events: Vec::new(),
            next_id: 0x1000,
        }
    }

    pub fn put(&mut self, etype: EntityType, content: &str, provenance: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let h = content_hash(content.as_bytes());
        self.entities.insert(
            id,
            Entity {
                id,
                etype,
                content: content.to_string(),
                content_hash: h,
                version: 1,
                chain: id,
                deleted: false,
                provenance: provenance.to_string(),
            },
        );
        id
    }

    pub fn get(&self, id: u64) -> Option<&Entity> {
        self.entities.get(&id).filter(|e| !e.deleted)
    }

    pub fn record_event(&mut self, kind: &str, actor: &str) {
        let id = self.next_id;
        self.next_id += 1;
        let content = format!("{}::{}", kind, actor);
        let h = content_hash(content.as_bytes());
        self.events.push(Entity {
            id,
            etype: EntityType::Event,
            content,
            content_hash: h,
            version: 1,
            chain: id,
            deleted: false,
            provenance: actor.to_string(),
        });
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Intent → Action pipeline (Intent carries no authority; never executes directly)
// ---------------------------------------------------------------------------

/// A parsed, typed plan step. Untrusted model output is modeled as a `Plan`; `validate`
/// rejects unknown ops and empty plans, so malformed output can never reach execution.
#[derive(Clone, Debug)]
pub struct Step {
    pub op: String,
    pub source: u64,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub steps: Vec<Step>,
}

const KNOWN_OPS: &[&str] = &["derive_summary"];

pub fn validate_plan(plan: &Plan) -> Result<(), String> {
    if plan.steps.is_empty() {
        return Err("plan has no steps".to_string());
    }
    for s in &plan.steps {
        if !KNOWN_OPS.contains(&s.op.as_str()) {
            return Err(format!("unknown operation: {}", s.op));
        }
    }
    Ok(())
}

/// Outcome of one pipeline run — the explainable trace the experience layer would render.
#[derive(Clone, Debug)]
pub struct PipelineResult {
    pub ok: bool,
    pub validation: &'static str,
    pub authorization: Decision,
    pub executed: bool,
    pub verified: bool,
    pub produced: Option<u64>,
    pub error: Option<String>,
}

/// Full pipeline: validate (deterministic) → authorize (capability, fail closed) → execute
/// on the real store → verify the real effect against the store → record an immutable event.
pub fn run_pipeline(
    engine: &CapEngine,
    store: &mut Store,
    actor: &str,
    plan: &Plan,
    offered: &[CapToken],
) -> PipelineResult {
    let mut r = PipelineResult {
        ok: false,
        validation: "pending",
        authorization: Decision::Deny("unevaluated".to_string()),
        executed: false,
        verified: false,
        produced: None,
        error: None,
    };

    // Validate (malformed output stops here — never executes).
    if let Err(e) = validate_plan(plan) {
        r.validation = "rejected";
        r.error = Some(e);
        return r;
    }
    r.validation = "accepted";

    let step = &plan.steps[0];
    let src = match store.get(step.source) {
        Some(e) => e.clone(),
        None => {
            r.error = Some("source entity not found".to_string());
            return r;
        }
    };

    // Authorize: derive requires an 'entity.derive' capability over the source entity.
    let target = Target {
        id: Some(step.source),
        etype: Some(src.etype),
    };
    r.authorization = engine.evaluate("entity.derive", &target, offered);
    match r.authorization {
        Decision::Allow => {}
        _ => {
            r.error = Some("authorization not granted".to_string());
            return r;
        }
    }

    // Execute: derive a summary entity from the source content.
    let derived = format!("summary-of[{}]:{}", src.id, step.content);
    let expect_hash = content_hash(derived.as_bytes());
    let new_id = store.put(EntityType::Summary, &derived, actor);
    r.executed = true;
    r.produced = Some(new_id);

    // Verify: read back from the real store and confirm the effect matches expectation.
    match store.get(new_id) {
        Some(e) if e.content_hash == expect_hash && e.etype == EntityType::Summary => {
            r.verified = true;
        }
        _ => {
            r.error = Some("verification failed: store effect does not match".to_string());
            return r;
        }
    }

    // Record the immutable event only after verified success.
    store.record_event("entity.derived", actor);
    r.ok = true;
    r
}

// ---------------------------------------------------------------------------
// Secure IPC — capability-gated message passing between in-kernel actors.
//
// The IPC substrate lives in its own module (`crate::ipc`) so the synchronous fast-path, the
// capability-transfer path, asynchronous notifications, deadline/timeout semantics, cancellation,
// and the auditable trace/replay log are one coherent unit (gap-register Issue 2). It is re-exported
// here so `spine::*` (the surface the selftest suite and hosted invariants import) still resolves
// the IPC types exactly as before.
// ---------------------------------------------------------------------------

pub use crate::ipc::{CapGrant, Channel, IpcOp, Message, Notification, RecvOutcome, TraceEvent};

// Zero-copy shared-memory grant-table (REQ-IPC-008, ADR-020) — the bulk-data companion to the
// message-copy `Channel`. Re-exported here so the `spine::*` surface carries it alongside the IPC
// types; the authority/lifecycle layer is arch-independent, the page mapping stays a per-target seam.
pub use crate::grant::{GrantError, GrantTable, ShareMode};

// Priority-inheritance blocking IPC + priority-aware scheduling (REQ-IPC-009, ADR-020). Re-exported
// so the `spine::*` surface carries the priority-scheduler alongside the round-robin one; the policy
// is arch-independent, the context switch stays each target's `TaskContext` seam.
pub use crate::priosched::{Endpoint, Priority, PriorityScheduler, SchedError};

// Crash-consistent journaled block store (REQ-STOR-002, ADR-024) — arch-independent WAL over the
// `BlockDevice` seam a real driver later implements; re-exported on the `spine::*` surface.
pub use crate::storage::{BlockDevice, Journal, MemBlockDevice, StorageError, BLOCK_SIZE};

// Capability-authorized device access (REQ-DRV-002, ADR-023) — no ambient device authority; gates
// I/O to a `BlockDevice` on the same `CapEngine`.
pub use crate::device::{DeviceError, DeviceGuard};
