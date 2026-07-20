//! Agents as first-class capability-controlled actors (PRD-002 §20, ADR-007).
use crate::capabilities::CapToken;
use crate::domain::Id;

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: Id,
    pub identity: String,
    pub caps: Vec<CapToken>,
    pub goals: Vec<String>,
}
impl Agent {
    pub fn new(identity: &str) -> Self {
        Agent { id: crate::domain::new_id(), identity: identity.to_string(), caps: vec![], goals: vec![] }
    }
}
