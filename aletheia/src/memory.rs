//! Memory types (thin for M1 per SAD §24 — classified, provenance-bearing; distillation is post-M1).
use crate::domain::{Id, Provenance};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MemClass {
    ObservedFact,
    HumanStatement,
    Decision,
    DerivedRelationship,
    AISummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: Id,
    pub class: MemClass,
    pub text: String,
    pub confidence: f32,
    pub entity_links: Vec<Id>,
    pub provenance: Provenance,
}
