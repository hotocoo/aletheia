//! Hosted proof of the shared SpinLock (kernel-core/src/sync.rs, REQ-SMP-002): mutual exclusion
//! and Acquire/Release publication under real `std::thread` contention. The bare-metal SMP suites
//! use the SAME type on real cores; this keeps its semantics pinned in fast hosted CI.
use kernel_core::sync::SpinLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn spinlock_serializes_read_modify_write_across_threads() {
    // A non-atomic counter behind the lock: exactness is only possible if the lock excludes.
    let lock = Arc::new(SpinLock::new(0usize));
    const THREADS: usize = 8;
    const ROUNDS: usize = 10_000;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let mut guard = lock.lock();
                    // Deliberately non-atomic RMW — a broken lock loses increments here.
                    *guard += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(*lock.lock(), THREADS * ROUNDS, "lost increments => lock failed to exclude");
}

#[test]
fn spinlock_release_publishes_writes_to_next_holder() {
    // Writer publishes a payload under the lock; readers must observe it fully (Acquire pairing).
    let lock = Arc::new(SpinLock::new((0u64, 0u64)));
    let seen_torn = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let lock = Arc::clone(&lock);
            let seen_torn = Arc::clone(&seen_torn);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while stop.load(Ordering::Acquire) == 0 {
                    let guard = lock.lock();
                    let (a, b) = *guard;
                    // Invariant maintained by the writer: both halves always match.
                    if a != b {
                        seen_torn.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for i in 1..=5_000u64 {
        let mut guard = lock.lock();
        *guard = (i, i);
    }
    stop.store(1, Ordering::Release);
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(seen_torn.load(Ordering::Relaxed), 0, "reader observed a torn write");
}
