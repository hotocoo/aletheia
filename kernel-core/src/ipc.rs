//! Capability-secure kernel IPC substrate (gap-register Issue 2).
//!
//! Every higher-level service, component, application, and AI agent in Aletheia communicates through
//! explicit, capability-authorized boundaries — never ambient APIs. This module is the arch-independent
//! reification of that boundary in `kernel-core`, so all three CPU targets inherit it from one source.
//!
//! What it provides, each authorized by the SAME [`CapEngine`] the deterministic pipeline uses:
//!
//! * **Synchronous message passing** ([`Channel`]) — a send is authorized against the channel's
//!   `send_action` before delivery; an unauthorized send is dropped fail-closed and the receiver never
//!   observes it. Channels may be **bounded** — a send to a full inbox is refused fail-closed.
//! * **Capability transfer with attenuation** ([`Channel::send_transfer`]) — a send may also delegate a
//!   capability from the sender to the recipient, bounded by the SAME attenuation rules as
//!   [`CapEngine::delegate`], so a transfer can never amplify. All-or-nothing + fail-closed.
//! * **Asynchronous notifications** ([`Notification`]) — non-blocking, coalescing signal bits (the
//!   seL4-style badge model); signalling is capability-gated, waiting is by possession of the object.
//!   Unlike a channel this never queues or blocks: a signal ORs a badge into pending state.
//! * **Deadline / timeout semantics** — a [`Message`] may carry an `expires_at`; [`Channel::recv_at`]
//!   drops any message whose deadline has passed (a late command can never execute) and reports it.
//! * **Cancellation** — an undelivered message can be cancelled by its channel-assigned id
//!   ([`Channel::cancel`]); once received it can no longer be cancelled.
//! * **Tracing + deterministic replay** — every channel operation appends a [`TraceEvent`]; the pure
//!   function [`replay`] reconstructs the exact delivered-message sequence from the trace alone, so IPC
//!   behaviour is auditable and reproducible (gap-register Issue 2 "IPC tracing and replay support").
//!
//! Still architecture/design work (documented, not blind-coded — see `docs/adr/ADR-020`): zero-copy
//! shared-memory channels (needs the per-target MMU + a grant-table), priority inheritance/donation
//! (needs the scheduler in the loop), and wiring `send_transfer` into each target's cross-address-space
//! `usermode.rs` fast-path.
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Target};

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A kernel-mediated message. `id` is assigned by the channel on a successful enqueue (0 before that);
/// it is what [`Channel::cancel`] targets. `expires_at`, when set, is a deadline on the sender's logical
/// clock: a message not received by then is dropped in [`Channel::recv_at`] rather than delivered late.
#[derive(Clone, Debug)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub body: u64,
    /// A capability *transferred* to the recipient along with this message, if any. It is a real
    /// registry token (minted by the engine as an attenuated delegate of the sender's authority),
    /// so the recipient can use it exactly like any granted capability — and, because it is a
    /// registry entry, the transfer is auditable and revocable. `None` for an ordinary message.
    pub cap: Option<CapToken>,
    /// Channel-assigned delivery id (0 until enqueued). Stable for the message's queued lifetime.
    pub id: u64,
    /// Optional deadline on the sender's logical clock; see [`Channel::recv_at`].
    pub expires_at: Option<u64>,
}

impl Message {
    /// An ordinary message carrying no transferred capability and no deadline.
    pub fn new(from: &str, to: &str, body: u64) -> Self {
        Message {
            from: from.to_string(),
            to: to.to_string(),
            body,
            cap: None,
            id: 0,
            expires_at: None,
        }
    }

    /// Builder: attach a receive-by deadline (sender's logical clock). Consumed by [`Channel::recv_at`].
    pub fn with_deadline(mut self, expires_at: u64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }
}

/// The capability a sender asks the kernel to transfer to a recipient over a channel. The engine
/// delegates it from one of the sender's held capabilities, so it is bounded by attenuation.
#[derive(Clone, Debug)]
pub struct CapGrant {
    pub action: String,
    pub scope: Scope,
    pub constraints: Constraints,
}

// ---------------------------------------------------------------------------
// Trace / replay
// ---------------------------------------------------------------------------

/// The kind of IPC operation recorded in a channel's [`TraceEvent`] log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcOp {
    /// A message was authorized and enqueued.
    Send,
    /// A capability-transferring send authorized and enqueued.
    SendTransfer,
    /// A send was refused (unauthorized, full, or amplifying) — nothing enqueued.
    SendDenied,
    /// A message was delivered to a receiver.
    Recv,
    /// A message was dropped because its deadline had passed (never delivered).
    Expired,
    /// An undelivered message was cancelled by id.
    Cancel,
}

/// One recorded IPC operation. The trace is complete and ordered, so [`replay`] can reconstruct the
/// exact delivery behaviour from it alone — the auditable + reproducible property a real kernel needs.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceEvent {
    pub seq: u64,
    pub op: IpcOp,
    pub msg_id: u64,
    pub body: u64,
}

/// Deterministically reconstruct the ordered sequence of delivered message bodies from a channel
/// trace ALONE — no live channel needed. This is the IPC "replay": feeding the recorded
/// `Send`/`SendTransfer`/`Cancel`/`Recv`/`Expired` events into a fresh reducer reproduces exactly which
/// messages reached a receiver, and in what order. Asserting `replay(ch.trace()) == observed_deliveries`
/// proves the trace is a faithful, replayable record of the channel's behaviour.
pub fn replay(trace: &[TraceEvent]) -> Vec<u64> {
    // Reconstruct the queue from enqueue/cancel/expire events and emit a body for each Recv, in order.
    let mut queue: Vec<(u64, u64)> = Vec::new(); // (id, body) still-enqueued
    let mut delivered: Vec<u64> = Vec::new();
    for ev in trace {
        match ev.op {
            IpcOp::Send | IpcOp::SendTransfer => queue.push((ev.msg_id, ev.body)),
            IpcOp::Cancel | IpcOp::Expired => {
                if let Some(pos) = queue.iter().position(|(id, _)| *id == ev.msg_id) {
                    queue.remove(pos);
                }
            }
            IpcOp::Recv => {
                if let Some(pos) = queue.iter().position(|(id, _)| *id == ev.msg_id) {
                    let (_, body) = queue.remove(pos);
                    delivered.push(body);
                }
            }
            IpcOp::SendDenied => {}
        }
    }
    delivered
}

// ---------------------------------------------------------------------------
// Channel — synchronous, capability-gated, bounded, traced
// ---------------------------------------------------------------------------

/// A capability-gated channel. A send is authorized by the capability engine against the
/// `send_action` before the message is delivered; an unauthorized send is dropped (fail closed) and
/// the receiver never observes it. This models the microkernel IPC fast-path: authority check +
/// authenticated delivery, no ambient send rights.
///
/// A channel may be **bounded** ([`Channel::bounded`]) — a send to a full inbox is refused fail-closed
/// rather than growing without limit. Every operation appends to an ordered [`TraceEvent`] log so the
/// channel's behaviour is auditable and [`replay`]-able.
pub struct Channel {
    pub send_action: String,
    inbox: Vec<Message>,
    capacity: Option<usize>,
    next_seq: u64,
    trace: Vec<TraceEvent>,
    trace_seq: u64,
}

impl Channel {
    pub fn new(send_action: &str) -> Self {
        Channel {
            send_action: send_action.to_string(),
            inbox: Vec::new(),
            capacity: None,
            next_seq: 1,
            trace: Vec::new(),
            trace_seq: 0,
        }
    }

    /// A channel whose inbox holds at most `capacity` undelivered messages; further sends are
    /// refused fail-closed until the receiver drains one.
    pub fn bounded(send_action: &str, capacity: usize) -> Self {
        let mut ch = Channel::new(send_action);
        ch.capacity = Some(capacity);
        ch
    }

    /// True when a bounded channel's inbox is full (an unbounded channel is never full).
    fn is_full(&self) -> bool {
        matches!(self.capacity, Some(cap) if self.inbox.len() >= cap)
    }

    fn record(&mut self, op: IpcOp, msg_id: u64, body: u64) {
        self.trace_seq += 1;
        self.trace.push(TraceEvent {
            seq: self.trace_seq,
            op,
            msg_id,
            body,
        });
    }

    fn enqueue(&mut self, mut msg: Message) -> u64 {
        let id = self.next_seq;
        self.next_seq += 1;
        msg.id = id;
        self.inbox.push(msg);
        id
    }

    /// Authorized send. Returns the capability decision; on Allow the message is delivered.
    /// A full bounded channel refuses fail-closed (`Deny("inbox full")`) before any authorization
    /// check, and nothing is enqueued. Every outcome is traced.
    pub fn send(&mut self, engine: &CapEngine, msg: Message, offered: &[CapToken]) -> Decision {
        let body = msg.body;
        if self.is_full() {
            self.record(IpcOp::SendDenied, 0, body);
            return Decision::Deny("inbox full".to_string());
        }
        let decision = engine.evaluate(&self.send_action, &Target::default(), offered);
        if decision == Decision::Allow {
            let id = self.enqueue(msg);
            self.record(IpcOp::Send, id, body);
        } else {
            self.record(IpcOp::SendDenied, 0, body);
        }
        decision
    }

    /// Capability-transferring send. Authorizes the send via the channel's `send_action`, then
    /// delegates `grant` from `parent` (one of the sender's held capabilities) to the message's
    /// recipient (`msg.to`), attenuated by the SAME rules as [`CapEngine::delegate`] — so a transfer
    /// can never grant more than the sender holds. All-or-nothing + fail-closed: if the channel is
    /// full, the send is unauthorized, or the delegation would amplify (or `parent` is
    /// revoked/unknown), NOTHING is enqueued and NO token is minted. On success the delivered
    /// message carries the freshly minted attenuated recipient token in its `cap` field (any `cap`
    /// the caller set on `msg` is replaced), and that token is returned to the sender too.
    pub fn send_transfer(
        &mut self,
        engine: &mut CapEngine,
        mut msg: Message,
        parent: CapToken,
        grant: CapGrant,
        offered: &[CapToken],
    ) -> Result<CapToken, Decision> {
        let body = msg.body;
        if self.is_full() {
            self.record(IpcOp::SendDenied, 0, body);
            return Err(Decision::Deny("inbox full".to_string()));
        }
        // Authorize the send itself first — no delegation happens on an unauthorized send.
        let decision = engine.evaluate(&self.send_action, &Target::default(), offered);
        if decision != Decision::Allow {
            self.record(IpcOp::SendDenied, 0, body);
            return Err(decision);
        }
        // Delegate the transferred capability, attenuated. Amplification / revoked / unknown parent
        // => fail closed: nothing is minted and nothing is enqueued.
        let token = match engine.delegate(
            parent,
            &msg.to,
            &grant.action,
            grant.scope,
            grant.constraints,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.record(IpcOp::SendDenied, 0, body);
                return Err(Decision::Deny(e));
            }
        };
        msg.cap = Some(token);
        let id = self.enqueue(msg);
        self.record(IpcOp::SendTransfer, id, body);
        Ok(token)
    }

    /// Receive the oldest undelivered message, ignoring deadlines (the backward-compatible fast-path).
    /// Prefer [`Channel::recv_at`] when messages may carry deadlines.
    pub fn recv(&mut self) -> Option<Message> {
        if self.inbox.is_empty() {
            None
        } else {
            let msg = self.inbox.remove(0);
            self.record(IpcOp::Recv, msg.id, msg.body);
            Some(msg)
        }
    }

    /// Deadline-aware receive at logical time `now`. Any message at the front whose `expires_at` has
    /// passed (`now > expires_at`) is dropped — a late command is never delivered (fail-closed timeout).
    /// Returns the first live message if one remains, else [`RecvOutcome::Expired`] with the count of
    /// messages dropped this call, else [`RecvOutcome::Empty`]. Dropped and delivered messages are both
    /// traced.
    pub fn recv_at(&mut self, now: u64) -> RecvOutcome {
        let mut expired = 0usize;
        while let Some(front) = self.inbox.first() {
            match front.expires_at {
                Some(exp) if now > exp => {
                    let dead = self.inbox.remove(0);
                    self.record(IpcOp::Expired, dead.id, dead.body);
                    expired += 1;
                }
                _ => {
                    let msg = self.inbox.remove(0);
                    self.record(IpcOp::Recv, msg.id, msg.body);
                    return RecvOutcome::Delivered(msg);
                }
            }
        }
        if expired > 0 {
            RecvOutcome::Expired(expired)
        } else {
            RecvOutcome::Empty
        }
    }

    /// Cancel an undelivered message by its channel-assigned id. Returns true if it was still queued
    /// and is now removed; false if it was never queued or has already been received/expired/cancelled.
    pub fn cancel(&mut self, id: u64) -> bool {
        if let Some(pos) = self.inbox.iter().position(|m| m.id == id) {
            let body = self.inbox[pos].body;
            self.inbox.remove(pos);
            self.record(IpcOp::Cancel, id, body);
            true
        } else {
            false
        }
    }

    /// Ids of the currently undelivered messages, oldest first — the handles [`Channel::cancel`] takes.
    pub fn pending_ids(&self) -> Vec<u64> {
        self.inbox.iter().map(|m| m.id).collect()
    }

    /// The complete ordered operation log for this channel (for audit and [`replay`]).
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }
}

// ---------------------------------------------------------------------------
// RecvOutcome
// ---------------------------------------------------------------------------

/// The result of a deadline-aware [`Channel::recv_at`].
#[derive(Clone, Debug)]
pub enum RecvOutcome {
    /// A live message was delivered.
    Delivered(Message),
    /// No live message remained; `n` expired messages were dropped this call.
    Expired(usize),
    /// The inbox was empty.
    Empty,
}

// ---------------------------------------------------------------------------
// Notification — asynchronous, non-blocking, coalescing signal
// ---------------------------------------------------------------------------

/// A capability-gated asynchronous notification (the seL4-style badge model). Unlike a [`Channel`] it
/// carries no queue and never blocks: a `signal` ORs a badge into the pending bits and returns
/// immediately whether or not a receiver is waiting; repeated signals **coalesce** (OR together) until
/// the owner consumes them with `poll`. Signalling is authorized against `signal_action` by the same
/// [`CapEngine`]; waiting/polling is by possession of the notification object itself (the receive
/// authority is the object, mirroring the possession-based capability model).
pub struct Notification {
    pub signal_action: String,
    bits: u64,
}

impl Notification {
    pub fn new(signal_action: &str) -> Self {
        Notification {
            signal_action: signal_action.to_string(),
            bits: 0,
        }
    }

    /// Capability-gated signal: on Allow, OR `badge` into the pending bits (non-blocking, coalescing).
    /// Fail-closed: without authority nothing is set and the receiver observes no signal.
    pub fn signal(&mut self, engine: &CapEngine, badge: u64, offered: &[CapToken]) -> Decision {
        let decision = engine.evaluate(&self.signal_action, &Target::default(), offered);
        if decision == Decision::Allow {
            self.bits |= badge;
        }
        decision
    }

    /// Consume and clear all pending signals, returning the accumulated badge (0 if none pending).
    pub fn poll(&mut self) -> u64 {
        let b = self.bits;
        self.bits = 0;
        b
    }

    /// The pending badge without consuming it.
    pub fn peek(&self) -> u64 {
        self.bits
    }
}
