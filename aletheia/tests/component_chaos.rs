//! P2 stress/chaos — the gating property campaign for the WASM component runtime (ADR-014, PRD §41).
//!
//! The 14 fixed acceptance tests in `component.rs` prove the invariants for enumerated cases; this
//! suite proves them over RANDOMIZED (capability-set × host-call-sequence × fuel) — the space no one
//! enumerated. Two invariants must hold for EVERY generated component, no matter what:
//!   (1) no effect without a capability — a component changes nothing it wasn't authorized to;
//!   (2) effects ⊆ grant — the set of effects it produces never exceeds what its grant permits (and
//!       fuel exhaustion can only REDUCE effects, never manufacture unauthorized ones);
//! and the OS is never hung: every run returns a verdict (proven by reaching the assertions).
use aletheia::capabilities::{Constraints, Scope};
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;
use proptest::prelude::*;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-chaos-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

fn grant(core: &mut SysCore, owner: &str, subject: &str, action: &str) -> String {
    core.grant_to(&[owner.to_string()], subject, action, Scope::All, Constraints::none())
        .expect("grant")
        .token
}

fn count_events(core: &SysCore, etype: &str) -> usize {
    core.store().events().iter().filter(|e| e.etype == etype).count()
}

/// A component that calls `write` `w` times then `emit` `e` times and returns 0. Each `write` creates
/// a fresh Output entity, so with ample fuel and an `entity.write` grant the store gains exactly `w`.
fn seq_wasm(w: usize, e: usize) -> Vec<u8> {
    let mut body = String::new();
    for _ in 0..w {
        body.push_str("    (drop (call $write (i32.const 0) (i32.const 4)))\n");
    }
    for _ in 0..e {
        body.push_str("    (drop (call $emit  (i32.const 0) (i32.const 4)))\n");
    }
    let wat = format!(
        r#"(module
  (import "aletheia" "write" (func $write (param i32 i32) (result i64)))
  (import "aletheia" "emit"  (func $emit  (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "data")
  (func (export "run") (result i32)
{body}    (i32.const 0)))"#
    );
    wat::parse_str(&wat).expect("seq wat compiles")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// With ample fuel, effects EXACTLY equal the grant: `w` writes land iff write was granted, `e`
    /// emits land iff emit was granted, and an ungranted action is denied and produces nothing —
    /// for any randomized mix of call counts and granted capabilities.
    #[test]
    fn effects_equal_grant_with_ample_fuel(
        w in 0usize..6,
        e in 0usize..6,
        grant_w in any::<bool>(),
        grant_e in any::<bool>(),
    ) {
        let (mut core, owner) = open();
        let mut caps = Vec::new();
        if grant_w { caps.push(grant(&mut core, &owner, "chaos:seq", "entity.write")); }
        if grant_e { caps.push(grant(&mut core, &owner, "chaos:seq", "event.emit")); }
        let wasm = seq_wasm(w, e);

        let out = core.run_component(&[owner], &caps, "chaos:seq", &wasm, 50_000_000).unwrap();
        prop_assert!(out.ok, "ample fuel -> the guest completes: {:?}", out.error);

        let exp_w = if grant_w { w } else { 0 };
        let exp_e = if grant_e { e } else { 0 };
        prop_assert_eq!(out.wrote.len(), exp_w);
        prop_assert_eq!(count_events(&core, "ComponentWroteEntity"), exp_w);
        prop_assert_eq!(count_events(&core, "ComponentEmitted"), exp_e);

        // No effect without a capability (invariant 1).
        if !grant_w {
            prop_assert!(out.wrote.is_empty());
            if w > 0 { prop_assert!(out.denied("write")); }
        }
        if !grant_e && e > 0 {
            prop_assert!(out.denied("emit"));
        }
    }

    /// Under ARBITRARY fuel (including budgets far too small to finish), effects still never exceed
    /// the grant, and an ungranted action still produces nothing. Fuel exhaustion can only truncate
    /// the authorized effects — it can never manufacture an unauthorized one, and never hangs the OS.
    #[test]
    fn effects_never_exceed_grant_under_random_fuel(
        w in 0usize..10,
        e in 0usize..10,
        grant_w in any::<bool>(),
        grant_e in any::<bool>(),
        fuel in 1_000u64..2_000_000,
    ) {
        let (mut core, owner) = open();
        let mut caps = Vec::new();
        if grant_w { caps.push(grant(&mut core, &owner, "chaos:fuel", "entity.write")); }
        if grant_e { caps.push(grant(&mut core, &owner, "chaos:fuel", "event.emit")); }
        let wasm = seq_wasm(w, e);

        // The call ALWAYS returns a verdict — reaching the asserts proves the OS was not hung.
        let out = core.run_component(&[owner], &caps, "chaos:fuel", &wasm, fuel).unwrap();

        let cap_w = if grant_w { w } else { 0 };
        let cap_e = if grant_e { e } else { 0 };
        prop_assert!(out.wrote.len() <= cap_w, "writes must not exceed the grant");
        prop_assert!(count_events(&core, "ComponentWroteEntity") <= cap_w);
        prop_assert!(count_events(&core, "ComponentEmitted") <= cap_e);
        if !grant_w {
            prop_assert!(out.wrote.is_empty(), "no write may occur without entity.write");
            prop_assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
        }
        if !grant_e {
            prop_assert_eq!(count_events(&core, "ComponentEmitted"), 0);
        }
    }
}

/// Authority does not leak across component runs: after a privileged component writes, a later
/// component launched WITHOUT a write capability still cannot write — each run's authority is exactly
/// its own grant, never anything a previous run held.
#[test]
fn authority_does_not_leak_between_runs() {
    let (mut core, owner) = open();
    let wcap = grant(&mut core, &owner, "chaos:priv", "entity.write");
    let wasm = seq_wasm(1, 0);

    let privileged = core
        .run_component(std::slice::from_ref(&owner), std::slice::from_ref(&wcap), "chaos:priv", &wasm, 5_000_000)
        .unwrap();
    assert_eq!(privileged.wrote.len(), 1, "privileged run writes under its grant");

    let unprivileged = core
        .run_component(std::slice::from_ref(&owner), &[], "chaos:unpriv", &wasm, 5_000_000)
        .unwrap();
    assert!(unprivileged.wrote.is_empty(), "no leaked authority: the unprivileged run cannot write");
    assert!(unprivileged.denied("write"));

    // Exactly one write event total — only the privileged run's.
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 1);
}
