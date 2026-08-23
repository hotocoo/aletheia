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
//! Execution is fuel-bounded (REQ-COMP-002) and — since ALET-P1-021 / ADR-065 — bounded by an
//! explicit resource model beyond fuel: linear-memory bytes, table elements, call-stack depth and
//! recursion depth are hard caps ([`SandboxLimits`]), and a wall-clock deadline is enforced at
//! every host-call crossing. Every bound has a fail-closed default; an unbounded sandbox is not
//! constructible without naming each dimension deliberately.
use crate::capabilities::{CapEngine, CapToken, Decision, Target};
use crate::domain::{new_id, now, Entity, EntityType, EventRecord, Id, Provenance};
use crate::storage::Store;
use serde::Serialize;
use serde_json::json;
use std::time::{Duration, Instant};
use wasmi::{
    Caller, Config, Engine, Error as WasmiError, Linker, Module, Store as WStore, StoreLimits,
    StoreLimitsBuilder, TrapCode,
};

/// Capability actions the component ABI checks. These are ordinary action strings the existing
/// `CapEngine` matches; a `*` root covers all, an attenuated grant covers only what it names.
pub const READ_ACTION: &str = "entity.read";
pub const WRITE_ACTION: &str = "entity.write";
pub const EMIT_ACTION: &str = "event.emit";
pub const SPAWN_ACTION: &str = "component.spawn";

/// Marker carried by the trap a deadline kill raises, so an outcome can name WHY it stopped
/// without matching on error text written for humans.
pub const DEADLINE_TRAP: &str = "aletheia-sandbox-deadline";

/// Explicit per-run resource bounds beyond fuel (ALET-P1-021, ADR-065).
///
/// Fuel bounds HOW MUCH COMPUTE a guest may buy; these bounds cap the four resources fuel does not
/// measure: linear-memory BYTES, table ELEMENTS, call-stack depth (slots and frames), and WALL-CLOCK
/// time. Every field is a hard cap with a fail-closed default — there is no constructor that yields
/// "no limit", because a sandbox whose limits were forgotten must fail small, not run wide.
///
/// # What each bound does when hit
///
/// * `max_memory_bytes` / `max_table_elements` — enforced by wasmi's store limiter. A growth past
///   the cap FAILS (`memory.grow`/`table.grow` answer -1 as the spec allows); the guest keeps
///   running inside the cap it was given. The cap holds even against a guest that ignores the -1;
///   such a guest is still burning fuel, which remains its compute bound.
/// * `max_stack_height` / `max_recursion_depth` — engine compilation limits; exceeding either
///   traps with `TrapCode::StackOverflow`, reported as [`KillReason::Stack`].
/// * `deadline_ms` — wall clock. Enforced at every HOST-CALL CROSSING: a guest that crosses after
///   its deadline has passed is trapped and reported as [`KillReason::Deadline`]. Between crossings
///   fuel bounds the work, so a pure-compute guest that never calls the host is bounded by fuel
///   alone; if such a guest finishes having overrun its deadline, the overrun is REPORTED in the
///   outcome's `deadline_exceeded` rather than silently dropped.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxLimits {
    /// Linear-memory ceiling in BYTES across all of the guest's growth, initial included.
    pub max_memory_bytes: usize,
    /// Table-element ceiling per table.
    pub max_table_elements: usize,
    /// Maximum operand/stack slots the compiled functions may address.
    pub max_stack_height: usize,
    /// Maximum call-frame recursion depth.
    pub max_recursion_depth: usize,
    /// Wall-clock budget in milliseconds from run start; `0` means unbounded, which must be
    /// WRITTEN to be had — the default bounds every dimension.
    pub deadline_ms: u64,
}

impl SandboxLimits {
    /// The shipped defaults (ADR-065): 4 MiB of linear memory, 1024 table elements, 16 Ki stack
    /// slots, 256 frames of recursion, a 30 s wall-clock budget. Chosen so every component in this
    /// repository's suites passes with room to spare while no dimension is infinite.
    pub const fn defaults() -> Self {
        Self {
            max_memory_bytes: 4 * 1024 * 1024,
            max_table_elements: 1024,
            max_stack_height: 16 * 1024,
            max_recursion_depth: 256,
            deadline_ms: 30_000,
        }
    }

    fn deadline(&self) -> Option<Duration> {
        if self.deadline_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.deadline_ms))
        }
    }
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Why a run was killed by ITS OWN BOUNDS (as opposed to trapping on its own bug). Reported in
/// [`ComponentOutcome::killed_by`] so the audit log names the bound that held, not just "an error".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum KillReason {
    /// Fuel bought by the run ran out.
    Fuel,
    /// The wall-clock deadline passed at a host-call crossing; the crossing was refused.
    Deadline,
    /// Call-stack depth or height exceeded its configured cap.
    Stack,
}

impl KillReason {
    /// Stable short name for logs and the audit event.
    pub const fn as_str(self) -> &'static str {
        match self {
            KillReason::Fuel => "fuel",
            KillReason::Deadline => "deadline",
            KillReason::Stack => "stack",
        }
    }
}

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

/// A request by a component to spawn another installed component (multi-agent composition). It names
/// the child application and the capability action the parent wants the child to have. The System
/// Core fulfils it AFTER this component finishes, delegating an ATTENUATED capability from the
/// parent's own authority — so the child can never exceed the parent (enforced by the cap engine).
#[derive(Debug, Clone, Serialize)]
pub struct SpawnRequest {
    pub app_id: String,
    pub action: String,
}

/// The result of running one component: whether it completed, its exit code, which of the run's own
/// bounds killed it (fuel, deadline, stack — named, never guessed from error text), any host-side
/// error, the per-call audit, and the entities it created.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentOutcome {
    pub ok: bool,
    pub exit_code: i32,
    pub fuel_exhausted: bool,
    /// The bound that KILLED this run ([`KillReason`]) — `None` for every other ending, including
    /// ordinary traps that were the guest's own bug rather than its bounds holding.
    pub killed_by: Option<KillReason>,
    /// True when the run's wall clock passed its deadline at ANY observed point — a crossing kill
    /// OR a completion-time overrun the crossing rule could not stop. An overrun is reported even
    /// when the guest finished, because silence would read as compliance.
    pub deadline_exceeded: bool,
    pub error: Option<String>,
    pub calls: Vec<HostCall>,
    pub wrote: Vec<Id>,
    /// Spawn requests this component made (fulfilled by the System Core after the run).
    pub spawns: Vec<SpawnRequest>,
    /// Outcomes of the children the System Core spawned on this component's behalf.
    pub spawned: Vec<ComponentOutcome>,
}

impl ComponentOutcome {
    fn load_err(msg: String) -> Self {
        ComponentOutcome {
            ok: false,
            exit_code: 0,
            fuel_exhausted: false,
            killed_by: None,
            deadline_exceeded: false,
            error: Some(msg),
            calls: vec![],
            wrote: vec![],
            spawns: vec![],
            spawned: vec![],
        }
    }
    /// True iff the component made a host call to `func` that the capability engine ALLOWED.
    pub fn allowed(&self, func: &str) -> bool {
        self.calls
            .iter()
            .any(|c| c.func == func && c.decision == "ALLOW")
    }
    /// True iff the component *attempted* `func` and was denied (fail-closed).
    pub fn denied(&self, func: &str) -> bool {
        self.calls
            .iter()
            .any(|c| c.func == func && c.decision == "DENY")
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
    spawns: Vec<SpawnRequest>,
    /// The run's resource bounds. Lives here so wasmi's limiter closure can reach it —
    /// `Store::limiter` takes a closure over the store's own data.
    sandbox: StoreLimits,
    /// When this run started (wall clock, for [`SandboxLimits::deadline_ms`]).
    started: Instant,
    /// The wall-clock budget, pre-resolved (`None` = explicitly unbounded).
    deadline: Option<Duration>,
    /// Set the first time a host-call crossing finds the deadline already passed.
    deadline_hit: bool,
}

impl<'a> HostState<'a> {
    /// Has this run's wall-clock budget been exhausted? Records the hit once, so exactly one
    /// crossing is named as the kill even if the guest would cross again.
    fn deadline_passed(&mut self) -> bool {
        match self.deadline {
            Some(d) => {
                let passed = self.started.elapsed() > d;
                if passed {
                    self.deadline_hit = true;
                }
                passed
            }
            None => false,
        }
    }
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

/// Wall-clock gate at a host-call crossing (ADR-065): a guest that arrives after its deadline has
/// passed is refused BY NAME — the attempt is audited as `DEADLINE` and the run is trapped, so no
/// further effect can be authorized on a clock the run has already outrun.
fn gate_deadline(
    caller: &mut Caller<'_, HostState<'_>>,
    func: &str,
    action: &str,
) -> Result<(), WasmiError> {
    let st = caller.data_mut();
    if st.deadline_passed() {
        st.calls.push(HostCall {
            func: func.into(),
            action: action.into(),
            decision: "DEADLINE".into(),
            target: None,
        });
        return Err(WasmiError::new(DEADLINE_TRAP));
    }
    Ok(())
}

fn host_write(caller: &mut Caller<'_, HostState<'_>>, bytes: Vec<u8>) -> Result<i64, WasmiError> {
    gate_deadline(caller, "write", WRITE_ACTION)?;
    let st = caller.data_mut();
    let target = Target {
        id: None,
        etype: Some(EntityType::Output),
    };
    let decision = st.caps.evaluate(WRITE_ACTION, &target, &st.offered);
    st.calls.push(HostCall {
        func: "write".into(),
        action: WRITE_ACTION.into(),
        decision: decision_str(&decision),
        target: None,
    });
    match decision {
        Decision::Allow => {}
        Decision::RequireApproval => return Ok(APPROVAL),
        Decision::Deny(_) => return Ok(DENIED),
    }
    let hash = match st.store.put_blob(&bytes) {
        Ok(h) => h,
        Err(_) => return Ok(BAD),
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
        return Ok(BAD);
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
    Ok(OK_CODE)
}

fn host_read(
    caller: &mut Caller<'_, HostState<'_>>,
    id_bytes: Vec<u8>,
    out_ptr: i32,
    out_cap: i32,
) -> Result<i64, WasmiError> {
    gate_deadline(caller, "read", READ_ACTION)?;
    let id = match String::from_utf8(id_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(BAD),
    };
    if out_ptr < 0 || out_cap < 0 {
        return Ok(BAD);
    }
    // Authorize + fetch the content as an owned copy so the store borrow ends before we touch the
    // guest's memory. `return` inside the block exits the whole host call (denied/approval/bad).
    let content: Vec<u8> = {
        let st = caller.data_mut();
        let etype = st.store.get_entity(&id).map(|e| e.etype);
        let target = Target {
            id: Some(id.clone()),
            etype,
        };
        let decision = st.caps.evaluate(READ_ACTION, &target, &st.offered);
        st.calls.push(HostCall {
            func: "read".into(),
            action: READ_ACTION.into(),
            decision: decision_str(&decision),
            target: Some(id.clone()),
        });
        match decision {
            Decision::Allow => {}
            Decision::RequireApproval => return Ok(APPROVAL),
            Decision::Deny(_) => return Ok(DENIED),
        }
        match st.store.get_entity(&id) {
            Some(e) if !e.deleted => e
                .content_ref
                .as_ref()
                .and_then(|h| st.store.get_blob(h))
                .cloned()
                .unwrap_or_default(),
            _ => return Ok(BAD),
        }
    };
    // Copy up to `out_cap` bytes of content into the guest's linear memory at `out_ptr`. The content
    // is DATA the component is authorized to consume; it is never interpreted as instruction (SEC-003).
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return Ok(BAD),
    };
    let n = content.len().min(out_cap as usize);
    if mem
        .write(&mut *caller, out_ptr as usize, &content[..n])
        .is_err()
    {
        return Ok(BAD);
    }
    // Return the FULL content length; the guest compares it to its capacity to detect truncation.
    Ok(content.len() as i64)
}

fn host_emit(caller: &mut Caller<'_, HostState<'_>>, bytes: Vec<u8>) -> Result<i64, WasmiError> {
    gate_deadline(caller, "emit", EMIT_ACTION)?;
    let message = String::from_utf8_lossy(&bytes).to_string();
    let st = caller.data_mut();
    let decision = st
        .caps
        .evaluate(EMIT_ACTION, &Target::default(), &st.offered);
    st.calls.push(HostCall {
        func: "emit".into(),
        action: EMIT_ACTION.into(),
        decision: decision_str(&decision),
        target: None,
    });
    match decision {
        Decision::Allow => {}
        Decision::RequireApproval => return Ok(APPROVAL),
        Decision::Deny(_) => return Ok(DENIED),
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
    Ok(OK_CODE)
}

/// Record a spawn request (multi-agent composition). The parent names a child application and the
/// capability action it wants the child to have; the System Core fulfils it after this run, giving
/// the child an ATTENUATED capability delegated from the parent (never more than the parent holds).
fn host_spawn(
    caller: &mut Caller<'_, HostState<'_>>,
    app_bytes: Vec<u8>,
    action_bytes: Vec<u8>,
) -> Result<i64, WasmiError> {
    gate_deadline(caller, "spawn", SPAWN_ACTION)?;
    let app_id = match String::from_utf8(app_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(BAD),
    };
    let action = match String::from_utf8(action_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(BAD),
    };
    let st = caller.data_mut();
    st.calls.push(HostCall {
        func: "spawn".into(),
        action: SPAWN_ACTION.into(),
        decision: "QUEUED".into(),
        target: Some(app_id.clone()),
    });
    st.spawns.push(SpawnRequest { app_id, action });
    Ok(OK_CODE)
}

/// Run an untrusted WASM component against the System Core's store + capability engine under the
/// DEFAULT sandbox bounds ([`SandboxLimits::defaults`]).
///
/// `offered` is the component's exact authority. `fuel` bounds execution. The component must export
/// a `run() -> i32` entry point and an exported `memory`. Returns a full outcome; an unauthorized
/// call changes nothing, and a trap (fuel, deadline or stack — the bounds holding; anything else the
/// guest's own bug) cannot corrupt state — effects are all-or-nothing per host call, which is the
/// store's append granularity.
pub fn run(
    caps: &CapEngine,
    store: &mut Store,
    offered: &[CapToken],
    subject: &str,
    wasm: &[u8],
    fuel: u64,
) -> ComponentOutcome {
    run_with_limits(
        caps,
        store,
        offered,
        subject,
        wasm,
        fuel,
        &SandboxLimits::defaults(),
    )
}

/// Run an untrusted component under EXPLICIT resource bounds (ALET-P1-021, ADR-065): fuel for
/// compute, [`SandboxLimits`] for memory bytes, table elements, stack depth/height and wall clock.
pub fn run_with_limits(
    caps: &CapEngine,
    store: &mut Store,
    offered: &[CapToken],
    subject: &str,
    wasm: &[u8],
    fuel: u64,
    limits: &SandboxLimits,
) -> ComponentOutcome {
    let mut config = Config::default();
    config.consume_fuel(true);
    // Stack bounds are ENGINE compilation limits in wasmi (not store limiter state), and the engine
    // here is created fresh for THIS run — so an engine-level cap is exactly a per-run cap.
    config.set_max_stack_height(limits.max_stack_height);
    config.set_max_recursion_depth(limits.max_recursion_depth);
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
        spawns: Vec::new(),
        // Memory/table caps ride wasmi's store limiter; growth past a cap fails (spec -1) rather
        // than trapping, so a guest that checks the answer keeps running INSIDE its cap.
        sandbox: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .table_elements(limits.max_table_elements)
            .memories(1)
            .tables(1)
            .instances(1)
            .trap_on_grow_failure(false)
            .build(),
        started: Instant::now(),
        deadline: limits.deadline(),
        deadline_hit: false,
    };
    let mut wstore = WStore::new(&engine, host);
    // The limiter closure borrows the limits from the store's own data, so they live and die with
    // this one run — no static, no ambient configuration.
    wstore.limiter(|st: &mut HostState| &mut st.sandbox);
    if let Err(e) = wstore.set_fuel(fuel) {
        return ComponentOutcome::load_err(format!("set fuel: {e}"));
    }

    let mut linker = Linker::new(&engine);
    linker
        .func_wrap(
            "aletheia",
            "read",
            |mut c: Caller<'_, HostState<'_>>,
             id_ptr: i32,
             id_len: i32,
             out_ptr: i32,
             out_cap: i32|
             -> Result<i64, WasmiError> {
                match guest_bytes(&mut c, id_ptr, id_len) {
                    Some(b) => host_read(&mut c, b, out_ptr, out_cap),
                    None => Ok(BAD),
                }
            },
        )
        .expect("define read");
    linker
        .func_wrap(
            "aletheia",
            "write",
            |mut c: Caller<'_, HostState<'_>>, ptr: i32, len: i32| -> Result<i64, WasmiError> {
                match guest_bytes(&mut c, ptr, len) {
                    Some(b) => host_write(&mut c, b),
                    None => Ok(BAD),
                }
            },
        )
        .expect("define write");
    linker
        .func_wrap(
            "aletheia",
            "emit",
            |mut c: Caller<'_, HostState<'_>>, ptr: i32, len: i32| -> Result<i64, WasmiError> {
                match guest_bytes(&mut c, ptr, len) {
                    Some(b) => host_emit(&mut c, b),
                    None => Ok(BAD),
                }
            },
        )
        .expect("define emit");
    linker
        .func_wrap(
            "aletheia",
            "spawn",
            |mut c: Caller<'_, HostState<'_>>,
             app_ptr: i32,
             app_len: i32,
             act_ptr: i32,
             act_len: i32|
             -> Result<i64, WasmiError> {
                match (
                    guest_bytes(&mut c, app_ptr, app_len),
                    guest_bytes(&mut c, act_ptr, act_len),
                ) {
                    (Some(app), Some(act)) => host_spawn(&mut c, app, act),
                    _ => Ok(BAD),
                }
            },
        )
        .expect("define spawn");

    let instance = match linker.instantiate_and_start(&mut wstore, &module) {
        Ok(i) => i,
        Err(e) => {
            let killed = classify_kill(wstore.data().deadline_hit, e.as_trap_code());
            return finish(&wstore, false, 0, Some(format!("instantiate: {e}")), killed);
        }
    };
    let run_fn = match instance.get_typed_func::<(), i32>(&wstore, "run") {
        Ok(f) => f,
        Err(_) => {
            return finish(
                &wstore,
                false,
                0,
                Some("component has no `run() -> i32` export".into()),
                None,
            )
        }
    };

    let (ok, code, err, killed) = match run_fn.call(&mut wstore, ()) {
        Ok(code) => (true, code, None, None),
        Err(e) => {
            let killed = classify_kill(wstore.data().deadline_hit, e.as_trap_code());
            (false, 0, Some(format!("{e}")), killed)
        }
    };
    finish(&wstore, ok, code, err, killed)
}

/// Name the bound that killed a run from how it ended: an explicit trap code wins (fuel and stack
/// overflow have codes), otherwise a deadline hit at some crossing does. A run that merely trapped
/// on its own bug gets `None` — the audit log must not blame the sandbox for a guest's fault.
fn classify_kill(deadline_hit: bool, trap_code: Option<TrapCode>) -> Option<KillReason> {
    match trap_code {
        Some(TrapCode::OutOfFuel) => Some(KillReason::Fuel),
        Some(TrapCode::StackOverflow) => Some(KillReason::Stack),
        _ if deadline_hit => Some(KillReason::Deadline),
        _ => None,
    }
}

fn finish(
    wstore: &WStore<HostState<'_>>,
    ok: bool,
    exit_code: i32,
    error: Option<String>,
    killed: Option<KillReason>,
) -> ComponentOutcome {
    let st = wstore.data();
    // Deadline reporting is independent of the kill classification: a pure-compute guest that never
    // crosses the host boundary cannot be stopped by the crossing rule, so an overrun it managed to
    // FINISH inside must still be reported here — silence would read as compliance.
    let deadline_exceeded =
        st.deadline_hit || st.deadline.is_some_and(|d| st.started.elapsed() > d);
    ComponentOutcome {
        ok,
        exit_code,
        fuel_exhausted: killed == Some(KillReason::Fuel),
        killed_by: killed,
        deadline_exceeded,
        error,
        calls: st.calls.clone(),
        wrote: st.wrote.clone(),
        spawns: st.spawns.clone(),
        spawned: Vec::new(),
    }
}
