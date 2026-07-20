//! Context assembly (thin for M1 per SAD §24 — types + minimal assembly; ranking engine is post-M1).
use crate::intent_action::{Intent, Verb};
use crate::storage::Store;

#[derive(Debug, Clone)]
pub struct ContextItem {
    pub source_type: String,
    pub source_id: String,
    pub relevance: f32,
    pub confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub items: Vec<ContextItem>,
}

/// Assemble bounded, provenance-tagged context: the focused entity (if any) + recent events.
pub fn assemble(store: &Store, _subject: &str, intent: &Intent) -> Context {
    let mut items = Vec::new();
    let focus = match &intent.verb {
        Verb::Read { id } => Some(id.clone()),
        Verb::Traverse { from, .. } => Some(from.clone()),
        Verb::Derive { source, .. } => Some(source.clone()),
        Verb::Delete { id } => Some(id.clone()),
        _ => None,
    };
    if let Some(id) = focus {
        if store.get_entity(&id).is_some() {
            items.push(ContextItem { source_type: "entity".into(), source_id: id, relevance: 1.0, confidence: 1.0 });
        }
    }
    for ev in store.events().iter().rev().take(3) {
        items.push(ContextItem { source_type: "event".into(), source_id: ev.id.clone(), relevance: 0.5, confidence: 1.0 });
    }
    Context { items }
}
