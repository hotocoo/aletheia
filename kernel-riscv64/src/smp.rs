//! SMP — secondary-hart bring-up and cross-hart concurrency substrate (REQ-SMP-002, RISC-V twin
//! of `kernel/src/smp.rs`).
//!
//! The boot hart starts every other hart through the SBI HSM `hart_start` call (OpenSBI holds
//! them in STOPPED until then), hands each a private stack + per-hart identity (`tp`), enables
//! Sv39 translation over the SAME kernel tables, and then proves the cross-hart substrate on real
//! concurrently-executing harts: exact atomic accounting, release/acquire message passing, the
//! ADR-027 atomic authorize+execute primitive under live cross-hart revocation (behind the SHARED
//! `kernel_core::sync::SpinLock`), and an SBI IPI (sip.SSIP) delivered end-to-end.
//!
//! SCOPE (contract-honest): bring-up + concurrency substrate; cross-hart *scheduling* stays open
//! under REQ-SMP-001. With `-smp 1` the suite skips green (like the aarch64 twin); the VM gate
//! boots `-smp 4` and asserts the full marker, so CI cannot silently skip.
//!
//! CONCURRENCY RULES (load-bearing, same as aarch64): secondaries NEVER print; every capability-
//! engine access happens under the one SpinLock; liveness waits are progress-gated with wall-clock
//! deadlines (never fixed spin counts). IPI reception POLLS `sip.SSIP` with SIE masked — the
//! secondary never enters the boot hart's trap flow.
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::hal::ActiveHal;
use crate::sbi;
use crate::vm;
use kernel_core::spine::{CapEngine, CapToken, Constraints, Scope, Target};
use kernel_core::sync::SpinLock;
use kernel_core::Hal;

extern "C" {
    /// The boot hart's id, recorded by boot.s before BSS is zeroed (OpenSBI's lottery may pick
    /// any hart as boot hart — never assume 0).
    static BOOT_HART: u64;
    fn _secondary_start();
}

/// Max harts this suite drives (the gate boots 4).
const MAX_CPUS: usize = 8;
const STACK_BYTES: usize = 16 * 1024;

#[repr(C, align(16))]
struct SecondaryStack([u8; STACK_BYTES]);
/// One private stack per possible hart, indexed by hartid (BSS; zeroed before any hart_start).
static mut SECONDARY_STACKS: [SecondaryStack; MAX_CPUS] =
    [const { SecondaryStack([0; STACK_BYTES]) }; MAX_CPUS];

/// Sv39 root every secondary installs (captured from the boot hart's live satp by `selftest`).
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);
/// Bit i set ⇒ hart i is online (MMU on, per-hart state installed).
static ONLINE_MASK: AtomicUsize = AtomicUsize::new(0);
/// The hartid (from a0) and `tp` observed by each secondary (per-hart identity proof).
static SEEN_HARTID: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static SEEN_TP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
/// Secondaries that reached the final parking loop.
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// Phase gate — the boot hart advances it; secondaries wait on it.
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DONE_MAILBOX: AtomicUsize = AtomicUsize::new(0);
static DONE_CAPS: AtomicUsize = AtomicUsize::new(0);

// Phase 1 — exact cross-hart atomic accounting.
const COUNTER_ROUNDS: usize = 10_000;
static SHARED_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Phase 2 — release/acquire mailbox.
static MAILBOX_DATA: AtomicU64 = AtomicU64::new(0);
static MAILBOX_FLAG: AtomicBool = AtomicBool::new(false);
static MAILBOX_RESP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
const MAILBOX_PAYLOAD: u64 = 0x5EED_2026_C0DE_CAFE;

// Phase 3 — capability concurrency on real harts (ADR-027 mechanism).
static COMMITS: AtomicUsize = AtomicUsize::new(0);
/// Set by the boot hart INSIDE the engine lock, right after `revoke` — the linearization marker.
static REVOKED: AtomicBool = AtomicBool::new(false);
static POST_DENIED: AtomicUsize = AtomicUsize::new(0);
static POST_ALLOWED: AtomicUsize = AtomicUsize::new(0);
const POST_ATTEMPTS: usize = 64;

// Phase 4 — SBI IPI (sip.SSIP).
static IPI_SEEN: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

struct CapState {
    engine: CapEngine,
    cap: CapToken,
}
static ENGINE: SpinLock<Option<CapState>> = SpinLock::new(None);

// --- Time (deadlines) ------------------------------------------------------------------------
fn now_ticks() -> u64 {
    ActiveHal::timer_ticks()
}
fn deadline_after(secs: u64) -> u64 {
    now_ticks() + secs * ActiveHal::timer_freq_hz()
}

/// Spin until `cond` or `deadline` (ticks). True ⇒ condition met (progress-gated).
fn wait_until(deadline: u64, mut cond: impl FnMut() -> bool) -> bool {
    while !cond() {
        if now_ticks() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// --- sip.SSIP polling (the RISC-V IPI receive path) -------------------------------------------
const SIP_SSIP: usize = 1 << 1;

fn sip_read() -> usize {
    let v: usize;
    // SAFETY: reading sip is always sound in S-mode.
    unsafe { asm!("csrr {v}, sip", v = out(reg) v, options(nomem, nostack)) };
    v
}
fn sip_clear_ssip() {
    // SAFETY: clearing the supervisor software-interrupt pending bit is the documented S-mode ack.
    unsafe { asm!("csrc sip, {v}", v = in(reg) SIP_SSIP, options(nomem, nostack)) };
}

// --- Secondary-hart code path ------------------------------------------------------------------
/// Rust entry for a started hart (from `_secondary_start`: satp=0, SIE masked, SP = private
/// stack, a0 = hartid). Never returns; parks in WFI when the suite completes.
#[no_mangle]
pub extern "C" fn ksecondary(hartid: u64) -> ! {
    let hart = hartid as usize;

    // Trap hygiene first (per-hart stvec), then adopt the shared kernel address space.
    crate::trap::init();
    // SAFETY: root captured from the boot hart's live satp; identity-map precondition holds.
    unsafe { vm::enable(KERNEL_ROOT.load(Ordering::Acquire)) };

    // Per-hart identity: `tp` is the architectural per-hart pointer register.
    // SAFETY: writing/reading tp is always sound.
    unsafe { asm!("mv tp, {v}", v = in(reg) hartid, options(nomem, nostack)) };
    let tp: u64;
    // SAFETY: see above.
    unsafe { asm!("mv {v}, tp", v = out(reg) tp, options(nomem, nostack)) };
    SEEN_HARTID[hart].store(hartid, Ordering::SeqCst);
    SEEN_TP[hart].store(tp, Ordering::SeqCst);
    ONLINE_MASK.fetch_or(1 << hart, Ordering::SeqCst);

    // Phase 1 — hammer the shared counter; exactness proves real cross-hart atomicity.
    while PHASE.load(Ordering::SeqCst) < 1 {
        core::hint::spin_loop();
    }
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    DONE_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Phase 2 — release/acquire mailbox: observe the published payload, answer per-hart.
    while PHASE.load(Ordering::SeqCst) < 2 {
        core::hint::spin_loop();
    }
    while !MAILBOX_FLAG.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let data = MAILBOX_DATA.load(Ordering::Relaxed);
    MAILBOX_RESP[hart].store(data ^ (0xA1E7 + hartid), Ordering::SeqCst);
    DONE_MAILBOX.fetch_add(1, Ordering::SeqCst);

    // Phase 3 — capability concurrency on real harts: commit through the ADR-027 primitive until
    // the boot hart revokes; then every retry must fail closed.
    while PHASE.load(Ordering::SeqCst) < 3 {
        core::hint::spin_loop();
    }
    while !REVOKED.load(Ordering::Acquire) {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            let _ = state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            );
        }
        drop(guard);
        core::hint::spin_loop();
    }
    for _ in 0..POST_ATTEMPTS {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            match state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            ) {
                Ok(_) => POST_ALLOWED.fetch_add(1, Ordering::SeqCst),
                Err(_) => POST_DENIED.fetch_add(1, Ordering::SeqCst),
            };
        }
    }
    DONE_CAPS.fetch_add(1, Ordering::SeqCst);

    // Phase 4 — receive the boot hart's IPI: poll sip.SSIP (SIE stays masked ⇒ pending, not
    // taken), acknowledge by clearing the bit.
    while PHASE.load(Ordering::SeqCst) < 4 {
        core::hint::spin_loop();
    }
    let deadline = deadline_after(5);
    loop {
        if sip_read() & SIP_SSIP != 0 {
            sip_clear_ssip();
            IPI_SEEN[hart].store(true, Ordering::SeqCst);
            break;
        }
        if now_ticks() > deadline {
            break; // the boot hart's invariant 12 will report the miss
        }
        core::hint::spin_loop();
    }

    PARKED.fetch_add(1, Ordering::SeqCst);
    loop {
        // SAFETY: WFI in a loop is the architectural idle park.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

// --- Boot-hart selftest --------------------------------------------------------------------------
/// Prove the SMP invariants live. `Ok(n)` = all passed (`Ok(0)` = no other harts, skip green —
/// the VM gate boots `-smp 4` so a silent skip cannot pass CI); `Err((idx, name))` = failure.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            if !($cond) {
                kprintln!("  [FAIL {:>2}] {}", n, $name);
                return Err((n, $name));
            }
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }

    KERNEL_ROOT.store(vm::active_root(), Ordering::Release);
    // SAFETY: BOOT_HART is written once by boot.s before kmain; read-only here.
    let boot_hart = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BOOT_HART)) } as usize;
    let entry = _secondary_start as *const () as usize;

    // 1 — start every present hart via SBI HSM. Errors mark the end of the topology.
    let mut started_mask: usize = 0;
    let mut secondaries: usize = 0;
    // Index loop kept: SECONDARY_STACKS is `static mut`, which cannot be iterated safely.
    #[allow(clippy::needless_range_loop)]
    for hart in 0..MAX_CPUS {
        if hart == boot_hart {
            continue;
        }
        let stack_top = {
            // SAFETY: address-of a static array element; no data is read or written here.
            let base = unsafe { SECONDARY_STACKS[hart].0.as_ptr() as usize };
            base + STACK_BYTES
        };
        if sbi::hart_start(hart, entry, stack_top) == 0 {
            started_mask |= 1 << hart;
            secondaries += 1;
        }
    }
    if secondaries == 0 {
        kprintln!(
            "  [info  ] single-hart machine (HSM reports no other harts) — SMP suite skipped"
        );
        return Ok(0);
    }
    check!(
        secondaries >= 1,
        "smp: SBI HSM hart_start accepted for secondary harts"
    );

    // 2 — every started hart comes online (stack up, Sv39 on over the shared tables).
    check!(
        wait_until(deadline_after(5), || ONLINE_MASK.load(Ordering::SeqCst)
            == started_mask),
        "smp: all secondaries online with Sv39 translation on (shared kernel tables)"
    );
    kprintln!(
        "  [info  ] boot hart {}, {} secondary hart(s) online, mask {:#x}",
        boot_hart,
        secondaries,
        ONLINE_MASK.load(Ordering::SeqCst)
    );

    // 3 — per-hart identity: each hart saw its own hartid (a0) and its own `tp` value.
    let mut identity_ok = true;
    for hart in 0..MAX_CPUS {
        if started_mask & (1 << hart) == 0 {
            continue;
        }
        identity_ok &= SEEN_HARTID[hart].load(Ordering::SeqCst) == hart as u64;
        identity_ok &= SEEN_TP[hart].load(Ordering::SeqCst) == hart as u64;
    }
    check!(
        identity_ok,
        "smp: per-hart identity distinct (hartid + tp per hart)"
    );

    // 4 — exact atomic accounting across all harts (the boot hart participates too).
    PHASE.store(1, Ordering::SeqCst);
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    check!(
        wait_until(deadline_after(10), || DONE_COUNTER.load(Ordering::SeqCst)
            == secondaries),
        "smp: counter phase completes on every hart"
    );
    check!(
        SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: cross-hart atomic counter is exact (no lost increments)"
    );

    // 5 — release/acquire publication: every hart observed the exact payload.
    PHASE.store(2, Ordering::SeqCst);
    MAILBOX_DATA.store(MAILBOX_PAYLOAD, Ordering::Relaxed);
    MAILBOX_FLAG.store(true, Ordering::Release);
    check!(
        wait_until(deadline_after(10), || DONE_MAILBOX.load(Ordering::SeqCst)
            == secondaries),
        "smp: mailbox phase completes on every hart"
    );
    let mut mailbox_ok = true;
    for (hart, resp) in MAILBOX_RESP.iter().enumerate() {
        if started_mask & (1 << hart) == 0 {
            continue;
        }
        mailbox_ok &= resp.load(Ordering::SeqCst) == MAILBOX_PAYLOAD ^ (0xA1E7 + hart as u64);
    }
    check!(
        mailbox_ok,
        "smp: release/acquire mailbox observed exactly by every hart"
    );

    // 6..8 — ADR-027 on real parallel harts, behind the SHARED kernel-core SpinLock.
    {
        let mut guard = ENGINE.lock();
        let mut engine = CapEngine::new(0x5EED_2026, 1_000);
        let cap = engine.mint("smp-worker", "data.write", Scope::All, Constraints::none());
        *guard = Some(CapState { engine, cap });
    }
    PHASE.store(3, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(10), || {
            COMMITS.load(Ordering::SeqCst) >= secondaries * 4
        }),
        "smp: cross-hart commits flow through with_authorization (progress gate)"
    );
    let commits_at_revoke;
    {
        let mut guard = ENGINE.lock();
        let state = guard.as_mut().expect("engine installed above");
        let cap = state.cap;
        state.engine.revoke(cap);
        REVOKED.store(true, Ordering::Release); // linearization marker, inside the lock hold
        commits_at_revoke = COMMITS.load(Ordering::SeqCst);
    }
    check!(
        wait_until(deadline_after(10), || DONE_CAPS.load(Ordering::SeqCst)
            == secondaries),
        "smp: capability phase completes on every hart"
    );
    check!(
        COMMITS.load(Ordering::SeqCst) == commits_at_revoke,
        "smp: ZERO commits after revoke linearizes (atomic authorize+execute, ADR-027)"
    );
    check!(
        POST_ALLOWED.load(Ordering::SeqCst) == 0
            && POST_DENIED.load(Ordering::SeqCst) == secondaries * POST_ATTEMPTS,
        "smp: every post-revoke attempt on every hart fails closed"
    );

    // 9 — inter-processor interrupt: SBI send_ipi raises sip.SSIP on each hart; each acknowledges.
    PHASE.store(4, Ordering::SeqCst);
    for hart in 0..MAX_CPUS {
        if started_mask & (1 << hart) == 0 {
            continue;
        }
        let _ = sbi::send_ipi(1, hart);
    }
    check!(
        wait_until(deadline_after(5), || {
            (0..MAX_CPUS)
                .filter(|h| started_mask & (1 << h) != 0)
                .all(|h| IPI_SEEN[h].load(Ordering::SeqCst))
        }),
        "smp: SBI IPI (sip.SSIP) delivered + acknowledged on every secondary hart"
    );

    // 10 — the machine is still coherent: every secondary parked, nothing regressed.
    PHASE.store(5, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(5), || PARKED.load(Ordering::SeqCst)
            == secondaries)
            && ONLINE_MASK.load(Ordering::SeqCst) == started_mask
            && SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: all secondaries parked; online mask + counters stable"
    );

    Ok(n)
}
