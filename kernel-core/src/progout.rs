//! What a console-started program writes, held without allocating (ADR-204).
//!
//! `SYS_WRITE_CONSOLE` is served on the trap path, where nothing may allocate (ADR-086) and nothing
//! may be lost without saying so. The sink keeps the first [`CAPACITY`] bytes a run writes and
//! COUNTS the rest; the console shows what was kept and says how much was not.

use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Target};
use crate::usermem::UserSlice;

/// Bytes one run may leave for the console.
pub const CAPACITY: usize = 256;

/// The authority a console `run` hands its program (ADR-204): one `console.output` capability,
/// minted when the run starts and gone when it ends. The chain is the operator's `run` (itself
/// authorized as `system.schedule`) -> this grant -> `SYS_WRITE_CONSOLE`; a task started any other
/// way holds no grant and is refused by the same evaluation every syscall effect goes through.
pub struct Grant {
    engine: CapEngine,
    token: CapToken,
}

/// The action `SYS_WRITE_CONSOLE` is authorized as.
pub const ACTION: &str = "console.output";

impl Grant {
    pub fn new(secret: u64) -> Self {
        let mut engine = CapEngine::new(secret, 0);
        let token = engine.mint("program:run", ACTION, Scope::All, Constraints::none());
        Grant { engine, token }
    }

    fn allows(&self) -> bool {
        self.engine
            .evaluate(ACTION, &Target::default(), &[self.token])
            == Decision::Allow
    }
}

/// Serve one `SYS_WRITE_CONSOLE(addr, len)`: refuse without a grant, refuse a range outside the
/// program's window `[user_start, user_end)` (which, on every target, is exactly its two mapped
/// pages), otherwise hand the range to `read` - the target's copy, in the task's address space -
/// and keep what fits. Returns the bytes kept, or `u64::MAX` when refused; nothing is appended on a
/// refusal.
pub fn serve_write<'a>(
    grant: Option<&Grant>,
    sink: &mut OutputSink,
    addr: u64,
    len: u64,
    user_start: u64,
    user_end: u64,
    read: impl FnOnce(UserSlice) -> &'a [u8],
) -> u64 {
    if !grant.is_some_and(Grant::allows) {
        return u64::MAX;
    }
    let Ok(len) = usize::try_from(len) else {
        return u64::MAX;
    };
    match UserSlice::validate(addr, len, user_start, user_end) {
        Ok(range) => sink.append(read(range)) as u64,
        Err(_) => u64::MAX,
    }
}

/// One run's output: the bytes kept and the number refused for want of room.
pub struct OutputSink {
    buf: [u8; CAPACITY],
    len: usize,
    dropped: u64,
}

impl OutputSink {
    pub const fn new() -> Self {
        OutputSink {
            buf: [0; CAPACITY],
            len: 0,
            dropped: 0,
        }
    }

    /// Empty the sink for the next run.
    pub fn reset(&mut self) {
        self.len = 0;
        self.dropped = 0;
    }

    /// Keep as much of `bytes` as fits; count the rest. Returns the bytes kept.
    pub fn append(&mut self, bytes: &[u8]) -> usize {
        let room = CAPACITY - self.len;
        let kept = bytes.len().min(room);
        self.buf[self.len..self.len + kept].copy_from_slice(&bytes[..kept]);
        self.len += kept;
        self.dropped += (bytes.len() - kept) as u64;
        kept
    }

    /// The bytes kept, in order.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Bytes refused because the sink was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for OutputSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static PAGE: [u8; 64] = [b'a'; 64];

    #[test]
    fn a_write_needs_the_grant_and_a_range_inside_the_window() {
        let grant = Grant::new(7);
        let mut sink = OutputSink::new();
        let read = |r: UserSlice| &PAGE[..r.len()];
        // No grant: refused, nothing kept.
        assert_eq!(
            serve_write(None, &mut sink, 0x1000, 4, 0x1000, 0x3000, read),
            u64::MAX
        );
        assert!(sink.bytes().is_empty());
        // Inside the window: kept.
        assert_eq!(
            serve_write(Some(&grant), &mut sink, 0x1000, 4, 0x1000, 0x3000, read),
            4
        );
        // Outside, straddling, overflowing, past the copy budget: refused, nothing appended.
        for (addr, len) in [
            (0x0800u64, 4u64),
            (0x2ffe, 4),
            (u64::MAX - 1, 4),
            (0x1000, (crate::usermem::MAX_USER_COPY + 1) as u64),
            (0xffff_0000_0000_0000, 4),
        ] {
            assert_eq!(
                serve_write(Some(&grant), &mut sink, addr, len, 0x1000, 0x3000, read),
                u64::MAX
            );
        }
        assert_eq!(sink.bytes(), b"aaaa");
    }

    #[test]
    fn keeps_what_fits_and_counts_the_rest() {
        let mut s = OutputSink::new();
        assert_eq!(s.append(b"hello"), 5);
        assert_eq!(s.bytes(), b"hello");
        assert_eq!(s.append(&[b'x'; 300]), CAPACITY - 5);
        assert_eq!(s.bytes().len(), CAPACITY);
        assert_eq!(s.dropped(), 300 - (CAPACITY as u64 - 5));
        assert_eq!(s.append(b"more"), 0);
        assert_eq!(s.dropped(), 300 - (CAPACITY as u64 - 5) + 4);
        s.reset();
        assert!(s.bytes().is_empty());
        assert_eq!(s.dropped(), 0);
    }
}
