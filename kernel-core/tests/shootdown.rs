//! Host-proof of the cross-core TLB shootdown coordination contract (REQ-SMP-004, ADR-021 Phase 3)
//! under REAL threads. Same doctrine as `tests/smpsched.rs` / `tests/sync.rs`: the coordination
//! that will run on real cores is hammered by genuinely parallel host threads, progress-gated
//! (never a fixed spin count — that races the thread scheduler and flakes).
//!
//! THIS is where the discriminating power lives (see `shootdown.rs` HONESTY note): the barrier
//! ordering is deterministic on host threads, so a broken barrier (reclaim-before-invalidate) is a
//! genuine test failure here — whereas the VM gates can only prove the mechanism runs on real
//! cores, not that QEMU TCG exhibits a stale TLB entry.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel_core::shootdown::{Invalidation, TlbShootdown};

const DEADLINE: Duration = Duration::from_secs(30);

/// A caller deadline hook that keeps waiting until a wall-clock budget elapses.
fn until(start: Instant, budget: Duration) -> impl FnMut() -> bool {
    move || start.elapsed() < budget
}

/// The core ordering guarantee: `request` returns only AFTER every target has run its invalidation.
/// Proved with a global monotonic op-stamp — the reclaim stamp taken right after `request` returns
/// is strictly greater than the stamp every target recorded when it performed this round's
/// invalidation. A barrier that returned early would let the reclaim stamp precede a target's
/// perform stamp, and the assertion would fire. Deterministic (no reliance on TLB semantics).
#[test]
fn request_returns_only_after_every_target_invalidated() {
    const NCPUS: usize = 4; // cpu 0 = initiator; 1..=3 = targets
    const ROUNDS: u64 = 200;
    let targets: Vec<usize> = (1..NCPUS).collect();

    let sd = Arc::new(TlbShootdown::new(NCPUS));
    let clock = Arc::new(AtomicU64::new(1)); // global monotonic op stamp
    let perform_stamp: Arc<Vec<AtomicU64>> =
        Arc::new((0..NCPUS).map(|_| AtomicU64::new(0)).collect());
    let done = Arc::new(AtomicBool::new(false));

    let workers: Vec<_> = (1..NCPUS)
        .map(|cpu| {
            let sd = Arc::clone(&sd);
            let clock = Arc::clone(&clock);
            let perform_stamp = Arc::clone(&perform_stamp);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let start = Instant::now();
                while !done.load(Ordering::Acquire) && start.elapsed() < DEADLINE {
                    sd.service(cpu, |_inv| {
                        // Stamp the moment this target completed its invalidation.
                        let s = clock.fetch_add(1, Ordering::SeqCst);
                        perform_stamp[cpu].store(s, Ordering::SeqCst);
                    });
                    std::hint::spin_loop();
                }
            })
        })
        .collect();

    for round in 0..ROUNDS {
        let inv = Invalidation::page(1, 0x4000 * (round + 1));
        let ok = sd.request(
            &targets,
            inv,
            until(Instant::now(), Duration::from_secs(10)),
        );
        assert!(
            ok,
            "round {round}: barrier must complete (targets are live)"
        );
        let reclaim = clock.fetch_add(1, Ordering::SeqCst);
        for &t in &targets {
            let ps = perform_stamp[t].load(Ordering::SeqCst);
            assert!(
                ps != 0 && ps < reclaim,
                "round {round}: target {t} must have invalidated (stamp {ps}) BEFORE the reclaim \
                 (stamp {reclaim}) — the barrier ordering"
            );
        }
    }

    done.store(true, Ordering::Release);
    for w in workers {
        w.join().unwrap();
    }
}

/// The use-after-free scenario the barrier exists to prevent, framed concretely: a "physical frame"
/// shared word is reclaimed (rewritten with a new tenant's SECRET) only after the shootdown. Every
/// target reads the frame through its "cached mapping" and records whether it ever saw SECRET while
/// still holding that mapping. With the barrier, SECRET is written only after every target has
/// dropped its mapping, so NO target can ever leak it. A broken barrier would let a target still
/// holding the stale mapping read the reclaimed SECRET.
#[test]
fn reclaim_never_races_a_live_stale_mapping() {
    const NCPUS: usize = 4;
    const ROUNDS: usize = 300;
    const OLD: u64 = 0x0000_0000_0000_0001;
    const SECRET: u64 = 0xDEAD_BEEF_FEED_FACE;
    let targets: Vec<usize> = (1..NCPUS).collect();

    let sd = Arc::new(TlbShootdown::new(NCPUS));
    let frame = Arc::new(AtomicU64::new(OLD));
    // Per-target: does this core currently hold a cached mapping to `frame`?
    let holds: Arc<Vec<AtomicBool>> =
        Arc::new((0..NCPUS).map(|_| AtomicBool::new(false)).collect());
    let leaked = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    let workers: Vec<_> = (1..NCPUS)
        .map(|cpu| {
            let sd = Arc::clone(&sd);
            let frame = Arc::clone(&frame);
            let holds = Arc::clone(&holds);
            let leaked = Arc::clone(&leaked);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let start = Instant::now();
                while !done.load(Ordering::Acquire) && start.elapsed() < DEADLINE {
                    // Read through the cached mapping while we still hold it. If the initiator
                    // reclaimed the frame (SECRET) while we still hold a live mapping, that is the
                    // leak the barrier must make impossible.
                    if holds[cpu].load(Ordering::Acquire) && frame.load(Ordering::Acquire) == SECRET
                    {
                        leaked.store(true, Ordering::Release);
                    }
                    // Service any shootdown: invalidating = dropping the cached mapping.
                    sd.service(cpu, |_inv| holds[cpu].store(false, Ordering::Release));
                    std::hint::spin_loop();
                }
            })
        })
        .collect();

    for r in 0..ROUNDS {
        // Arm: fresh frame owned by the OLD tenant; every target caches a mapping to it.
        frame.store(OLD, Ordering::Release);
        for &t in &targets {
            holds[t].store(true, Ordering::Release);
        }

        // Shoot down every target's mapping and WAIT for completion (the barrier).
        let ok = sd.request(
            &targets,
            Invalidation::all(1),
            until(Instant::now(), Duration::from_secs(10)),
        );
        assert!(ok, "round {r}: shootdown barrier must complete");
        for &t in &targets {
            assert!(
                !holds[t].load(Ordering::Acquire),
                "round {r}: target {t} must have dropped its mapping before reclaim"
            );
        }
        // Reclaim: hand the frame to a new tenant. Safe ONLY because the barrier completed.
        frame.store(SECRET, Ordering::Release);
    }

    done.store(true, Ordering::Release);
    for w in workers {
        w.join().unwrap();
    }
    assert!(
        !leaked.load(Ordering::Acquire),
        "no target may read the reclaimed SECRET through a stale mapping — the barrier failed"
    );
}

/// No request is lost under concurrent requesters hitting one target: the target's acknowledged
/// count must equal the total posted, and every requester's `request` must return true.
#[test]
fn concurrent_requests_to_one_target_are_never_lost() {
    const NCPUS: usize = 3;
    const REQUESTERS: usize = 4;
    const PER: usize = 500;
    const TARGET: usize = 2;

    let sd = Arc::new(TlbShootdown::new(NCPUS));
    let done = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicUsize::new(0));

    // One target servicing continuously.
    let servicer = {
        let sd = Arc::clone(&sd);
        let done = Arc::clone(&done);
        let served = Arc::clone(&served);
        std::thread::spawn(move || {
            let start = Instant::now();
            while !done.load(Ordering::Acquire) && start.elapsed() < DEADLINE {
                let n = sd.service(TARGET, |_inv| {});
                if n > 0 {
                    served.fetch_add(n, Ordering::SeqCst);
                }
                std::hint::spin_loop();
            }
        })
    };

    let requesters: Vec<_> = (0..REQUESTERS)
        .map(|k| {
            let sd = Arc::clone(&sd);
            std::thread::spawn(move || {
                for i in 0..PER {
                    let ok = sd.request(
                        &[TARGET],
                        Invalidation::page(1, (0x1000 * (k * PER + i)) as u64),
                        until(Instant::now(), Duration::from_secs(20)),
                    );
                    assert!(ok, "requester {k} item {i} must complete");
                }
            })
        })
        .collect();

    for r in requesters {
        r.join().unwrap();
    }
    // Give the servicer a moment to drain any final batch, then stop it.
    let drain_start = Instant::now();
    while sd.acked(TARGET) < (REQUESTERS * PER) as u64 && drain_start.elapsed() < DEADLINE {
        std::hint::spin_loop();
    }
    done.store(true, Ordering::Release);
    servicer.join().unwrap();

    assert_eq!(
        sd.acked(TARGET),
        (REQUESTERS * PER) as u64,
        "every posted invalidation must be acknowledged exactly once (none lost, none phantom)"
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        REQUESTERS * PER,
        "the servicer's own tally must match the acknowledged count"
    );
    assert_eq!(
        sd.pending(TARGET),
        0,
        "the target inbox must be fully drained"
    );
}

/// Fail-visible, never hang: a target that never services must make `request` return false when the
/// caller's deadline elapses — the reclaiming core then refuses to reclaim (fail closed), rather
/// than spinning forever. Deterministic: the deadline hook flips false after a bounded budget.
#[test]
fn unresponsive_target_makes_request_fail_not_hang() {
    let sd = TlbShootdown::new(2);
    // Target 1 never calls `service`, so its ack watermark never advances.
    let ok = sd.request(
        &[1],
        Invalidation::all(1),
        until(Instant::now(), Duration::from_millis(50)),
    );
    assert!(
        !ok,
        "request must abort (return false), not hang, when a target never acknowledges"
    );
    // The item is still queued; nothing was falsely acknowledged.
    assert_eq!(
        sd.pending(1),
        1,
        "the unacknowledged invalidation stays pending"
    );
    assert_eq!(sd.acked(1), 0, "no false acknowledgement");
}

/// Single-threaded FIFO + count correctness: several invalidations posted to one target, serviced
/// in order and acknowledged as a batch; a later request needs a higher watermark, satisfied by one
/// more service. The deadline hook doubles as an inline self-servicer so no second thread is needed.
#[test]
fn service_drains_fifo_and_acks_the_batch() {
    let sd = TlbShootdown::new(2);
    let seen = std::cell::RefCell::new(Vec::new());

    for va in [0x1000u64, 0x2000, 0x3000] {
        let done = std::cell::Cell::new(false);
        let ok = sd.request(&[1], Invalidation::page(1, va), || {
            if !done.get() {
                sd.service(1, |inv| seen.borrow_mut().push(inv.va.unwrap()));
                done.set(true);
            }
            true
        });
        assert!(
            ok,
            "request for page {va:#x} must complete once we service it"
        );
    }

    assert_eq!(
        *seen.borrow(),
        vec![0x1000u64, 0x2000, 0x3000],
        "invalidations must be serviced in FIFO order"
    );
    assert_eq!(sd.acked(1), 3, "all three invalidations acknowledged");
    assert_eq!(sd.pending(1), 0, "inbox drained");

    // A fresh request now needs watermark 4; servicing once more satisfies it.
    let done = std::cell::Cell::new(false);
    let ok = sd.request(&[1], Invalidation::all(1), || {
        if !done.get() {
            sd.service(1, |_| {});
            done.set(true);
        }
        true
    });
    assert!(ok);
    assert_eq!(sd.acked(1), 4);
}

// ---------------------------------------------------------------------------
// INV-TLB contract (docs/INVARIANT-CONTRACTS.md) — adversarial cases, ALET-P1-005.
//
// The tests above prove the barrier WORKS. These attempt to make it lie: a silent core, a borrowed
// acknowledgement, an aborted wait reported as success, an out-of-range target. Each names the
// contract id it defends, so a future reader can go from a failure to the written rule.
// ---------------------------------------------------------------------------

/// INV-TLB-1 + INV-TLB-5: while ANY addressed target stays silent, `request` must not complete — and
/// when the caller's deadline gives up, the answer must be `false` (never a partial success the
/// caller would read as "safe to reclaim").
#[test]
fn a_request_never_completes_while_any_target_is_silent() {
    let tlb = Arc::new(TlbShootdown::new(3));
    // Target 1 services; target 2 never does. The request addresses all three.
    tlb.service(0, |_| {});
    let start = Instant::now();
    let mut polls = 0u64;
    let ok = tlb.request(&[0, 1, 2], Invalidation::page(7, 0x1000), || {
        polls += 1;
        // Let target 1 drain mid-wait; target 2 remains silent forever.
        if polls == 5 {
            tlb.service(1, |_| {});
        }
        start.elapsed() < Duration::from_millis(200)
    });
    assert!(
        !ok,
        "INV-TLB-1/5: request claimed completion while target 2 never acknowledged"
    );
    // The silent target's work is still queued — nothing was silently dropped to make progress.
    assert_eq!(
        tlb.pending(2),
        1,
        "INV-TLB-3: the un-serviced invalidation vanished instead of staying queued"
    );
    // And once it services, a fresh request completes: the earlier failure left no wedge behind.
    tlb.service(2, |_| {});
    let start = Instant::now();
    let done = tlb.request(&[2], Invalidation::all(7), || {
        tlb.service(2, |_| {}); // the target keeps draining while the requester waits
        start.elapsed() < DEADLINE
    });
    assert!(done, "a later request must still work after an aborted one");
}

/// INV-TLB-2: an acknowledgement must count only AFTER the invalidation ran. Checked from inside
/// `perform`: at that instant the ack watermark must not yet include this item.
#[test]
fn an_ack_never_precedes_the_invalidation_it_covers() {
    let tlb = TlbShootdown::new(1);
    for round in 1..=8u64 {
        // Post WITHOUT waiting: the hook refuses to spin, so `request` returns false having queued
        // the item. Waiting here would deadlock — nothing services target 0 until below, which is the
        // point (INV-TLB-1: the barrier really does block until an ack arrives).
        let queued = tlb.request(&[0], Invalidation::page(1, round * 0x1000), || false);
        assert!(!queued, "INV-TLB-1: request completed with no acknowledgement");
        let acked_before = tlb.acked(0);
        let mut seen = 0usize;
        tlb.service(0, |_inv| {
            seen += 1;
            assert_eq!(
                tlb.acked(0),
                acked_before,
                "INV-TLB-2: the ack counter advanced BEFORE perform finished (round {round})"
            );
        });
        assert_eq!(seen, 1);
        assert_eq!(
            tlb.acked(0),
            acked_before + 1,
            "INV-TLB-3: exactly one ack per performed invalidation"
        );
    }
}

/// INV-TLB-3: every posted invalidation is performed exactly once — no drops, no duplicates — even
/// when many are posted before any are serviced, and even when they interleave with servicing.
#[test]
fn every_posted_invalidation_is_performed_exactly_once() {
    let tlb = TlbShootdown::new(1);
    let mut performed: Vec<u64> = Vec::new();
    let mut posted: Vec<u64> = Vec::new();
    for i in 0..25u64 {
        let va = 0x1000 * (i + 1);
        posted.push(va);
        // Post without waiting (deadline hook gives up immediately when not yet serviced).
        let _ = tlb.request(&[0], Invalidation::page(3, va), || false);
        if i % 4 == 3 {
            tlb.service(0, |inv| performed.push(inv.va.expect("a page invalidation carries a VA")));
        }
    }
    tlb.service(0, |inv| performed.push(inv.va.expect("a page invalidation carries a VA")));
    assert_eq!(
        performed, posted,
        "INV-TLB-3: the performed sequence is not exactly the posted sequence (drop or duplicate)"
    );
    assert_eq!(tlb.pending(0), 0, "queue not drained");
}

/// INV-TLB-4: two requesters must not satisfy each other's watermarks. Requester A posts and the
/// target services ONE item only; A completes, but B — which posted after — must not.
#[test]
fn concurrent_requests_never_borrow_each_others_acknowledgements() {
    let tlb = TlbShootdown::new(1);
    // A posts (item 1), B posts (item 2). Only one service call, draining BOTH is what we must avoid
    // asserting — so post A, service A, then post B and give B a short deadline.
    let _ = tlb.request(&[0], Invalidation::page(9, 0x2000), || false); // A's item queued
    tlb.service(0, |_| {}); // exactly A's item performed + acked
    let start = Instant::now();
    let b_ok = tlb.request(&[0], Invalidation::page(9, 0x3000), || {
        start.elapsed() < Duration::from_millis(150)
    });
    assert!(
        !b_ok,
        "INV-TLB-4: B completed on an acknowledgement that covered only A's item"
    );
    assert_eq!(tlb.pending(0), 1, "B's item should still be queued");
    tlb.service(0, |_| {});
}

/// INV-TLB-6: a target id outside the machine is ignored — never posted to, never waited on. A
/// request naming only bogus targets must complete immediately rather than hang.
#[test]
fn an_out_of_range_target_is_ignored_not_waited_on() {
    let tlb = TlbShootdown::new(2);
    let start = Instant::now();
    let ok = tlb.request(&[2, 7, 99], Invalidation::all(1), || {
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "INV-TLB-6: waited on an out-of-range target"
        );
        true
    });
    assert!(ok, "a request with no valid targets must complete");
    assert_eq!(tlb.pending(0), 0, "an in-range target was posted to");
    assert_eq!(tlb.pending(1), 0, "an in-range target was posted to");
    // A mixed set posts only to the valid member.
    let start = Instant::now();
    let mixed = tlb.request(&[1, 42], Invalidation::all(1), || {
        if tlb.pending(1) > 0 {
            tlb.service(1, |_| {});
        }
        start.elapsed() < DEADLINE
    });
    assert!(mixed, "the valid target's ack must be enough");
    assert_eq!(tlb.pending(0), 0, "target 0 was never addressed");
}
