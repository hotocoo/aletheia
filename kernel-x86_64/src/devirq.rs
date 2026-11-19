//! Interrupt-driven completions for the NVMe and AHCI drivers (REQ-DRV-019, ADR-264) on x86-64.
//!
//! Both controllers deliver message-signalled interrupts to the BSP's xAPIC (ADR-260): NVMe's I/O
//! completion queue on an MSI-X table entry of its own (entry 0, the admin queue's, stays masked, so
//! an interrupt is exactly one I/O completion), AHCI on its single MSI vector. A handler counts and
//! acknowledges the LAPIC and nothing else (ADR-080): the waiting foreground reads the completion
//! queue, or clears PxIS then IS, itself.
//!
//! The wait is ADR-260's: with the interrupt flag clear check `ready` (a completion that already
//! landed is never slept on), arm the LAPIC one-shot that bounds the sleep, `sti; hlt`. The one-shot
//! is the wake that keeps the clock bound alive if the device never answers.

use kernel_core::devirq::{IrqSample, IrqStats};
use kernel_core::virtioblk::VirtioHal;

use crate::pci::{Bdf, Ports};

/// Interrupts, sleeps and bound wakes of the NVMe controller's I/O completion queue.
static NVME: IrqStats = IrqStats::new();
/// The same for the AHCI controller.
static AHCI: IrqStats = IrqStats::new();

/// LAPIC one-shot ticks (bus clock / 16) bounding one device-completion sleep: about 270 ms under
/// QEMU's 1 GHz bus, 16x ADR-260's console bound. A boot suite's commands finish in microseconds,
/// so a sleep the bound had to end is a lost interrupt, not a slow device.
const DEVICE_WAKE_TICKS: u32 = 1 << 24;
/// How long a probe lets pending message interrupts land before it reads the counters, in
/// nanoseconds of the platform clock. An MSI is a posted write the LAPIC takes at the next
/// instruction boundary with IF set, microseconds away; a millisecond is orders of margin.
const SETTLE_NS: u64 = 1_000_000;
/// The xAPIC's MSI doorbell (Intel SDM 10.11.1): base, destination APIC id at bit 12.
const MSI_ADDRESS_BASE: u64 = 0xFEE0_0000;
const MSI_DEST_SHIFT: u32 = 12;
/// MSI-X table entry the NVMe I/O completion queue uses (entry 0 is the admin queue's, masked).
pub const NVME_IO_ENTRY: u16 = 1;

/// The NVMe handler's one job (idt.rs).
pub fn on_nvme_irq() {
    NVME.interrupt();
}

/// The AHCI handler's one job (idt.rs).
pub fn on_ahci_irq() {
    AHCI.interrupt();
}

fn halt_until(ready: &dyn Fn() -> bool, stats: &IrqStats) {
    use x86_64::instructions::interrupts;
    if !interrupts::are_enabled() {
        // Not a silent poll: a wait that never sleeps leaves `sleeps` at zero and the suite's
        // verdict names it (NeverSlept).
        core::hint::spin_loop();
        return;
    }
    interrupts::disable();
    if ready() {
        interrupts::enable();
        return;
    }
    // The one-shot's vector is shared with the console's block wait, whose counter this must not
    // feed: the expiry count is only read as an edge across this one sleep, and the bound wake is
    // booked on this source's own counters.
    let (irqs, fired) = (stats.sample().interrupts, crate::idt::blk_bound_wakes());
    crate::smp::lapic_oneshot(crate::idt::BLK_TIMEOUT_VECTOR, DEVICE_WAKE_TICKS);
    stats.sleep();
    // SAFETY: the canonical idle pair; `sti`'s shadow covers the `hlt`.
    unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack, preserves_flags)) };
    // Disarm with interrupts off (ADR-260 addendum), then count: a one-shot that expired in the
    // instant after a completion interrupt already woke the CPU is not a dead interrupt. A bound
    // wake is the timer firing while this device did not interrupt during the sleep.
    interrupts::disable();
    crate::smp::lapic_oneshot(crate::idt::BLK_TIMEOUT_VECTOR, 0);
    if crate::idt::blk_bound_wakes() != fired && stats.sample().interrupts == irqs {
        stats.bound_wake();
    }
    interrupts::enable();
}

/// The NVMe driver's completion wait.
pub fn wait_nvme(ready: &dyn Fn() -> bool) {
    halt_until(ready, &NVME);
}

/// The AHCI driver's completion wait.
pub fn wait_ahci(ready: &dyn Fn() -> bool) {
    halt_until(ready, &AHCI);
}

fn settle() {
    use x86_64::instructions::interrupts;
    let were = interrupts::are_enabled();
    interrupts::enable();
    let t0 = crate::virtio::X86Virtio::now_ns();
    while crate::virtio::X86Virtio::now_ns().wrapping_sub(t0) < SETTLE_NS {
        core::hint::spin_loop();
    }
    if !were {
        interrupts::disable();
    }
}

/// Run `f` with the interrupt flag set, restoring it after (ADR-264). The boot suites run with
/// interrupts off (the ring-3 suite leaves them so); a message interrupt that finds IF clear waits
/// in the LAPIC's one pending bit per vector, where two completions would merge into one and a
/// count of interrupts would read short. With IF set each lands as it happens.
pub fn with_interrupts<R>(f: impl FnOnce() -> R) -> R {
    use x86_64::instructions::interrupts;
    let were = interrupts::are_enabled();
    interrupts::enable();
    settle();
    let r = f();
    if !were {
        interrupts::disable();
    }
    r
}

fn sample(stats: &IrqStats) -> IrqSample {
    settle();
    stats.sample()
}

/// The NVMe counters, after pending interrupts landed.
pub fn probe_nvme() -> IrqSample {
    sample(&NVME)
}

/// The AHCI counters, after pending interrupts landed.
pub fn probe_ahci() -> IrqSample {
    sample(&AHCI)
}

/// Where a message goes: the BSP's xAPIC doorbell, with the interrupt vector as data.
fn msi_address() -> Result<u64, &'static str> {
    if !crate::smp::enable_msi_target() {
        return Err("x86: the xAPIC is not usable as an MSI target (x2APIC mode)");
    }
    Ok(MSI_ADDRESS_BASE | (crate::smp::bsp_apic_id() as u64) << MSI_DEST_SHIFT)
}

/// Route NVMe's I/O completion queue to its vector. Call before the controller is initialised, so
/// the queue is created with the entry it interrupts on.
///
/// # Safety
/// The caller owns `bdf`'s configuration space, has enabled decoding, and nothing is in flight.
pub unsafe fn arm_nvme(bdf: Bdf) -> Result<u16, &'static str> {
    let addr = msi_address()?;
    // SAFETY: forwarded.
    unsafe {
        kernel_core::virtiopci::enable_msix_entry(
            &Ports,
            bdf,
            NVME_IO_ENTRY,
            addr,
            crate::idt::NVME_MSIX_VECTOR as u32,
        )?;
    }
    Ok(NVME_IO_ENTRY)
}

/// Route AHCI's single MSI to its vector.
///
/// # Safety
/// As [`arm_nvme`].
pub unsafe fn arm_ahci(bdf: Bdf) -> Result<(), &'static str> {
    let addr = msi_address()?;
    // SAFETY: forwarded.
    unsafe {
        kernel_core::virtiopci::enable_msi(&Ports, bdf, addr, crate::idt::AHCI_MSI_VECTOR as u16)
    }
}
