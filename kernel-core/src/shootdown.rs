//! Cross-core TLB shootdown coordination — the arch-independent request/completion contract
//! (REQ-SMP-004, ADR-021 Phase 3), defined ONCE (gap-register Issue 1).
//!
//! WHY THIS MATTERS: once a second CPU exists, a page-table edit made by one core is not enough.
//! When core A unmaps a virtual→physical mapping (or remaps it to a different frame) in an address
//! space another core has active, that other core may keep using a STALE translation cached in its
//! TLB — reading or writing the OLD frame after A has unmapped it and possibly handed that physical
//! frame to a different security domain. That is a cross-core use-after-free straddling the address
//! space boundary: the exact class of bug the audit (GAPS2 #4) flags as the SMP correctness cliff.
//!
//! THE CONTRACT (what this module owns): a core that is about to **reclaim** a frame must not
//! proceed until every other core that could hold a stale translation to it has **completed** its
//! local invalidation. This module is the arch-independent *coordination* — an all-acknowledged
//! barrier: post an [`Invalidation`] to a set of target CPUs and block until every one has drained
//! it through its inbox AND run its local invalidation AND acknowledged. It owns nothing about HOW
//! a target invalidates — that is each backend's native mechanism (aarch64 `tlbi …is` broadcast;
//! x86-64 hand-rolled IPI + `invlpg` + ack; RISC-V SBI RFENCE) supplied through the `perform`
//! callback. Same arch-independent-policy-over-arch-specific-mechanism split as `smpsched`/`sched`.
//!
//! THE LOAD-BEARING ORDERING: a target [`service`](TlbShootdown::service)s by draining its inbox,
//! running `perform` for each item, and only THEN bumping its acknowledged count. So when
//! [`request`](TlbShootdown::request) observes every target's ack watermark reached, every target's
//! invalidation has genuinely happened-before the caller's next instruction (the reclaim). A
//! barrier that returned earlier would reintroduce exactly the stale-TLB window it exists to close.
//!
//! HONESTY (ADR-010, ADR-021 Phase 3): the *discriminating* proof lives here, on host threads
//! (`tests/shootdown.rs`) where the ordering is deterministic — without the barrier, the reclaim
//! races an unfinished invalidation and a target observes the reclaimed value. The per-target VM
//! gates prove the *mechanism + barrier on real cores* (the invalidation runs and is acknowledged
//! before the initiator proceeds), NOT that QEMU exhibits a stale entry: QEMU TCG's softmmu TLB is
//! a performance cache, not an architecturally faithful retention model, so "a core reads stale
//! without a shootdown" is not a deterministic observable there. This module does not claim to
//! prove the absence of stale reads on emulated hardware; it proves the coordination that prevents
//! them is real and correctly ordered.
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// Upper bound on CPUs a single shootdown coordinates — keeps [`TlbShootdown::request`] alloc-free
/// (it runs on the reclaiming core, not a hot spin, but the bare-metal bump allocators never
/// reclaim, so a fixed stack array is the disciplined choice). Matches the SMP suites' `MAX_CPUS`
/// (QEMU `virt` GICv2 tops out at 8; the gates boot 4).
pub const MAX_CPUS: usize = 8;

/// A single TLB invalidation request. `va == None` = invalidate the whole address space (by ASID);
/// `Some(va)` = a single page. The policy moves these opaquely; each backend interprets them with
/// its native invalidation instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalidation {
    /// Address-space identifier the invalidation targets.
    pub asid: u64,
    /// The page to invalidate, or `None` for the whole ASID.
    pub va: Option<u64>,
}

impl Invalidation {
    /// Invalidate one page in an address space.
    pub fn page(asid: u64, va: u64) -> Self {
        Invalidation { asid, va: Some(va) }
    }
    /// Invalidate an entire address space.
    pub fn all(asid: u64) -> Self {
        Invalidation { asid, va: None }
    }
}

/// Per-CPU TLB shootdown mailboxes with an all-acknowledged completion barrier. Every method takes
/// `&self`; each CPU's inbox is independently locked, so cores coordinate concurrently and contend
/// only when they touch the same inbox.
pub struct TlbShootdown {
    /// One inbox of pending invalidations per CPU.
    inboxes: Vec<SpinLock<VecDeque<Invalidation>>>,
    /// Monotonic count of items ever POSTED to each CPU's inbox (bumped under the inbox lock).
    posted: Vec<AtomicU64>,
    /// Monotonic count of items each CPU has ACKNOWLEDGED — drained AND invalidated via `perform`.
    /// Bumped only AFTER `perform` runs, so reaching a watermark proves the work happened.
    acked: Vec<AtomicU64>,
}

impl TlbShootdown {
    /// One inbox per CPU. `ncpus` is clamped to `[1, MAX_CPUS]`.
    pub fn new(ncpus: usize) -> Self {
        let n = ncpus.clamp(1, MAX_CPUS);
        let mut inboxes = Vec::with_capacity(n);
        let mut posted = Vec::with_capacity(n);
        let mut acked = Vec::with_capacity(n);
        for _ in 0..n {
            inboxes.push(SpinLock::new(VecDeque::new()));
            posted.push(AtomicU64::new(0));
            acked.push(AtomicU64::new(0));
        }
        TlbShootdown {
            inboxes,
            posted,
            acked,
        }
    }

    pub fn ncpus(&self) -> usize {
        self.inboxes.len()
    }

    /// Invalidations currently queued for `cpu` (0 for an out-of-range cpu).
    pub fn pending(&self, cpu: usize) -> usize {
        match self.inboxes.get(cpu) {
            Some(q) => q.lock().len(),
            None => 0,
        }
    }

    /// Total invalidations `cpu` has acknowledged over all time (0 for an out-of-range cpu).
    pub fn acked(&self, cpu: usize) -> u64 {
        match self.acked.get(cpu) {
            Some(a) => a.load(Ordering::Acquire),
            None => 0,
        }
    }

    /// Post `inv` to every target's inbox and BLOCK until each has acknowledged draining through it
    /// — the all-acknowledged barrier. `keep_waiting` is the caller's own deadline hook: it is
    /// polled while spinning and returning `false` aborts the wait. Returns `true` iff every target
    /// acknowledged before `keep_waiting` said stop.
    ///
    /// LOAD-BEARING (see module docs): the caller — about to reclaim / rewrite the physical frame
    /// the stale mappings point at — MUST treat a `false` return as failure and NOT reclaim. A
    /// `true` return means every target's local invalidation has completed and is visible.
    ///
    /// Targets not in `targets`, or `>= ncpus`, are ignored (never posted to, never waited on).
    pub fn request(
        &self,
        targets: &[usize],
        inv: Invalidation,
        mut keep_waiting: impl FnMut() -> bool,
    ) -> bool {
        // Post one item to each valid target, capturing the ack watermark our item sits at. Because
        // inbox order is FIFO and `service` acks by count, target t has drained+invalidated OUR
        // item once `acked[t] >= watermark[t]`. The watermark is captured under the inbox lock so a
        // concurrent poster cannot renumber our item.
        let mut watermark = [0u64; MAX_CPUS];
        let mut active = [false; MAX_CPUS];
        for &t in targets {
            if t >= self.inboxes.len() || t >= MAX_CPUS {
                continue;
            }
            let mut inbox = self.inboxes[t].lock();
            inbox.push_back(inv);
            // Our item is the (old+1)-th ever posted; it is acknowledged when acked[t] reaches that.
            let seq = self.posted[t].fetch_add(1, Ordering::AcqRel) + 1;
            watermark[t] = seq;
            active[t] = true;
        }

        // Spin until every active target reached its watermark, or the caller's deadline aborts.
        loop {
            let mut all_done = true;
            for t in 0..MAX_CPUS {
                if active[t] && self.acked[t].load(Ordering::Acquire) < watermark[t] {
                    all_done = false;
                    break;
                }
            }
            if all_done {
                return true;
            }
            if !keep_waiting() {
                return false;
            }
            core::hint::spin_loop();
        }
    }

    /// Drain `cpu`'s inbox, run `perform` for each queued invalidation (the backend's native TLB
    /// invalidation), and THEN acknowledge them. Returns the number serviced this call (0 if the
    /// inbox was empty). Acking strictly after `perform` is the ordering [`TlbShootdown::request`]'s
    /// barrier relies on — never bump the count before the invalidation has actually run.
    pub fn service(&self, cpu: usize, mut perform: impl FnMut(Invalidation)) -> usize {
        if cpu >= self.inboxes.len() {
            return 0;
        }
        // Take the whole queue under the lock, then release it before doing the (potentially
        // slow) hardware invalidation — never hold an inbox lock across `perform`.
        let drained: VecDeque<Invalidation> = {
            let mut inbox = self.inboxes[cpu].lock();
            core::mem::take(&mut *inbox)
        };
        let count = drained.len();
        for inv in drained {
            perform(inv);
        }
        if count > 0 {
            // Release ordering: the perform effects above happen-before the watermark advance a
            // requester observes with Acquire.
            self.acked[cpu].fetch_add(count as u64, Ordering::Release);
        }
        count
    }
}
