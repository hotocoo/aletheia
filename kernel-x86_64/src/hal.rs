//! Aletheia HAL — the AMD64/x86-64 backend (ADR-019 first-class target).
//!
//! Implements the SAME arch-independent `Hal` contract the aarch64 bootstrap backend does
//! (`kernel/src/hal.rs`); the trait is duplicated here rather than shared to keep the two kernel
//! crates independent while the aarch64 build stays untouched (the workspace/`kernel-core`
//! extraction that unifies this one trait is the documented mechanical follow-up). x86-64 realizes
//! the primitives with `rdtsc` (monotonic ticks), the CS RPL (privilege), and the QEMU/firmware exit.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The active backend implements the shared `kernel_core::Hal` contract (defined once, not per crate).
pub use kernel_core::Hal;

pub struct Amd64Hal;

/// Frequency of whatever `timer_ticks` counts: the TSC, or the HPET once [`select_clock`] chose it.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Address of the HPET main counter when it is the clock (ADR-200); 0 = the clock is the TSC.
static HPET_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Hal for Amd64Hal {
    fn arch_name() -> &'static str {
        "x86_64 / AMD64 (UEFI; QEMU q35 + OVMF, VMware)"
    }

    fn timer_ticks() -> u64 {
        let hpet = HPET_COUNTER.load(Ordering::Relaxed);
        if hpet != 0 {
            // SAFETY: `select_clock` stored this only after validating the ACPI HPET table, a
            // 64-bit-capable counter and a legal period, and enabling it; the page is identity-mapped
            // MMIO below 4 GiB (kmap's floor) and a read of the main counter has no side effect.
            return unsafe { core::ptr::read_volatile(hpet as *const u64) };
        }
        let lo: u32;
        let hi: u32;
        // SAFETY: rdtsc has no memory effects; reads the 64-bit timestamp counter into edx:eax.
        unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
        ((hi as u64) << 32) | (lo as u64)
    }

    fn timer_freq_hz() -> u64 {
        TSC_HZ.load(Ordering::Relaxed)
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        let hz = Self::timer_freq_hz();
        if hz == 0 {
            ticks
        } else {
            ticks.saturating_mul(1_000_000_000).saturating_div(hz)
        }
    }

    fn current_privilege() -> u64 {
        let cs: u16;
        // SAFETY: reads the CS selector; its low two bits are the current privilege level (CPL).
        unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack)) };
        (cs & 0b11) as u64 // 0 = ring 0 (kernel)
    }

    fn exit(code: i32) -> ! {
        crate::exit::exit(code)
    }
}

/// The backend selected for this build target; the kernel refers to `ActiveHal`, never a CPU.
pub type ActiveHal = Amd64Hal;

/// Choose the machine's clock (ADR-200). The TSC is a clock across idle only when the CPU says so:
/// CPUID.80000007H:EDX[8], "invariant TSC". Without it the ISA allows the TSC to stop in a halt,
/// and on QEMU q35 (`-cpu qemu64`, which does not set the bit) it does: fifteen idle seconds of RTC
/// time advanced `uptime` by 0.47 s. Such a machine uses the HPET the firmware declares in ACPI,
/// if it is 64-bit-capable with a legal period; a machine with neither keeps the TSC and says so.
///
/// Called once, early, before the boot stopwatch starts, so no measured interval spans a switch.
pub fn select_clock() -> &'static str {
    if tsc_invariant() {
        return "TSC (invariant)";
    }
    match hpet_counter() {
        Some((counter, hz)) => {
            TSC_HZ.store(hz, Ordering::Relaxed);
            HPET_COUNTER.store(counter as u64, Ordering::Relaxed);
            "HPET (the TSC is not invariant)"
        }
        None => "TSC (NOT invariant and no usable HPET: uptime may stop while idle)",
    }
}

/// Whether the clock is the HPET (and `calibrate_tsc` has nothing to measure).
pub fn clock_is_hpet() -> bool {
    HPET_COUNTER.load(Ordering::Relaxed) != 0
}

fn tsc_invariant() -> bool {
    let max_ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    max_ext >= 0x8000_0007 && core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0
}

/// The ACPI-declared HPET's main-counter address and frequency, enabled, or `None` when the table
/// is absent, not in system memory, the counter is only 32 bits wide (it would wrap in minutes
/// with nothing here to extend it), or the period is outside the spec's (0, 100 ns].
fn hpet_counter() -> Option<(usize, u64)> {
    const GCAP_ID: usize = 0x00;
    const GEN_CONF: usize = 0x10;
    const MAIN_COUNTER: usize = 0xF0;
    const ENABLE_CNF: u64 = 1;

    let (table, len) = crate::acpi::find_table(b"HPET")?;
    // SAFETY: `find_table` checksum-validated `len` bytes at `table` in identity-mapped ACPI memory,
    // and nothing writes ACPI tables after boot.
    let bytes = unsafe { core::slice::from_raw_parts(table as *const u8, len) };
    let base = kernel_core::hpet::base_from_table(bytes)? as usize;
    // SAFETY: `base` is the HPET register block the firmware declared, identity-mapped below
    // 4 GiB; the capability register is read-only.
    let caps = unsafe { core::ptr::read_volatile((base + GCAP_ID) as *const u64) };
    let hz = kernel_core::hpet::counter_hz(caps)?;
    // SAFETY: as above; setting ENABLE_CNF only starts the main counter, the legacy-routing bit is
    // preserved, and no comparator is armed.
    unsafe {
        let conf = (base + GEN_CONF) as *mut u64;
        let v = core::ptr::read_volatile(conf);
        if v & ENABLE_CNF == 0 {
            core::ptr::write_volatile(conf, v | ENABLE_CNF);
        }
    }
    Some((base + MAIN_COUNTER, hz))
}

/// Calibrate TSC against the already-live PIT interrupt source. QEMU and real x86 hardware both
/// expose a TSC but do not guarantee its frequency through this kernel's existing contract; using
/// the PIT gives the benchmark an observed frequency instead of inventing one. Called once after
/// IRQ0 has been proved live. A short multi-tick window keeps interrupt jitter below the resolution
/// of the resulting frequency estimate while adding negligible boot cost.
pub fn calibrate_tsc() -> Option<u64> {
    // Prefer architectural CPUID.15 when firmware exposes it: waiting for ten PIT ticks costs
    // ~100 ms of boot latency solely to measure a counter whose ratio the CPU already reports.
    // Keep the timed path as a conservative fallback because older firmware/virtual CPUs may
    // advertise CPUID.15 without a usable crystal frequency.
    let leaf = core::arch::x86_64::__cpuid_count(0x15, 0);
    if leaf.eax != 0 && leaf.ebx != 0 && leaf.ecx != 0 {
        let hz = (leaf.ecx as u64)
            .saturating_mul(leaf.ebx as u64)
            .checked_div(leaf.eax as u64)
            .unwrap_or(0);
        if hz != 0 {
            TSC_HZ.store(hz, Ordering::Relaxed);
            return Some(hz);
        }
    }

    // An HPET is a far better reference than two PIT interrupts (±1 tick in 2 is up to 50%: three
    // boots of one VM measured 903, 1167 and 1292 MHz). 50 ms of HPET time, busy-waited.
    if let Some((counter, hpet_hz)) = hpet_counter() {
        // SAFETY: `hpet_counter` validated and enabled this main counter; reads have no side effect.
        let read = || unsafe { core::ptr::read_volatile(counter as *const u64) };
        let window = hpet_hz / 20;
        let (h0, t0) = (read(), ActiveHal::timer_ticks());
        while read().wrapping_sub(h0) < window {
            core::hint::spin_loop();
        }
        let (h1, t1) = (read(), ActiveHal::timer_ticks());
        let hz =
            (t1.wrapping_sub(t0) as u128 * hpet_hz as u128 / h1.wrapping_sub(h0) as u128) as u64;
        if hz != 0 {
            TSC_HZ.store(hz, Ordering::Relaxed);
            return Some(hz);
        }
    }

    let start_tick = crate::pit::ticks();
    let start = ActiveHal::timer_ticks();
    // Two ticks are enough for the fallback's coarse monotonic conversion while keeping the
    // boot penalty around 20 ms instead of the previous ~100 ms. The benchmark does not use this
    // estimate to manufacture hardware performance claims; it only needs a local time base.
    let target = start_tick.saturating_add(2);
    while crate::pit::ticks() < target {
        x86_64::instructions::hlt();
    }
    let end = ActiveHal::timer_ticks();
    let delta = end.wrapping_sub(start);
    let hz = delta.saturating_mul(crate::pit::FREQ_HZ as u64) / 2;
    if hz == 0 {
        return None;
    }
    TSC_HZ.store(hz, Ordering::Relaxed);
    Some(hz)
}
