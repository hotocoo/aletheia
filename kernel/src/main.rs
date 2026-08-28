//! Aletheia microkernel reference (bare-metal aarch64, QEMU virt).
//!
//! Boot -> in-kernel capability-secure spine -> invariant selftests -> IPC benchmark -> exit.
//! Runs entirely in kernel space (EL1) and enforces the same invariants the M1 hosted System
//! Core proved in userspace (ADR-010: contract-honest rehosting on real privilege). The VM's
//! semihosting exit code is the machine-checkable verdict:
//!   0     => all invariants held (e2e PASS)
//!   10+i  => invariant i failed
//!   150+i => risk-advisor invariant i failed (REQ-ML-001, ADR-056) — this window overlaps the
//!            160+ family, so a risk-advisor failure is identified by its `[mlrisk] FAILED at
//!            risk-advisor invariant N: <name>` line, never by the exit code alone
//!   101   => kernel panic
//!   102   => unexpected CPU exception
#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));

#[macro_use]
mod uart;
mod arch;
mod bench;
mod conirq;
mod dtb;
mod frames;
mod fwcfg;
mod hal;
mod heap;
mod pci;
mod semihosting;
mod shellio;
mod smmu;
mod smp;
mod usermode;
mod virtio;
mod vm;

// The capability-secure spine + the M1 invariant suite are arch-independent and live in
// `kernel-core` — defined once, shared by all three targets (gap-register Issue 1). This kernel
// provides only its own backend (`hal`) + console (`kprintln!`).
use kernel_core::{selftest, spine};

/// Kernel entry, called from `_start` (boot.s) after stack + BSS setup.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    use hal::{ActiveHal, Hal};
    // Device-tree discovery runs BEFORE any frame churns: the DTB lives in RAM the frame
    // pool manages, so a late parse could read buffers long since handed out (ADR-074).
    smmu::discover_early();

    kprintln!("========================================");
    kprintln!(
        " Aletheia microkernel — HAL backend: {}",
        ActiveHal::arch_name()
    );
    kprintln!("========================================");
    kprintln!(
        "[hal] first-class targets: AMD64/x86-64, RISC-V  (aarch64 = bootstrap/dev; ADR-019)"
    );
    kprintln!("[boot] OK: stack ready, BSS clear");
    kprintln!("[boot] privilege level: {}", ActiveHal::current_privilege());
    kprintln!("[boot] timer freq: {} Hz", ActiveHal::timer_freq_hz());
    kprintln!("[boot] heap: {} B used after init", heap::used_bytes());

    // Physical memory: bring up the frame allocator over the RAM above the static kernel region,
    // with its ownership table attached (REQ-MM-002). A pool whose tail has no ownership state
    // could not detect a double free there, so failing to attach is fatal, not a warning.
    if !frames::init() {
        kprintln!("[mm] FATAL: frame ownership table does not cover the pool");
        semihosting::exit(39);
    }
    kprintln!(
        "[mm] frame allocator: {} frames ({} MiB) free above kernel, up to {:#x}",
        frames::free_count(),
        frames::free_count() * frames::FRAME_SIZE / (1024 * 1024),
        frames::RAM_END,
    );

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in kernel space) ---");
    match selftest::run(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[selftest] ALL {} INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[selftest] FAILED at invariant {}: {}", idx, name);
            semihosting::exit(10 + idx as i32);
        }
    }

    // What a scancode MEANS (REQ-CON-003, ADR-049). This target has no PS/2 controller — the QEMU
    // `virt` machine exposes none, an honest architectural difference — but the decoder is
    // arch-independent and its output alphabet is the console's shared contract, so it is proved
    // here too rather than only where the hardware happens to be.
    kprintln!("");
    kprintln!("--- keyboard-decode selftests (scancodes to the bytes the console accepts) ---");
    match kernel_core::keymap::keymap_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[keys] ALL {} KEYBOARD-DECODE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[keys] FAILED at keyboard-decode invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(90 + idx as i32);
        }
    }

    // Capability lifetime across a reboot (REQ-CAP-008, ADR-048). The spine suite above proves
    // authority is correct while the machine is up; this proves what survives a restart and — far
    // more important — what a restored registry must REFUSE, since a persisted registry is
    // untrusted input and a load that trusts it is a minting path with no delegation behind it.
    kprintln!("");
    kprintln!("--- capability-lifetime selftests (a persisted registry is untrusted input) ---");
    match kernel_core::capstore::capstore_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[cap] ALL {} CAPABILITY-LIFETIME INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[cap] FAILED at capability-lifetime invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(110 + idx as i32);
        }
    }

    // The frozen risk forest this image carries (REQ-ML-001, ADR-056). It is ADVISORY: it may

    // The IOMMU contract (ALET-P1-018, ADR-071): per-device translation spaces,
    // deny-by-default, the kernel image unmappable on both sides, named faults.
    // Modeled in software (the boot heap cannot afford sweep churn - ADR-063);
    // hardware realization is scoped in the gap register.
    kprintln!("");
    kprintln!("--- iommu selftests (device-visible memory enforced by translation) ---");
    match kernel_core::iommu::iommu_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[iommu] ALL {} IOMMU-CONTRACT INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[iommu] FAILED at iommu-contract invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(130 + idx as i32);
        }
    }

    // The power/performance contract (ALET-P2-022, ADR-076): frequency is authority, heat is a
    // hard ceiling. Elevation into the overclock band needs a live per-domain grant (attenuated
    // on delegation, clamping the domain back to nominal on revocation), the thermal envelope is
    // absolute, a trip clamps every domain and latches a cooldown, the governor never overclocks
    // and parks zero-demand domains, device power moves only along legal arcs, and every act
    // lands in the audit ledger. Modeled in software (the boot heap cannot afford sweep churn -
    // ADR-063); a hardware rung (MSR/CPPC programming) stays scoped in the gap register.
    kprintln!("");
    kprintln!("--- power/performance selftests (frequency is authority, heat is a ceiling) ---");
    match kernel_core::pm::pm_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[pm] ALL {} POWER-PERFORMANCE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[pm] FAILED at power-performance invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(560 + idx as i32);
        }
    }

    // Lethe (REQ-ML-006, ADR-077): the resident performance advisor for the power/performance
    // contract - a frozen integer model (two decision trees, ALTH1) consulted by the advised
    // governor path. The suite proves the advisory discipline on the live path: with Lethe
    // present the overclock band stays authority-only and demanded silicon is never parked;
    // with the advisor absent or abstaining the advised path is bit-identical to the ADR-076
    // baseline governor; every way the blob can be wrong is a named refusal; and parity with
    // the trainer is a committed fixture replayed through the live observer.
    kprintln!("");
    kprintln!("--- lethe advisor selftests (the power governor learns) ---");
    match kernel_core::lethe::Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR) {
        Ok(a) => kprintln!(
            "[lethe] bundled advisor: {} freq nodes, {} idle nodes, worst case {} node visits per walk",
            a.shape().0,
            a.shape().1,
            a.shape().2
        ),
        Err(e) => kprintln!("[lethe] bundled advisor REFUSED: {:?}", e),
    }
    match kernel_core::lethe::lethe_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[lethe] ALL {} LETHE ADVISOR INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[lethe] FAILED at lethe invariant {}: {}", idx, name);
            semihosting::exit(580 + idx as i32);
        }
    }

    // reorder tasks whose effective priority is already equal, and nothing else. So the suite proves
    // the advisory property itself — an abstaining model schedules bit-identically to the model-free
    // kernel, and priority is never traded for risk — alongside exact parity with the trainer and a
    // NAMED refusal for every way a blob can be wrong. The printed invariant NAME is the
    // authoritative diagnosis; the exit code is a coarse index on top of it.
    kprintln!("");
    kprintln!("--- risk-advisor selftests (a frozen integer forest, advisory only) ---");
    match kernel_core::mlrisk::RiskAdvisor::load(kernel_core::mlrisk::BUNDLED_MODEL) {
        Ok(m) => kprintln!(
            "[mlrisk] bundled forest: {} trees, {} nodes, worst case {} compares per advice",
            m.trees(),
            m.nodes(),
            m.worst_case_compares()
        ),
        // Absence is NAMED, never silent: the kernel says which check refused the blob and carries
        // on with its deterministic policy (the suite below then fails on this same load).
        Err(e) => kprintln!("[mlrisk] bundled forest REFUSED: {:?}", e),
    }
    match kernel_core::mlrisk::mlrisk_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[mlrisk] ALL {} RISK-ADVISOR INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[mlrisk] FAILED at risk-advisor invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(150 + idx as i32);
        }
    }

    // The forest under LOAD, on this machine, with this machine's own clock: what an advice costs,
    // and what the advice actually changes about a schedule. Timings are REPORTED; only the
    // scale-invariant properties gate the boot (REQ-ML-002, ADR-056).
    kprintln!("[mlrisk-stress] heap before: {} B used", heap::used_bytes());
    match kernel_core::mlrisk::RiskAdvisor::load(kernel_core::mlrisk::BUNDLED_MODEL) {
        Ok(model) => {
            let advices = kernel_core::mlrisk_stress::BOOT_ADVICES;
            let tasks = kernel_core::mlrisk_stress::BOOT_TASKS;
            match kernel_core::mlrisk_stress::stress_suite(
                advices,
                tasks,
                |a, t| kernel_core::mlrisk_stress::measure::<ActiveHal>(&model, a, t),
                |a| {
                    kernel_core::mlrisk_stress::advice_stress::<ActiveHal>(
                        &model,
                        a,
                        0,
                        kernel_core::mlrisk_stress::HOT_SEED,
                    )
                },
                |n, passed, name| {
                    if passed {
                        kprintln!("  [pass {:>2}] {}", n, name);
                    } else {
                        kprintln!("  [FAIL {:>2}] {}", n, name);
                    }
                },
            ) {
                Ok((r, n)) => {
                    kprintln!(
                        "[mlrisk-stress] {} advices in {} ns => {} ps/advice, {} advices/s",
                        r.hot.advices,
                        r.hot.ns_total,
                        r.hot.ps_per_advice,
                        r.hot.per_second()
                    );
                    kprintln!(
                        "[mlrisk-stress] in-box census: {} low / {} elevated / {} abstain ({} from the conformal band)",
                        r.hot.low,
                        r.hot.elevated,
                        r.hot.abstain,
                        r.hot.band_abstain
                    );
                    kprintln!(
                        "[mlrisk-stress] out-of-box arrivals: {} of {} => {} abstain",
                        r.mixed.out_of_range,
                        r.mixed.advices,
                        r.mixed.abstain
                    );
                    kprintln!(
                        "[mlrisk-stress] schedule all-tied: {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns",
                        r.tied.tasks,
                        r.tied.decisive,
                        r.tied.divergences,
                        r.tied.plain_ns,
                        r.tied.advised_ns
                    );
                    kprintln!(
                        "[mlrisk-stress] schedule 8 bands: {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns",
                        r.banded.tasks,
                        r.banded.decisive,
                        r.banded.divergences,
                        r.banded.plain_ns,
                        r.banded.advised_ns
                    );
                    kprintln!(
                        "[mlrisk-stress] abstaining workload: {} tasks, {} positions move (must be 0)",
                        r.quiet.tasks,
                        r.quiet.divergences
                    );
                    kprintln!("[mlrisk-stress] ALL {} STRESS INVARIANTS HOLD", n);
                    kprintln!("[mlrisk-stress] heap after: {} B used", heap::used_bytes());
                }
                Err((idx, name)) => {
                    kprintln!(
                        "[mlrisk-stress] FAILED at stress invariant {}: {}",
                        idx,
                        name
                    );
                    semihosting::exit(170 + idx as i32);
                }
            }
        }
        // The load-time refusal above already said which check refused the blob; a kernel with no
        // model has nothing to stress, and that is not a failure of this gate.
        Err(_) => kprintln!("[mlrisk-stress] SKIPPED: no verified model to stress"),
    }

    // The advisor takes up residence (REQ-ML-003, ADR-056). Everything above proved the *model*;
    // this proves the *machine consults it*. One verified forest is installed for the rest of this
    // boot, a commissioning workload is admitted through it — real feature derivation from live task
    // state, real margins, the real scheduler — and the counters it leaves behind are the ones
    // `mlstat` reports to a human at any later moment in the session.
    kprintln!("");
    kprintln!("--- resident risk advisor (the model the machine consults while it runs) ---");
    if kernel_core::mlsched::resident::install(
        kernel_core::mlrisk::BUNDLED_MODEL,
        kernel_core::mlsched::SUITE_MACHINE,
    ) {
        match kernel_core::mlsched::resident::shape() {
            Some((trees, nodes, compares)) => kprintln!(
                "[mlsched] RESIDENT: {} trees, {} nodes, worst case {} compares per advice",
                trees,
                nodes,
                compares
            ),
            None => kprintln!("[mlsched] RESIDENT: shape unavailable"),
        }
    } else {
        // Named absence, never a silent model-free boot that looks like an advised one.
        kprintln!(
            "[mlsched] NO RESIDENT MODEL: {:?}",
            kernel_core::mlsched::resident::model_error()
        );
    }
    match kernel_core::mlsched::mlsched_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[mlsched] ALL {} LIVE-ADVISORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[mlsched] FAILED at live-advisory invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(190 + idx as i32);
        }
    }
    {
        let c = kernel_core::mlsched::commission(4_096, 7);
        kprintln!(
            "[mlsched] commissioning: {} tasks admitted over {} s of machine time ({} cell bins)",
            c.admitted,
            c.span_secs,
            c.bins
        );
        kprintln!(
            "[mlsched] live census: {} advices — {} low / {} elevated / {} abstain ({} in band), {} out-of-box",
            c.stats.advices,
            c.stats.low,
            c.stats.elevated,
            c.stats.abstain,
            c.stats.band_abstain,
            c.stats.out_of_range
        );
        kprintln!(
            "[mlsched] watching: {} dispatches, {} finished / {} failed / {} evicted, {} ticks",
            c.stats.schedules,
            c.stats.finished,
            c.stats.failed,
            c.stats.evicted,
            c.stats.ticks
        );
        kprintln!(
            "[mlsched] continuity: span {} s, longest gap between advices {} s",
            c.stats.span_secs(),
            c.stats.max_gap_secs
        );
        // The claim that keeps this from being a demo: advice reordered equals and did nothing else.
        if !c.permutation {
            kprintln!(
                "[mlsched] FAILED: the advised drain was not a permutation of the model-free one"
            );
            semihosting::exit(189);
        }
        kprintln!(
            "[mlsched] advised drain is a permutation of the model-free one: {} tasks in, {} tasks out",
            c.admitted,
            c.admitted
        );
        kprintln!("[mlsched] the same numbers, as the console's `mlstat` renders them:");
        kernel_core::shell::report_risk_advisor(&mut |line: &str| kprintln!("  {}", line));
    }

    // Physical-memory invariants (aarch64 dev backend; separate from the shared spine suite).
    kprintln!("");
    kprintln!("--- memory-management selftests (physical frames) ---");
    match frames::selftest() {
        Ok(n) => kprintln!("[mm] ALL {} MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[mm] FAILED at memory invariant {}: {}", idx, name);
            semihosting::exit(40 + idx as i32);
        }
    }

    // What a device is allowed to touch (REQ-DRV-006, ADR-043). Every driver here hands a device a RAW
    // physical address; since bus-master was enabled (ADR-037) a wrong address is a device writing wherever
    // the number points. This is the software boundary the kernel can enforce without an IOMMU: a frame is
    // registered before any descriptor may name it, the kernel image is never a legal target, and an
    // address nobody registered is refused.
    kprintln!("");
    kprintln!("--- DMA-boundary selftests (what a device may be told about) ---");
    match kernel_core::dma::selftest(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[dma] ALL {} DMA-BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[dma] FAILED at DMA invariant {}: {}", idx, name);
            semihosting::exit(240 + idx as i32);
        }
    }

    // Virtual memory: build page tables, enable the MMU, prove dynamic map/unmap (aarch64 only).
    kprintln!("");
    kprintln!("--- virtual-memory selftests (MMU: identity map + dynamic map/unmap) ---");
    match vm::selftest() {
        Ok(n) => kprintln!("[vm] ALL {} VIRTUAL-MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[vm] FAILED at vm invariant {}: {}", idx, name);
            semihosting::exit(60 + idx as i32);
        }
    }

    // EL0 user-mode: drop to unprivileged EL0 and prove the capability-gated syscall boundary
    // + hardware address-space isolation (aarch64 dev backend; requires the MMU, enabled above).
    kprintln!("");
    kprintln!(
        "--- user-mode selftests (EL0 privilege boundary: cap-gated syscall + isolation) ---"
    );
    match usermode::selftest() {
        Ok(n) => kprintln!("[usermode] ALL {} EL0-BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[usermode] FAILED at EL0 invariant {}: {}", idx, name);
            semihosting::exit(80 + idx as i32);
        }
    }

    // virtio-blk: the first REAL hardware driver (REQ-DRV-001, ADR-023). Skips green when no disk is
    // attached (bare `cargo run`); the VM gate attaches one and asserts the invariant marker below.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real driver: discovery + virtqueue I/O + journal over storage) ---"
    );
    match virtio::selftest() {
        Ok(0) => {} // no device attached — graceful skip, already logged
        Ok(n) => kprintln!("[virtio] ALL {} VIRTIO-BLK INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[virtio] FAILED at virtio invariant {}: {}", idx, name);
            semihosting::exit(120 + idx as i32);
        }
    }

    // SMP: power on the other CPUs via PSCI and prove the cross-core substrate (REQ-SMP-002,
    // ADR-028). Skips green on a single-CPU machine (bare `cargo run`); the VM gate boots
    // `-smp 4` and asserts the invariant marker below.
    kprintln!("");
    kprintln!(
        "--- SMP selftests (secondary bring-up + cross-core atomics/caps/IPI, real cores) ---"
    );
    match smp::selftest() {
        Ok(0) => {} // single-CPU machine — graceful skip, already logged
        Ok(n) => kprintln!("[smp] ALL {} SMP INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smp] FAILED at SMP invariant {}: {}", idx, name);
            semihosting::exit(140 + idx as i32);
        }
    }

    // Filesystem: the named-object namespace over the journaled block store (REQ-FS-001, ADR-035).
    // The namespace is arch-independent, so every target proves the SAME behaviors over a RAM-disk
    // device — and on this target `virtio::selftest` above additionally proves them over the REAL
    // virtio-blk driver, which is what makes the crash-atomicity claim a hardware claim.
    kprintln!("");
    kprintln!(
        "--- filesystem selftests (named objects over the journal: atomic create/remove) ---"
    );
    let mut disk = kernel_core::storage::MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 64);
    match kernel_core::fs::selftest_on(&mut disk, |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[fs] ALL {} FILESYSTEM INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[fs] FAILED at filesystem invariant {}: {}", idx, name);
            semihosting::exit(160 + idx as i32);
        }
    }

    // Durable store: the OS REMEMBERS (REQ-STOR-003, ADR-038). The spine was rebuilt in RAM every
    // boot; now it is encoded into one filesystem object, updated in ONE atomic transaction, and
    // re-verified against each entity's content address on load — a flipped bit is a refusal, not
    // silently-accepted state. Proved here over a RAM disk on every target.
    kprintln!("");
    kprintln!(
        "--- durable-store selftests (the spine survives: content-verified, atomically saved) ---"
    );
    let mut store_disk =
        kernel_core::storage::MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 64);
    match kernel_core::persist::selftest_on(&mut store_disk, |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[persist] ALL {} DURABLE-STORE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[persist] FAILED at durable-store invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(200 + idx as i32);
        }
    }

    // Long-running soak (ALET-P2-009, ADR-063): lifecycles under REPETITION — journal
    // transactions, namespace mutations, capability grants, task generations — on THIS machine's
    // own clock and heap meter. What gates is scale-free; what prints is this machine's truth:
    // throughput numbers are reported, never gated (QEMU-TCG nanoseconds are an emulator's), and
    // the heap line keeps the suite's own cost on this never-freeing heap a measured fact. The
    // journal phase's allocation-free claim is gated exactly where the meter can see it.
    kprintln!("");
    kprintln!("--- soak selftests (lifecycles under repetition: storage, grants, tasks) ---");
    {
        fn heap_meter() -> u64 {
            crate::heap::used_bytes() as u64
        }
        let before = heap_meter();
        match kernel_core::soak::soak_suite(
            kernel_core::soak::BOOT_LOAD,
            |load| {
                kernel_core::soak::campaign::<ActiveHal>(load, Some(&(heap_meter as fn() -> u64)))
            },
            |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            },
        ) {
            Ok((r, n)) => {
                kprintln!(
                    "[soak] journal: {} txs ({} verifies, {} recovers replayed) in {} ms => {} tx/s",
                    r.journal.txs,
                    r.journal.verifies,
                    r.journal.recovers_replayed,
                    r.journal.ns_total / 1_000_000,
                    r.journal.txs_per_second()
                );
                kprintln!(
                    "[soak] namespace: {} ops, every one audited => {} ops/s, {} survivors re-mounted",
                    r.fs.ops,
                    r.fs.ops_per_second(),
                    r.fs.final_survivors
                );
                kprintln!(
                    "[soak] grants: {} cycles, {}/{} unauthorized refused, {}/{} revoked accesses refused",
                    r.grants.cycles,
                    r.grants.unauthorized_refused,
                    r.grants.unauthorized_attempted,
                    r.grants.revoked_refused,
                    r.grants.revoked_attempted
                );
                kprintln!(
                    "[soak] tasks: {} generations, {} priority dispatches, each exactly-once",
                    r.tasks.generations,
                    r.tasks.priority_dispatched
                );
                kprintln!(
                    "[soak] heap: {} B used by the whole campaign (bump allocator never frees)",
                    heap_meter().saturating_sub(before)
                );
                kprintln!("[soak] ALL {} SOAK INVARIANTS HOLD", n);
            }
            Err((idx, name)) => {
                kprintln!("[soak] FAILED at soak invariant {}: {}", idx, name);
                semihosting::exit(400 + idx as i32);
            }
        }
    }

    // The machine measures itself (ALET-P2-010, ADR-064, REQ-PERF-002): the five load-bearing
    // paths — authority checks, capability-checked delivery, journal commits, scheduler
    // dispatches, console formatting — measured on THIS machine's own clock through the shared
    // Hal seam. Throughput is REPORTED, never gated (emulation timing is an emulator's); what
    // gates is everything structural: work really done, authority unbroken, commits read back,
    // steady state not first-touch setup, exactly-fair dispatch, byte-exact console arithmetic,
    // a rerun performing IDENTICAL work, and — the GUI half — the summary rendered GLYPH-EXACT
    // onto real framebuffer pages, wrap and scroll contracts included. The serial log above is
    // the TUI half of that claim.
    kprintln!("");
    kprintln!("--- benchmark selftests (this machine measures itself) ---");
    {
        let mut bench_pages: alloc::vec::Vec<usize> =
            alloc::vec::Vec::with_capacity(kernel_core::bench::FB_PAGES);
        for _ in 0..kernel_core::bench::FB_PAGES {
            match crate::frames::alloc_zeroed() {
                Some(f) => bench_pages.push(f.addr()),
                None => break,
            }
        }
        match kernel_core::bench::bench_suite::<ActiveHal>(
            kernel_core::bench::BOOT_LOAD,
            &bench_pages,
            |line| kprintln!("{}", line),
            |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            },
        ) {
            Ok((_report, n)) => {
                kprintln!("[bench] ALL {} BENCHMARK INVARIANTS HOLD", n);
                kprintln!("[bench] GUI half: the same numbers were proved ON THE FRAMEBUFFER");
            }
            Err((idx, name)) => {
                kprintln!("[bench] FAILED at benchmark invariant {}: {}", idx, name);
                semihosting::exit(420 + idx as i32);
            }
        }
    }

    // The cross-reboot claim, on REAL hardware (REQ-STOR-003, ADR-038). The persistent medium is the
    // SECOND disk: the scratch one above was reformatted by the destructive suites, this one is never
    // wiped. The boot gate boots twice against the same image file, so the second boot must FIND and
    // verify what the first wrote — the difference between "the OS can write" and "the OS remembers".
    kprintln!("");
    match virtio::persistent_device() {
        Some(mut medium) => match kernel_core::persist::open_and_witness(&mut medium) {
            Ok((boot, verified)) => kprintln!(
                "[persist] PERSISTENT MEDIUM: boot #{}, {} entities verified from earlier boots",
                boot,
                verified
            ),
            Err(e) => {
                kprintln!("[persist] PERSISTENT MEDIUM FAILED: {:?}", e);
                semihosting::exit(210);
            }
        },
        None => kprintln!("[persist] no persistent medium attached (skipped)"),
    }

    // Custody crosses the platform boundary (ALET-P1-034, ADR-072). The vault root arrives over
    // the firmware configuration channel - NOT from the caller, and never from the disk it
    // protects. Absent or malformed delivery is a NAMED fact; the machine continues without the
    // vault rather than pretending custody happened.
    kprintln!("");
    match virtio::persistent_device() {
        Some(mut medium) => {
            let mut bus = fwcfg::FwCfgMmio::new();
            let delivery = kernel_core::bootroot::deliver(&mut bus);
            kprintln!("[vault] {}", delivery.describe());
            if let kernel_core::bootroot::RootDelivery::Malformed(n) = &delivery {
                kprintln!(
                    "[vault] declared size: {} bytes (custody accepts exactly {})",
                    n,
                    kernel_core::bootroot::ROOT_LEN
                );
            }
            if matches!(delivery, kernel_core::bootroot::RootDelivery::Delivered(_)) {
                match kernel_core::bootroot::boot_suite(
                    &mut medium,
                    &delivery,
                    |n, passed, name| {
                        if passed {
                            kprintln!("  [pass {:>2}] {}", n, name);
                        } else {
                            kprintln!("  [FAIL {:>2}] {}", n, name);
                        }
                    },
                ) {
                    Ok(n) => {
                        kprintln!("[vault] ALL {} CUSTODY-DELIVERY INVARIANTS HOLD", n);
                    }
                    Err((idx, name)) => {
                        kprintln!(
                            "[vault] FAILED at custody-delivery invariant {}: {}",
                            idx,
                            name
                        );
                        semihosting::exit(460 + idx as i32);
                    }
                }
            }
        }
        None => kprintln!("[vault] no persistent medium attached (skipped)"),
    }

    // Networking (REQ-NET-001/002, ADR-041): the first real slice — a virtio-net device, and enough
    // protocol to prove the path against something that ANSWERS. A transmit-only driver proves nothing, so
    // the suite ARPs QEMU's gateway and pings it: the reply must carry the address asked about, and the
    // echo must come back with matching id, sequence and payload, its checksums verified.
    kprintln!("");
    kprintln!("--- network selftests (virtio-net: ARP-cache + ICMP echo + UDP-DHCP discovery against the gateway) ---");
    match virtio::network_device() {
        None => kprintln!("[net] no network device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[net] device init FAILED: {:?}", e);
            semihosting::exit(220);
        }
        Some(Ok(net)) => match kernel_core::virtionet::net_suite(net, |n, passed, name| {
            if passed {
                kprintln!("  [pass {:>2}] {}", n, name);
            } else {
                kprintln!("  [FAIL {:>2}] {}", n, name);
            }
        }) {
            Ok(n) => kprintln!("[net] ALL {} NETWORK INVARIANTS HOLD", n),
            Err((idx, name)) => {
                kprintln!("[net] FAILED at network invariant {}: {}", idx, name);
                semihosting::exit(220 + idx as i32);
            }
        },
    }

    // Graphics (REQ-GFX-001): the first real slice — a virtio-gpu device, the 2D resource
    // lifecycle, and a display-info round trip against hardware that ANSWERS. The suite ends by
    // asking the device to flush a resource it already destroyed, so the lifecycle proof is the
    // DEVICE's own error grammar, not our bookkeeping.
    kprintln!("");
    kprintln!("--- graphics selftests (virtio-gpu: display info + 2D resource lifecycle) ---");
    match virtio::graphics_device() {
        None => kprintln!("[gpu] no graphics device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[gpu] device init FAILED: {:?}", e);
            semihosting::exit(300);
        }
        Some(Ok(mut gpu)) => {
            // One boot-log line of fact before the suite: what the machine says it will display.
            // SAFETY: the device is live and owned here; GET_DISPLAY_INFO is read-only.
            match unsafe { gpu.get_display_info() } {
                Ok(scans) => {
                    for (i, s) in scans.iter().enumerate().filter(|(_, s)| s.enabled) {
                        kprintln!("[gpu] display {}: {}x{}", i, s.rect.width, s.rect.height);
                    }
                }
                Err(e) => kprintln!("[gpu] display info error: {:?}", e),
            }
            match kernel_core::virtiogpu::gpu_suite(&mut gpu, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[gpu] ALL {} VIRTIO-GPU INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!("[gpu] FAILED at gpu invariant {}: {}", idx, name);
                    semihosting::exit(301 + idx as i32);
                }
            }
            // The framebuffer console renders into REAL backing pages and hands the whole frame
            // to the display device — and proves DETACH revokes every page (REQ-GFX-002).
            match kernel_core::virtiogpu::console_suite(&mut gpu, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[fbcon] ALL {} FRAMEBUFFER-CONSOLE INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!("[fbcon] FAILED at fbconsole invariant {}: {}", idx, name);
                    semihosting::exit(341 + idx as i32);
                }
            }
        }
    }
    // The interactive console (REQ-CON-001, ADR-044). Every gate above ends in a verdict and an
    // exit; this is the subsystem that lets the machine STAY up and answer a human. It is proved
    // here the same way everything else is — a scripted session against a real namespace — so the
    // gate covers the code an interactive boot runs, not a separate path that only humans see.
    kprintln!("");
    kprintln!("--- input-ring selftests (what an interrupt hands the shell) ---");
    match shellio::ring_selftest() {
        Ok(n) => kprintln!("[conring] ALL {} INPUT-RING INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[conring] FAILED at input-ring invariant {}: {}", idx, name);
            semihosting::exit(230 + idx as i32);
        }
    }

    kprintln!("--- console selftests (line editing + command dispatch over the namespace) ---");
    match shellio::selftest() {
        Ok(n) => kprintln!("[console] ALL {} CONSOLE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[console] FAILED at console invariant {}: {}", idx, name);
            semihosting::exit(250 + idx as i32);
        }
    }

    // The SMMUv3 hardware rung (ALET-P1-018, ADR-074): discovered from the device tree,
    // programmed behind the shared contract seams, enforcement ON and proved live - LAST,
    // because what it turns on stays on until halt. Skips green, naming why, when the
    // platform declares no unit or no PCIe device rides behind it.
    kprintln!("");
    kprintln!("--- smmuv3 selftests (the IOMMU contract meets ARM silicon) ---");
    match smmu::suite(&mut |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(0) => {}
        Ok(n) => kprintln!("[smmu] ALL {} SMMUV3 INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smmu] FAILED at smmuv3 invariant {}: {}", idx, name);
            semihosting::exit(480 + idx as i32);
        }
    }

    bench::run();

    kprintln!("");
    kprintln!(
        "[e2e] PASS — boot + spine + {} invariants + capability-lifetime + memory-management + virtual-memory + user-mode + filesystem + console + benchmark complete",
        13
    );

    // Built with `--features interactive`, the boot does NOT end here: it hands the machine to
    // whoever is at the serial line. The gate builds without the feature, so its exit-code contract
    // is untouched (a session has no verdict to give).
    #[cfg(feature = "interactive")]
    shellio::interactive();

    #[cfg(not(feature = "interactive"))]
    semihosting::exit(0);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    semihosting::exit(101);
}
