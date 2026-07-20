//! Intelligence runtime: the ONLY probabilistic stage (PRD-002 §21, ADR-006). A runtime interprets
//! an Intent into raw plan JSON (untrusted). Output flows through the identical downstream pipeline
//! whether it came from a model or the deterministic fallback — never a bypass (INV-014).
use crate::intent_action::{Intent, Plan, Step, Verb};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelError {
    NotLoaded,
    Timeout,
    Cancelled,
    OutOfMemory,
    InvalidOutput,
    Runtime,
}

pub trait ModelRuntime {
    fn name(&self) -> &str;
    fn healthy(&self) -> bool;
    /// Interpret intent into RAW plan JSON (untrusted string). Errors are handled by the pipeline.
    fn interpret(&self, intent: &Intent) -> Result<String, ModelError>;
}

/// Deterministic interpreter: maps the bounded structured intent set (tests + UC-001..004) to plans.
/// It is NOT a natural-language parser; unrecognized free text is rejected.
pub struct DeterministicRuntime;

impl ModelRuntime for DeterministicRuntime {
    fn name(&self) -> &str { "deterministic" }
    fn healthy(&self) -> bool { true }
    fn interpret(&self, intent: &Intent) -> Result<String, ModelError> {
        let plan = match &intent.verb {
            Verb::Read { id } => step("entity.read", json!({ "id": id })),
            Verb::Derive { source, into_type, content } => {
                step("entity.derive", json!({ "source": source, "into_type": into_type, "content": content }))
            }
            Verb::Traverse { from, edge } => step("world.traverse", json!({ "from": from, "edge": edge })),
            Verb::Grant { subject, action, scope_entities, approval } => step(
                "capability.grant",
                json!({ "subject": subject, "action": action, "scope_entities": scope_entities, "approval": approval }),
            ),
            Verb::RestoreVersion { chain, version } => {
                step("entity.restore_version", json!({ "chain": chain, "version": version }))
            }
            Verb::Delete { id } => step("entity.delete", json!({ "id": id })),
            Verb::Raw { .. } => return Err(ModelError::InvalidOutput),
        };
        Ok(serde_json::to_string(&plan).expect("plan serializes"))
    }
}

fn step(op: &str, args: serde_json::Value) -> Plan {
    Plan { steps: vec![Step { op: op.to_string(), args }] }
}

/// Adapter for a local inference server (OpenAI/llama.cpp-class). In M1 no server is assumed present,
/// so it reports unhealthy and the System Core falls back to the deterministic interpreter (INT-004).
pub struct LocalModelRuntime {
    pub endpoint: String,
}
impl LocalModelRuntime {
    pub fn new(endpoint: &str) -> Self { LocalModelRuntime { endpoint: endpoint.to_string() } }
}
impl ModelRuntime for LocalModelRuntime {
    fn name(&self) -> &str { "local-model" }
    fn healthy(&self) -> bool { false }
    fn interpret(&self, _intent: &Intent) -> Result<String, ModelError> { Err(ModelError::NotLoaded) }
}
