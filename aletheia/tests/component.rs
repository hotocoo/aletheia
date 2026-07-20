//! P2 acceptance — WASM capability-secure component runtime (ADR-014).
//!
//! The single invariant that makes this increment real (advisor guardrail, PRD INV-011 / criteria
//! 18 & 19): an untrusted component's authority is EXACTLY the capabilities it was granted, checked
//! through the *same* capability engine as the deterministic pipeline, and every effect it is
//! allowed lands in the *same* immutable event log. No capability → it can do nothing; an attenuated
//! grant → it can do exactly that and no more; a runaway component cannot hang the OS.
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;
use proptest::prelude::*;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-comp-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

/// A component that writes an entity, then emits an event. Returns exit code 7.
fn writer_wasm(payload: &str, event: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (import "aletheia" "emit"  (func $emit  (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0)   "{payload}")
  (data (i32.const 512) "{event}")
  (func (export "run") (result i32)
    (drop (call $write (i32.const 0)   (i32.const {plen})))
    (drop (call $emit  (i32.const 512) (i32.const {elen})))
    (i32.const 7)))"#,
        payload = payload,
        event = event,
        plen = payload.len(),
        elen = event.len()
    );
    wat::parse_str(&wat).expect("writer wat compiles")
}

/// A component that reads the entity whose id is baked into its data segment into a return buffer.
/// Returns 0.
fn reader_wasm(id: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "read" (func $read (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{id}")
  (func (export "run") (result i32)
    (drop (call $read (i32.const 0) (i32.const {idlen}) (i32.const 256) (i32.const 128)))
    (i32.const 0)))"#,
        id = id,
        idlen = id.len()
    );
    wat::parse_str(&wat).expect("reader wat compiles")
}

/// A real program: read a source entity's content into memory, uppercase the ASCII letters, and
/// write the transformed bytes back as a new entity. Requires both read and write capabilities.
fn transform_wasm(id: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "read"  (func $read  (param i32 i32 i32 i32) (result i64)))
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{id}")
  (func (export "run") (result i32)
    (local $n i32) (local $i i32) (local $b i32)
    (local.set $n (i32.wrap_i64 (call $read (i32.const 0) (i32.const {idlen}) (i32.const 256) (i32.const 128))))
    (if (i32.gt_s (local.get $n) (i32.const 128)) (then (local.set $n (i32.const 128))))
    (local.set $i (i32.const 0))
    (block $done
      (loop $l
        (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
        (local.set $b (i32.load8_u (i32.add (i32.const 256) (local.get $i))))
        (if (i32.and (i32.ge_u (local.get $b) (i32.const 97)) (i32.le_u (local.get $b) (i32.const 122)))
          (then (i32.store8 (i32.add (i32.const 256) (local.get $i)) (i32.sub (local.get $b) (i32.const 32)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (drop (call $write (i32.const 256) (local.get $n)))
    (i32.const 0)))"#,
        id = id,
        idlen = id.len()
    );
    wat::parse_str(&wat).expect("transform wat compiles")
}

/// A component that writes an entity, then loops forever — used to prove a committed effect survives
/// a later fuel-kill (the trap cannot roll back or corrupt what already committed).
fn writer_then_spin_wasm(payload: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{payload}")
  (func (export "run") (result i32)
    (drop (call $write (i32.const 0) (i32.const {plen})))
    (loop $l (br $l))
    (unreachable)))"#,
        payload = payload,
        plen = payload.len()
    );
    wat::parse_str(&wat).expect("writer_then_spin wat compiles")
}

/// A component that loops forever — used to prove fuel bounding.
fn spinner_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32)
    (loop $l (br $l))
    (unreachable)))"#,
    )
    .expect("spinner wat compiles")
}

fn count_events(core: &SysCore, etype: &str) -> usize {
    core.store().events().iter().filter(|e| e.etype == etype).count()
}

/// A delegated, attenuated capability for `action` over `scope`, granted from the owner root.
fn grant(core: &mut SysCore, owner: &str, subject: &str, action: &str, scope: Scope, cons: Constraints) -> String {
    core.grant_to(&[owner.to_string()], subject, action, scope, cons).expect("grant").token
}

/// Criterion 18 (no ambient authority): a component with an empty grant can do NOTHING. It runs to
/// completion, but every host call is denied and the store gains no component effects.
#[test]
fn component_with_no_capability_can_do_nothing() {
    let (mut core, owner) = open();
    let wasm = writer_wasm("component-output-payload", "component-event-payload");

    let outcome = core.run_component(&[owner], &[], "component:untrusted", &wasm, 1_000_000).unwrap();

    assert!(outcome.ok, "guest itself ran fine: {:?}", outcome.error);
    assert_eq!(outcome.exit_code, 7);
    assert!(outcome.denied("write"), "write must be denied with no capability");
    assert!(outcome.denied("emit"), "emit must be denied with no capability");
    assert!(!outcome.allowed("write") && !outcome.allowed("emit"));
    assert!(outcome.wrote.is_empty(), "no entity may be created");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
    assert_eq!(count_events(&core, "ComponentEmitted"), 0);
}

/// Attenuation: a component granted ONLY entity.write can write, but its emit is denied — its
/// authority is exactly its grant and no more.
#[test]
fn component_authority_is_exactly_its_grant() {
    let (mut core, owner) = open();
    let write_cap = grant(&mut core, &owner, "component:writer", "entity.write", Scope::All, Constraints::none());
    let wasm = writer_wasm("component-output-payload", "component-event-payload");

    let outcome = core.run_component(&[owner], &[write_cap], "component:writer", &wasm, 1_000_000).unwrap();

    assert!(outcome.allowed("write"), "write is granted -> allowed");
    assert!(outcome.denied("emit"), "emit is NOT granted -> denied");
    assert_eq!(outcome.wrote.len(), 1, "exactly one entity created");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 1);
    assert_eq!(count_events(&core, "ComponentEmitted"), 0);

    // The write is a real, verifiable effect in the same store: the entity exists with our payload.
    let e = core.store().get_entity(&outcome.wrote[0]).expect("written entity persisted");
    assert_eq!(e.etype, EntityType::Output);
    let blob = core.store().get_blob(e.content_ref.as_ref().unwrap()).expect("content stored");
    assert_eq!(blob, b"component-output-payload");
}

/// Every host call — allowed or denied — appears in the explainable audit (EXP-005 for components),
/// and the allowed effect appears in the immutable event log with the acting subject (criterion 14).
#[test]
fn component_effects_are_recorded_and_explainable() {
    let (mut core, owner) = open();
    let write_cap = grant(&mut core, &owner, "component:writer", "entity.write", Scope::All, Constraints::none());
    let wasm = writer_wasm("audited-output", "audited-event");

    let outcome = core.run_component(&[owner], &[write_cap], "component:writer", &wasm, 1_000_000).unwrap();

    // The per-call audit is complete: both attempts recorded, each with an action + a decision.
    assert_eq!(outcome.calls.len(), 2);
    assert!(outcome.calls.iter().all(|c| !c.action.is_empty() && !c.decision.is_empty()));

    // The write landed in the one immutable log, attributed to the component subject.
    let wrote_ev = core.store().events().iter().find(|e| e.etype == "ComponentWroteEntity").expect("write event");
    assert_eq!(wrote_ev.actor, "component:writer");
    assert!(!wrote_ev.correlation_id.is_empty());

    // The launch itself is recorded as a summarized, auditable event.
    assert_eq!(count_events(&core, "ComponentRan"), 1);
}

/// Reads are capability-gated exactly like writes: allowed only with a scoped read capability.
#[test]
fn component_read_is_capability_gated() {
    let (mut core, owner) = open();
    let e = core
        .create_entity(std::slice::from_ref(&owner), "human:owner", EntityType::Document, b"secret readable bytes", serde_json::json!({}))
        .unwrap();
    let wasm = reader_wasm(&e.id);

    // (a) granted a read capability scoped to exactly this entity -> allowed.
    let read_cap = grant(&mut core, &owner, "component:reader", "entity.read", Scope::Entities(vec![e.id.clone()]), Constraints::none());
    let ok = core.run_component(std::slice::from_ref(&owner), &[read_cap], "component:reader", &wasm, 1_000_000).unwrap();
    assert!(ok.allowed("read"), "read allowed with scoped read capability");

    // (b) no capability -> denied, fail closed.
    let denied = core.run_component(&[owner], &[], "component:reader", &wasm, 1_000_000).unwrap();
    assert!(denied.denied("read"), "read denied with no capability");
}

/// Resource isolation: a runaway component is trapped by fuel exhaustion — it cannot hang the OS,
/// and it leaves no effects behind. (Pre-stages the P2 stress/chaos gates.)
#[test]
fn runaway_component_is_bounded_by_fuel() {
    let (mut core, owner) = open();
    let wasm = spinner_wasm();

    let outcome = core.run_component(&[owner], &[], "component:runaway", &wasm, 100_000).unwrap();

    assert!(!outcome.ok, "a fuel-exhausted component does not complete");
    assert!(outcome.fuel_exhausted, "the trap must be out-of-fuel, not something else");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
    assert!(outcome.wrote.is_empty());
}

/// Application-as-capability: launching a component AT ALL requires the component.run capability.
/// Without it the component never executes and produces no effects.
#[test]
fn launching_a_component_requires_authority() {
    let (mut core, owner) = open();
    // A capability that does NOT cover component.run.
    let unrelated = grant(&mut core, &owner, "component:x", "entity.read", Scope::All, Constraints::none());
    let wasm = writer_wasm("should-never-run", "should-never-emit");

    let err = core
        .run_component(std::slice::from_ref(&unrelated), std::slice::from_ref(&unrelated), "component:x", &wasm, 1_000_000)
        .unwrap_err();
    assert_eq!(err.category, aletheia::domain::ErrorCategory::Authorization);
    assert_eq!(count_events(&core, "ComponentRan"), 0, "the component must never execute");
    assert!(core.store().events().iter().any(|e| e.etype == "CapabilityDenied"));
}

/// A component can be installed as a first-class Application entity and later run from the store.
#[test]
fn installed_component_runs_from_the_store() {
    let (mut core, owner) = open();
    let wasm = writer_wasm("installed-output", "installed-event");

    let app = core.install_component(std::slice::from_ref(&owner), "human:owner", "note-writer", &wasm).unwrap();
    assert_eq!(app.etype, EntityType::Application);
    assert_eq!(count_events(&core, "ComponentInstalled"), 1);

    let write_cap = grant(&mut core, &owner, "app:note-writer", "entity.write", Scope::All, Constraints::none());
    let outcome = core.run_installed(&[owner], &[write_cap], "app:note-writer", &app.id, 1_000_000).unwrap();

    assert!(outcome.allowed("write"));
    assert_eq!(outcome.wrote.len(), 1);
    assert_eq!(count_events(&core, "ComponentRan"), 1);
}

/// Criterion 9 spirit at the component boundary: an approval-required capability does not let a
/// component perform the action inline. It is refused (not executed), preserving the human gate.
#[test]
fn approval_required_capability_is_refused_at_component_boundary() {
    let (mut core, owner) = open();
    let approve_cap = grant(&mut core, &owner, "component:w", "entity.write", Scope::All, Constraints::approval());
    let wasm = writer_wasm("approval-gated", "approval-event");

    let outcome = core.run_component(&[owner], &[approve_cap], "component:w", &wasm, 1_000_000).unwrap();

    let write_call = outcome.calls.iter().find(|c| c.func == "write").expect("write attempt recorded");
    assert_eq!(write_call.decision, "REQUIRE_APPROVAL");
    assert!(outcome.wrote.is_empty(), "an approval-required action does not execute inline");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
}

/// A real end-to-end program: the component reads a source entity it is authorized to read, computes
/// over the bytes (uppercase), and writes the result — proving the read return-buffer actually
/// delivers consumable data, and that read + write compose under distinct scoped capabilities.
#[test]
fn component_reads_transforms_and_writes() {
    let (mut core, owner) = open();
    let source = core
        .create_entity(std::slice::from_ref(&owner), "human:owner", EntityType::Document, b"hello component world", serde_json::json!({}))
        .unwrap();
    let read_cap = grant(&mut core, &owner, "component:xform", "entity.read", Scope::Entities(vec![source.id.clone()]), Constraints::none());
    let write_cap = grant(&mut core, &owner, "component:xform", "entity.write", Scope::All, Constraints::none());
    let wasm = transform_wasm(&source.id);

    let outcome = core.run_component(&[owner], &[read_cap, write_cap], "component:xform", &wasm, 5_000_000).unwrap();

    assert!(outcome.ok, "program ran: {:?}", outcome.error);
    assert!(outcome.allowed("read") && outcome.allowed("write"));
    assert_eq!(outcome.wrote.len(), 1, "one transformed entity written");

    // The written entity holds the actual transform of the source content — the component consumed
    // the bytes the read delivered and computed on them.
    let out = core.store().get_entity(&outcome.wrote[0]).expect("output persisted");
    let bytes = core.store().get_blob(out.content_ref.as_ref().unwrap()).expect("output content");
    assert_eq!(bytes, b"HELLO COMPONENT WORLD");
}

/// State integrity under a trap (matches ADR-014's "a trap cannot corrupt state"): a component that
/// commits a write and THEN is fuel-killed leaves the committed effect intact and attributed — the
/// trap neither rolls it back nor corrupts anything else.
#[test]
fn committed_effect_survives_a_later_fuel_kill() {
    let (mut core, owner) = open();
    let write_cap = grant(&mut core, &owner, "component:half", "entity.write", Scope::All, Constraints::none());
    let wasm = writer_then_spin_wasm("committed-before-trap");

    let outcome = core.run_component(&[owner], &[write_cap], "component:half", &wasm, 2_000_000).unwrap();

    assert!(!outcome.ok, "the component is killed mid-run");
    assert!(outcome.fuel_exhausted, "killed by fuel exhaustion, after the write committed");
    assert_eq!(outcome.wrote.len(), 1, "the pre-trap write committed exactly once");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 1);

    let e = core.store().get_entity(&outcome.wrote[0]).expect("committed entity persisted through the trap");
    assert_eq!(e.provenance.actor, "component:half");
    let bytes = core.store().get_blob(e.content_ref.as_ref().unwrap()).unwrap();
    assert_eq!(bytes, b"committed-before-trap");
}

/// A writer component whose memory arguments are chosen by the fuzzer.
fn writer_with_args(ptr: i32, len: i32) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "payload-bytes")
  (func (export "run") (result i32)
    (drop (call $write (i32.const {ptr}) (i32.const {len})))
    (i32.const 0)))"#,
        ptr = ptr,
        len = len
    );
    wat::parse_str(&wat).expect("fuzz writer wat compiles")
}

// Fuzzing the untrusted host-ABI boundary (PRD §38.4). Fuzzing is treated as a security control, not
// a QA nicety: the fail-closed default and host robustness must hold for inputs no one enumerated.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Fail-closed under fuzzing (PRD §38.3 capability release gate): for ANY memory arguments, a
    /// component with no capability produces no effect — the fail-closed default never leaks.
    #[test]
    fn fuzzed_component_without_capability_never_writes(ptr in 0i32..200_000, len in 0i32..200_000) {
        let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
        let owner = core.bootstrap_owner("human:owner").unwrap().token;
        let outcome = core.run_component(std::slice::from_ref(&owner), &[], "component:fuzz", &writer_with_args(ptr, len), 1_000_000).unwrap();
        prop_assert!(outcome.wrote.is_empty());
        prop_assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
    }

    /// Host robustness under fuzzing: even WITH a write capability, arbitrary (often out-of-bounds)
    /// memory arguments never panic the host; any write that lands did pass the capability check.
    #[test]
    fn fuzzed_writer_never_panics(ptr in 0i32..200_000, len in 0i32..200_000) {
        let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
        let owner = core.bootstrap_owner("human:owner").unwrap().token;
        let cap = core.grant_to(std::slice::from_ref(&owner), "component:fuzz", "entity.write", Scope::All, Constraints::none()).unwrap().token;
        let outcome = core.run_component(std::slice::from_ref(&owner), &[cap], "component:fuzz", &writer_with_args(ptr, len), 1_000_000).unwrap();
        prop_assert!(outcome.wrote.len() <= 1);
        if !outcome.wrote.is_empty() {
            prop_assert!(outcome.allowed("write"));
        }
    }
}
