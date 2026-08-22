//! Console approvals: ADR-015's pending-approval lifecycle applied to the kernel-console surface
//! (ALET-P2-046, REQ-AI-006/ADR-053).
//!
//! The invariants under test are the ones that make a recorded human yes MEAN one typed line:
//!
//!   * an ask is DURABLE — it survives the process that made it (reopen the store);
//!   * an ask is IDEMPOTENT — re-asking the same request never mints a second record;
//!   * a grant is SPENT ONCE — typing consumes it; a second take is refused by construction;
//!   * a grant binds EXACTLY its line — a different line cannot ride on someone's yes;
//!   * a denial is also a record — and a denied line types nothing, ever;
//!   * consumption survives restart — replaying the log cannot resurrect spent authority;
//!   * the two approval worlds do not blur — a console resolution refuses a core-intent record
//!     and vice versa, because one human answer authorizes exactly one thing.
use aletheia::intelligence::DeterministicRuntime;
use aletheia::policy::ApprovalState;
use aletheia::syscore::SysCore;
use aletheia::tools::Risk;

fn tmp() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-cappr-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> SysCore {
    SysCore::open(tmp(), Box::new(DeterministicRuntime)).unwrap()
}

#[test]
fn an_ask_is_durable_idempotent_and_bound_to_its_line() {
    let dir = tmp();
    let pa = {
        let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
        let pa = core
            .request_console_approval("human:operator", "rm notes", Risk::Destructive)
            .unwrap();
        assert_eq!(pa.state, ApprovalState::Pending);
        assert_eq!(pa.subject, "human:operator");
        pa
    };
    // Reopen: the question outlives the process that asked it.
    let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
    let again = core
        .request_console_approval("human:operator", "rm notes", Risk::Destructive)
        .unwrap();
    assert_eq!(again.id, pa.id, "re-asking the same line finds the same pending record");
    assert_eq!(again.state, ApprovalState::Pending);
    // A different line is a different question.
    let other = core
        .request_console_approval("human:operator", "rm manifesto", Risk::Destructive)
        .unwrap();
    assert_ne!(other.id, pa.id);
    // And another subject does not collide with the first ask either.
    let third = core
        .request_console_approval("human:owner", "rm notes", Risk::Destructive)
        .unwrap();
    assert_ne!(third.id, pa.id);
}

#[test]
fn policy_not_required_is_refused_not_recorded() {
    let mut core = open();
    // A safe line must never enter the approval store: asking for its approval would manufacture
    // a governance question where the policy engine says there is none.
    let err = core
        .request_console_approval("human:operator", "ls", Risk::Safe)
        .unwrap_err();
    assert!(
        err.message.contains("does not gate"),
        "named refusal, got: {}",
        err.message
    );
    assert!(core.approvals_snapshot().is_empty());
}

#[test]
fn a_grant_types_exactly_once_and_binds_exactly_its_line() {
    let dir = tmp();
    let id = {
        let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
        core.request_console_approval("human:operator", "rm notes", Risk::Destructive)
            .unwrap()
            .id
    };
    let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();

    // Before the human answers, nothing types.
    assert!(core.take_console_approval("human:operator", "rm notes").is_err());

    core.resolve_console_approval(&id, true).unwrap();

    // A different line cannot ride on this grant.
    assert!(core.take_console_approval("human:operator", "rm manifesto").is_err());
    // Neither can a different subject.
    assert!(core.take_console_approval("human:owner", "rm notes").is_err());

    // The right driver, the right line: the grant is spent, once.
    let spent = core.take_console_approval("human:operator", "rm notes").unwrap();
    assert_eq!(spent, id);
    assert!(core.take_console_approval("human:operator", "rm notes").is_err());

    // Reopen: replaying the log keeps the authority spent.
    let mut core2 = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
    let rec = core2.get_approval(&id).expect("record survives restart");
    assert_eq!(rec.state, ApprovalState::Consumed, "a spent grant stays spent");
    assert!(core2.take_console_approval("human:operator", "rm notes").is_err());
}

#[test]
fn a_denial_is_a_record_that_never_types() {
    let dir = tmp();
    let id = {
        let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
        core.request_console_approval("human:operator", "reboot", Risk::Destructive)
            .unwrap()
            .id
    };
    let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
    let denied = core.resolve_console_approval(&id, false).unwrap();
    assert_eq!(denied.state, ApprovalState::Denied);
    assert!(core.take_console_approval("human:operator", "reboot").is_err());
    // Resolution is terminal — no re-granting a denied record into a typed line.
    assert!(core.resolve_console_approval(&id, true).is_err());
    // And the operator asking AGAIN starts a NEW question; the old denial stands.
    let second = core
        .request_console_approval("human:operator", "reboot", Risk::Destructive)
        .unwrap();
    assert_ne!(second.id, id, "a fresh ask after denial is a new record");
    assert_eq!(second.state, ApprovalState::Pending);
}

#[test]
fn the_two_approval_worlds_do_not_blur() {
    let mut core = open();
    let owner = core.bootstrap_owner("human:owner").unwrap().token;
    // Console machinery refuses a core-intent approval...
    let intent_id = {
        use aletheia::intent_action::{Intent, Verb};
        core.begin_task("human:owner");
        // Authority must ALLOW (the owner's root cap) so the intent reaches the GOVERNANCE stage:
        // destructive risk with no inline approve is what records a pending approval.
        let trace = core.handle_intent(
            std::slice::from_ref(&owner),
            Intent {
                subject: "human:owner".into(),
                verb: Verb::Delete { id: "ghost".into() },
            },
            false,
        );
        trace
            .approval_id
            .expect("a destructive core intent without inline approval records a pending")
    };
    let err = core
        .resolve_console_approval(&intent_id, true)
        .unwrap_err();
    assert!(
        err.message.contains("not a console approval"),
        "got: {}",
        err.message
    );
    // ...and the generic resolver refuses a console-bound record instead of executing nothing.
    let line_id = core
        .request_console_approval("human:operator", "halt", Risk::Destructive)
        .unwrap()
        .id;
    let err = core
        .resolve_approval(&[], &line_id, true)
        .unwrap_err();
    assert!(!err.message.is_empty());
    // The console record was untouched by the refused attempt.
    assert_eq!(core.get_approval(&line_id).unwrap().state, ApprovalState::Pending);
}
