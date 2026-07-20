//! System Core composition + the deterministic Intent→Action pipeline + task runtime (SAD §8/§10).
//! This is the deterministic authority: it validates, authorizes (capabilities), executes, verifies,
//! and records. The only probabilistic step is interpretation; everything after is deterministic.
use crate::agents::Agent;
use crate::capabilities::{CapEngine, Constraints, Decision, Scope, StoredCapability, Target};
use crate::context;
use crate::domain::*;
use crate::intelligence::{DeterministicRuntime, ModelRuntime};
use crate::intent_action::{parse_plan, validate_plan, Intent, Step, Trace};
use crate::storage::Store;
use crate::tools::{self, Risk};
use crate::worldmodel::{self, Dir};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Created,
    Running,
    AwaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

pub struct SysCore {
    store: Store,
    caps: CapEngine,
    model: Box<dyn ModelRuntime>,
    fallback: DeterministicRuntime,
    tasks: HashMap<Id, TaskState>,
    cancelled: HashSet<Id>,
}

impl SysCore {
    pub fn open(dir: impl AsRef<std::path::Path>, model: Box<dyn ModelRuntime>) -> Result<Self> {
        let store = Store::open(dir)?;
        let mut caps = CapEngine::new();
        for c in store.loaded_caps() {
            caps.load(c.clone());
        }
        for t in store.revoked_tokens() {
            caps.mark_revoked(t);
        }
        Ok(SysCore { store, caps, model, fallback: DeterministicRuntime, tasks: HashMap::new(), cancelled: HashSet::new() })
    }

    /// Open with the local-model adapter (unhealthy in M1 → deterministic fallback, INT-004).
    pub fn open_default(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open(dir, Box::new(crate::intelligence::LocalModelRuntime::new("http://localhost:8080")))
    }

    pub fn store(&self) -> &Store { &self.store }
    pub fn caps(&self) -> &CapEngine { &self.caps }
    pub fn caps_mut(&mut self) -> &mut CapEngine { &mut self.caps }
    pub fn interpreter_name(&self) -> String {
        if self.model.healthy() { self.model.name().into() } else { self.fallback.name().into() }
    }
    pub fn task_state(&self, id: &Id) -> Option<TaskState> { self.tasks.get(id).copied() }

    fn emit(&mut self, etype: &str, corr: &Id, actor: &str, payload: Value) {
        let ev = EventRecord {
            id: new_id(),
            etype: etype.to_string(),
            at: now(),
            correlation_id: corr.clone(),
            actor: actor.to_string(),
            payload,
        };
        let _ = self.store.put_event(&ev);
    }

    /// Mint the human owner's root capability. Authority still comes from a HELD capability, not from
    /// merely running — this is the root of the delegation tree, not ambient authority (INV-011).
    pub fn bootstrap_owner(&mut self, subject: &str) -> Result<StoredCapability> {
        let cap = self.caps.mint(subject, "*", Scope::All, Constraints::none(), "system");
        self.store.put_capability(&cap)?;
        self.emit("CapabilityGranted", &new_id(), "system", json!({"token": cap.token, "subject": subject, "action": "*"}));
        Ok(cap)
    }

    pub fn create_entity(
        &mut self,
        offered: &[String],
        subject: &str,
        etype: EntityType,
        content: &[u8],
        metadata: Value,
    ) -> Result<Entity> {
        let target = Target { id: None, etype: Some(etype) };
        match self.caps.evaluate("entity.write", &target, offered) {
            Decision::Allow => {}
            Decision::RequireApproval => return Err(AlethError::authorization("approval required")),
            Decision::Deny(r) => return Err(AlethError::authorization(&format!("denied: {}", r))),
        }
        let hash = self.store.put_blob(content)?;
        let chain = new_id();
        let mut prov = Provenance::of(subject);
        prov.at = now();
        let e = Entity {
            id: new_id(),
            etype,
            content_ref: Some(hash),
            version: 1,
            version_chain: chain,
            metadata,
            provenance: prov,
            created_at: now(),
            updated_at: now(),
            deleted: false,
        };
        self.store.put_entity(&e)?;
        self.emit("EntityCreated", &new_id(), subject, json!({"entity": e.id, "type": e.etype}));
        Ok(e)
    }

    /// Create a new version of an entity chain (criterion 2). Prior version is retained/recoverable.
    pub fn update_entity(&mut self, offered: &[String], subject: &str, chain: &Id, content: &[u8]) -> Result<Entity> {
        let latest = self.store.latest_of_chain(chain).cloned().ok_or_else(|| AlethError::not_found("chain not found"))?;
        let target = Target { id: Some(latest.id.clone()), etype: Some(latest.etype) };
        match self.caps.evaluate("entity.write", &target, offered) {
            Decision::Allow => {}
            Decision::RequireApproval => return Err(AlethError::authorization("approval required")),
            Decision::Deny(r) => return Err(AlethError::authorization(&format!("denied: {}", r))),
        }
        let hash = self.store.put_blob(content)?;
        let e = Entity {
            id: new_id(),
            etype: latest.etype,
            content_ref: Some(hash),
            version: latest.version + 1,
            version_chain: chain.clone(),
            metadata: latest.metadata.clone(),
            provenance: Provenance::of(subject),
            created_at: latest.created_at,
            updated_at: now(),
            deleted: false,
        };
        self.store.put_entity(&e)?;
        self.emit("EntityVersioned", &new_id(), subject, json!({"chain": chain, "version": e.version}));
        Ok(e)
    }

    pub fn revoke(&mut self, token: &str) -> Result<()> {
        self.caps.revoke(token);
        self.store.put_revoke(token)?;
        self.emit("CapabilityRevoked", &new_id(), "system", json!({"token": token}));
        Ok(())
    }

    /// Grant (by delegation, preserving lineage) a capability to `subject`. Authorized by a held
    /// capability covering `capability.grant`; attenuation enforced by the engine.
    pub fn grant_to(
        &mut self,
        offered: &[String],
        subject: &str,
        action: &str,
        scope: Scope,
        constraints: Constraints,
    ) -> Result<StoredCapability> {
        if !matches!(self.caps.evaluate("capability.grant", &Target::default(), offered), Decision::Allow) {
            return Err(AlethError::authorization("not permitted to grant"));
        }
        for token in offered {
            if self.caps.get(token).is_some() {
                if let Ok(cap) = self.caps.delegate(token, subject, action, scope.clone(), constraints.clone(), subject) {
                    self.store.put_capability(&cap)?;
                    self.emit("CapabilityGranted", &new_id(), subject, json!({"token": cap.token, "subject": subject, "action": action}));
                    return Ok(cap);
                }
            }
        }
        Err(AlethError::authorization("no suitable parent capability to delegate from"))
    }

    pub fn create_agent(&mut self, identity: &str) -> Agent {
        let agent = Agent::new(identity);
        self.emit("AgentCreated", &new_id(), "system", json!({"agent": agent.id, "identity": identity}));
        agent
    }

    // --- task lifecycle ---

    pub fn begin_task(&mut self, _subject: &str) -> Id {
        let id = new_id();
        self.tasks.insert(id.clone(), TaskState::Created);
        id
    }
    pub fn cancel_task(&mut self, id: &Id) {
        self.cancelled.insert(id.clone());
        self.tasks.insert(id.clone(), TaskState::Cancelled);
    }

    /// Convenience: begin a task and run one intent through the full pipeline.
    pub fn handle_intent(&mut self, offered: &[String], intent: Intent, approve: bool) -> Trace {
        let subject = intent.subject.clone();
        let task = self.begin_task(&subject);
        self.run_intent(&task, offered, intent, approve)
    }

    /// The full deterministic pipeline (SAD §10). Always returns a Trace; `ok` reflects success.
    pub fn run_intent(&mut self, task_id: &Id, offered: &[String], intent: Intent, approve: bool) -> Trace {
        let mut trace = Trace::new(&intent.subject, task_id.clone());
        trace.intent = format!("{:?}", intent.verb);
        self.tasks.insert(task_id.clone(), TaskState::Running);

        if self.cancelled.contains(task_id) {
            trace.execution = "cancelled before start".into();
            self.tasks.insert(task_id.clone(), TaskState::Cancelled);
            return trace;
        }

        // Context (bounded, provenance-tracked).
        let ctx = context::assemble(&self.store, &intent.subject, &intent);
        trace.context_provenance = ctx.items.iter().map(|i| format!("{}:{}", i.source_type, i.source_id)).collect();

        // Interpretation — the ONLY probabilistic stage. Model-unhealthy → deterministic fallback.
        // Model-healthy-but-errored → this request fails (no silent fallback); state stays intact.
        let raw = if self.model.healthy() {
            trace.interpreter = self.model.name().into();
            match self.model.interpret(&intent) {
                Ok(r) => r,
                Err(e) => {
                    trace.error = Some(AlethError::model(&format!("interpretation failed: {:?}", e)));
                    trace.execution = "interpretation error — no state changed".into();
                    self.tasks.insert(task_id.clone(), TaskState::Failed);
                    self.emit("AIActionFailed", task_id, &intent.subject, json!({"stage": "interpret"}));
                    return trace;
                }
            }
        } else {
            trace.interpreter = self.fallback.name().into();
            match self.fallback.interpret(&intent) {
                Ok(r) => r,
                Err(e) => {
                    trace.error = Some(AlethError::model(&format!("no valid plan: {:?}", e)));
                    self.tasks.insert(task_id.clone(), TaskState::Failed);
                    self.emit("AIActionFailed", task_id, &intent.subject, json!({"stage": "interpret"}));
                    return trace;
                }
            }
        };
        trace.proposed_plan_raw = raw.clone();

        // Parse (untrusted boundary) + validate.
        let plan = match parse_plan(&raw) {
            Ok(p) => p,
            Err(e) => {
                trace.validation = format!("parse failed: {}", e);
                trace.error = Some(e);
                self.tasks.insert(task_id.clone(), TaskState::Failed);
                self.emit("AIActionFailed", task_id, &intent.subject, json!({"stage": "parse"}));
                return trace;
            }
        };
        if let Err(e) = validate_plan(&plan) {
            trace.validation = format!("invalid: {}", e);
            trace.error = Some(e);
            self.tasks.insert(task_id.clone(), TaskState::Failed);
            self.emit("AIActionFailed", task_id, &intent.subject, json!({"stage": "validate"}));
            return trace;
        }
        trace.validation = "ok".into();

        // Authorize + execute each step.
        let mut results: Vec<Value> = Vec::new();
        for step in &plan.steps {
            let meta = match tools::lookup(&step.op) {
                Some(m) => m,
                None => {
                    trace.validation = format!("unknown op {}", step.op);
                    self.tasks.insert(task_id.clone(), TaskState::Failed);
                    return trace;
                }
            };
            let target = match self.target_for(step) {
                Ok(t) => t,
                Err(e) => {
                    trace.validation = format!("bad args: {}", e);
                    trace.error = Some(e);
                    self.tasks.insert(task_id.clone(), TaskState::Failed);
                    return trace;
                }
            };
            let decision = self.caps.evaluate(meta.action, &target, offered);
            if let Decision::Deny(r) = &decision {
                trace.capability_decision = format!("DENY ({}) for {}", r, meta.action);
                trace.error = Some(AlethError::authorization("capability denied"));
                self.tasks.insert(task_id.clone(), TaskState::Failed);
                self.emit("CapabilityDenied", task_id, &intent.subject, json!({"action": meta.action}));
                return trace;
            }
            let destructive = meta.risk == Risk::Destructive;
            if matches!(decision, Decision::RequireApproval) || destructive {
                trace.capability_decision = if destructive {
                    format!("ALLOW but DESTRUCTIVE ({})", meta.action)
                } else {
                    format!("REQUIRE_APPROVAL ({})", meta.action)
                };
                if !approve {
                    trace.approval = "pending — awaiting human approval".into();
                    self.tasks.insert(task_id.clone(), TaskState::AwaitingApproval);
                    self.emit("AIActionProposed", task_id, &intent.subject, json!({"action": meta.action, "needs_approval": true}));
                    return trace;
                }
                trace.approval = "approved".into();
            } else {
                trace.capability_decision = format!("ALLOW ({})", meta.action);
                trace.approval = "not required".into();
            }

            // Cancellation checkpoint before any effect.
            if self.cancelled.contains(task_id) {
                trace.execution = "cancelled before execute — no state changed".into();
                self.tasks.insert(task_id.clone(), TaskState::Cancelled);
                return trace;
            }

            match self.execute_step(&intent.subject, task_id, step, offered) {
                Ok(v) => results.push(v),
                Err(e) => {
                    trace.execution = format!("execution failed: {}", e);
                    trace.error = Some(e);
                    self.tasks.insert(task_id.clone(), TaskState::Failed);
                    self.emit("AIActionFailed", task_id, &intent.subject, json!({"stage": "execute"}));
                    return trace;
                }
            }
        }

        trace.execution = "executed".into();
        trace.verification = "verified against store".into();
        trace.result = json!(results);
        trace.ok = true;
        self.tasks.insert(task_id.clone(), TaskState::Completed);
        self.emit("AIActionExecuted", task_id, &intent.subject, json!({"result": trace.result}));
        trace
    }

    fn target_for(&self, step: &Step) -> Result<Target> {
        let a = &step.args;
        match step.op.as_str() {
            "entity.read" | "entity.delete" => {
                let id = arg_str(a, "id")?;
                let et = self.store.get_entity(&id).map(|e| e.etype);
                Ok(Target { id: Some(id), etype: et })
            }
            "entity.restore_version" => {
                let chain = arg_str(a, "chain")?;
                let l = self.store.latest_of_chain(&chain);
                Ok(Target { id: l.map(|e| e.id.clone()), etype: l.map(|e| e.etype) })
            }
            "entity.derive" => {
                let into: EntityType = serde_json::from_value(a.get("into_type").cloned().unwrap_or(Value::Null))
                    .map_err(|_| AlethError::validation("into_type"))?;
                Ok(Target { id: None, etype: Some(into) })
            }
            "world.traverse" => {
                let from = arg_str(a, "from")?;
                let et = self.store.get_entity(&from).map(|e| e.etype);
                Ok(Target { id: Some(from), etype: et })
            }
            "capability.grant" => Ok(Target::default()),
            _ => Ok(Target::default()),
        }
    }

    fn execute_step(&mut self, subject: &str, corr: &Id, step: &Step, offered: &[String]) -> Result<Value> {
        let a = &step.args;
        match step.op.as_str() {
            "entity.read" => {
                let id = arg_str(a, "id")?;
                let e = self.store.get_entity(&id).cloned().ok_or_else(|| AlethError::not_found("entity not found"))?;
                if e.deleted {
                    return Err(AlethError::not_found("entity deleted"));
                }
                // Verify: re-read from store.
                self.store.get_entity(&id).ok_or_else(|| AlethError::internal("verify: entity vanished"))?;
                let content = e.content_ref.as_ref()
                    .and_then(|h| self.store.get_blob(h))
                    .map(|b| String::from_utf8_lossy(b).to_string());
                // NOTE: content is DATA. It is returned, never interpreted as instructions (SEC-003).
                Ok(json!({"id": e.id, "type": e.etype, "version": e.version, "content": content}))
            }
            "entity.derive" => {
                let source = arg_str(a, "source")?;
                if self.store.get_entity(&source).is_none() {
                    return Err(AlethError::not_found("source entity not found"));
                }
                let into: EntityType = serde_json::from_value(a.get("into_type").cloned().unwrap_or(Value::Null))
                    .map_err(|_| AlethError::validation("into_type"))?;
                let content = arg_str(a, "content")?;
                let hash = self.store.put_blob(content.as_bytes())?;
                let mut prov = Provenance::of(subject);
                prov.source_entities = vec![source.clone()];
                prov.action_id = Some(corr.clone());
                let e = Entity {
                    id: new_id(),
                    etype: into,
                    content_ref: Some(hash),
                    version: 1,
                    version_chain: new_id(),
                    metadata: json!({}),
                    provenance: prov,
                    created_at: now(),
                    updated_at: now(),
                    deleted: false,
                };
                self.store.put_entity(&e)?;
                let rel = Relationship {
                    id: new_id(),
                    rtype: "derived_from".into(),
                    from: e.id.clone(),
                    to: source.clone(),
                    provenance: Provenance::of(subject),
                    created_at: now(),
                };
                self.store.put_relationship(&rel)?;
                // Verify: derived entity + relationship exist.
                self.store.get_entity(&e.id).ok_or_else(|| AlethError::internal("verify: derived missing"))?;
                self.store.get_relationship(&rel.id).ok_or_else(|| AlethError::internal("verify: edge missing"))?;
                Ok(json!({"derived_id": e.id, "relationship": rel.id, "source": source}))
            }
            "world.traverse" => {
                let from = arg_str(a, "from")?;
                let edge = arg_str(a, "edge")?;
                let ids = worldmodel::traverse(&self.store, &from, &edge, Dir::Incoming, 8);
                for id in &ids {
                    self.store.get_entity(id).ok_or_else(|| AlethError::internal("verify: traversal node missing"))?;
                }
                Ok(json!({"from": from, "edge": edge, "results": ids}))
            }
            "capability.grant" => {
                let gsubject = arg_str(a, "subject")?;
                let action = arg_str(a, "action")?;
                let scope_entities: Vec<String> =
                    serde_json::from_value(a.get("scope_entities").cloned().unwrap_or(json!([]))).unwrap_or_default();
                let approval = a.get("approval").and_then(|v| v.as_bool()).unwrap_or(false);
                let scope = if scope_entities.is_empty() { Scope::All } else { Scope::Entities(scope_entities) };
                let constraints = if approval { Constraints::approval() } else { Constraints::none() };
                let cap = self.grant_to(offered, &gsubject, &action, scope, constraints)?;
                self.caps.get(&cap.token).ok_or_else(|| AlethError::internal("verify: grant missing"))?;
                Ok(json!({"token": cap.token, "subject": gsubject, "action": action}))
            }
            "entity.restore_version" => {
                let chain = arg_str(a, "chain")?;
                let version = a.get("version").and_then(|v| v.as_u64()).ok_or_else(|| AlethError::validation("version"))?;
                let target = self.store.versions_of_chain(&chain).into_iter().find(|e| e.version == version).cloned()
                    .ok_or_else(|| AlethError::not_found("version not found"))?;
                let latest = self.store.latest_of_chain(&chain).cloned().ok_or_else(|| AlethError::not_found("chain not found"))?;
                let mut prov = Provenance::of(subject);
                prov.source_entities = vec![target.id.clone()];
                let e = Entity {
                    id: new_id(),
                    etype: target.etype,
                    content_ref: target.content_ref.clone(),
                    version: latest.version + 1,
                    version_chain: chain.clone(),
                    metadata: target.metadata.clone(),
                    provenance: prov,
                    created_at: latest.created_at,
                    updated_at: now(),
                    deleted: false,
                };
                self.store.put_entity(&e)?;
                let rel = Relationship {
                    id: new_id(),
                    rtype: "version_of".into(),
                    from: e.id.clone(),
                    to: target.id.clone(),
                    provenance: Provenance::of(subject),
                    created_at: now(),
                };
                self.store.put_relationship(&rel)?;
                let l = self.store.latest_of_chain(&chain).ok_or_else(|| AlethError::internal("verify: chain missing"))?;
                if l.id != e.id {
                    return Err(AlethError::internal("verify: restore not latest"));
                }
                Ok(json!({"restored_from_version": version, "new_version": e.version}))
            }
            "entity.delete" => {
                let id = arg_str(a, "id")?;
                let e = self.store.get_entity(&id).cloned().ok_or_else(|| AlethError::not_found("entity not found"))?;
                let latest = self.store.latest_of_chain(&e.version_chain).cloned().unwrap_or_else(|| e.clone());
                let tomb = Entity {
                    id: new_id(),
                    etype: e.etype,
                    content_ref: None,
                    version: latest.version + 1,
                    version_chain: e.version_chain.clone(),
                    metadata: json!({"deleted": true}),
                    provenance: Provenance::of(subject),
                    created_at: latest.created_at,
                    updated_at: now(),
                    deleted: true,
                };
                self.store.put_entity(&tomb)?;
                let l = self.store.latest_of_chain(&e.version_chain).ok_or_else(|| AlethError::internal("verify: chain missing"))?;
                if !l.deleted {
                    return Err(AlethError::internal("verify: delete not applied"));
                }
                Ok(json!({"deleted": id}))
            }
            other => Err(AlethError::validation(&format!("no executor for {}", other))),
        }
    }
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AlethError::validation(&format!("missing arg: {}", key)))
}
