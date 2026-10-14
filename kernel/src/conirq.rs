//! Interrupt-driven console input on aarch64 (REQ-CON-002, ADR-045).
//!
//! The console was polled: `run_loop` spun on `getc`, reading a register that was empty almost every
//! time, and burned the core to do it. This is the other half — the UART raises an interrupt when a
//! byte lands, the handler moves it into [`kernel_core::conring`], and the loop reads from the ring.
//!
//! **What is new here that the rest of the kernel did not have.** Until now the only IRQ this kernel
//! took was the timer PPI, delivered while a task ran at EL0 (vector 0x480). The console runs in
//! kernel space, so its interrupt arrives at *the current EL with SP_ELx* — vector 0x280, which was
//! a fatal catch-all. It is now a handler, and it is **still fatal for every INTID except the
//! console's**: turning a fatal vector into a live one must not quietly swallow the interrupts that
//! were never expected in the first place.
//!
//! QEMU `virt` wires PL011 UART0 to **SPI 1**, i.e. INTID 32 + 1 = 33.
//!
//! Concurrency model: single core for the console, and the ring is touched by exactly two parties —
//! the handler (producer) and the loop (consumer). The loop masks IRQs around its `pop`, so the two
//! never overlap; the handler needs no mask because it cannot itself be interrupted (the CPU masks
//! IRQs on entry and we never unmask before `eret`).
#[cfg(feature = "interactive")]
use core::arch::asm;

use kernel_core::conring::ConsoleRing;

use crate::uart;

/// PL011 UART0 on QEMU `virt`: SPI 1 → INTID 33.
const UART_INTID: u32 = 33;
/// The EL1 physical timer's PPI on this platform (INTID 30, banked per CPU). It wakes the live
/// desktop's pump (ADR-085): a desktop nobody ticks shows the first frame and nothing after it.
#[cfg(feature = "interactive")]
const TIMER_INTID: u32 = 30;
/// Timer slice: the counter's own frequency divided by the desktop's refresh, 1 kHz unless the
/// operator chose another (ADR-219). The IRQ handler remains tiny; compositor/device work stays in
/// the foreground pump.
#[cfg(feature = "interactive")]
fn timer_slice() -> u64 {
    let freq: u64;
    // SAFETY: CNTFRQ_EL0 is readable at EL1 and has no side effects.
    unsafe { asm!("mrs {f}, cntfrq_el0", f = out(reg) freq, options(nomem, nostack)) };
    // A console without a desktop ticks only to ask a held USB keyboard for keys (ADR-256).
    let hz = if crate::desktop::is_live() {
        kernel_core::settings::pump_hz() as u64
    } else {
        USB_POLL_HZ
    };
    (freq / hz).max(1)
}

/// The console's tick while a USB keyboard is held and no desktop is live (ADR-256): 10 ms bound on
/// keystroke latency, paid only on a machine that holds one.
#[cfg(feature = "interactive")]
const USB_POLL_HZ: u64 = 100;

/// Whether the timer runs under the console: a live desktop's pump, or a held USB keyboard.
#[cfg(feature = "interactive")]
fn ticking() -> bool {
    crate::desktop::is_live() || crate::usbkbd::is_armed()
}

// --- block completions wake the CPU (ADR-261) ---------------------------------------------------------

/// The persistent virtio-blk transport's base and its GIC INTID, once armed; 0 = polled.
#[cfg(feature = "interactive")]
static mut BLK_BASE: usize = 0;
#[cfg(feature = "interactive")]
static mut BLK_INTID: u32 = 0;
#[cfg(feature = "interactive")]
static BLK_IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "interactive")]
static BLK_SLEEPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "interactive")]
static BLK_BOUND_WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// QEMU virt wires virtio-mmio slot `n` to SPI 16 + n, which is INTID 48 + n.
#[cfg(feature = "interactive")]
const VIRTIO_MMIO_FIRST_INTID: u32 = 48;
#[cfg(feature = "interactive")]
const VIRTIO_MMIO_WINDOW: usize = 0x0a00_0000;
#[cfg(feature = "interactive")]
const VIRTIO_MMIO_STRIDE: usize = 0x200;
#[cfg(feature = "interactive")]
const MMIO_INTERRUPT_STATUS: usize = 0x060;
#[cfg(feature = "interactive")]
const MMIO_INTERRUPT_ACK: usize = 0x064;

/// Name the persistent block device's transport so `init` routes its interrupt to this CPU
/// (ADR-261). Called on the boot path before `init`, with IRQs still masked.
#[cfg(feature = "interactive")]
pub fn arm_blk(base: usize) -> bool {
    if !(VIRTIO_MMIO_WINDOW..VIRTIO_MMIO_WINDOW + 32 * VIRTIO_MMIO_STRIDE).contains(&base) {
        return false;
    }
    // SAFETY: single-core boot path, before IRQs are unmasked; written once.
    unsafe {
        BLK_BASE = base;
        BLK_INTID =
            VIRTIO_MMIO_FIRST_INTID + ((base - VIRTIO_MMIO_WINDOW) / VIRTIO_MMIO_STRIDE) as u32;
    }
    true
}

/// (completion interrupts, sleeps, sleeps the bounding timer ended), once armed.
#[cfg(feature = "interactive")]
pub fn blk_stats() -> Option<(u64, u64, u64)> {
    use core::sync::atomic::Ordering::Relaxed;
    // SAFETY: written once on the boot path.
    (unsafe { BLK_INTID } != 0).then(|| {
        (
            BLK_IRQS.load(Relaxed),
            BLK_SLEEPS.load(Relaxed),
            BLK_BOUND_WAKES.load(Relaxed),
        )
    })
}

/// The block device's completion wait (ADR-261): with IRQs masked (the boot path, the mount before
/// `init`) it spins; otherwise it masks, checks `ready` (a completion already in the ring is never
/// slept on, ADR-180), arms the timer as a one-shot if nothing else keeps it running, and `wfi`s.
#[cfg(feature = "interactive")]
pub fn blk_wait(ready: &dyn Fn() -> bool) {
    let were = mask_irqs();
    // SAFETY: the flag is written once before IRQs are first unmasked.
    if !were || unsafe { BLK_INTID } == 0 {
        restore_irqs(were);
        core::hint::spin_loop();
        return;
    }
    if !ready() {
        if !ticking() {
            timer_arm();
        }
        BLK_SLEEPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // SAFETY: a hint with no memory effects; a pending IRQ ends it even while masked.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) };
    }
    restore_irqs(were);
}

/// Stop the EL1 physical timer (a block-wait one-shot that fired).
#[cfg(feature = "interactive")]
fn timer_disarm() {
    // SAFETY: CNTP_CTL_EL0 is accessible at EL1; ENABLE=0.
    unsafe { asm!("msr cntp_ctl_el0, {v}", v = in(reg) 0u64, options(nomem, nostack)) };
}

/// Arm the EL1 physical timer for one slice.
#[cfg(feature = "interactive")]
fn timer_arm() {
    let slice = timer_slice();
    // SAFETY: CNTP_TVAL_EL0 / CNTP_CTL_EL0 are accessible at EL1 (NS-EL1, no EL2 here).
    unsafe {
        asm!("msr cntp_tval_el0, {v}", v = in(reg) slice, options(nomem, nostack));
        asm!("msr cntp_ctl_el0, {v}", v = in(reg) 1u64, options(nomem, nostack));
        // ENABLE, IMASK=0
    }
}

#[cfg(feature = "interactive")]
const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;
#[cfg(feature = "interactive")]
const GICD_CTLR: usize = 0x000;
#[cfg(feature = "interactive")]
const GICD_ISENABLER: usize = 0x100;
#[cfg(feature = "interactive")]
const GICD_ITARGETSR: usize = 0x800;
#[cfg(feature = "interactive")]
const GICD_IPRIORITYR: usize = 0x400;
#[cfg(feature = "interactive")]
const GICC_CTLR: usize = 0x000;
#[cfg(feature = "interactive")]
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;
const SPURIOUS: u32 = 1023;

/// The bytes the device has handed us that the console has not read yet.
///
/// A `static mut` rather than a lock: a spinlock taken in an interrupt handler and in the loop it
/// interrupts is the classic self-deadlock, and on one core with IRQs masked around the consumer
/// side there is nothing left for a lock to protect.
static mut RING: ConsoleRing = ConsoleRing::new();

/// Whether `init` ran. The handler must not touch a UART nobody configured, and — more importantly —
/// an INTID we never enabled arriving here is a real fault, not a console byte.
static mut ARMED: bool = false;

#[cfg(feature = "interactive")]
#[inline]
fn gicd_w32(off: usize, v: u32) {
    // SAFETY: GICD is Device-mapped MMIO at a fixed platform address (vm.rs maps the peripheral GiB).
    unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u32, v) };
}

#[inline]
fn gicc_w32(off: usize, v: u32) {
    // SAFETY: GICC is Device-mapped MMIO at a fixed platform address.
    unsafe { core::ptr::write_volatile((GICC_BASE + off) as *mut u32, v) };
}

#[inline]
fn gicc_r32(off: usize) -> u32 {
    // SAFETY: GICC is Device-mapped MMIO at a fixed platform address.
    unsafe { core::ptr::read_volatile((GICC_BASE + off) as *const u32) }
}

/// Route UART0's interrupt to this CPU and tell the UART to raise one when a byte arrives.
///
/// Order follows the GICv2 spec the timer path already established: distributor on, priority, CPU
/// target, enable the INTID, open the priority mask, CPU interface on. Then the UART's own mask
/// (`IMSC`) — a GIC that would deliver an interrupt the device never raises is silent, which looks
/// exactly like a broken console.
#[cfg(feature = "interactive")]
pub fn init() {
    uart::rx_clear_pending();
    gicd_w32(GICD_CTLR, 1);
    // SAFETY: IPRIORITYR and ITARGETSR are byte-addressed per INTID; both are valid Device registers.
    unsafe {
        core::ptr::write_volatile(
            (GICD_BASE + GICD_IPRIORITYR + UART_INTID as usize) as *mut u8,
            0x10, // below the timer's 0x00: a keystroke must not outrank preemption
        );
        core::ptr::write_volatile(
            (GICD_BASE + GICD_ITARGETSR + UART_INTID as usize) as *mut u8,
            0x01, // CPU 0
        );
    }
    gicd_w32(
        GICD_ISENABLER + (UART_INTID as usize / 32) * 4,
        1 << (UART_INTID % 32),
    );
    // The desktop's tick, if this machine brought a desktop up (ADR-085). The PPI is banked per
    // CPU, so it is enabled in ISENABLER0 without a target register; its priority sits ABOVE the
    // console's, because a pump that misses its slice is a desktop that stutters while a
    // keystroke can wait one interrupt.
    // SAFETY: written once on the boot path before IRQs are unmasked.
    let blk_intid = unsafe { BLK_INTID };
    if blk_intid != 0 {
        // SAFETY: IPRIORITYR and ITARGETSR are byte-addressed per INTID; both valid Device registers.
        unsafe {
            core::ptr::write_volatile(
                (GICD_BASE + GICD_IPRIORITYR + blk_intid as usize) as *mut u8,
                0x10,
            );
            core::ptr::write_volatile(
                (GICD_BASE + GICD_ITARGETSR + blk_intid as usize) as *mut u8,
                0x01,
            );
        }
        gicd_w32(
            GICD_ISENABLER + (blk_intid as usize / 32) * 4,
            1 << (blk_intid % 32),
        );
    }
    // The timer PPI also bounds a block wait (ADR-261), so it is enabled when either needs it; it
    // is ARMED only while something is ticking or a block request is waiting.
    if ticking() || blk_intid != 0 {
        // SAFETY: IPRIORITYR is byte-addressed per INTID and is a valid Device register.
        unsafe {
            core::ptr::write_volatile(
                (GICD_BASE + GICD_IPRIORITYR + TIMER_INTID as usize) as *mut u8,
                0x00,
            );
        }
        gicd_w32(GICD_ISENABLER, 1 << TIMER_INTID);
    }
    gicc_w32(GICC_PMR, 0xF0);
    gicc_w32(GICC_CTLR, 1);
    uart::rx_interrupt_enable();
    if ticking() {
        timer_arm();
    }
    // SAFETY: single-core console; the handler is installed at vector 0x280 by vectors.s.
    unsafe { ARMED = true };
    unmask_irqs();
}

/// Unmask IRQs at EL1 (`DAIF.I`).
#[cfg(feature = "interactive")]
fn unmask_irqs() {
    // SAFETY: `msr daifclr` only changes this CPU's interrupt mask.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
}

/// Mask IRQs at EL1 for a caller outside this module (the desktop's door, ADR-085), reporting
/// whether they had been enabled — the caller unmasks again only if they were.
#[cfg(feature = "interactive")]
pub fn mask_irqs_pub() -> bool {
    mask_irqs()
}

/// Unmask IRQs at EL1 for a caller outside this module (ADR-085).
#[cfg(feature = "interactive")]
pub fn unmask_irqs_pub() {
    unmask_irqs();
}

/// Mask IRQs at EL1 and report whether they had been enabled.
#[cfg(feature = "interactive")]
#[inline]
fn mask_irqs() -> bool {
    let daif: u64;
    // SAFETY: reading DAIF and setting the I bit affect only this CPU's mask.
    unsafe {
        asm!("mrs {d}, daif", d = out(reg) daif, options(nomem, nostack));
        asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
    daif & (1 << 7) == 0
}

#[cfg(feature = "interactive")]
#[inline]
fn restore_irqs(were_enabled: bool) {
    if were_enabled {
        unmask_irqs();
    }
}

/// Take the oldest byte the interrupt collected, or `None`.
///
/// IRQs are masked across the read: the handler is the only other party that touches the ring, and a
/// byte arriving in the middle of a `pop` would race the very indices being updated.
#[cfg(feature = "interactive")]
pub fn pop() -> Option<u8> {
    let were = mask_irqs();
    // SAFETY: IRQs are masked and this is the only consumer, so no other reference to RING exists.
    let byte = unsafe { (*core::ptr::addr_of_mut!(RING)).pop() };
    restore_irqs(were);
    byte
}

/// Sleep until an interrupt might have delivered input - and never while input is already waiting
/// (ADR-180). The ring is checked with IRQs MASKED, and `wfi` still wakes on a pending IRQ with them
/// masked; the IRQ is then taken when they are restored. The old idle ran `wfi` after `pop` had
/// already unmasked, so a byte that landed in between slept in the ring until the NEXT interrupt:
/// the console fuzz saw a prompt never come back, at random lines, on all three CPUs.
#[cfg(feature = "interactive")]
pub fn wait_for_input() {
    let were = mask_irqs();
    // SAFETY: IRQs are masked and this is the only consumer, so no other reference to RING exists.
    let empty = unsafe { (*core::ptr::addr_of!(RING)).is_empty() };
    if empty {
        // SAFETY: a hint with no memory effects; a pending IRQ ends it even while masked.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) };
    }
    restore_irqs(were);
}

/// How many bytes the ring refused because the console could not keep up.
#[cfg(feature = "interactive")]
pub fn dropped() -> u64 {
    let were = mask_irqs();
    // SAFETY: as `pop`.
    let n = unsafe { (*core::ptr::addr_of!(RING)).dropped() };
    restore_irqs(were);
    n
}

/// The EL1 IRQ handler, called from vector 0x280 (`el1_irq_entry` in vectors.s).
///
/// Fatal by default. The vector it replaces was a catch-all that exited 102, and that safety net is
/// kept for every source except the one this module deliberately enabled: an unexpected interrupt in
/// kernel space is a bug to surface, not a byte to look for.
#[no_mangle]
pub extern "C" fn el1_irq() {
    let iar = gicc_r32(GICC_IAR);
    let intid = iar & 0x3FF;
    if intid == SPURIOUS {
        return; // the GIC's "nothing to see here" — no EOI is owed for a spurious read
    }
    // The desktop's tick: rearm first (so a slow pump cannot silently stop the clock), pump,
    // then acknowledge. A machine with no desktop never enabled this PPI and never gets here.
    #[cfg(feature = "interactive")]
    if intid == TIMER_INTID {
        if ticking() {
            timer_arm();
            crate::desktop::tick_pump();
        } else {
            // Only a block wait's one-shot runs the timer on this machine (ADR-261).
            timer_disarm();
            BLK_BOUND_WAKES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        gicc_w32(GICC_EOIR, iar);
        return;
    }
    // The persistent block device completed (ADR-261): acknowledge at the device (its line is
    // level-triggered), count, EOI. The waiting foreground reads the used ring itself.
    #[cfg(feature = "interactive")]
    // SAFETY: written once on the boot path before IRQs are unmasked.
    if unsafe { BLK_INTID } != 0 && intid == unsafe { BLK_INTID } {
        // SAFETY: BLK_BASE is the mapped virtio-mmio transport `arm_blk` validated.
        unsafe {
            let base = BLK_BASE;
            let st = core::ptr::read_volatile((base + MMIO_INTERRUPT_STATUS) as *const u32);
            core::ptr::write_volatile((base + MMIO_INTERRUPT_ACK) as *mut u32, st);
        }
        BLK_IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        gicc_w32(GICC_EOIR, iar);
        return;
    }
    // SAFETY: single-core; ARMED is written once before interrupts are unmasked.
    let armed = unsafe { ARMED };
    if intid != UART_INTID || !armed {
        // Not ours. Acknowledge it so the machine does not storm, then fail the way the fatal
        // vector used to — loudly, with the same exit code.
        gicc_w32(GICC_EOIR, iar);
        kprintln!("[irq] FATAL: unexpected EL1 interrupt, INTID {}", intid);
        crate::semihosting::exit(102);
    }
    // Acknowledge BEFORE draining, not after. Clearing afterwards loses a byte that lands during the
    // drain: its RX condition would be cleared while the byte still sits in the FIFO, so no further
    // interrupt is raised for it and the console goes deaf until the operator types again — which
    // is exactly how this first appeared, as a session that answered six commands and then ignored
    // the seventh. Clearing first means a byte arriving mid-drain re-asserts and fires again.
    uart::rx_clear_pending();
    // Drain the whole receive FIFO: the UART raises one interrupt for a burst, so stopping after one
    // byte would leave the rest sitting there until the next keystroke pushed them out.
    while let Some(b) = uart::rx_take() {
        // SAFETY: the handler is the only producer and cannot be re-entered (IRQs stay masked until
        // `eret`), so no other reference to RING exists here.
        unsafe {
            (*core::ptr::addr_of_mut!(RING)).push(b);
        }
    }
    gicc_w32(GICC_EOIR, iar);
}
