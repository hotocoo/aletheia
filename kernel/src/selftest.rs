//! In-kernel invariant selftests — the end-to-end VM acceptance suite.
//!
//! These are the M1 acceptance criteria, re-proved in kernel space against the real
//! in-kernel spine (not a mock). The first failing check sets the VM exit code (10 + index),
//! so `scripts/vm-e2e.sh` gets a precise, machine-checkable pass/fail per invariant.
use crate::spine::*;
use alloc::vec;

/// Run every invariant. `Ok(n)` = all n passed; `Err((idx,name))` = check idx failed.
pub fn run() -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;

    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            if !($cond) {
                kprintln!("  [FAIL {:>2}] {}", n, $name);
                return Err((n, $name));
            }
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }

    // 1 — fail closed: no capability => deny.
    {
        let e = CapEngine::new(0xA5A5, 1000);
        let d = e.evaluate("entity.derive", &Target::default(), &[]);
        check!(matches!(d, Decision::Deny(_)), "fail-closed: no capability => deny");
    }

    // 2 — authorized derive runs the full pipeline: validate->authorize->execute->verify->event.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let mut s = Store::new();
        let doc = s.put(EntityType::Document, "hello world", "user");
        let cap = e.mint("user", "entity.derive", Scope::Type(EntityType::Document), Constraints::none());
        let plan = Plan { steps: vec![Step { op: "derive_summary".into(), source: doc, content: "tldr".into() }] };
        let r = run_pipeline(&e, &mut s, "user", &plan, &[cap]);
        check!(r.ok && r.verified && s.event_count() == 1, "pipeline: authorized derive verified + event recorded");
    }

    // 3 — forged/fabricated token is not authority.
    {
        let e = CapEngine::new(0xA5A5, 1000);
        let forged = CapToken::forge_for_test(0xDEAD_BEEF);
        let d = e.evaluate("entity.derive", &Target::default(), &[forged]);
        check!(matches!(d, Decision::Deny(_)), "unforgeable: fabricated token denied");
    }

    // 4 — delegation attenuates (narrower allowed).
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
        let ok = e.delegate(root, "agent", "entity.derive", Scope::Type(EntityType::Document), Constraints::none());
        check!(ok.is_ok(), "delegation: equal-or-narrower allowed");
    }

    // 5 — delegation cannot amplify authority.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let narrow = e.mint("user", "entity.derive", Scope::Type(EntityType::Document), Constraints::none());
        let amp = e.delegate(narrow, "agent", "entity.delete", Scope::All, Constraints::none());
        check!(amp.is_err(), "delegation: amplification denied");
    }

    // 6 — revocation cascades to descendants, immediately.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
        let child = e.delegate(root, "agent", "entity.derive", Scope::All, Constraints::none()).unwrap();
        e.revoke(root);
        let d = e.evaluate("entity.derive", &Target::default(), &[child]);
        check!(matches!(d, Decision::Deny(_)) && e.is_revoked(child), "revocation: cascades to descendants");
    }

    // 7 — malformed (untrusted) plan cannot execute.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let mut s = Store::new();
        let doc = s.put(EntityType::Document, "x", "u");
        let cap = e.mint("u", "entity.derive", Scope::All, Constraints::none());
        let bad = Plan { steps: vec![Step { op: "rm -rf /".into(), source: doc, content: "".into() }] };
        let r = run_pipeline(&e, &mut s, "u", &bad, &[cap]);
        check!(!r.ok && !r.executed && r.validation == "rejected" && s.event_count() == 0, "malformed output cannot execute");
    }

    // 8 — expired capability is denied (same engine mints AND evaluates; now > expiry).
    {
        let mut e = CapEngine::new(0xA5A5, 5000);
        let cap = e.mint(
            "u",
            "entity.derive",
            Scope::All,
            Constraints { expires_at: Some(1000), approval_required: false, local_only: true },
        );
        let d = e.evaluate("entity.derive", &Target::default(), &[cap]);
        check!(matches!(d, Decision::Deny(_)), "expired capability denied");
    }

    // 9 — scope confinement: capability for entity A does not authorize entity B.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let cap = e.mint("u", "entity.derive", Scope::Entities(vec![0x1000]), Constraints::none());
        let d = e.evaluate("entity.derive", &Target { id: Some(0x2000), etype: Some(EntityType::Document) }, &[cap]);
        check!(matches!(d, Decision::Deny(_)), "scope confinement: other entity denied");
    }

    // 10 — secure IPC requires a capability; unauthorized send dropped, authorized delivered.
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let mut ch = Channel::new("ipc.send");
        let d0 = ch.send(&e, Message { from: "A".into(), to: "B".into(), body: 1 }, &[]);
        let dropped = matches!(d0, Decision::Deny(_)) && ch.recv().is_none();
        let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
        let d1 = ch.send(&e, Message { from: "A".into(), to: "B".into(), body: 2 }, &[cap]);
        let delivered = d1 == Decision::Allow && ch.recv().is_some();
        check!(dropped && delivered, "secure IPC: unauthorized dropped, authorized delivered");
    }

    // 11 — destructive action requires approval (same engine mints AND evaluates).
    {
        let mut e = CapEngine::new(0xA5A5, 1000);
        let cap = e.mint("u", "entity.delete", Scope::All, Constraints::approval());
        let d = e.evaluate("entity.delete", &Target::default(), &[cap]);
        check!(d == Decision::RequireApproval, "destructive action requires approval");
    }

    Ok(n)
}
