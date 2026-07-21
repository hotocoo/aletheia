//! Intent & Action types + the pure parse/validate stages (PRD-002 §17, SAD §10).
//! Intent carries no authority and never executes directly. The pipeline itself lives in syscore.
use crate::domain::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Verb {
    Read { id: Id },
    Derive { source: Id, into_type: EntityType, content: String },
    Traverse { from: Id, edge: String },
    Grant { subject: String, action: String, scope_entities: Vec<Id>, approval: bool },
    RestoreVersion { chain: Id, version: u64 },
    Delete { id: Id },
    Raw { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub subject: String,
    pub verb: Verb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub op: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

/// The full explainable record of one request, from intent through verification (PRD EXP-005).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub correlation_id: Id,
    pub subject: String,
    pub intent: String,
    pub context_provenance: Vec<String>,
    pub proposed_plan_raw: String,
    pub interpreter: String,
    pub validation: String,
    pub capability_decision: String,
    pub approval: String,
    pub execution: String,
    pub verification: String,
    pub result: serde_json::Value,
    pub ok: bool,
    pub error: Option<AlethError>,
    /// Set when the action stopped awaiting human approval — the id of the recorded pending
    /// approval a human can later grant or deny (SAD §10 `approve()`, ADR-015).
    pub approval_id: Option<Id>,
}
impl Trace {
    pub fn new(subject: &str, correlation_id: Id) -> Self {
        Trace {
            correlation_id,
            subject: subject.to_string(),
            intent: String::new(),
            context_provenance: vec![],
            proposed_plan_raw: String::new(),
            interpreter: String::new(),
            validation: String::new(),
            capability_decision: String::new(),
            approval: String::new(),
            execution: String::new(),
            verification: String::new(),
            result: serde_json::Value::Null,
            ok: false,
            error: None,
            approval_id: None,
        }
    }
}

/// Parse untrusted raw model output into a typed Plan. Malformed output fails here (never executes).
pub fn parse_plan(raw: &str) -> Result<Plan> {
    serde_json::from_str::<Plan>(raw).map_err(|e| AlethError::validation(&format!("plan parse: {}", e)))
}

/// Validate a parsed plan: non-empty, every op is a registered operation, args is an object.
pub fn validate_plan(plan: &Plan) -> Result<()> {
    if plan.steps.is_empty() {
        return Err(AlethError::validation("plan has no steps"));
    }
    for step in &plan.steps {
        if crate::tools::lookup(&step.op).is_none() {
            return Err(AlethError::validation(&format!("unknown operation: {}", step.op)));
        }
        if !step.args.is_object() {
            return Err(AlethError::validation(&format!("args must be an object for {}", step.op)));
        }
    }
    Ok(())
}
