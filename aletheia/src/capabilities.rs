//! Capability engine — the sole authority mechanism (PRD-002 §15, ADR-003).
//! Capabilities are possession-based unforgeable handles: a token authorizes only if it exists
//! in the engine's private registry. Holding the struct is NOT authority (see forgery test).
//! NOTE (ADR-010): true unforgeability is a P4 kernel property; M1 proves the *contract* — the
//! engine validates handles against a registry no external code can populate.
use crate::domain::{EntityType, Id, Provenance};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub type CapToken = String;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Scope {
    All,
    Type(EntityType),
    Entities(Vec<Id>),
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Constraints {
    pub expires_at: Option<u64>,
    pub max_count: Option<u32>,
    pub approval_required: bool,
    pub local_only: bool,
}
impl Constraints {
    pub fn none() -> Self {
        Constraints { expires_at: None, max_count: None, approval_required: false, local_only: true }
    }
    pub fn approval() -> Self {
        Constraints { approval_required: true, ..Self::none() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredCapability {
    pub token: CapToken,
    pub subject: String,
    pub action: String,
    pub scope: Scope,
    pub constraints: Constraints,
    pub parent: Option<CapToken>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny(String),
    RequireApproval,
}

/// What an action targets. `id`/`etype` are None for global actions (e.g. capability.grant).
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub id: Option<Id>,
    pub etype: Option<EntityType>,
}

pub struct CapEngine {
    registry: HashMap<CapToken, StoredCapability>,
    revoked: HashSet<CapToken>,
    children: HashMap<CapToken, Vec<CapToken>>,
    now_fn: fn() -> u64,
}

impl CapEngine {
    pub fn new() -> Self {
        CapEngine {
            registry: HashMap::new(),
            revoked: HashSet::new(),
            children: HashMap::new(),
            now_fn: crate::domain::now,
        }
    }

    /// Load a persisted capability during store replay.
    pub fn load(&mut self, cap: StoredCapability) {
        if let Some(p) = &cap.parent {
            self.children.entry(p.clone()).or_default().push(cap.token.clone());
        }
        self.registry.insert(cap.token.clone(), cap);
    }

    pub fn mark_revoked(&mut self, token: &str) {
        self.revoke(token);
    }

    /// Mint a fresh root capability. Only the engine can produce a valid token.
    pub fn mint(
        &mut self,
        subject: &str,
        action: &str,
        scope: Scope,
        constraints: Constraints,
        actor: &str,
    ) -> StoredCapability {
        let cap = StoredCapability {
            token: crate::crypto::random_token(),
            subject: subject.to_string(),
            action: action.to_string(),
            scope,
            constraints,
            parent: None,
            provenance: Provenance::of(actor),
        };
        self.registry.insert(cap.token.clone(), cap.clone());
        cap
    }

    /// Delegate an existing capability, only with equal-or-narrower authority (attenuation).
    pub fn delegate(
        &mut self,
        parent_token: &CapToken,
        subject: &str,
        action: &str,
        scope: Scope,
        constraints: Constraints,
        actor: &str,
    ) -> crate::domain::Result<StoredCapability> {
        use crate::domain::AlethError;
        if self.revoked.contains(parent_token) {
            return Err(AlethError::authorization("parent capability revoked"));
        }
        let parent = self
            .registry
            .get(parent_token)
            .ok_or_else(|| AlethError::authorization("unknown parent capability"))?
            .clone();
        if !action_covers(&parent.action, action) {
            return Err(AlethError::authorization("delegation would amplify action"));
        }
        if !scope_subset(&parent.scope, &scope) {
            return Err(AlethError::authorization("delegation would amplify scope"));
        }
        if !constraints_not_looser(&parent.constraints, &constraints) {
            return Err(AlethError::authorization("delegation would loosen constraints"));
        }
        let cap = StoredCapability {
            token: crate::crypto::random_token(),
            subject: subject.to_string(),
            action: action.to_string(),
            scope,
            constraints,
            parent: Some(parent_token.clone()),
            provenance: Provenance::of(actor),
        };
        self.children.entry(parent_token.clone()).or_default().push(cap.token.clone());
        self.registry.insert(cap.token.clone(), cap.clone());
        Ok(cap)
    }

    /// Revoke a capability and all descendants, transitively and immediately.
    pub fn revoke(&mut self, token: &str) {
        let mut stack = vec![token.to_string()];
        while let Some(t) = stack.pop() {
            if self.revoked.insert(t.clone()) {
                if let Some(kids) = self.children.get(&t) {
                    stack.extend(kids.iter().cloned());
                }
            }
        }
    }

    /// The core authorization decision. Fail closed: no matching capability yields Deny.
    pub fn evaluate(&self, action: &str, target: &Target, offered: &[CapToken]) -> Decision {
        let mut needs_approval = false;
        for token in offered {
            if self.revoked.contains(token) { continue; }
            let cap = match self.registry.get(token) {
                Some(c) => c,
                None => continue, // forged/unknown handle — not authority
            };
            if !action_covers(&cap.action, action) { continue; }
            if !scope_covers(&cap.scope, target) { continue; }
            if let Some(exp) = cap.constraints.expires_at {
                if (self.now_fn)() > exp { continue; }
            }
            if cap.constraints.approval_required {
                needs_approval = true;
                continue;
            }
            return Decision::Allow;
        }
        if needs_approval { Decision::RequireApproval } else { Decision::Deny("no capability".into()) }
    }

    pub fn get(&self, token: &str) -> Option<&StoredCapability> {
        if self.revoked.contains(token) { None } else { self.registry.get(token) }
    }
    pub fn is_revoked(&self, token: &str) -> bool { self.revoked.contains(token) }
}

impl Default for CapEngine {
    fn default() -> Self { Self::new() }
}

pub fn action_covers(pattern: &str, action: &str) -> bool {
    if pattern == "*" { return true; }
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
        Scope::Entities(set) => target.id.as_ref().map(|id| set.contains(id)).unwrap_or(false),
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
    let count_ok = match (parent.max_count, child.max_count) {
        (Some(p), Some(c)) => c <= p,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let local_ok = !parent.local_only || child.local_only;
    let approval_ok = !parent.approval_required || child.approval_required;
    expiry_ok && count_ok && local_ok && approval_ok
}
