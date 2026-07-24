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
