//! The seven-primitive domain model (PRD-002 §6, ADR-002). Leaf module: depends on nothing Aletheia-specific.
use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub type Id = String;

pub fn new_id() -> Id { Ulid::new().to_string() }

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityType {
    Document, Project, Task, Person, Device, Event,
    Agent, Capability, Application, Session, Output, Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub actor: String,
    pub action_id: Option<Id>,
    pub source_entities: Vec<Id>,
    pub at: u64,
}
impl Provenance {
    pub fn of(actor: &str) -> Self {
        Provenance { actor: actor.to_string(), action_id: None, source_entities: vec![], at: now() }
    }
}

/// An Entity is the universal unit of meaning. Content is immutable and content-addressed;
/// mutation produces a new version linked to the prior one via `version_chain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Id,
    pub etype: EntityType,
    pub content_ref: Option<String>,
    pub version: u64,
    pub version_chain: Id,
    pub metadata: serde_json::Value,
    pub provenance: Provenance,
    pub created_at: u64,
    pub updated_at: u64,
    pub deleted: bool,
}

/// A typed, directed, provenance-bearing edge. The union of relationships is the world model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: Id,
    pub rtype: String,
    pub from: Id,
    pub to: Id,
    pub provenance: Provenance,
    pub created_at: u64,
}

/// An immutable record of something that actually happened (itself an entity-class object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: Id,
    pub etype: String,
    pub at: u64,
    pub correlation_id: Id,
    pub actor: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCategory {
    Validation, Authorization, NotFound, Conflict,
    Timeout, Resource, Model, Persistence, Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlethError {
    pub code: String,
    pub message: String,
    pub category: ErrorCategory,
    pub retryable: bool,
}
impl AlethError {
    fn new(code: &str, msg: &str, cat: ErrorCategory, retryable: bool) -> Self {
        AlethError { code: code.to_string(), message: msg.to_string(), category: cat, retryable }
    }
    pub fn validation(m: &str) -> Self { Self::new("VALIDATION", m, ErrorCategory::Validation, false) }
    pub fn authorization(m: &str) -> Self { Self::new("AUTHORIZATION", m, ErrorCategory::Authorization, false) }
    pub fn not_found(m: &str) -> Self { Self::new("NOT_FOUND", m, ErrorCategory::NotFound, false) }
    pub fn conflict(m: &str) -> Self { Self::new("CONFLICT", m, ErrorCategory::Conflict, false) }
    pub fn model(m: &str) -> Self { Self::new("MODEL", m, ErrorCategory::Model, true) }
    pub fn persistence(m: &str) -> Self { Self::new("PERSISTENCE", m, ErrorCategory::Persistence, false) }
    pub fn internal(m: &str) -> Self { Self::new("INTERNAL", m, ErrorCategory::Internal, false) }
}
impl std::fmt::Display for AlethError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {:?}: {}", self.code, self.category, self.message)
    }
}
impl std::error::Error for AlethError {}

pub type Result<T> = std::result::Result<T, AlethError>;
