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
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 42).with_deadline(100),
    );
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
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 7).with_deadline(100),
    );
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
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 1).with_deadline(100),
    ); // will expire
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 2).with_deadline(200),
    ); // still live at 150
    match ch.recv_at(150) {
        RecvOutcome::Delivered(m) => {
            assert_eq!(m.body, 2, "expired head skipped, live tail delivered")
        }
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
    assert!(
        !ch.cancel(id),
        "a delivered message can no longer be cancelled"
    );
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
    assert!(matches!(
        ch.send(&e, Message::new("A", "B", 1), &[]),
        Decision::Deny(_)
    )); // unauthorized
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 10));
    send_ok(&mut ch, &e, cap, Message::new("A", "B", 20));
    let ids = ch.pending_ids();
    assert!(ch.cancel(ids[0])); // cancel body 10
    let observed: Vec<u64> = core::iter::from_fn(|| ch.recv().map(|m| m.body)).collect();
    assert_eq!(
        observed,
        vec![20],
        "only the uncancelled message is delivered"
    );

    // The trace records every operation, in order...
    let trace = ch.trace();
    let ops: Vec<IpcOp> = trace.iter().map(|t| t.op).collect();
    assert_eq!(
        ops,
        vec![
            IpcOp::SendDenied,
            IpcOp::Send,
            IpcOp::Send,
            IpcOp::Cancel,
            IpcOp::Recv
        ],
        "the trace is a complete, ordered log of every IPC operation"
    );
    // ...and replay() reconstructs the exact delivered sequence from the trace ALONE.
    assert_eq!(
        replay(trace),
        observed,
        "the trace deterministically replays the delivery behaviour"
    );
}

#[test]
fn trace_replay_matches_deadline_expiry_run() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::new("ipc.send");
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 1).with_deadline(100),
    ); // expires at 150
    send_ok(
        &mut ch,
        &e,
        cap,
        Message::new("A", "B", 2).with_deadline(200),
    ); // survives
    let delivered = match ch.recv_at(150) {
        RecvOutcome::Delivered(m) => vec![m.body],
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(delivered, vec![2]);
    // The expired message is in the trace as Expired and replay agrees only body 2 was delivered.
    assert!(ch
        .trace()
        .iter()
        .any(|t| t.op == IpcOp::Expired && t.body == 1));
    assert_eq!(replay(ch.trace()), delivered);
}

// ---------------------------------------------------------------------------
// INV-IPC-CANCEL contract (docs/INVARIANT-CONTRACTS.md) — adversarial cases, ALET-P1-017.
//
// Cancellation is a sender WITHDRAWING an undelivered command. The failure mode these defend against
// is a withdrawn command executing anyway — or a cancel that lies about having won the race.
// ---------------------------------------------------------------------------

/// A channel plus an engine that authorizes sends on it.
fn cancel_fixture() -> (CapEngine, CapToken, Channel) {
    let mut e = CapEngine::new(0x0CA1, 1_000_000);
    let cap = e.mint("sender", "ipc.send", Scope::All, Constraints::none());
    (e, cap, Channel::new("ipc.send"))
}

/// INV-IPC-CANCEL-1: a cancelled message is never delivered by ANY later receive — not by `recv`, not
/// by `recv_at`, no matter how many receives follow.
#[test]
fn a_cancelled_message_is_never_delivered_afterwards() {
    let (e, cap, mut ch) = cancel_fixture();
    for body in 1..=5u64 {
        assert_eq!(
            ch.send(&e, Message::new("A", "B", body), &[cap]),
            Decision::Allow
        );
    }
    let ids = ch.pending_ids();
    // Withdraw the middle three.
    for id in &ids[1..4] {
        assert!(ch.cancel(*id), "a queued message must be cancellable");
    }
    let mut delivered = Vec::new();
    while let Some(m) = ch.recv() {
        delivered.push(m.body);
    }
    assert_eq!(
        delivered,
        vec![1, 5],
        "INV-IPC-CANCEL-1: a cancelled command was delivered"
    );
    // Further receives cannot resurrect them either.
    assert!(ch.recv().is_none());
    assert!(matches!(ch.recv_at(u64::MAX), RecvOutcome::Empty));
}

/// INV-IPC-CANCEL-2: cancelling an id that is already gone — delivered, cancelled, or never queued —
/// returns false and changes nothing. A `true` return is the sender's evidence it won the race.
#[test]
fn cancelling_something_already_gone_is_a_refusal_not_a_lie() {
    let (e, cap, mut ch) = cancel_fixture();
    ch.send(&e, Message::new("A", "B", 1), &[cap]);
    ch.send(&e, Message::new("A", "B", 2), &[cap]);
    let ids = ch.pending_ids();

    let first = ch.recv().expect("delivered").id;
    assert!(
        !ch.cancel(first),
        "INV-IPC-CANCEL-2: cancelling an already-DELIVERED message claimed success"
    );
    assert!(ch.cancel(ids[1]), "the still-queued message is cancellable");
    assert!(
        !ch.cancel(ids[1]),
        "INV-IPC-CANCEL-2: a second cancel of the same id claimed success"
    );
    assert!(!ch.cancel(0), "id 0 is never a queued message");
    assert!(
        !ch.cancel(u64::MAX),
        "INV-IPC-CANCEL-2: a forged id claimed success"
    );
    assert!(
        ch.pending_ids().is_empty(),
        "a refused cancel mutated the queue"
    );
}

/// INV-IPC-CANCEL-3: cancellation removes exactly the named message and preserves the order of the
/// rest — removing the wrong one would execute a command the sender never withdrew.
#[test]
fn cancellation_removes_exactly_the_named_message_and_keeps_the_order() {
    let (e, cap, mut ch) = cancel_fixture();
    for body in 10..20u64 {
        ch.send(&e, Message::new("A", "B", body), &[cap]);
    }
    let ids = ch.pending_ids();
    // Cancel every third, from the back so indices stay meaningful in the expectation below.
    let mut cancelled = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if i % 3 == 2 {
            assert!(ch.cancel(*id));
            cancelled.push(*id);
        }
    }
    let remaining = ch.pending_ids();
    assert_eq!(
        remaining.len(),
        ids.len() - cancelled.len(),
        "INV-IPC-CANCEL-3: the wrong number of messages disappeared"
    );
    for id in &cancelled {
        assert!(!remaining.contains(id), "a cancelled id is still queued");
    }
    // Order preserved: the surviving ids appear in their original relative order.
    let expected: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|i| !cancelled.contains(i))
        .collect();
    assert_eq!(remaining, expected, "INV-IPC-CANCEL-3: queue order changed");
    // And delivery follows that same order.
    let mut delivered = Vec::new();
    while let Some(m) = ch.recv() {
        delivered.push(m.id);
    }
    assert_eq!(delivered, expected);
}

/// INV-IPC-CANCEL-4: every message ends in EXACTLY ONE terminal trace event — Recv, Expired or
/// Cancel. Two fates (or none) makes the audit log useless.
#[test]
fn every_message_reaches_exactly_one_terminal_trace_event() {
    let (e, cap, mut ch) = cancel_fixture();
    // A mix: some delivered, some cancelled, some expired.
    for body in 0..9u64 {
        let msg = if body % 3 == 0 {
            Message::new("A", "B", body).with_deadline(5)
        } else {
            Message::new("A", "B", body)
        };
        ch.send(&e, msg, &[cap]);
    }
    let ids = ch.pending_ids();
    assert!(ch.cancel(ids[1]));
    assert!(ch.cancel(ids[4]));
    // Advance past the deadline so the deadlined ones expire, and drain everything.
    while !matches!(ch.recv_at(9), RecvOutcome::Empty) {}

    let mut terminal: Vec<(u64, usize)> = Vec::new();
    for ev in ch.trace() {
        if matches!(ev.op, IpcOp::Recv | IpcOp::Expired | IpcOp::Cancel) {
            match terminal.iter_mut().find(|(id, _)| *id == ev.msg_id) {
                Some((_, n)) => *n += 1,
                None => terminal.push((ev.msg_id, 1)),
            }
        }
    }
    for id in &ids {
        let count = terminal.iter().find(|(i, _)| i == id).map(|(_, n)| *n);
        assert_eq!(
            count,
            Some(1),
            "INV-IPC-CANCEL-4: message {id} has {count:?} terminal events, not exactly one"
        );
    }
}

/// INV-IPC-CANCEL-5: a cancelled slot is reusable — cancelling frees capacity on a bounded channel —
/// and cancellation never lifts the bound.
#[test]
fn cancelling_frees_the_slot_for_a_later_send() {
    let mut e = CapEngine::new(0x0CA2, 1_000_000);
    let cap = e.mint("sender", "ipc.send", Scope::All, Constraints::none());
    let mut ch = Channel::bounded("ipc.send", 2);
    assert_eq!(
        ch.send(&e, Message::new("A", "B", 1), &[cap]),
        Decision::Allow
    );
    assert_eq!(
        ch.send(&e, Message::new("A", "B", 2), &[cap]),
        Decision::Allow
    );
    // Full: refused fail-closed.
    assert!(matches!(
        ch.send(&e, Message::new("A", "B", 3), &[cap]),
        Decision::Deny(_)
    ));
    let ids = ch.pending_ids();
    assert!(ch.cancel(ids[0]));
    // Exactly ONE slot came back — not the bound lifted.
    assert_eq!(
        ch.send(&e, Message::new("A", "B", 4), &[cap]),
        Decision::Allow
    );
    assert!(
        matches!(
            ch.send(&e, Message::new("A", "B", 5), &[cap]),
            Decision::Deny(_)
        ),
        "INV-IPC-CANCEL-5: cancellation lifted the capacity bound"
    );
    let bodies: Vec<u64> = {
        let mut v = Vec::new();
        while let Some(m) = ch.recv() {
            v.push(m.body);
        }
        v
    };
    assert_eq!(
        bodies,
        vec![2, 4],
        "the cancelled message must not be delivered"
    );
}

/// INV-IPC-CANCEL-6: a deadline and a cancel never both claim the same message.
#[test]
fn a_deadline_and_a_cancel_never_both_claim_the_same_message() {
    let (e, cap, mut ch) = cancel_fixture();
    ch.send(&e, Message::new("A", "B", 1).with_deadline(3), &[cap]);
    let id = ch.pending_ids()[0];
    // Expire it by receiving past the deadline...
    assert!(matches!(ch.recv_at(10), RecvOutcome::Expired(1)));
    // ...then a cancel must REFUSE: the message already has its fate.
    assert!(
        !ch.cancel(id),
        "INV-IPC-CANCEL-6: a cancel claimed a message the deadline had already dropped"
    );
    let terminals = ch
        .trace()
        .iter()
        .filter(|ev| {
            ev.msg_id == id && matches!(ev.op, IpcOp::Recv | IpcOp::Expired | IpcOp::Cancel)
        })
        .count();
    assert_eq!(
        terminals, 1,
        "INV-IPC-CANCEL-6: two terminal events for one message"
    );

    // The reverse order: cancelled first, then a deadline sweep must not also expire it.
    ch.send(&e, Message::new("A", "B", 2).with_deadline(3), &[cap]);
    let id2 = ch.pending_ids()[0];
    assert!(ch.cancel(id2));
    assert!(matches!(ch.recv_at(10), RecvOutcome::Empty));
    let terminals2 = ch
        .trace()
        .iter()
        .filter(|ev| {
            ev.msg_id == id2 && matches!(ev.op, IpcOp::Recv | IpcOp::Expired | IpcOp::Cancel)
        })
        .count();
    assert_eq!(
        terminals2, 1,
        "INV-IPC-CANCEL-6: the deadline also claimed a cancelled message"
    );
}
