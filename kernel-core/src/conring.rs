//! The console's input ring — what an interrupt hands to the shell (REQ-CON-002, ADR-045).
//!
//! A polled console reads the UART only while it is looking. An interrupt-driven one is handed bytes
//! at a moment of the device's choosing, which means there must be somewhere to put them, and that
//! place is bounded: a human can type faster than a journal transaction commits, and a peer on a
//! serial line can send without ever pausing.
//!
//! **The overflow policy is DROP-NEWEST, and that is the whole design decision.** A ring that
//! overwrites its oldest byte is easy to write and silently corrupts meaning: `rm notes` with its
//! head overwritten becomes `notes`, a *different command* that the editor will happily accept.
//! Dropping the newest byte instead truncates a burst — the user sees a short line, retypes it, and
//! nothing they already typed was rewritten underneath them. Every dropped byte is counted, so the
//! loss is reportable rather than invisible.
//!
//! **Capacity is [`RING_CAPACITY`] = `shell::MAX_LINE` + 1, and the last slot is kept for a line
//! terminator** (ADR-180), so a complete line AND the CR that ends it always fit. Until 2026-09-26 the
//! capacity was `MAX_LINE` exactly: a pasted line of 256 bytes filled the ring, drop-newest discarded
//! its CR, and the console looked hung until the operator pressed Enter again - at which point the
//! truncated line ran. The live console fuzz found it on all three CPUs.
//!
//! **A line that lost bytes is cancelled, never run.** Once any byte of a line is dropped, that line's
//! terminator is delivered as Ctrl-C: the editor abandons the damaged line (`^C`), and the next line
//! starts clean. The attribution is in-band, so drops can never be charged to the wrong line.
//!
//! This type is plain data with no locking of its own. The target owns the static instance and the
//! critical section around it, because "how do I keep an interrupt out of this" is a CPU question and
//! this crate answers none of those.

/// Bytes the ring holds: one full line plus the terminator that ends it.
pub const RING_CAPACITY: usize = crate::shell::MAX_LINE + 1;

/// Bytes a line may occupy: every slot but the one kept for its terminator.
pub const DATA_CAPACITY: usize = RING_CAPACITY - 1;

/// What the terminator of a damaged line becomes: the editor's cancel key.
const CANCEL: u8 = crate::shell::CTRL_C;

fn is_terminator(b: u8) -> bool {
    b == b'\r' || b == b'\n'
}

/// A single-producer (the interrupt) single-consumer (the console loop) byte ring.
pub struct ConsoleRing {
    buf: [u8; RING_CAPACITY],
    /// Where the next byte is written (producer side).
    head: usize,
    /// Where the next byte is read (consumer side).
    tail: usize,
    /// Live bytes. Kept explicitly rather than derived from head/tail, because the classic
    /// "leave one slot empty" trick makes full and empty indistinguishable exactly when it matters.
    len: usize,
    /// Bytes the device offered that the ring refused. Never resets: a console that lost input
    /// should be able to say so at any later moment.
    dropped: u64,
    /// A byte of the line now arriving was dropped: its terminator is delivered as Ctrl-C.
    damaged: bool,
}

impl Default for ConsoleRing {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleRing {
    pub const fn new() -> Self {
        ConsoleRing {
            buf: [0; RING_CAPACITY],
            head: 0,
            tail: 0,
            len: 0,
            dropped: 0,
            damaged: false,
        }
    }

    /// Bytes waiting to be read.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == RING_CAPACITY
    }

    /// How many bytes have been refused since boot.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Accept one byte from the device. Returns `false` when the ring was full and the byte was
    /// DROPPED — nothing already accepted is disturbed.
    ///
    /// Called from interrupt context, so it does no work that can fail, block, or allocate.
    pub fn push(&mut self, byte: u8) -> bool {
        let terminator = is_terminator(byte);
        // A data byte may not take the slot kept for the terminator.
        let room = if terminator {
            RING_CAPACITY
        } else {
            DATA_CAPACITY
        };
        if self.len >= room {
            self.dropped = self.dropped.saturating_add(1);
            self.damaged = true;
            return false;
        }
        let byte = if terminator && self.damaged {
            self.damaged = false;
            CANCEL
        } else {
            if terminator {
                self.damaged = false;
            }
            byte
        };
        self.buf[self.head] = byte;
        self.head = (self.head + 1) % RING_CAPACITY;
        self.len += 1;
        true
    }

    /// Free slots.
    pub fn free(&self) -> usize {
        RING_CAPACITY - self.len
    }

    /// Accept a whole sequence, or none of it (REQ-CON-004, ADR-050).
    ///
    /// A navigation key is not one byte: the left arrow is `ESC [ D`. Pushing those three
    /// individually would let a full ring keep the head and drop the tail, and a *truncated* escape
    /// sequence is worse than a dropped one — the editor's parser would still be waiting for a final
    /// byte when the operator's next real keystroke arrived, and would eat it. So the ring's unit of
    /// admission for a decoded key is the whole sequence: it fits and is accepted, or it does not
    /// fit and every one of its bytes is counted as dropped.
    ///
    /// Returns whether the sequence was accepted.
    pub fn push_seq(&mut self, bytes: &[u8]) -> bool {
        // A decoded key is data: it may not take the terminator's slot either.
        if bytes.len() > DATA_CAPACITY.saturating_sub(self.len) {
            self.dropped = self.dropped.saturating_add(bytes.len() as u64);
            self.damaged = true;
            return false;
        }
        for b in bytes {
            self.push(*b);
        }
        true
    }

    /// Take the oldest byte, or `None` when nothing has been typed.
    pub fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % RING_CAPACITY;
        self.len -= 1;
        Some(byte)
    }
}

/// Prove the ring on the live target. `logger` receives `(index, passed, name)` per invariant.
///
/// Runs on every target because the ring is arch-independent: a target whose ring behaved
/// differently under overflow would lose different keystrokes than its siblings, which is precisely
/// the sort of divergence the conformance contract exists to refuse.
pub fn ring_suite<F: FnMut(u32, bool, &str)>(logger: &mut F) -> Result<u32, (u32, &'static str)> {
    let mut n = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let passed = $cond;
            logger(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1. Empty means empty: a read before anything is typed yields nothing, not a stale byte.
    let mut r = ConsoleRing::new();
    check!(
        "conring: a read from an empty ring yields nothing (no stale byte)",
        // `is_empty` and `len` are separate accessors; asserting BOTH is the point — a ring whose
        // two answers disagree would report "nothing to read" while holding bytes.
        r.pop().is_none() && r.is_empty() && matches!(r.len(), 0)
    );

    // 2. Bytes come out in the order they were typed. A console that reorders input is unusable in
    //    a way no single-byte test would reveal.
    let mut r = ConsoleRing::new();
    for b in b"halt" {
        r.push(*b);
    }
    let mut out = [0u8; 4];
    for slot in out.iter_mut() {
        *slot = r.pop().unwrap_or(0);
    }
    check!(
        "conring: bytes are read in the order they were written (FIFO)",
        &out == b"halt" && r.is_empty()
    );

    // 3. The ring holds exactly its capacity — which is one whole line, so a full line never
    //    overflows the buffer that carries it.
    let mut r = ConsoleRing::new();
    let mut accepted = 0usize;
    for i in 0..crate::shell::MAX_LINE {
        if r.push(b'a' + (i % 26) as u8) {
            accepted += 1;
        }
    }
    let ended = r.push(b'\r');
    check!(
        "conring: the ring accepts exactly its capacity, and one whole line fits",
        accepted == crate::shell::MAX_LINE && ended && r.is_full() && r.dropped() == 0
    );

    // 4. THE policy: a full ring drops the NEWEST byte. What was already typed is never rewritten,
    //    so a command in the buffer cannot be silently turned into a different command.
    let mut r = ConsoleRing::new();
    for _ in 0..DATA_CAPACITY {
        r.push(b'A');
    }
    let refused = !r.push(b'Z');
    let first = r.pop();
    check!(
        "conring: a full ring refuses the NEWEST byte and never overwrites the oldest",
        refused && first == Some(b'A')
    );

    // 5. And the loss is counted, exactly — a console that lost input can say how much.
    let mut r = ConsoleRing::new();
    for _ in 0..DATA_CAPACITY {
        r.push(b'A');
    }
    for _ in 0..17 {
        r.push(b'Z');
    }
    check!(
        "conring: every refused byte is counted (loss is reportable, not invisible)",
        r.dropped() == 17
    );

    // 6. Dropping does not corrupt what is held: after an overflow the ring still reads back exactly
    //    the bytes it accepted, in order.
    let mut r = ConsoleRing::new();
    for i in 0..DATA_CAPACITY {
        r.push(0x20 + (i % 90) as u8);
    }
    for _ in 0..50 {
        r.push(0xFF);
    }
    let mut intact = true;
    for i in 0..DATA_CAPACITY {
        if r.pop() != Some(0x20 + (i % 90) as u8) {
            intact = false;
            break;
        }
    }
    check!(
        "conring: after an overflow the accepted bytes are still intact and in order",
        intact && r.is_empty()
    );

    // 7. The indices wrap. A ring that works once and breaks at the seam is a console that dies
    //    after the first few hundred keystrokes.
    let mut r = ConsoleRing::new();
    let mut wrapped_ok = true;
    for round in 0..(RING_CAPACITY * 3) {
        let b = (round % 251) as u8;
        if !r.push(b) || r.pop() != Some(b) {
            wrapped_ok = false;
            break;
        }
    }
    check!(
        "conring: head and tail wrap correctly over many times the capacity",
        wrapped_ok && r.is_empty() && r.dropped() == 0
    );

    // 8. Interleaved production and consumption — the real pattern, since the interrupt fires while
    //    the console is draining. Fill part-way, drain part-way, repeatedly, and stay consistent.
    let mut r = ConsoleRing::new();
    let mut expect_next = 0u32;
    let mut wrote = 0u32;
    let mut ok = true;
    // The consumer must out-pace the producer here (bursts of 1..=3 against drains of 1..=5), so
    // this invariant is about ORDER across the wrap seam and nothing else. Overflow has its own
    // invariants above; mixing the two would let a reordering bug hide behind an expected drop.
    for step in 0..1000u32 {
        let burst = (step % 3) + 1;
        for _ in 0..burst {
            if r.push((wrote % 251) as u8) {
                wrote += 1;
            }
        }
        let drain = (step % 5) + 1;
        for _ in 0..drain {
            match r.pop() {
                Some(b) => {
                    if b != (expect_next % 251) as u8 {
                        ok = false;
                    }
                    expect_next += 1;
                }
                None => break,
            }
        }
        if !ok || r.len() > RING_CAPACITY {
            ok = false;
            break;
        }
    }
    check!(
        "conring: interleaved writes and reads never reorder, duplicate or exceed capacity",
        ok && r.dropped() == 0
    );

    // 9. A decoded key is admitted WHOLE or not at all. With two slots free, a three-byte arrow
    //    sequence must not half-enter the ring: the editor would be left mid-sequence and would eat
    //    the operator's next real keystroke looking for a final byte that never comes.
    let mut r = ConsoleRing::new();
    for _ in 0..(RING_CAPACITY - 2) {
        r.push(b'x');
    }
    let refused = !r.push_seq(b"\x1b[D");
    let unchanged = r.len() == RING_CAPACITY - 2 && r.dropped() == 3;
    // And with room, the same sequence enters intact and in order.
    let mut r2 = ConsoleRing::new();
    let accepted = r2.push_seq(b"\x1b[D");
    let intact = r2.pop() == Some(0x1b) && r2.pop() == Some(b'[') && r2.pop() == Some(b'D');
    check!(
        "conring: an escape sequence is admitted whole or not at all, never truncated",
        refused && unchanged && accepted && intact
    );

    // 10. A pasted line longer than the ring (ADR-180): it keeps its terminator, the terminator
    //     arrives as Ctrl-C so the damaged line is cancelled rather than run truncated, and the next
    //     line is delivered clean.
    let mut r = ConsoleRing::new();
    for _ in 0..300 {
        r.push(b'a');
    }
    let ended = r.push(b'\r');
    let mut data = 0usize;
    let mut last = 0u8;
    while let Some(b) = r.pop() {
        if b == b'a' {
            data += 1;
        }
        last = b;
    }
    for b in b"ls\r" {
        r.push(*b);
    }
    let clean = r.pop() == Some(b'l') && r.pop() == Some(b's') && r.pop() == Some(b'\r');
    check!(
        "conring: a line that overflows keeps its terminator as Ctrl-C, so it is cancelled, never run truncated",
        ended && data == DATA_CAPACITY && last == CANCEL && clean && r.dropped() == 300 - DATA_CAPACITY as u64
    );

    Ok(n)
}
