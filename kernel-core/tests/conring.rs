//! Host proof of the console input ring (REQ-CON-002, ADR-045).
//!
//! The live suite proves the ring behaves on each CPU. These tests attack the one property that
//! decides whether an overflow is a nuisance or a security problem: **what is still in the buffer
//! afterwards**. A ring that overwrote its oldest byte could turn a typed command into a DIFFERENT
//! command that the editor would accept without complaint, so the tests below check the surviving
//! contents, not merely the counters.
use kernel_core::conring::{ConsoleRing, RING_CAPACITY};
use kernel_core::shell::MAX_LINE;

/// Deterministic pseudo-random byte, so a sweep is reproducible without an RNG in `no_std`.
fn mix(i: u32) -> u8 {
    ((i.wrapping_mul(2_654_435_761) >> 13) & 0xff) as u8
}

fn drain(r: &mut ConsoleRing) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(b) = r.pop() {
        out.push(b);
    }
    out
}

#[test]
fn a_full_line_always_fits_so_overflow_never_means_line_too_long() {
    assert_eq!(RING_CAPACITY, MAX_LINE);
}

#[test]
fn the_ring_is_fifo_at_every_fill_level() {
    for fill in 0..=RING_CAPACITY {
        let mut r = ConsoleRing::new();
        let written: Vec<u8> = (0..fill as u32).map(mix).collect();
        for b in &written {
            assert!(
                r.push(*b),
                "capacity is {RING_CAPACITY}, refused at fill {fill}"
            );
        }
        assert_eq!(r.len(), fill);
        assert_eq!(drain(&mut r), written, "reordered at fill {fill}");
        assert_eq!(r.dropped(), 0);
    }
}

#[test]
fn an_overflow_drops_the_newest_and_the_survivors_are_the_oldest_prefix() {
    // The property that matters: after any amount of overpressure, what remains is EXACTLY the
    // first RING_CAPACITY bytes offered — never a window that slid forward.
    for excess in [1usize, 7, 100, RING_CAPACITY, RING_CAPACITY * 3] {
        let mut r = ConsoleRing::new();
        let offered: Vec<u8> = (0..(RING_CAPACITY + excess) as u32).map(mix).collect();
        let mut accepted = 0usize;
        for b in &offered {
            if r.push(*b) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, RING_CAPACITY, "excess {excess}");
        assert_eq!(r.dropped(), excess as u64, "excess {excess}");
        assert_eq!(drain(&mut r), offered[..RING_CAPACITY], "excess {excess}");
    }
}

#[test]
fn a_command_already_in_the_buffer_cannot_be_turned_into_a_different_command() {
    // The concrete failure a drop-oldest ring would produce: `rm notes` losing its head reads as
    // `notes`. Fill the ring with a command, overpressure it, and require the command back intact.
    let mut r = ConsoleRing::new();
    let cmd = b"rm notes\r";
    for b in cmd {
        assert!(r.push(*b));
    }
    for _ in 0..(RING_CAPACITY * 2) {
        r.push(b'X'); // a flood arriving behind the command
    }
    let got = drain(&mut r);
    assert_eq!(&got[..cmd.len()], cmd, "the typed command was rewritten");
    assert!(got[cmd.len()..].iter().all(|&b| b == b'X'));
}

#[test]
fn the_dropped_counter_is_exact_and_monotonic_across_many_overflows() {
    let mut r = ConsoleRing::new();
    let mut expected_dropped = 0u64;
    for round in 0..50u32 {
        // Fill to capacity, then overpressure by a varying amount, then drain half.
        while r.push(mix(round)) {}
        expected_dropped += 1; // the push that returned false
        let excess = (round % 11) as u64;
        for _ in 0..excess {
            r.push(0xEE);
        }
        expected_dropped += excess;
        assert_eq!(r.dropped(), expected_dropped, "round {round}");
        for _ in 0..(RING_CAPACITY / 2) {
            r.pop();
        }
    }
}

#[test]
fn draining_an_empty_ring_is_none_forever_and_does_not_count_as_a_drop() {
    let mut r = ConsoleRing::new();
    for _ in 0..1000 {
        assert!(r.pop().is_none());
    }
    assert_eq!(r.dropped(), 0);
    assert!(r.is_empty());
    // And it still works afterwards — an empty-read must not corrupt the indices.
    assert!(r.push(b'k'));
    assert_eq!(r.pop(), Some(b'k'));
}

#[test]
fn interleaved_bursts_never_reorder_duplicate_or_lose_an_accepted_byte() {
    // The real pattern: the interrupt pushes while the console drains, at uneven rates. Every byte
    // the ring ACCEPTED must come out exactly once, in order — dropped bytes are the only losses.
    let mut r = ConsoleRing::new();
    let mut sent = 0u32;
    let mut accepted: Vec<u8> = Vec::new();
    let mut received: Vec<u8> = Vec::new();

    for step in 0..5000u32 {
        let burst = step % 13; // sometimes 0: the device goes quiet
        for _ in 0..burst {
            let b = mix(sent);
            sent += 1;
            if r.push(b) {
                accepted.push(b);
            }
        }
        let take = step % 9;
        for _ in 0..take {
            match r.pop() {
                Some(b) => received.push(b),
                None => break,
            }
        }
        assert!(r.len() <= RING_CAPACITY);
    }
    received.extend(drain(&mut r));
    assert_eq!(received, accepted);
    assert_eq!(r.dropped() as usize + accepted.len(), sent as usize);
}

#[test]
fn wrapping_is_exercised_far_past_the_capacity_without_drift() {
    let mut r = ConsoleRing::new();
    for i in 0..(RING_CAPACITY as u32 * 40) {
        let b = mix(i);
        assert!(r.push(b));
        assert_eq!(r.pop(), Some(b), "seam failure at {i}");
    }
    assert!(r.is_empty());
    assert_eq!(r.dropped(), 0);
}

#[test]
fn the_live_ring_suite_passes_on_the_host_too() {
    let mut names = Vec::new();
    let n = kernel_core::conring::ring_suite(&mut |_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        names.push(name.to_string());
    })
    .expect("the live ring suite holds on the host");
    assert_eq!(n as usize, names.len());
    assert!(n >= 8, "the suite lost invariants: {n}");
}
