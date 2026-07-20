//! WASM capability-secure component runtime (PRD §22 "Component & Application Model", P2, ADR-014).
//!
//! A component is UNTRUSTED code (an application, a tool, a third-party agent body). It runs in a
//! wasmi sandbox and can reach the operating system **only** through the explicit host functions
//! defined here. There is deliberately **no WASI**: standard WASI would hand the guest ambient
//! filesystem/clock/rand/env access, which violates INV-011 (no ambient authority) and SEC-003
//! (untrusted content is data, not instruction). Instead, every host call is authorized through the
//! *same* `CapEngine::evaluate` used by the deterministic pipeline, against the exact set of
//! capabilities the component was granted — nothing is inherited from the launcher.
//!
//! Effects (entity creation, event emission) flow through the *same* `Store` instance the System
//! Core owns, so a component's actions land in the one immutable audit log, not a side channel.
//! Execution is fuel-bounded: a runaway guest is trapped, never hangs the OS (resource isolation).
use crate::capabilities::{CapEngine, CapToken, Decision, Target};
use crate::domain::{new_id, now, Entity, EntityType, EventRecord, Id, Provenance};
use crate::storage::Store;
use serde::Serialize;
use serde_json::json;
use wasmi::{Caller, Config, Engine, Linker, Module, Store as WStore, TrapCode};

/// Capability actions the component ABI checks. These are ordinary action strings the existing
/// `CapEngine` matches; a `*` root covers all, an attenuated grant covers only what it names.
pub const READ_ACTION: &str = "entity.read";
pub const WRITE_ACTION: &str = "entity.write";
pub const EMIT_ACTION: &str = "event.emit";

// Host-call return codes seen by the guest (i64). Non-negative = success/result; negative = refused.
const OK_CODE: i64 = 0;
const DENIED: i64 = -1; // fail-closed: no capability
const APPROVAL: i64 = -2; // action needs human approval — refused at the component boundary
const BAD: i64 = -3; // malformed request (bad pointer, missing entity, …) — no effect

/// One host-call attempt and how the capability engine ruled on it. This is the component-level
/// extension of the explainable trace (EXP-005): every attempt is recorded, allowed or not.
#[derive(Debug, Clone, Serialize)]
pub struct HostCall {
    pub func: String,
    pub action: String,
    pub decision: String,
    pub target: Option<Id>,
}

/// The result of running one component: whether it completed, its exit code, whether it was killed
/// for exhausting fuel, any host-side error, the per-call audit, and the entities it created.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentOutcome {
    pub ok: bool,
    pub exit_code: i32,
    pub fuel_exhausted: bool,
    pub error: Option<String>,
    pub calls: Vec<HostCall>,
    pub wrote: Vec<Id>,
}

impl ComponentOutcome {
    fn load_err(msg: String) -> Self {
        ComponentOutcome { ok: false, exit_code: 0, fuel_exhausted: false, error: Some(msg), calls: vec![], wrote: vec![] }
    }
    /// True iff the component made a host call to `func` that the capability engine ALLOWED.
    pub fn allowed(&self, func: &str) -> bool {
        self.calls.iter().any(|c| c.func == func && c.decision == "ALLOW")
    }
    /// True iff the component *attempted* `func` and was denied (fail-closed).
    pub fn denied(&self, func: &str) -> bool {
        self.calls.iter().any(|c| c.func == func && c.decision == "DENY")
    }
}

/// Host state lent to wasmi for the duration of one run. It borrows the System Core's real store and
/// capability engine (not copies), so effects and authorization use the one source of truth.
struct HostState<'a> {
    caps: &'a CapEngine,
    store: &'a mut Store,
    /// The component's EXACT authority. Host calls evaluate only against this — never the launcher's.
    offered: Vec<CapToken>,
    subject: String,
    corr: Id,
    calls: Vec<HostCall>,
    wrote: Vec<Id>,
}

fn decision_str(d: &Decision) -> String {
    match d {
        Decision::Allow => "ALLOW".into(),
        Decision::Deny(_) => "DENY".into(),
        Decision::RequireApproval => "REQUIRE_APPROVAL".into(),
    }
}

/// Copy `len` bytes out of the guest's exported linear memory at `ptr`. Bounds-checked; a bad
/// pointer/length yields `None` (the host then returns BAD — untrusted input never traps the host).
fn guest_bytes(caller: &mut Caller<'_, HostState<'_>>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let mem = caller.get_export("memory")?.into_memory()?;
    let (start, len) = (ptr as usize, len as usize);
    let data = mem.data(&*caller);
    let end = start.checked_add(len)?;
    data.get(start..end).map(|s| s.to_vec())
}

fn host_write(caller: &mut Caller<'_, HostState<'_>>, bytes: Vec<u8>) -> i64 {
    let st = caller.data_mut();
    let target = Target { id: None, etype: Some(EntityType::Output) };
    let decision = st.caps.evaluate(WRITE_ACTION, &target, &st.offered);
    st.calls.push(HostCall { func: "write".into(), action: WRITE_ACTION.into(), decision: decision_str(&decision), target: None });
    match decision {
        Decision::Allow => {}
        Decision::RequireApproval => return APPROVAL,
        Decision::Deny(_) => return DENIED,
    }
    let hash = match st.store.put_blob(&bytes) {
        Ok(h) => h,
        Err(_) => return BAD,
    };
    let mut prov = Provenance::of(&st.subject);
    prov.action_id = Some(st.corr.clone());
    let entity = Entity {
        id: new_id(),
        etype: EntityType::Output,
        content_ref: Some(hash),
        version: 1,
        version_chain: new_id(),
        metadata: json!({ "origin": "component" }),
        provenance: prov,
        created_at: now(),
        updated_at: now(),
        deleted: false,
    };
    if st.store.put_entity(&entity).is_err() {
        return BAD;
    }
    let ev = EventRecord {
        id: new_id(),
        etype: "ComponentWroteEntity".into(),
        at: now(),
        correlation_id: st.corr.clone(),
        actor: st.subject.clone(),
        payload: json!({ "entity": entity.id, "bytes": bytes.len() }),
    };
    let _ = st.store.put_event(&ev);
    if let Some(c) = st.calls.last_mut() {
        c.target = Some(entity.id.clone());
    }
    st.wrote.push(entity.id);
    OK_CODE
}

fn host_read(caller: &mut Caller<'_, HostState<'_>>, bytes: Vec<u8>) -> i64 {
    let id = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return BAD,
    };
    let st = caller.data_mut();
    let etype = st.store.get_entity(&id).map(|e| e.etype);
    let target = Target { id: Some(id.clone()), etype };
    let decision = st.caps.evaluate(READ_ACTION, &target, &st.offered);
    st.calls.push(HostCall { func: "read".into(), action: READ_ACTION.into(), decision: decision_str(&decision), target: Some(id.clone()) });
    match decision {
        Decision::Allow => {}
        Decision::RequireApproval => return APPROVAL,
        Decision::Deny(_) => return DENIED,
    }
    // Effect: return the content byte-length — proves the capability-gated read succeeded. Content is
    // DATA; a richer return-buffer ABI (copying bytes back into guest memory) is a follow-on.
    match st.store.get_entity(&id) {
        Some(e) if !e.deleted => {
            let len = e.content_ref.as_ref().and_then(|h| st.store.get_blob(h)).map(|b| b.len()).unwrap_or(0);
            len as i64
        }
        _ => BAD,
    }
}

fn host_emit(caller: &mut Caller<'_, HostState<'_>>, bytes: Vec<u8>) -> i64 {
    let message = String::from_utf8_lossy(&bytes).to_string();
    let st = caller.data_mut();
    let decision = st.caps.evaluate(EMIT_ACTION, &Target::default(), &st.offered);
    st.calls.push(HostCall { func: "emit".into(), action: EMIT_ACTION.into(), decision: decision_str(&decision), target: None });
    match decision {
        Decision::Allow => {}
        Decision::RequireApproval => return APPROVAL,
        Decision::Deny(_) => return DENIED,
    }
    let ev = EventRecord {
        id: new_id(),
        etype: "ComponentEmitted".into(),
        at: now(),
        correlation_id: st.corr.clone(),
        actor: st.subject.clone(),
        payload: json!({ "message": message }),
    };
    let _ = st.store.put_event(&ev);
    OK_CODE
}

/// Run an untrusted WASM component against the System Core's store + capability engine.
///
/// `offered` is the component's exact authority. `fuel` bounds execution. The component must export
/// a `run() -> i32` entry point and an exported `memory`. Returns a full outcome; an unauthorized
/// call changes nothing, and a trap (including fuel exhaustion) cannot corrupt state — effects are
/// all-or-nothing per host call, which is the store's append granularity.
pub fn run(
    caps: &CapEngine,
    store: &mut Store,
    offered: &[CapToken],
    subject: &str,
    wasm: &[u8],
    fuel: u64,
) -> ComponentOutcome {
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);

    let module = match Module::new(&engine, wasm) {
        Ok(m) => m,
        Err(e) => return ComponentOutcome::load_err(format!("module load: {e}")),
    };

    let host = HostState {
        caps,
        store,
        offered: offered.to_vec(),
        subject: subject.to_string(),
        corr: new_id(),
        calls: Vec::new(),
        wrote: Vec::new(),
    };
    let mut wstore = WStore::new(&engine, host);
    if let Err(e) = wstore.set_fuel(fuel) {
        return ComponentOutcome::load_err(format!("set fuel: {e}"));
    }

    let mut linker = Linker::new(&engine);
    linker
        .func_wrap("aletheia", "read", |mut c: Caller<'_, HostState<'_>>, ptr: i32, len: i32| -> i64 {
            match guest_bytes(&mut c, ptr, len) {
                Some(b) => host_read(&mut c, b),
                None => BAD,
            }
        })
        .expect("define read");
    linker
        .func_wrap("aletheia", "write", |mut c: Caller<'_, HostState<'_>>, ptr: i32, len: i32| -> i64 {
            match guest_bytes(&mut c, ptr, len) {
                Some(b) => host_write(&mut c, b),
                None => BAD,
            }
        })
        .expect("define write");
    linker
        .func_wrap("aletheia", "emit", |mut c: Caller<'_, HostState<'_>>, ptr: i32, len: i32| -> i64 {
            match guest_bytes(&mut c, ptr, len) {
                Some(b) => host_emit(&mut c, b),
                None => BAD,
            }
        })
        .expect("define emit");

    let instance = match linker.instantiate_and_start(&mut wstore, &module) {
        Ok(i) => i,
        Err(e) => {
            let fuel_out = e.as_trap_code() == Some(TrapCode::OutOfFuel);
            return finish(&wstore, false, 0, Some(format!("instantiate: {e}")), fuel_out);
        }
    };
    let run_fn = match instance.get_typed_func::<(), i32>(&wstore, "run") {
        Ok(f) => f,
        Err(_) => return finish(&wstore, false, 0, Some("component has no `run() -> i32` export".into()), false),
    };

    let (ok, code, err, fuel_out) = match run_fn.call(&mut wstore, ()) {
        Ok(code) => (true, code, None, false),
        Err(e) => {
            let fuel_out = e.as_trap_code() == Some(TrapCode::OutOfFuel);
            (false, 0, Some(format!("{e}")), fuel_out)
        }
    };
    finish(&wstore, ok, code, err, fuel_out)
}

fn finish(wstore: &WStore<HostState<'_>>, ok: bool, exit_code: i32, error: Option<String>, fuel_exhausted: bool) -> ComponentOutcome {
    let st = wstore.data();
    ComponentOutcome { ok, exit_code, fuel_exhausted, error, calls: st.calls.clone(), wrote: st.wrote.clone() }
}
