//! Hosted tests for the extended capability-secure IPC substrate (gap-register Issue 2 tail):
//! asynchronous notifications, deadline/timeout-aware receive, cancellation, and trace/replay.
//!
//! These complement `invariants.rs` (which covers the M1 synchronous-send + capability-transfer +
//! bounded-queue invariants). Every new primitive is authorized through the SAME `CapEngine`, so the
//! fail-closed discipline is re-proved at each new boundary.

use kernel_core::ipc::{replay, Channel, IpcOp, Message, Notification, RecvOutcome};
use kernel_core::spine::{CapEngine, CapToken, Constraints, Decision, Scope};

// ---------------------------------------------------------------------------
// Asynchronous notifications
// ---------------------------------------------------------------------------

#[test]
fn notification_signal_is_capability_gated_and_coalesces() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut n = Notification::new("notify.signal");
    let cap = e.mint("driver", "notify.signal", Scope::All, Constraints::none());
    // Two authorized signals before a poll COALESCE (OR together) — the async badge property.
    assert_eq!(n.signal(&e, 0b001, &[cap]), Decision::Allow);
    assert_eq!(n.signal(&e, 0b010, &[cap]), Decision::Allow);
    assert_eq!(n.peek(), 0b011, "signals accumulate until consumed");
    assert_eq!(n.poll(), 0b011, "poll returns the coalesced badge");
    assert_eq!(n.poll(), 0, "poll consumes: a second poll sees nothing");
}

#[test]
fn notification_fail_closed_without_capability() {
    let e = CapEngine::new(0xA5A5, 1000);
    let mut n = Notification::new("notify.signal");
    // No capability offered => the signal is denied and NOTHING is set (fail closed).
    assert!(matches!(n.signal(&e, 0xFF, &[]), Decision::Deny(_)));
    assert_eq!(n.peek(), 0, "an unauthorized signal sets no bits");
    assert_eq!(n.poll(), 0);
}

// ---------------------------------------------------------------------------
// Deadline / timeout semantics
// ---------------------------------------------------------------------------

fn send_ok(ch: &mut Channel, e: &CapEngine, cap: CapToken, msg: Message) {
    assert_eq!(ch.send(e, msg, &[cap]), Decision::Allow);
}

#[test]
fn recv_at_delivers_message_before_its_deadline() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 42).with_deadline(100));
    match ch.recv_at(50) {
        RecvOutcome::Delivered(m) => assert_eq!(m.body, 42),
        other => panic!("expected delivery before deadline, got {other:?}"),
    }
}

#[test]
fn recv_at_drops_expired_message_fail_closed() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 7).with_deadline(100));
    // now (150) > deadline (100): the message is dropped, never delivered late.
    match ch.recv_at(150) {
        RecvOutcome::Expired(1) => {}
        other => panic!("expected 1 expired, got {other:?}"),
    }
    // The inbox is now empty — a late command cannot resurface.
    assert!(matches!(ch.recv_at(150), RecvOutcome::Empty));
    assert!(ch.pending_ids().is_empty());
}

#[test]
fn recv_at_skips_expired_and_delivers_the_live_one() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 1).with_deadline(100)); // will expire
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 2).with_deadline(200)); // still live at 150
    match ch.recv_at(150) {
        RecvOutcome::Delivered(m) => assert_eq!(m.body, 2, "expired head skipped, live tail delivered"),
        other => panic!("expected delivery of the live message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[test]
fn cancel_removes_an_undelivered_message() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 10));
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 20));
    let ids = ch.pending_ids();
    assert_eq!(ids.len(), 2);
    // Cancel the first: it must never be delivered; the second still is.
    assert!(ch.cancel(ids[0]), "an undelivered message can be cancelled");
    assert_eq!(ch.recv().map(|m| m.body), Some(20));
    assert!(ch.recv().is_none());
}

#[test]
fn cancel_after_delivery_returns_false() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 99));
    let id = ch.pending_ids()[0];
    let m = ch.recv().unwrap();
    assert_eq!(m.id, id);
    // Already delivered — cancellation is a no-op that reports it could not act.
    assert!(!ch.cancel(id), "a delivered message can no longer be cancelled");
}

// ---------------------------------------------------------------------------
// Trace + deterministic replay
// ---------------------------------------------------------------------------

#[test]
fn trace_replay_reconstructs_exact_delivery_order() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");

    // A mixed run exercising every op: a denied send, two live sends, a cancel, a delivery.
    assert!(matches!(ch.send(&e, Message::new("A", "B", 1), &[]), Decision::Deny(_))); // unauthorized
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 10));
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 20));
    let ids = ch.pending_ids();
    assert!(ch.cancel(ids[0])); // cancel body 10
    let observed: Vec<u64> = core::iter::from_fn(|| ch.recv().map(|m| m.body)).collect();
    assert_eq!(observed, vec![20], "only the uncancelled message is delivered");

    // The trace records every operation, in order...
    let trace = ch.trace();
    let ops: Vec<IpcOp> = trace.iter().map(|t| t.op).collect();
    assert_eq!(
        ops,
        vec![IpcOp::SendDenied, IpcOp::Send, IpcOp::Send, IpcOp::Cancel, IpcOp::Recv],
        "the trace is a complete, ordered log of every IPC operation"
    );
    // ...and replay() reconstructs the exact delivered sequence from the trace ALONE.
    assert_eq!(replay(trace), observed, "the trace deterministically replays the delivery behaviour");
}

#[test]
fn trace_replay_matches_deadline_expiry_run() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 1).with_deadline(100)); // expires at 150
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 2).with_deadline(200)); // survives
    let delivered = match ch.recv_at(150) {
        RecvOutcome::Delivered(m) => vec![m.body],
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(delivered, vec![2]);
    // The expired message is in the trace as Expired and replay agrees only body 2 was delivered.
    assert!(ch.trace().iter().any(|t| t.op == IpcOp::Expired && t.body == 1));
    assert_eq!(replay(ch.trace()), delivered);
}
