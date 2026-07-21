//! System Core composition + the deterministic Intent→Action pipeline + task runtime (SAD §8/§10).
//! This is the deterministic authority: it validates, authorizes (capabilities), executes, verifies,
//! and records. The only probabilistic step is interpretation; everything after is deterministic.
use crate::agents::Agent;
use crate::capabilities::{CapEngine, Constraints, Decision, Scope, StoredCapability, Target};
use crate::domain::*;
use crate::intelligence::{DeterministicRuntime, ModelRuntime};
use crate::intent_action::{parse_plan, validate_plan, Intent, Step, Trace};
use crate::policy::{ApprovalState, ApprovalStore, ApprovalVerdict, PendingApproval, PolicyEngine};
use crate::storage::Store;
use crate::tools;
use crate::worldmodel::{self, Dir};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Bound on component spawn depth (multi-agent composition), so a spawn cycle cannot exhaust the
/// system. Effect authority already attenuates to nothing down a chain; this bounds resource use.
const MAX_SPAWN_DEPTH: usize = 8;

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
    policy: PolicyEngine,
    approvals: ApprovalStore,
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
        let mut core = SysCore {
            store,
            caps,
            model,
            fallback: DeterministicRuntime,
            policy: PolicyEngine::new(),
            approvals: ApprovalStore::new(),
            tasks: HashMap::new(),
            cancelled: HashSet::new(),
        };
        core.rebuild_approvals();
        Ok(core)
    }

    /// Open with the AI provider selected from `AiConfig` (env-configured, ADR-017): the real local
    /// model is the primary interpreter, with the deterministic interpreter as fallback whenever the
    /// model is unavailable (INT-004). The OS is fully functional with no resident model.
    pub fn open_default(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open(dir, crate::ai::select_provider(&crate::ai::config::AiConfig::from_env()))
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

    // --- components (P2: applications as capability-secure WASM, ADR-014) ---

    /// Install an untrusted WASM component as a first-class `Application` entity: its code is stored
    /// as an encrypted content-addressed blob, and installing at all requires the `component.install`
    /// capability. The install is itself a recorded, authorized action.
    pub fn install_component(&mut self, offered: &[String], subject: &str, name: &str, wasm: &[u8]) -> Result<Entity> {
        let target = Target { id: None, etype: Some(EntityType::Application) };
        if !matches!(self.caps.evaluate("component.install", &target, offered), Decision::Allow) {
            self.emit("CapabilityDenied", &new_id(), subject, json!({"action": "component.install"}));
            return Err(AlethError::authorization("not permitted to install components"));
        }
        let hash = self.store.put_blob(wasm)?;
        let e = Entity {
            id: new_id(),
            etype: EntityType::Application,
            content_ref: Some(hash),
            version: 1,
            version_chain: new_id(),
            metadata: json!({"name": name, "kind": "wasm-component", "bytes": wasm.len()}),
            provenance: Provenance::of(subject),
            created_at: now(),
            updated_at: now(),
            deleted: false,
        };
        self.store.put_entity(&e)?;
        self.emit("ComponentInstalled", &new_id(), subject, json!({"app": e.id, "name": name, "bytes": wasm.len()}));
        Ok(e)
    }

    /// Launch an untrusted component. Two distinct authority layers (application-as-capability):
    /// `launch_caps` must satisfy `component.run` (the right to run any component at all), while the
    /// component executes with EXACTLY `grant_caps` as its authority — nothing is inherited from the
    /// launcher, so a component with an empty grant can do nothing (INV-011, no ambient authority).
    pub fn run_component(
        &mut self,
        launch_caps: &[String],
        grant_caps: &[String],
        subject: &str,
        wasm: &[u8],
        fuel: u64,
    ) -> Result<crate::component::ComponentOutcome> {
        if !matches!(self.caps.evaluate("component.run", &Target::default(), launch_caps), Decision::Allow) {
            self.emit("CapabilityDenied", &new_id(), subject, json!({"action": "component.run"}));
            return Err(AlethError::authorization("not permitted to run components"));
        }
        // Launch is authorized once at the top; this component and any children it spawns then run
        // with capabilities strictly attenuated down the tree (no child can exceed its parent).
        Ok(self.compose_run(grant_caps, subject, wasm, fuel, 0))
    }

    /// Run a component and fulfil any children it spawns (multi-agent composition). Each child runs
    /// with a capability ATTENUATED from this component's grant — delegated through the cap engine,
    /// which rejects amplification — so no child can exceed its parent's authority. Spawn depth is
    /// bounded (`MAX_SPAWN_DEPTH`) so a spawn cycle cannot exhaust the system.
    fn compose_run(&mut self, grant_caps: &[String], subject: &str, wasm: &[u8], fuel: u64, depth: usize) -> crate::component::ComponentOutcome {
        // Split borrow of self: `&self.caps` (read) + `&mut self.store` (effects) are disjoint fields.
        let mut outcome = crate::component::run(&self.caps, &mut self.store, grant_caps, subject, wasm, fuel);
        self.emit(
            "ComponentRan",
            &new_id(),
            subject,
            json!({
                "ok": outcome.ok,
                "exit_code": outcome.exit_code,
                "fuel_exhausted": outcome.fuel_exhausted,
                "host_calls": outcome.calls.len(),
                "wrote": outcome.wrote.len(),
                "spawns": outcome.spawns.len()
            }),
        );
        if depth >= MAX_SPAWN_DEPTH {
            if !outcome.spawns.is_empty() {
                self.emit("ComponentSpawnDenied", &new_id(), subject, json!({"reason": "max spawn depth", "depth": depth}));
            }
            return outcome;
        }
        let requests = outcome.spawns.clone();
        let mut children = Vec::new();
        for req in requests {
            if let Some((child_wasm, child_grant, child_subject)) = self.prepare_spawn(grant_caps, subject, &req) {
                self.emit(
                    "ComponentSpawned",
                    &new_id(),
                    subject,
                    json!({"app": req.app_id, "action": req.action, "child": child_subject, "granted": !child_grant.is_empty()}),
                );
                let child_outcome = self.compose_run(&child_grant, &child_subject, &child_wasm, fuel, depth + 1);
                children.push(child_outcome);
            }
        }
        outcome.spawned = children;
        outcome
    }

    /// Resolve a spawn request: load the child's code and delegate an attenuated capability for the
    /// requested action from the parent's grant. Returns None if the app is unknown/not runnable.
    /// The child grant is empty when the parent holds nothing covering the requested action — the
    /// child then runs but can do nothing (it cannot exceed the parent).
    fn prepare_spawn(&mut self, parent_caps: &[String], parent_subject: &str, req: &crate::component::SpawnRequest) -> Option<(Vec<u8>, Vec<String>, String)> {
        let app = self.store.get_entity(&req.app_id).cloned()?;
        if app.etype != EntityType::Application {
            return None;
        }
        let hash = app.content_ref.clone()?;
        let wasm = self.store.get_blob(&hash).cloned()?;
        let child_subject = format!("{}>{}", parent_subject, req.app_id);
        let child_grant: Vec<String> = self.attenuate_for_child(parent_caps, &child_subject, &req.action).into_iter().collect();
        Some((wasm, child_grant, child_subject))
    }

    /// Delegate a capability for `action` to `child_subject`, attenuated from whichever parent cap
    /// covers it (same scope + constraints — no amplification). Returns None if no parent cap covers
    /// the action; the cap engine's attenuation rule is what enforces "child <= parent".
    fn attenuate_for_child(&mut self, parent_caps: &[String], child_subject: &str, action: &str) -> Option<String> {
        for pt in parent_caps {
            let attn = self.caps.get(pt).map(|p| (p.scope.clone(), p.constraints.clone()));
            if let Some((scope, cons)) = attn {
                if let Ok(child) = self.caps.delegate(pt, child_subject, action, scope, cons, child_subject) {
                    let _ = self.store.put_capability(&child);
                    return Some(child.token);
                }
            }
        }
        None
    }

    /// Launch a previously-installed component by its `Application` entity id (loads code from the store).
    pub fn run_installed(
        &mut self,
        launch_caps: &[String],
        grant_caps: &[String],
        subject: &str,
        app_id: &Id,
        fuel: u64,
    ) -> Result<crate::component::ComponentOutcome> {
        let app = self.store.get_entity(app_id).cloned().ok_or_else(|| AlethError::not_found("application not found"))?;
        if app.etype != EntityType::Application {
            return Err(AlethError::validation("entity is not an application"));
        }
        let hash = app.content_ref.clone().ok_or_else(|| AlethError::validation("application has no code"))?;
        let wasm = self.store.get_blob(&hash).cloned().ok_or_else(|| AlethError::not_found("application code missing"))?;
        self.run_component(launch_caps, grant_caps, subject, &wasm, fuel)
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

    // --- approvals (policy layer, ADR-015; SAD §10 `approve()`) ---

    /// Record a durable pending approval bound to the exact intent. The immutable event log is the
    /// source of truth (`ApprovalRequested` carries the full serialized record); the in-memory
    /// registry is a replayable projection of it.
    fn record_pending_approval(&mut self, intent: &Intent, reason: &str) -> PendingApproval {
        let pa = PendingApproval::new(&intent.subject, intent.clone(), reason);
        self.approvals.insert(pa.clone());
        self.emit("ApprovalRequested", &new_id(), &pa.subject, serde_json::to_value(&pa).unwrap_or(Value::Null));
        pa
    }

    /// A human grants or denies a pending approval. Granting re-runs the EXACT bound intent with
    /// approval satisfied — approval confers no authority, so the offered capabilities are still
    /// re-evaluated and can independently deny. Denying records the decision and executes nothing.
    pub fn resolve_approval(&mut self, offered: &[String], approval_id: &Id, granted: bool) -> Result<Trace> {
        let pa = self.approvals.get(approval_id).cloned().ok_or_else(|| AlethError::not_found("approval not found"))?;
        if pa.state != ApprovalState::Pending {
            return Err(AlethError::conflict("approval already resolved"));
        }
        if pa.is_expired(now()) {
            self.approvals.mark_state(approval_id, ApprovalState::Expired);
            self.emit("ApprovalResolved", &new_id(), &pa.subject, json!({"approval": approval_id, "state": "Expired"}));
            return Err(AlethError::conflict("approval expired"));
        }
        self.approvals.mark_state(approval_id, if granted { ApprovalState::Granted } else { ApprovalState::Denied });
        self.emit("ApprovalResolved", &new_id(), &pa.subject, json!({"approval": approval_id, "granted": granted}));
        if !granted {
            let mut trace = Trace::new(&pa.subject, new_id());
            trace.intent = format!("{:?}", pa.intent.verb);
            trace.approval = "denied by human".into();
            trace.execution = "not executed — approval denied".into();
            return Ok(trace);
        }
        let task = self.begin_task(&pa.subject);
        Ok(self.run_intent(&task, offered, pa.intent, true))
    }

    /// All approvals still awaiting a human decision (freshest first).
    pub fn list_pending_approvals(&self) -> Vec<PendingApproval> {
        self.approvals.list_pending(now())
    }
    pub fn get_approval(&self, id: &Id) -> Option<PendingApproval> {
        self.approvals.get(id).cloned()
    }

    /// Rebuild the approval registry from the immutable event log on open (persistence across
    /// restart, AT-003). `ApprovalRequested` carries the full serialized `PendingApproval`;
    /// `ApprovalResolved` carries the terminal state.
    fn rebuild_approvals(&mut self) {
        let events: Vec<EventRecord> = self.store.events().to_vec();
        for ev in &events {
            match ev.etype.as_str() {
                "ApprovalRequested" => {
                    if let Ok(pa) = serde_json::from_value::<PendingApproval>(ev.payload.clone()) {
                        self.approvals.insert(pa);
                    }
                }
                "ApprovalResolved" => {
                    if let Some(id) = ev.payload.get("approval").and_then(|v| v.as_str()) {
                        let state = match (ev.payload.get("granted").and_then(|v| v.as_bool()), ev.payload.get("state").and_then(|v| v.as_str())) {
                            (Some(true), _) => ApprovalState::Granted,
                            (Some(false), _) => ApprovalState::Denied,
                            (_, Some("Expired")) => ApprovalState::Expired,
                            _ => ApprovalState::Denied,
                        };
                        self.approvals.mark_state(&id.to_string(), state);
                    }
                }
                _ => {}
            }
        }
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

        // Context via the native capability-aware Context Engine (ADR-018): structured world +
        // relationship + memory retrieval, each entity authorized BEFORE inclusion, budgeted for the
        // small model. Not RAG — the World Model is the source of truth. The model receives a compact
        // rendering of this, never a raw store dump.
        let aictx = crate::ai::context::ContextEngine::new().build(
            &self.store,
            &self.caps,
            offered,
            &intent.subject,
            &intent,
            crate::ai::context::ContextBudget::small(),
        );
        trace.context_provenance = aictx.provenance();
        // Compact, budgeted rendering handed to the model (never a raw store dump).
        let ctx_brief = aictx.render(crate::ai::context::ContextBudget::small().max_chars);

        // Interpretation — the ONLY probabilistic stage. Model-unhealthy → deterministic fallback.
        // Model-healthy-but-errored → this request fails (no silent fallback); state stays intact.
        let raw = if self.model.healthy() {
            trace.interpreter = self.model.name().into();
            match self.model.interpret_with_context(&intent, &ctx_brief) {
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
            match self.fallback.interpret_with_context(&intent, &ctx_brief) {
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
            // Authority axis (capability engine): what the held capabilities permit.
            trace.capability_decision = match &decision {
                Decision::Allow => format!("ALLOW ({})", meta.action),
                Decision::RequireApproval => format!("REQUIRE_APPROVAL ({})", meta.action),
                Decision::Deny(_) => unreachable!("deny handled above"),
            };
            // Governance axis (policy engine), INDEPENDENT of authority (ADR-015): even an authorized
            // action may need a human to approve it (destructive risk, or an approval-constrained cap).
            match self.policy.evaluate(&decision, meta.risk) {
                ApprovalVerdict::NotRequired => trace.approval = "not required".into(),
                ApprovalVerdict::Required { reason } => {
                    if approve {
                        trace.approval = format!("approved ({})", reason);
                    } else {
                        // Record a durable pending approval bound to this exact intent; stop with no
                        // effect. A human later grants/denies it via `resolve_approval`.
                        let pa = self.record_pending_approval(&intent, &reason);
                        trace.approval = format!("pending [{}] — {}", pa.id, reason);
                        trace.approval_id = Some(pa.id.clone());
                        self.tasks.insert(task_id.clone(), TaskState::AwaitingApproval);
                        self.emit("AIActionProposed", task_id, &intent.subject, json!({"action": meta.action, "needs_approval": true, "approval": pa.id}));
                        return trace;
                    }
                }
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
