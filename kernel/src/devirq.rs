//! Interrupt-driven completions for the NVMe and AHCI drivers on aarch64 (REQ-DRV-019, ADR-264).
//!
//! QEMU `virt` declares a GICv2m MSI frame in the device tree; a PCI function raises a message
//! interrupt by writing an SPI number to its `MSI_SETSPI_NS` doorbell. The two controllers take the
//! first two SPIs the frame owns: NVMe's I/O completion queue on an MSI-X table entry of its own
//! (entry 0, the admin queue's, stays masked, so an interrupt is exactly one I/O completion), AHCI
//! on its single MSI. The handler counts and acknowledges at the GIC and nothing else (ADR-080); the
//! waiting foreground reads the completion queue, or clears PxIS then IS, itself.
//!
//! The boot suites run with IRQs masked and the distributor off (the user-mode suite tears the GIC
//! down, and the SMP suite expects it that way). A [`Window`] opens the GIC for the interrupt group
//! only, then puts every register it touched back as it found it.
//!
//! The wait is ADR-261's: with IRQs masked check `ready` (a completion that already landed is never
//! slept on), arm the EL1 physical timer as the one-shot that bounds the sleep, `wfi` (a pending
//! interrupt ends it even while masked), then drop the one-shot and unmask so the handler runs.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

use kernel_core::devirq::{IrqSample, IrqStats};
use kernel_core::virtioblk::VirtioHal;

use crate::virtio::Aarch64Virtio;

/// Interrupts, sleeps and bound wakes of the NVMe controller's I/O completion queue.
static NVME: IrqStats = IrqStats::new();
/// The same for the AHCI controller.
static AHCI: IrqStats = IrqStats::new();
/// The GIC INTIDs the controllers' messages raise; 0 = not armed (INTID 0 is an SGI, never ours).
static NVME_INTID: AtomicU32 = AtomicU32::new(0);
static AHCI_INTID: AtomicU32 = AtomicU32::new(0);
/// Whether a [`Window`] is open: the timer PPI is then ours to acknowledge.
static WINDOW_OPEN: AtomicU32 = AtomicU32::new(0);

/// MSI-X table entry the NVMe I/O completion queue uses (entry 0 is the admin queue's, masked).
pub const NVME_IO_ENTRY: u16 = 1;
/// Which of the frame's SPIs each controller takes (offset from the frame's first).
const NVME_SPI_SLOT: u32 = 0;
const AHCI_SPI_SLOT: u32 = 1;

// GICv2m frame registers (Arm GICv2m, "MSI frame"): TYPER holds the first SPI's INTID (25:16) and
// how many the frame owns (9:0); SETSPI_NS is the doorbell.
const V2M_TYPER: usize = 0x008;
const V2M_SETSPI_NS: usize = 0x040;
const V2M_TYPER_BASE_SHIFT: u32 = 16;
const V2M_TYPER_FIELD_MASK: u32 = 0x3FF;
/// The frame's register page: both registers this driver touches lie inside it.
const V2M_FRAME_BYTES: usize = 0x1000;

// GICv2 distributor and CPU interface (the same QEMU `virt` addresses conirq.rs and usermode.rs use).
const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xC00;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;
/// Priority mask that lets every priority below 0xF0 through (the console's, ADR-045).
const PMR_OPEN: u32 = 0xF0;
/// SPI priority: below the timer's, so the bound outranks a device that is being slow.
const PRIO_DEVICE: u8 = 0x10;
const PRIO_TIMER: u8 = 0x00;
/// Route to CPU 0, the boot CPU the suites run on.
const TARGET_CPU0: u8 = 0x01;
/// GICD_ICFGR: two bits per INTID, bit 1 set = edge-triggered (a message interrupt is an edge).
const ICFGR_EDGE: u32 = 0b10;
const ICFGR_PER_WORD: u32 = 16;
/// The EL1 physical timer's PPI (banked per CPU).
const TIMER_INTID: u32 = 30;
const SPURIOUS: u32 = 1023;

/// The one-shot that bounds a device-completion sleep: a quarter of a second, a fraction of the
/// counter's frequency. A boot suite's commands finish in microseconds, so a sleep the bound had to
/// end is a lost interrupt, not a slow device.
const BOUND_DIVISOR: u64 = 4;
/// CNTP_CTL_EL0: ENABLE, and the read-only ISTATUS that says the condition is met (bit 2).
const CNTP_ENABLE: u64 = 1;
const CNTP_ISTATUS: u64 = 1 << 2;
/// How long a probe lets pending message interrupts land before it reads the counters, in
/// nanoseconds of the platform clock.
const SETTLE_NS: u64 = 1_000_000;

fn gicd_w8(off: usize, v: u8) {
    // SAFETY: GICD is Device-mapped MMIO at a fixed platform address; the byte registers are
    // addressed per INTID.
    unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u8, v) };
}
fn gicd_w32(off: usize, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u32, v) };
}
fn gicd_r32(off: usize) -> u32 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile((GICD_BASE + off) as *const u32) }
}
fn gicc_w32(off: usize, v: u32) {
    // SAFETY: GICC is Device-mapped MMIO at a fixed platform address.
    unsafe { core::ptr::write_volatile((GICC_BASE + off) as *mut u32, v) };
}
fn gicc_r32(off: usize) -> u32 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile((GICC_BASE + off) as *const u32) }
}

/// Mask IRQs, returning whether they were unmasked.
fn mask_irqs() -> bool {
    let daif: u64;
    // SAFETY: reads and sets the CPU's own interrupt mask.
    unsafe {
        asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
        asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
    daif & (1 << 7) == 0
}

fn unmask_irqs() {
    // SAFETY: only changes the CPU's interrupt mask.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
}

fn restore_irqs(were_unmasked: bool) {
    if were_unmasked {
        unmask_irqs();
    }
}

fn timer_arm_oneshot() {
    let freq = crate::arch::cntfrq();
    let ticks = (freq / BOUND_DIVISOR).max(1);
    // SAFETY: CNTP_TVAL_EL0 / CNTP_CTL_EL0 are accessible at EL1.
    unsafe {
        asm!("msr cntp_tval_el0, {v}", v = in(reg) ticks, options(nomem, nostack));
        asm!("msr cntp_ctl_el0, {v}", v = in(reg) CNTP_ENABLE, options(nomem, nostack));
    }
}

fn timer_ctl() -> u64 {
    let v: u64;
    // SAFETY: CNTP_CTL_EL0 is readable at EL1 and has no side effect.
    unsafe { asm!("mrs {}, cntp_ctl_el0", out(reg) v, options(nomem, nostack)) };
    v
}

fn timer_disarm() {
    // SAFETY: CNTP_CTL_EL0 is writable at EL1; ENABLE = 0 drops the level.
    unsafe { asm!("msr cntp_ctl_el0, {v}", v = in(reg) 0u64, options(nomem, nostack)) };
}

/// The IRQ vector's first look (conirq.rs `el1_irq`): `true` = this was ours and has been counted;
/// the caller writes EOIR. Anything else falls through to the console's own dispatch.
pub fn on_irq(intid: u32) -> bool {
    if intid == NVME_INTID.load(Ordering::Relaxed) && intid != 0 {
        NVME.interrupt();
        return true;
    }
    if intid == AHCI_INTID.load(Ordering::Relaxed) && intid != 0 {
        AHCI.interrupt();
        return true;
    }
    if intid == TIMER_INTID && WINDOW_OPEN.load(Ordering::Relaxed) != 0 {
        // A one-shot that fired after the wait gave up on it: drop the level so it cannot storm.
        timer_disarm();
        return true;
    }
    false
}

fn halt_until(ready: &dyn Fn() -> bool, stats: &IrqStats) {
    let were = mask_irqs();
    if !were {
        // IRQs off: nothing could wake this. Not a silent poll: `sleeps` stays zero and the
        // suite's verdict names it (NeverSlept).
        core::hint::spin_loop();
        return;
    }
    if !ready() {
        timer_arm_oneshot();
        stats.sleep();
        // SAFETY: a hint with no memory effects; a pending interrupt ends it even while masked.
        unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
        // The one-shot reached its deadline: the sleep ended on the bound, whether or not the
        // completion is visible by now. A wake lost by wfi (completion posted, interrupt pending)
        // is exactly that case, so `ready()` must not gate the count.
        if timer_ctl() & CNTP_ISTATUS != 0 {
            stats.bound_wake();
        }
        timer_disarm();
    }
    restore_irqs(were);
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
    let t0 = Aarch64Virtio::now_ns();
    while Aarch64Virtio::now_ns().wrapping_sub(t0) < SETTLE_NS {
        core::hint::spin_loop();
    }
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

/// What an armed controller needs: where its message goes and what it carries.
#[derive(Clone, Copy)]
pub struct Message {
    pub address: u64,
    pub data: u32,
}

fn claim(v2m_base: Option<usize>, slot: u32, intid: &AtomicU32) -> Result<Message, &'static str> {
    let base = v2m_base.ok_or(
        "aarch64: the device tree declares no GICv2m MSI frame - completions cannot wake the CPU",
    )?;
    // The base is the device tree's claim: it must be a whole register page inside the Device-mapped
    // peripheral GiB the identity map covers (vm.rs), the way an assigned BAR must (pci.rs
    // `map_region`). RAM or anything past the GiB is refused before it is dereferenced.
    if base
        .checked_add(V2M_FRAME_BYTES)
        .is_none_or(|end| end > crate::vm::GIB)
    {
        return Err("aarch64: the GICv2m frame lies outside the Device-mapped peripheral window");
    }
    // SAFETY: checked above: the frame's register page is inside the peripheral GiB (vm.rs).
    let typer = unsafe { core::ptr::read_volatile((base + V2M_TYPER) as *const u32) };
    let first = (typer >> V2M_TYPER_BASE_SHIFT) & V2M_TYPER_FIELD_MASK;
    let count = typer & V2M_TYPER_FIELD_MASK;
    if first == 0 || slot >= count {
        return Err("aarch64: the GICv2m frame owns too few SPIs for this controller");
    }
    let id = first + slot;
    intid.store(id, Ordering::Relaxed);
    Ok(Message {
        address: (base + V2M_SETSPI_NS) as u64,
        data: id,
    })
}

/// Name NVMe's interrupt: the message its MSI-X entry carries.
pub fn claim_nvme(v2m_base: Option<usize>) -> Result<Message, &'static str> {
    claim(v2m_base, NVME_SPI_SLOT, &NVME_INTID)
}

/// NVMe's message could not be armed after all: forget its INTID, so the window does not open an
/// SPI nothing will raise and the handler counts nothing for it.
pub fn release_nvme() {
    NVME_INTID.store(0, Ordering::Relaxed);
}

/// Name AHCI's interrupt: the message its MSI carries.
pub fn claim_ahci(v2m_base: Option<usize>) -> Result<Message, &'static str> {
    claim(v2m_base, AHCI_SPI_SLOT, &AHCI_INTID)
}

/// The GIC as the boot suites found it, and the interrupt mask, to put back (ADR-264).
pub struct Window {
    gicd_ctlr: u32,
    gicc_ctlr: u32,
    gicc_pmr: u32,
    irqs_were_unmasked: bool,
    ids: [u32; 3],
}

fn enable_spi(id: u32) {
    gicd_w8(GICD_IPRIORITYR + id as usize, PRIO_DEVICE);
    gicd_w8(GICD_ITARGETSR + id as usize, TARGET_CPU0);
    let word = GICD_ICFGR + (id / ICFGR_PER_WORD) as usize * 4;
    let shift = (id % ICFGR_PER_WORD) * 2;
    gicd_w32(
        word,
        (gicd_r32(word) & !(0b11 << shift)) | (ICFGR_EDGE << shift),
    );
    gicd_w32(GICD_ISENABLER + (id / 32) as usize * 4, 1 << (id % 32));
}

/// Open the GIC for the armed controllers and unmask IRQs. Order follows ADR-045: distributor on,
/// priority, target, trigger, enable each INTID, open the priority mask, CPU interface on.
pub fn open_window() -> Window {
    let w = Window {
        gicd_ctlr: gicd_r32(GICD_CTLR),
        gicc_ctlr: gicc_r32(GICC_CTLR),
        gicc_pmr: gicc_r32(GICC_PMR),
        irqs_were_unmasked: mask_irqs(),
        ids: [
            NVME_INTID.load(Ordering::Relaxed),
            AHCI_INTID.load(Ordering::Relaxed),
            TIMER_INTID,
        ],
    };
    gicd_w32(GICD_CTLR, 1);
    for id in w.ids.into_iter().take(2).filter(|&i| i != 0) {
        enable_spi(id);
    }
    gicd_w8(GICD_IPRIORITYR + TIMER_INTID as usize, PRIO_TIMER);
    gicd_w32(GICD_ISENABLER, 1 << TIMER_INTID);
    gicc_w32(GICC_PMR, PMR_OPEN);
    gicc_w32(GICC_CTLR, 1);
    WINDOW_OPEN.store(1, Ordering::Relaxed);
    unmask_irqs();
    settle();
    w
}

/// Put the GIC and the interrupt mask back as [`open_window`] found them.
pub fn close_window(w: Window) {
    mask_irqs();
    timer_disarm();
    WINDOW_OPEN.store(0, Ordering::Relaxed);
    for id in w.ids.into_iter().filter(|&i| i != 0) {
        gicd_w32(GICD_ICENABLER + (id / 32) as usize * 4, 1 << (id % 32));
    }
    // Anything still active or pending after the disable is acknowledged so the CPU interface is
    // left idle (an unread IAR would leave the next suite's first interrupt blocked).
    loop {
        let iar = gicc_r32(GICC_IAR);
        if iar & 0x3FF == SPURIOUS {
            break;
        }
        gicc_w32(GICC_EOIR, iar);
    }
    gicc_w32(GICC_CTLR, w.gicc_ctlr);
    gicc_w32(GICC_PMR, w.gicc_pmr);
    gicd_w32(GICD_CTLR, w.gicd_ctlr);
    restore_irqs(w.irqs_were_unmasked);
}

/// Run `f` with the GIC open and IRQs unmasked, restoring both after.
pub fn with_window<R>(f: impl FnOnce() -> R) -> R {
    let w = open_window();
    let r = f();
    close_window(w);
    r
}
