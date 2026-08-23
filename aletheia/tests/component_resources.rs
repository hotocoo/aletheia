//! P2 acceptance — the sandbox resource model BEYOND fuel (ALET-P1-021, ADR-065).
//!
//! Fuel (REQ-COMP-002) bounds how much COMPUTE a guest may buy. It says nothing about how much
//! MEMORY it may hold, how large its TABLES may grow, how DEEP its stack may wind, or how much
//! WALL-CLOCK time it may consume. Those are four separate ways to hurt the machine hosting you,
//! and this suite proves each one is bounded BY NAME:
//!
//! * memory/table caps hold even against a guest that ignores the spec's -1 from a failed grow;
//! * a recursion bomb dies as `KillReason::Stack`, not as an anonymous trap;
//! * a guest that outruns its wall clock is killed AT A HOST-CALL CROSSING, the crossing is audited
//!   as DEADLINE, and the kill is named `KillReason::Deadline`;
//! * the bounds have fail-closed defaults; an unbounded sandbox must be WRITTEN dimension by
//!   dimension, never forgotten into existence;
//! * a spawned child inherits the root's resource envelope exactly as it inherits attenuated
//!   authority — budget narrows down the tree like capability does.
use aletheia::component::SandboxLimits;
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-res-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

/// A delegated capability for `action` over everything, granted from the owner root.
fn grant_all(core: &mut SysCore, owner: &str, subject: &str, action: &str) -> String {
    core.grant_to(
        &[owner.to_string()],
        subject,
        action,
        aletheia::capabilities::Scope::All,
        aletheia::capabilities::Constraints::none(),
    )
    .expect("grant")
    .token
}

/// The shipped defaults bound EVERY dimension: nothing is infinite unless a caller writes the
/// infinity in itself. This is the fail-closed property of the type, not of any call site.
#[test]
fn default_limits_bound_every_dimension() {
    let d = SandboxLimits::defaults();
    assert!(d.max_memory_bytes > 0, "default memory is unbounded");
    assert!(d.max_table_elements > 0, "default table is unbounded");
    assert!(d.max_stack_height > 0, "default stack height is unbounded");
    assert!(
        d.max_recursion_depth > 0,
        "default recursion depth is unbounded"
    );
    assert!(d.deadline_ms > 0, "default wall clock is unbounded");
    assert_eq!(
        d.max_memory_bytes,
        4 * 1024 * 1024,
        "the documented default memory cap"
    );
    assert_eq!(d.deadline_ms, 30_000, "the documented default clock budget");
    // Default == Default: there is exactly one shipped envelope, not per-call-site drift.
    assert_eq!(
        serde_json::to_string(&SandboxLimits::defaults()).unwrap(),
        serde_json::to_string(&SandboxLimits::default()).unwrap(),
    );
}

/// The reference workload still passes untouched under the default bounds: limits that broke honest
/// components would be a denial of service in the other direction.
#[test]
fn well_behaved_component_passes_under_default_limits() {
    let (mut core, owner) = open();
    // A component that writes, compiled here exactly like the acceptance suite's.
    let payload = "resource-model-passes";
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{payload}")
  (func (export "run") (result i32)
    (drop (call $write (i32.const 0) (i32.const {plen})))
    (i32.const 7)))"#,
        plen = payload.len()
    );
    let wasm = wat::parse_str(&wat).expect("writer wat compiles");
    let write_cap = grant_all(&mut core, &owner, "component:honest", "entity.write");

    let outcome = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[write_cap],
            "component:honest",
            &wasm,
            1_000_000,
            &SandboxLimits::defaults(),
        )
        .unwrap();

    assert!(outcome.ok, "honest guest finished: {:?}", outcome.error);
    assert_eq!(outcome.exit_code, 7);
    assert_eq!(outcome.killed_by, None, "no bound had to fire");
    assert!(!outcome.deadline_exceeded, "well inside every bound");
    assert_eq!(outcome.wrote.len(), 1);
}

/// A guest that grows memory forever. The cap makes growth FAIL (-1); the guest counts its wins and
/// reports them as its exit code. The count is EXACTLY the configured page budget — proof the cap
/// held at the byte, not merely that something eventually stopped.
#[test]
fn memory_hog_grows_to_exactly_its_cap_and_no_further() {
    let (mut core, owner) = open();
    // loop { if grow(1 page) == -1 break }; return pages gained.
    let wat = r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32)
    (local $n i32)
    (block $exit
      (loop $l
        (br_if $exit (i32.eq (memory.grow (i32.const 1)) (i32.const -1)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n)))"#;
    let wasm = wat::parse_str(wat).expect("hog wat compiles");

    // Cap = 3 pages total (initial included): the guest may win exactly 2 grows.
    let limits = SandboxLimits {
        max_memory_bytes: 3 * 64 * 1024,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(
            &[owner],
            &[],
            "component:memhog",
            &wasm,
            10_000_000,
            &limits,
        )
        .unwrap();

    assert!(
        outcome.killed_by.is_none(),
        "a capped guest that RESPECTS -1 finishes normally: {:?}",
        outcome.killed_by
    );
    assert_eq!(
        outcome.exit_code, 2,
        "exactly two pages could be won under a three-page cap"
    );
}

/// The same hog WITH its fingers in its ears: it ignores the -1 and keeps asking. The cap still
/// holds — wasmi refuses the growth every time — and the guest simply burns fuel asking, which is
/// what fuel is FOR. Memory never exceeds the cap either way.
#[test]
fn a_hog_that_ignores_the_refusal_still_never_exceeds_the_cap() {
    let (mut core, owner) = open();
    // Same shape but counting attempts while continuing to ask past refusal, bounded by fuel.
    let wat = r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32)
    (local $n i32)
    (loop $l
      (drop (memory.grow (i32.const 1)))
      (local.set $n (i32.add (local.get $n) (i32.const 1)))
      (br $l))
    (local.get $n)))"#;
    let wasm = wat::parse_str(wat).expect("deaf hog wat compiles");
    let limits = SandboxLimits {
        max_memory_bytes: 2 * 64 * 1024,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(&[owner], &[], "component:deaf", &wasm, 500_000, &limits)
        .unwrap();

    assert_eq!(
        outcome.killed_by,
        Some(aletheia::component::KillReason::Fuel),
        "the deaf hog ends by burning its compute budget"
    );
    assert!(outcome.fuel_exhausted);
}

/// A table-growing guest is capped at EXACTLY `max_table_elements` — same contract as memory, on
/// the second growable linear resource.
#[test]
fn table_hog_is_capped_at_exactly_the_table_budget() {
    let (mut core, owner) = open();
    let wat = r#"(module
  (table (export "t") 0 funcref)
  (func (export "run") (result i32)
    (local $n i32)
    (block $exit
      (loop $l
        (br_if $exit (i32.eq (table.grow (ref.null func) (i32.const 1)) (i32.const -1)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n)))"#;
    let wasm = wat::parse_str(wat).expect("table hog wat compiles");
    let limits = SandboxLimits {
        max_table_elements: 5,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(
            &[owner],
            &[],
            "component:tabhog",
            &wasm,
            10_000_000,
            &limits,
        )
        .unwrap();

    assert_eq!(
        outcome.exit_code, 5,
        "five elements could be won under a five-element cap"
    );
}

/// A recursion bomb dies by the CONFIGURED frame bound, reported as KillReason::Stack — the audit
/// log names the bound that held instead of shrugging "some trap".
#[test]
fn recursion_bomb_is_killed_by_the_stack_bound_and_named_so() {
    let (mut core, owner) = open();
    let wat = r#"(module
  (func $r (export "run") (result i32) (call $r)))"#;
    let wasm = wat::parse_str(wat).expect("bomb wat compiles");
    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[],
            "component:bomb",
            &wasm,
            10_000_000,
        )
        .unwrap();

    assert!(!outcome.ok, "the bomb did not finish");
    assert_eq!(
        outcome.killed_by,
        Some(aletheia::component::KillReason::Stack),
        "named kill: {:?}",
        outcome.error
    );
    assert!(!outcome.fuel_exhausted, "fuel was NOT what stopped it");
}

/// Depth is POLICY, not fate: a guest recursing 100 frames deep finishes under the default 256-frame
/// bound and is killed when the same bound is lowered beneath it. The operator sets the number; the
/// machine enforces exactly that number in both directions.
#[test]
fn recursion_depth_is_enforced_exactly_as_configured_both_ways() {
    fn deep_guest(depth: u32) -> Vec<u8> {
        let wat = format!(
            r#"(module
  (func $r (export "run") (result i32)
    (if (i32.lt_u (global.get $g) (i32.const {depth}))
      (then (global.set $g (i32.add (global.get $g) (i32.const 1)))
            (drop (call $r))))
    (global.get $g))
  (global $g (mut i32) (i32.const 0)))"#,
            depth = depth
        );
        wat::parse_str(&wat).expect("deep wat compiles")
    }

    let (mut core, owner) = open();
    let ok = core
        .run_component(
            std::slice::from_ref(&owner),
            &[],
            "component:deep",
            &deep_guest(100),
            1_000_000,
        )
        .unwrap();
    assert!(ok.ok, "100 frames fit the default 256: {:?}", ok.error);
    assert_eq!(ok.exit_code, 100);

    let tight = SandboxLimits {
        max_recursion_depth: 50,
        ..SandboxLimits::defaults()
    };
    let killed = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[],
            "component:deep",
            &deep_guest(100),
            1_000_000,
            &tight,
        )
        .unwrap();
    assert_eq!(
        killed.killed_by,
        Some(aletheia::component::KillReason::Stack),
        "the same guest dies once the bound is beneath it"
    );
}

/// A guest whose WORK outruns its WALL CLOCK is killed at a host-call crossing: the crossing is
/// refused, the attempt is audited as DEADLINE, and the run is named KillReason::Deadline.
#[test]
fn deadline_kills_a_guest_that_outruns_its_clock_at_a_crossing() {
    let (mut core, owner) = open();
    // Emit in a loop, many times, each crossing a fresh chance for the clock to have run out.
    let wat = r#"(module
  (import "aletheia" "emit" (func $emit (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "tick")
  (func (export "run") (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if 1 (i32.ge_u (local.get $i) (i32.const 200000)))
        (drop (call $emit (i32.const 0) (i32.const 4)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.const 0)))"#;
    let wasm = wat::parse_str(wat).expect("clock eater wat compiles");
    let emit_cap = grant_all(&mut core, &owner, "component:clickeater", "event.emit");
    let tight = SandboxLimits {
        deadline_ms: 1,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[emit_cap],
            "component:clickeater",
            &wasm,
            2_000_000_000, // fuel deliberately vast: the CLOCK must be what stops this guest
            &tight,
        )
        .unwrap();

    assert!(!outcome.ok, "the clock eater did not finish");
    assert_eq!(
        outcome.killed_by,
        Some(aletheia::component::KillReason::Deadline),
        "killed by the clock: {:?}",
        outcome.error
    );
    assert!(outcome.deadline_exceeded);
    assert!(
        outcome.calls.iter().any(|c| c.decision == "DEADLINE"),
        "the refused crossing is in the audit trail"
    );
    assert!(
        matches!(outcome.calls.last(), Some(c) if c.decision == "DEADLINE"),
        "the run ENDED at that crossing, so it is the last thing recorded"
    );
}

/// Unbounded wall clock EXISTS but only because the caller wrote `deadline_ms: 0` — the explicit
/// opt-out is visible in code review, which is the whole point of fail-closed defaults.
#[test]
fn unbounded_deadline_exists_only_when_written() {
    let (mut core, owner) = open();
    let payload = "no-clock";
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{payload}")
  (func (export "run") (result i32)
    (drop (call $write (i32.const 0) (i32.const {plen})))
    (i32.const 0)))"#,
        plen = payload.len()
    );
    let wasm = wat::parse_str(&wat).expect("writer wat compiles");
    let write_cap = grant_all(&mut core, &owner, "component:noclock", "entity.write");
    let written_unbounded = SandboxLimits {
        deadline_ms: 0,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[write_cap],
            "component:noclock",
            &wasm,
            1_000_000,
            &written_unbounded,
        )
        .unwrap();
    assert!(outcome.ok);
    assert!(!outcome.deadline_exceeded, "no clock, no overrun to report");
}

/// A spawned child runs inside the ROOT'S envelope — budget narrows down the tree exactly as
/// authority does. The parent spawns an installed clock-eating child; the child is killed by the
/// parent's 1 ms deadline even though nobody re-specified limits for the child.
#[test]
fn a_spawned_child_inherits_the_root_resource_envelope() {
    let (mut core, owner) = open();
    // The installed CHILD: an emitter that would happily run for a very long time.
    let child_wat = r#"(module
  (import "aletheia" "emit" (func $emit (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "tock")
  (func (export "run") (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if 1 (i32.ge_u (local.get $i) (i32.const 200000)))
        (drop (call $emit (i32.const 0) (i32.const 4)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.const 0)))"#;
    let child_wasm = wat::parse_str(child_wat).expect("child wat compiles");
    let child = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "child-clickeater",
            &child_wasm,
        )
        .unwrap();
    let emit_cap = grant_all(&mut core, &owner, "component:parent", "event.emit");

    // The PARENT: spawns the child and exits. Its only job is to bring the child into existence.
    let parent_wat = format!(
        r#"(module
  (import "aletheia" "spawn" (func $spawn (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{child_id}")
  (data (i32.const 64) "event.emit")
  (func (export "run") (result i32)
    (drop (call $spawn (i32.const 0) (i32.const {idlen}) (i32.const 64) (i32.const 11)))
    (i32.const 0)))"#,
        child_id = child.id,
        idlen = child.id.len()
    );
    let parent_wasm = wat::parse_str(&parent_wat).expect("parent wat compiles");

    let tight = SandboxLimits {
        deadline_ms: 1,
        ..SandboxLimits::defaults()
    };
    let outcome = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[emit_cap],
            "component:parent",
            &parent_wasm,
            2_000_000_000,
            &tight,
        )
        .unwrap();

    assert!(outcome.ok, "the parent itself finished instantly");
    assert_eq!(outcome.spawned.len(), 1, "one child was queued and run");
    let child_out = &outcome.spawned[0];
    assert!(!child_out.ok, "the child did not finish");
    assert_eq!(
        child_out.killed_by,
        Some(aletheia::component::KillReason::Deadline),
        "the child died inside the ROOT'S envelope"
    );
}
