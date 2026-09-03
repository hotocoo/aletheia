//! Aletheia microkernel — bare-metal RISC-V (RV64GC, QEMU virt), the SECOND first-class target
//! (ADR-019). OpenSBI (M-mode) hands off to us in S-mode; we install a trap vector, prove the S->M
//! SBI boundary, show the `rdtime` counter is live, then re-prove the M1 capability-secure spine
//! invariants IN KERNEL SPACE — identical invariants to the aarch64 and x86-64 backends, from the
//! SAME shared `spine.rs` / `selftest.rs` (pulled via `#[path]`, no fork). The VM's process exit
//! code (via the SiFive-test device) is the machine-checkable verdict (ADR-010: this runs):
//!   0     => all invariants held (e2e PASS)
//!   10+i  => invariant i failed
//!   150+i => risk-advisor invariant i failed (REQ-ML-001, ADR-056) — this window overlaps the
//!            160+ family, so a risk-advisor failure is identified by its `[mlrisk] FAILED at
//!            risk-advisor invariant N: <name>` line, never by the exit code alone
//!   101   => kernel panic
//!   102   => unexpected S-mode trap
#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(include_str!("boot.s"));

#[macro_use]
mod console;
mod arch;
mod conirq;
mod exit;
mod frames;
mod fwcfg;
mod hal;
mod heap;
mod sbi;
mod shellio;
mod smp;
mod trap;
mod usermode;
mod virtio;
mod vm;

// Shared, arch-independent Aletheia spine + invariant suite — the SAME source the aarch64 and
// x86-64 kernels compile, pulled in via `#[path]` so every target proves identical invariants
// (no fork, no copy). The shared spine exposes more surface (entity/agent/capability variants,
// provenance fields, IPC message fields) than this particular kernel exercises — the hosted crate
// and the aarch64 `bench` module use the rest — so dead_code is allowed on the shared module to
// keep `clippy -D warnings` clean without touching the shared source.
// Shared, arch-independent Aletheia spine + invariant suite — now a real `kernel-core` dependency
// (defined once there, not `#[path]`-copied per target; gap-register Issue 1). The spine exposes
// more surface than this particular kernel exercises, but as a library crate its `pub` items don't
// trip `dead_code`, so no allow is needed here anymore.
use kernel_core::{selftest, spine};

/// Kernel entry, called from `_start` (boot.s) after stack + BSS setup.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    use hal::{ActiveHal, Hal};

    trap::init();

    kprintln!("");
    kprintln!("========================================");
    kprintln!(
        " Aletheia microkernel — HAL backend: {}",
        ActiveHal::arch_name()
    );
    kprintln!("========================================");
    kprintln!(
        "[hal] first-class targets: AMD64/x86-64, RISC-V  (aarch64 = bootstrap/dev; ADR-019)"
    );
    kprintln!("[boot] OK: stack ready, BSS clear, stvec installed");
    kprintln!(
        "[boot] privilege level: {} (S-mode; entered via OpenSBI handoff)",
        ActiveHal::current_privilege()
    );
    kprintln!(
        "[boot] timer freq: {} Hz (rdtime `time` CSR)",
        ActiveHal::timer_freq_hz()
    );

    // Prove the S->M SBI firmware boundary works (the RISC-V privilege-crossing interface).
    sbi::probe();

    // Prove the time counter is actually advancing (interrupts stay off; polled monotonic read).
    let t0 = ActiveHal::timer_ticks();
    let mut t1 = t0;
    while t1 == t0 {
        t1 = ActiveHal::timer_ticks();
    }
    kprintln!(
        "[timer] rdtime advancing: {} -> {} (~{} ns elapsed)",
        t0,
        t1,
        ActiveHal::ticks_to_ns(t1 - t0)
    );
    kprintln!("[boot] heap: {} B used after init", heap::used_bytes());

    // Physical memory: bring up the frame allocator over the RAM above the static kernel region.
    if !frames::init() {
        kprintln!("[mm] FATAL: frame ownership table does not cover the pool");
        ActiveHal::exit(39);
    }
    kprintln!(
        "[mm] frame allocator: {} frames ({} MiB) free above kernel, up to {:#x}",
        frames::free_count(),
        frames::free_count() * frames::FRAME_SIZE / (1024 * 1024),
        frames::RAM_END,
    );

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in RISC-V kernel space) ---");
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
            ActiveHal::exit(10 + idx as i32);
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
            ActiveHal::exit(90 + idx as i32);
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
            ActiveHal::exit(110 + idx as i32);
        }
    }

    // The frozen risk forest this image carries (REQ-ML-001, ADR-056). It is ADVISORY: it may
    // reorder tasks whose effective priority is already equal, and nothing else. So the suite proves
    // the advisory property itself — an abstaining model schedules bit-identically to the model-free
    // kernel, and priority is never traded for risk — alongside exact parity with the trainer and a

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
            ActiveHal::exit(130 + idx as i32);
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
            ActiveHal::exit(560 + idx as i32);
        }
    }
    // The composition contract (ALET-P2-021, ADR-077): pixels are authority, the scanout is
    // a hard bound. Surfaces answer only to their owner token, placements clip exactly to
    // the scanout, the painter's order is the z-order, buffers are size-honest, placement
    // changes are visible the same frame, and an unchanged frame writes ZERO pixels - the
    // cost of every frame is counted, not assumed. Modeled in software; composing onto REAL
    // scanout pixels over the virtio-gpu flush path stays scoped in the gap register.
    kprintln!("");
    kprintln!("--- compositor selftests (pixels are authority, the scanout is a bound) ---");
    match kernel_core::compositor::compositor_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!(
            "[compositor] ALL {} COMPOSITION-CONTRACT INVARIANTS HOLD",
            n
        ),
        Err((idx, name)) => {
            kprintln!(
                "[compositor] FAILED at composition-contract invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(600 + idx as i32);
        }
    }

    // Input routing and the cursor plane (ALET-P2-021, ADR-079): the input path is ONE
    // session, focus is ONE surface, the owner alone drains, the cursor is the
    // compositor's own — and a keystroke is not a pixel.
    kprintln!("");
    kprintln!("--- input selftests (focus is authority, the cursor is the compositor's) ---");
    match kernel_core::compositor::input_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[input] ALL {} INPUT-ROUTING INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[input] FAILED at input-routing invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(660 + idx as i32);
        }
    }

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
            ActiveHal::exit(150 + idx as i32);
        }
    }

    // The forest under LOAD, on this machine, with this machine's own clock: what an advice costs,
    // and what the advice actually changes about a schedule. Timings are REPORTED; only the
    // scale-invariant properties gate the boot (REQ-ML-002, ADR-056).
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
                }
                Err((idx, name)) => {
                    kprintln!(
                        "[mlrisk-stress] FAILED at stress invariant {}: {}",
                        idx,
                        name
                    );
                    ActiveHal::exit(170 + idx as i32);
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
    // The memory boundary (ADR-081): the allocator's own count goes to the resident service BEFORE
    // anything is admitted through it, and the boot log carries the reading the door will judge by.
    {
        let meter = kernel_core::mlsched::MemoryMeter {
            total_pages: frames::total_count() as u64,
            free_pages: frames::free_count() as u64,
        };
        match kernel_core::mlsched::resident::observe_memory(meter) {
            Ok(true) => kprintln!(
                "[mlsched] memory: {} of {} frames free - bounded admission ON",
                meter.free_pages,
                meter.total_pages
            ),
            Ok(false) => {
                kprintln!("[mlsched] memory: no resident service - bounded admission has no door")
            }
            Err(e) => kprintln!(
                "[mlsched] memory: the allocator's reading was refused: {:?}",
                e
            ),
        }
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
            ActiveHal::exit(190 + idx as i32);
        }
    }
    {
        let c = kernel_core::mlsched::commission(4_096, 7);
        kprintln!(
            "[mlsched] commissioning: {} tasks admitted over {} s of machine time ({} cell bins), {} refused at the memory boundary",
            c.admitted,
            c.span_secs,
            c.bins,
            c.refused
        );
        // A refusal during commissioning is a target that did not report its allocator, or a
        // workload sized past it - a boot failure either way, never a statistic (ADR-081).
        if c.refused != 0 {
            kprintln!(
                "[mlsched] FAILED: {} commissioning arrivals were refused at the memory boundary",
                c.refused
            );
            ActiveHal::exit(187);
        }
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
            ActiveHal::exit(189);
        }
        kprintln!(
            "[mlsched] advised drain is a permutation of the model-free one: {} tasks in, {} tasks out",
            c.admitted,
            c.admitted
        );
        kprintln!("[mlsched] the same numbers, as the console's `mlstat` renders them:");
        kernel_core::shell::report_risk_advisor(&mut |line: &str| kprintln!("  {}", line));
    }

    // Reclaim under pressure (REQ-ML-005 wired, ADR-082): the eviction-event forest verified by
    // the same loader as the risk forest, the policy proved on synthetic candidates, then a REAL
    // storm on this machine's own allocator - frames taken until the meter is under the
    // watermark, the reclaimer asked, every frame back, the free count restored EXACTLY.
    kprintln!("");
    kprintln!("--- reclaim under pressure (the allocator triggers, the policy chooses, the forest advises) ---");
    {
        let r = kernel_core::reclaim::Reclaimer::load(kernel_core::reclaim::BUNDLED_RECLAIM_MODEL);
        match r.model() {
            Some(m) => kprintln!(
                "[reclaim] forest: RESIDENT - {} trees, {} nodes (eviction-event model, same loader, same contract)",
                m.trees(),
                m.nodes()
            ),
            None => kprintln!("[reclaim] forest: REFUSED - {:?}", r.model_error()),
        }
    }
    match kernel_core::reclaim::reclaim_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[reclaim] ALL {} RECLAIM INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[reclaim] FAILED at reclaim invariant {}: {}", idx, name);
            ActiveHal::exit(700 + idx as i32);
        }
    }
    {
        let report = reclaim_storm();
        kprintln!(
            "[reclaim] storm: {} frames taken until pressure ({} of {} free), {} reclaimed, free restored to {} (was {})",
            report.taken,
            report.free_at_pressure,
            report.total,
            report.frames_reclaimed,
            report.free_after,
            report.free_before
        );
        if !report.holds() {
            kprintln!("[reclaim] FAILED: the storm did not come back to where it started");
            ActiveHal::exit(699);
        }
        kprintln!("[reclaim] storm: pressure entered and cleared, every frame back EXACTLY");
    }

    // Physical-memory invariants (riscv64 backend; separate from the shared spine suite).
    kprintln!("");
    kprintln!("--- memory-management selftests (physical frames) ---");
    match frames::selftest() {
        Ok(n) => kprintln!("[mm] ALL {} MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[mm] FAILED at memory invariant {}: {}", idx, name);
            ActiveHal::exit(40 + idx as i32);
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
            ActiveHal::exit(240 + idx as i32);
        }
    }

    // Virtual memory: build Sv39 tables, enable paging, prove dynamic map/unmap (riscv64 backend).
    kprintln!("");
    kprintln!("--- virtual-memory selftests (Sv39 MMU: identity map + dynamic map/unmap) ---");
    match vm::selftest() {
        Ok(n) => kprintln!("[vm] ALL {} VIRTUAL-MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[vm] FAILED at vm invariant {}: {}", idx, name);
            ActiveHal::exit(60 + idx as i32);
        }
    }

    // U-mode: drop to unprivileged U-mode and prove the capability-gated ecall boundary, hardware
    // address-space isolation, per-process satp spaces, cooperative + timer-preemptive scheduling,
    // and kernel-mediated IPC (riscv64 backend; requires the MMU, enabled above).
    kprintln!("");
    kprintln!("--- user-mode selftests (U-mode boundary: cap-gated ecall + isolation + preemption + IPC) ---");
    match usermode::selftest() {
        Ok(n) => kprintln!("[usermode] ALL {} USER-MODE BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[usermode] FAILED at user-mode invariant {}: {}", idx, name);
            ActiveHal::exit(80 + idx as i32);
        }
    }

    // SMP: start the other harts via SBI HSM and prove the cross-hart substrate (REQ-SMP-002,
    // ADR-021). Skips green on a single-hart machine; the VM gate boots `-smp 4` and asserts the
    // invariant marker below.
    kprintln!("");
    kprintln!(
        "--- SMP selftests (secondary hart bring-up + cross-hart atomics/caps/IPI, real harts) ---"
    );
    match smp::selftest() {
        Ok(0) => {} // single-hart machine — graceful skip, already logged
        Ok(n) => kprintln!("[smp] ALL {} SMP INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smp] FAILED at SMP invariant {}: {}", idx, name);
            ActiveHal::exit(140 + idx as i32);
        }
    }

    // virtio-blk: a REAL block device on a FIRST-CLASS target (REQ-DRV-004, ADR-036). The driver is
    // the shared `kernel_core::virtioblk`; this target supplies only its MMIO window, its frame
    // allocator and its fence. Skips green when no disk is attached (bare `cargo run`); the VM gate
    // attaches one and requires the marker. The suite ends by proving the whole filesystem namespace
    // over the real device, through the virtqueue.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real driver: discovery + virtqueue I/O + journal + filesystem) ---"
    );
    match virtio::selftest() {
        Ok(0) => {} // no device attached — graceful skip, already logged
        Ok(n) => kprintln!("[virtio] ALL {} VIRTIO-BLK INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[virtio] FAILED at virtio invariant {}: {}", idx, name);
            ActiveHal::exit(180 + idx as i32);
        }
    }

    // Filesystem: the named-object namespace over the journaled block store (REQ-FS-001, ADR-035).
    // The namespace is arch-independent, so every target proves the SAME behaviors over a RAM-disk
    // device; aarch64 additionally proves them over the real virtio-blk driver.
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
            ActiveHal::exit(160 + idx as i32);
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
            ActiveHal::exit(200 + idx as i32);
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
                ActiveHal::exit(400 + idx as i32);
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
                ActiveHal::exit(420 + idx as i32);
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
                ActiveHal::exit(210);
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
                    Ok(n) => kprintln!("[vault] ALL {} CUSTODY-DELIVERY INVARIANTS HOLD", n),
                    Err((idx, name)) => {
                        kprintln!(
                            "[vault] FAILED at custody-delivery invariant {}: {}",
                            idx,
                            name
                        );
                        ActiveHal::exit(460 + idx as i32);
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
            ActiveHal::exit(220);
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
                ActiveHal::exit(220 + idx as i32);
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
            ActiveHal::exit(300);
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
                    ActiveHal::exit(301 + idx as i32);
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
                    ActiveHal::exit(341 + idx as i32);
                }
            }
            // The composition contract meets the scanout (ALET-P2-021, ADR-078): the model's
            // sink is now REAL backing pages and the composed frame is handed to the display
            // device itself. A quiet frame moves nothing - zero writes, zero device commands,
            // measured on the driver's own command counter.
            match kernel_core::virtiogpu::compose_suite(&mut gpu, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[compose] ALL {} REAL-PIXEL COMPOSITION INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!(
                        "[compose] FAILED at real-pixel composition invariant {}: {}",
                        idx,
                        name
                    );
                    ActiveHal::exit(640 + idx as i32);
                }
            }
        }
    }

    // Input HARDWARE (ALET-P2-021's device rung, ADR-080): the REAL devices through the input
    // session. The keyboard and pointer answer for their identity from their own config space
    // (pinned), the event path is DMA-gated, armed silence is MEASURED, and the decode->route
    // path the live desktop pumps is driven end to end with synthetic records — the same
    // shared functions, so what the suite proves is what the machine runs.
    // The terminal window's text grid (ALET-P2-021's text rung, ADR-083): arch-neutral,
    // pixel-exact, allocation-bounded - proved on every CPU, painted on the one with a desktop.
    kprintln!("");
    kprintln!("--- text-grid selftests (the terminal window's text, pixel-exact) ---");
    match kernel_core::textgrid::textgrid_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[textgrid] ALL {} TEXT-GRID INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[textgrid] FAILED at text-grid invariant {}: {}", idx, name);
            ActiveHal::exit(700 + idx as i32);
        }
    }

    // The window manager (ALET-P2-021's window rung, ADR-084): windows are a managed SET -
    // chrome the painter and the hit test agree on, a press that routes to the topmost window
    // alone, a close that ends a window's surface, queue and token together, and focus that
    // falls to a survivor or to nobody. Arch-neutral, so every CPU proves it.
    kprintln!("");
    kprintln!(
        "--- window-manager selftests (a managed set of windows: raise, drag, close, focus) ---"
    );
    match kernel_core::wm::wm_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[wm] ALL {} WINDOW-MANAGER INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[wm] FAILED at window-manager invariant {}: {}", idx, name);
            ActiveHal::exit(720 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!("--- input hardware selftests (virtio-input: a real keyboard and pointer through the session) ---");
    match virtio::input_pair() {
        None => kprintln!("[vinput] no input device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[vinput] device init FAILED: {:?}", e);
            ActiveHal::exit(679);
        }
        Some(Ok((mut kb, mut tab))) => {
            // The machine's own input facts on the boot log (the gpu display-info line's twin):
            // what the devices DECLARE, read from their config space, before anything is driven.
            kprintln!(
                "[vinput] keyboard '{}' keybits={:#x}; tablet '{}' absbits={:#x} axes {:?} {:?}",
                kb.device_name(),
                kb.ev_bits(1),
                tab.device_name(),
                tab.ev_bits(3),
                tab.abs_info(kernel_core::vinput::ABS_X),
                tab.abs_info(kernel_core::vinput::ABS_Y)
            );
            match kernel_core::vinput::vinput_suite(&mut kb, &mut tab, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[vinput] ALL {} INPUT-HARDWARE INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!(
                        "[vinput] FAILED at input-hardware invariant {}: {}",
                        idx,
                        name
                    );
                    ActiveHal::exit(680 + idx as i32);
                }
            }
        }
    }
    // The interactive console (REQ-CON-001, ADR-044): the subsystem that lets the machine stay up
    // and answer a human, proved here the same way everything else is — a scripted session against
    // a real namespace, so the gate covers the code an interactive boot runs.
    kprintln!("");
    kprintln!("--- input-ring selftests (what an interrupt hands the shell) ---");
    match shellio::ring_selftest() {
        Ok(n) => kprintln!("[conring] ALL {} INPUT-RING INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[conring] FAILED at input-ring invariant {}: {}", idx, name);
            ActiveHal::exit(230 + idx as i32);
        }
    }

    kprintln!("--- console selftests (line editing + command dispatch over the namespace) ---");
    match shellio::selftest() {
        Ok(n) => kprintln!("[console] ALL {} CONSOLE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[console] FAILED at console invariant {}: {}", idx, name);
            ActiveHal::exit(250 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!(
        "[e2e] PASS — RISC-V S-mode boot + SBI + rdtime + 11 spine + memory + virtual-memory + user-mode + filesystem + console invariants"
    );
    kprintln!("[e2e] Aletheia re-proved its invariants on its second first-class target. Halting.");

    // With `--features interactive` the boot hands the machine to the serial line instead of
    // exiting. The gate builds without the feature, so its exit-code contract is untouched.
    #[cfg(feature = "interactive")]
    shellio::interactive();

    #[cfg(not(feature = "interactive"))]
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}

/// The live storm (ADR-082): take frames from this machine's OWN allocator under a storm owner
/// until the meter is under the watermark, then let the reclaimer take them back through the real
/// ownership table. The free count must come back EXACTLY to where it started.
fn reclaim_storm() -> kernel_core::reclaim::StormReport {
    use kernel_core::frameown::Owner;
    use kernel_core::mlsched::MemoryMeter;
    use kernel_core::reclaim::{Candidate, ReclaimOps, Reclaimer, StormReport};
    struct RealOps;
    impl ReclaimOps for RealOps {
        fn evict(&mut self, _task: kernel_core::sched::TaskId, owner: Owner) -> u64 {
            // Return every frame the owner holds through the ownership table - the primitive
            // address-space teardown uses (ADR-032) - walked frame by frame so the count is the
            // TABLE's, not the storm's memory of itself.
            let base = frames::base();
            let total = frames::total_count();
            let mut freed = 0u64;
            for i in 0..total {
                let pa = base + i * kernel_core::vmaddr::PAGE_SIZE;
                if frames::owner_of(pa) == Some(owner) && frames::free_addr_as(pa, owner) {
                    freed += 1;
                }
            }
            freed
        }
    }
    let owner = Owner::address_space(199).expect("the storm owner tag is in range");
    let total = frames::total_count() as u64;
    let free_before = frames::free_count() as u64;
    let meter = |free: u64| MemoryMeter {
        total_pages: total,
        free_pages: free,
    };
    let mut taken = 0u64;
    while !meter(frames::free_count() as u64).under_pressure() {
        match frames::alloc_as(owner) {
            Some(_) => taken += 1,
            None => break,
        }
    }
    let free_at_pressure = frames::free_count() as u64;
    // The resident advisor sees the pressure too: its ledger counts the crossing (ADR-081).
    let _ = kernel_core::mlsched::resident::observe_memory(meter(free_at_pressure));
    let mut r = Reclaimer::load(kernel_core::reclaim::BUNDLED_RECLAIM_MODEL);
    let storm = Candidate {
        task: kernel_core::sched::TaskId(0x5701),
        owner,
        footprint_pages: taken,
        priority: kernel_core::priosched::Priority(11),
        submitted_secs: 0,
        protected: false,
        features: [0; kernel_core::mlrisk_contract::N_FEATURES],
    };
    let frames_reclaimed = match r.reclaim(meter(free_at_pressure), &[storm], &mut RealOps) {
        Ok(out) => out.frames_reclaimed,
        Err(_) => 0,
    };
    let free_after = frames::free_count() as u64;
    let _ = kernel_core::mlsched::resident::observe_memory(meter(free_after));
    StormReport {
        free_before,
        total,
        taken,
        free_at_pressure,
        frames_reclaimed,
        free_after,
    }
}
