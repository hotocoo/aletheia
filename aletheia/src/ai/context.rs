//! Native Context Engine — Aletheia's Context Fabric (ADR-018).
//!
//! This is NOT retrieval-augmented generation. Aletheia understands its own world — entities,
//! relationships, provenance, permissions, temporal state, ownership — and assembles the *smallest
//! useful* context for a task rather than dumping histories or documents into the model. Critical
//! for the small MiniCPM5-1B model: context efficiency is a first-class concern.
//!
//! Layered retrieval, structured-first (priority order):
//! ```text
//! intent → Context Planner → capability-aware retrieval
//!   ├── direct         (subject, focus entity/task, held authority)   [always]
//!   ├── structured     (entity queries: type, properties, ownership)  [always]
//!   ├── relationships  (world-model traversal from the focus)         [always]
//!   ├── memory         (relevant past actions/decisions)              [when relevant]
//!   ├── semantic       (embeddings for ambiguous NL search)           [OPTIONAL seam]
//!   └── knowledge      (documents/transcripts/images, unstructured)   [OPTIONAL seam]
//! → rank / dedup / compress / budget → compact typed AiContext → model
//! ```
//! Semantic and knowledge retrieval are OPTIONAL interfaces (`SemanticRetriever`, `KnowledgeService`)
//! — never mandatory dependencies of the core runtime. No always-running embedding server or vector
//! database is required for normal operation.
//!
//! CAPABILITY-AWARE (non-negotiable): retrieval happens AFTER identity/capability is established and
//! enforces authorization BEFORE any information enters the model context. The AI never receives
//! anything the requesting subject is not authorized to access. Entity content is DATA, never
//! instructions (SEC-003).
use crate::capabilities::{CapEngine, Decision, Target};
use crate::domain::{now, Entity, EntityType, Id};
use crate::intent_action::{Intent, Verb};
use crate::storage::Store;
use serde::Serialize;

/// Explicit context budget. Tuned small for the 1B model; the engine never exceeds it.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    pub max_entities: usize,
    pub max_relationships: usize,
    pub max_memory: usize,
    pub max_chars: usize,
}
impl ContextBudget {
    /// Default profile for the hosted MiniCPM5-1B model — deliberately tight.
    pub fn small() -> Self {
        ContextBudget { max_entities: 6, max_relationships: 8, max_memory: 5, max_chars: 2000 }
    }
}
impl Default for ContextBudget {
    fn default() -> Self {
        Self::small()
    }
}

/// Optional semantic retriever (embeddings) for ambiguous natural-language search. An extension
/// seam — NOT a core dependency. Left unimplemented until a real use case requires it.
pub trait SemanticRetriever {
    fn search(&self, query: &str, k: usize) -> Vec<Id>;
}

/// Optional unstructured-knowledge service (documents/transcripts/images). Extension seam only.
pub trait KnowledgeService {
    fn fetch(&self, refs: &[Id]) -> Vec<String>;
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityRef {
    pub id: Id,
    pub etype: EntityType,
    pub version: u64,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EdgeRef {
    pub from: Id,
    pub rtype: String,
    pub to: Id,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryRef {
    pub etype: String,
    pub actor: String,
    pub at: u64,
}

/// Who and what the request concerns — established first, before any world retrieval.
#[derive(Debug, Clone, Serialize)]
pub struct DirectContext {
    pub subject: String,
    pub focus: Option<Id>,
    /// Capability *classes* the subject holds (action strings), summarizing its authority — never
    /// the tokens themselves.
    pub authority: Vec<String>,
}

/// Compact, typed, capability-scoped context. The model receives a rendering of this — never a raw
/// store dump.
#[derive(Debug, Clone, Serialize)]
pub struct AiContext {
    pub direct: DirectContext,
    pub world: Vec<EntityRef>,
    pub relationships: Vec<EdgeRef>,
    pub memory: Vec<MemoryRef>,
}

impl AiContext {
    /// Provenance labels for the trace (`source_type:source_id`), inspectable per PRD EXP-005.
    pub fn provenance(&self) -> Vec<String> {
        let mut p = Vec::new();
        for e in &self.world {
            p.push(format!("entity:{}", e.id));
        }
        for r in &self.relationships {
            p.push(format!("edge:{}", r.rtype));
        }
        for m in &self.memory {
            p.push(format!("memory:{}", m.etype));
        }
        p
    }

    /// Render a compact, typed brief for the model, hard-capped at `max_chars`.
    pub fn render(&self, max_chars: usize) -> String {
        let mut s = String::new();
        s.push_str(&format!("subject: {}\n", self.direct.subject));
        if let Some(f) = &self.direct.focus {
            s.push_str(&format!("focus: {}\n", f));
        }
        s.push_str(&format!("authority: {}\n", self.direct.authority.join(", ")));
        s.push_str("world:\n");
        for e in &self.world {
            s.push_str(&format!("  {} {:?} v{}\n", e.id, e.etype, e.version));
        }
        s.push_str("relationships:\n");
        for r in &self.relationships {
            s.push_str(&format!("  {} --{}--> {}\n", r.from, r.rtype, r.to));
        }
        s.push_str("recent:\n");
        for m in &self.memory {
            s.push_str(&format!("  {} by {}\n", m.etype, m.actor));
        }
        if s.len() > max_chars {
            s.truncate(max_chars);
        }
        s
    }
}

/// Assembles capability-scoped context from the World Model. Provider-independent.
#[derive(Default)]
pub struct ContextEngine {
    // Optional retrievers are held as extension points; absent by default (no external services).
}

impl ContextEngine {
    pub fn new() -> Self {
        ContextEngine::default()
    }

    /// Build the smallest useful, authorized context for `intent`. Structured-first; relationship
    /// traversal seeded by the focus entity; memory from recent events. Every entity is checked for
    /// `entity.read` authority against `offered` BEFORE inclusion — unauthorized state never enters.
    pub fn build(
        &self,
        store: &Store,
        caps: &CapEngine,
        offered: &[String],
        subject: &str,
        intent: &Intent,
        budget: ContextBudget,
    ) -> AiContext {
        let focus = focus_entity(intent);

        // Direct: the subject's held authority (action classes only, never tokens).
        let mut authority: Vec<String> = offered
            .iter()
            .filter_map(|t| caps.get(t).map(|c| c.action.clone()))
            .collect();
        authority.sort();
        authority.dedup();

        let direct = DirectContext { subject: subject.to_string(), focus: focus.clone(), authority };

        // Structured world (priority 1) + relationship traversal (priority 2), capability-filtered.
        let mut world: Vec<EntityRef> = Vec::new();
        let mut relationships: Vec<EdgeRef> = Vec::new();
        let mut seen: Vec<Id> = Vec::new();

        if let Some(fid) = &focus {
            // Only traverse from a focus the subject is authorized to read, and surface an edge ONLY
            // when the neighbour is ALSO authorized — an edge reveals a neighbour id + relationship
            // type, itself protected state. This enforces capability-before-inclusion for EDGES, not
            // just entities (the prior version pushed the edge before the neighbour check).
            let focus_authorized = store.get_entity(fid).map(|e| authorized_read(caps, offered, e)).unwrap_or(false);
            if focus_authorized {
                if let Some(e) = store.get_entity(fid) {
                    world.push(entity_ref(e));
                    seen.push(e.id.clone());
                }
                for r in store.relationships() {
                    if relationships.len() >= budget.max_relationships {
                        break;
                    }
                    if &r.from == fid || &r.to == fid {
                        let other = if &r.from == fid { &r.to } else { &r.from };
                        let ne = match store.get_entity(other) {
                            Some(ne) if authorized_read(caps, offered, ne) => ne,
                            _ => continue, // neighbour not authorized → do not reveal the edge
                        };
                        relationships.push(EdgeRef { from: r.from.clone(), rtype: r.rtype.clone(), to: r.to.clone() });
                        if world.len() < budget.max_entities && !seen.contains(other) {
                            world.push(entity_ref(ne));
                            seen.push(ne.id.clone());
                        }
                    }
                }
            }
        }

        // Memory (priority 3, when relevant): recent events attributed to the subject.
        let mut memory: Vec<MemoryRef> = Vec::new();
        for ev in store.events().iter().rev() {
            if memory.len() >= budget.max_memory {
                break;
            }
            if ev.actor == subject {
                memory.push(MemoryRef { etype: ev.etype.clone(), actor: ev.actor.clone(), at: ev.at });
            }
        }

        // Semantic (priority 4) and knowledge (priority 5) are OPTIONAL and intentionally not
        // consulted here — the World Model resolves structured tasks without them (e.g. "the
        // recording I edited yesterday" is a relationship+time query, not a vector search).

        AiContext { direct, world, relationships, memory }
    }
}

fn focus_entity(intent: &Intent) -> Option<Id> {
    match &intent.verb {
        Verb::Read { id } | Verb::Delete { id } => Some(id.clone()),
        Verb::Derive { source, .. } => Some(source.clone()),
        Verb::Traverse { from, .. } => Some(from.clone()),
        Verb::RestoreVersion { .. } | Verb::Grant { .. } | Verb::Raw { .. } => None,
    }
}

fn entity_ref(e: &Entity) -> EntityRef {
    EntityRef { id: e.id.clone(), etype: e.etype, version: e.version }
}

fn authorized_read(caps: &CapEngine, offered: &[String], e: &Entity) -> bool {
    let target = Target { id: Some(e.id.clone()), etype: Some(e.etype) };
    matches!(caps.evaluate("entity.read", &target, offered), Decision::Allow)
}

/// Back-compat convenience: a one-line situational brief (used by simple callers/tests). Prefer
/// `ContextEngine::build` for capability-aware, budgeted context.
pub fn build_brief(store: &Store, subject: &str, max_items: usize) -> String {
    let mut s = format!("subject: {subject}\nrecent:\n");
    for e in store.events().iter().rev().take(max_items) {
        s.push_str(&format!("  {} by {}\n", e.etype, e.actor));
    }
    let _ = now();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{Constraints, Scope};
    use crate::intent_action::{Intent, Verb};

    fn tmp() -> String {
        std::env::temp_dir().join(format!("aletheia-ctx-{}", crate::domain::new_id())).to_string_lossy().into_owned()
    }

    #[test]
    fn context_is_capability_scoped_never_leaks_unauthorized_entities() {
        let mut store = Store::open(tmp()).unwrap();
        let mut caps = CapEngine::new();
        // Two documents; the subject is authorized to read only e1.
        let root = caps.mint("human:owner", "*", Scope::All, Constraints::none(), "system");
        let h = store.put_blob(b"a").unwrap();
        let e1 = Entity { id: crate::domain::new_id(), etype: EntityType::Document, content_ref: Some(h.clone()), version: 1, version_chain: crate::domain::new_id(), metadata: serde_json::json!({}), provenance: crate::domain::Provenance::of("human:owner"), created_at: now(), updated_at: now(), deleted: false };
        let e2 = Entity { id: crate::domain::new_id(), ..e1.clone() };
        store.put_entity(&e1).unwrap();
        store.put_entity(&e2).unwrap();
        let read_e1 = caps.delegate(&root.token, "agent:a", "entity.read", Scope::Entities(vec![e1.id.clone()]), Constraints::none(), "human:owner").unwrap();

        let eng = ContextEngine::new();
        let intent = Intent { subject: "agent:a".into(), verb: Verb::Read { id: e1.id.clone() } };
        let ctx = eng.build(&store, &caps, &[read_e1.token], "agent:a", &intent, ContextBudget::small());
        assert!(ctx.world.iter().any(|e| e.id == e1.id), "authorized entity present");
        assert!(!ctx.world.iter().any(|e| e.id == e2.id), "unauthorized entity must NOT enter context");

        // A subject offering no capability sees nothing structural (no ambient authority).
        let empty = eng.build(&store, &caps, &[], "attacker", &intent, ContextBudget::small());
        assert!(empty.world.is_empty(), "no capability -> no world context");
    }

    #[test]
    fn budget_profile_is_tight_for_small_model() {
        let b = ContextBudget::small();
        assert!(b.max_entities <= 8 && b.max_chars <= 4000);
    }
}
