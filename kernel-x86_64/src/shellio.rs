//! The x86-64 seam for the interactive console (REQ-CON-001, ADR-044).
//!
//! `kernel_core::shell` owns the editor, the commands, the refusals and the loop. What is genuinely
//! this target's: where a typed byte comes from (COM1's receive buffer, port I/O so it works after
//! `ExitBootServices`), the machine's facts, and how to stop (`isa-debug-exit`, then `cli; hlt`).
use kernel_core::fs;
#[cfg(feature = "interactive")]
use kernel_core::fs::Filesystem;
use kernel_core::shell::{self, ShellHost};
#[cfg(feature = "interactive")]
use kernel_core::storage::BlockDevice;
use kernel_core::storage::MemBlockDevice;
use kernel_core::Hal;

use crate::frames;
use crate::hal::ActiveHal;

/// This target's answers to the questions a command may ask.
pub struct Host;

impl ShellHost for Host {
    fn arch(&self) -> &str {
        ActiveHal::arch_name()
    }
    fn uptime_ns(&self) -> u64 {
        ActiveHal::ticks_to_ns(ActiveHal::timer_ticks())
    }
    fn free_frames(&self) -> usize {
        frames::free_count()
    }
    fn total_frames(&self) -> usize {
        frames::total_count()
    }
    fn privilege(&self) -> u64 {
        ActiveHal::current_privilege()
    }
    #[cfg(feature = "interactive")]
    fn input_dropped(&self) -> u64 {
        crate::conirq::dropped()
    }
    /// `sti; hlt` — safe here because this target's console is interrupt-driven through the 8259A on
    /// IRQ4 (REQ-CON-002), and the UART interrupt is what ends the wait.
    ///
    /// `sti` before `hlt` rather than a bare `hlt`, and the ordering is the whole correctness
    /// argument: `hlt` with interrupts masked is a machine that never wakes. `sti` has a
    /// one-instruction interrupt shadow, so the pair cannot lose an interrupt that arrives between
    /// them — this is the canonical idle idiom for exactly that reason.
    fn idle(&self) {
        // SAFETY: enabling interrupts is what the console loop already runs with, and `hlt` merely
        // parks the CPU until one arrives. Neither instruction touches memory.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack, preserves_flags)) }
    }
    fn cpu_count(&self) -> usize {
        crate::smp::declared_cpu_count()
    }
    /// Reset through the i8042's pulse line — the reset path that exists on every PC-compatible
    /// machine, including the one this OS is qualified on under two hypervisors. It is asserted
    /// rather than requested: the controller drives the CPU's RESET pin, so nothing after the write
    /// runs on a machine that has one. A machine that does NOT (a legacy-free platform, the same
    /// case the keyboard driver already handles) falls through, and the spin below is what makes
    /// that visible as "the machine did not restart" rather than as a jump into whatever the halt
    /// path happened to leave behind.
    fn reboot(&self) -> bool {
        // SAFETY: a single byte to the i8042 command port. Interrupts are cleared first so nothing
        // runs between the request and the reset the hardware performs.
        unsafe {
            x86_64::instructions::interrupts::disable();
            x86_64::instructions::port::Port::<u8>::new(0x64).write(0xFEu8);
        }
        for _ in 0..100_000_000u64 {
            core::hint::spin_loop();
        }
        false
    }
}

/// Blocks for the console's scratch namespace when no disk is attached.
const SCRATCH_BLOCKS: usize = fs::FILE_DATA_START + 64;

/// Prove the console on this target, in kernel space, over a real namespace. Failure returns
/// `(index, name)` → the caller exits `250 + index`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut disk = MemBlockDevice::new(SCRATCH_BLOCKS);
    shell::console_suite(&Host, &mut disk, &mut |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    })
}

/// Console output goes through `kprint!`, not straight at the serial port: on this target the
/// framebuffer is a second sink, so a session is visible in a VM window and not only on the wire.
#[cfg(feature = "interactive")]
fn emit(s: &str) {
    kprint!("{}", s);
}

/// Mount the console's namespace, formatting only a device that carries none. A device that mounts
/// is NEVER reformatted — an interactive session must not be the thing that eats the disk.
#[cfg(feature = "interactive")]
fn mount_or_format<D: BlockDevice>(dev: &mut D) -> Option<Filesystem> {
    if let Ok(fs) = Filesystem::mount(dev) {
        return Some(fs);
    }
    kprintln!("[console] no namespace on this device — formatting a fresh one");
    Filesystem::format(dev).ok()?;
    Filesystem::mount(dev).ok()
}

#[cfg(feature = "interactive")]
fn session_on<D: BlockDevice>(dev: &mut D) -> ! {
    let Some(mut fs) = mount_or_format(dev) else {
        kprintln!("[console] FATAL: no usable namespace");
        ActiveHal::exit(251)
    };
    // Interrupt-driven from here (REQ-CON-002, ADR-045): COM1 raises IRQ4, the handler moves the
    // bytes into the ring, and the loop reads the ring instead of polling a mostly-empty register.
    crate::conirq::init();
    shell::run_loop(&Host, &mut fs, dev, &mut crate::conirq::pop, &mut emit);
    ActiveHal::exit(0)
}

/// Enter the interactive console and never come back. The PERSISTENT disk is preferred when one is
/// attached, so what a user writes survives the reboot; a RAM disk otherwise.
#[cfg(feature = "interactive")]
pub fn interactive() -> ! {
    kprintln!("");
    kprintln!("========================================");
    kprintln!(" Aletheia interactive console — type `help`");
    kprintln!("========================================");
    match crate::virtio::persistent_device() {
        Some(mut disk) => {
            kprintln!(
                "[console] namespace: the persistent virtio-blk device (writes survive reboot)"
            );
            session_on(&mut disk)
        }
        None => {
            kprintln!("[console] namespace: a RAM disk (no persistent device attached)");
            let mut disk = MemBlockDevice::new(SCRATCH_BLOCKS);
            session_on(&mut disk)
        }
    }
}

/// Prove the console's INPUT RING on this target (REQ-CON-002, ADR-045). The ring is what an
/// interrupt hands the shell; its overflow policy decides whether a burst truncates a line or
/// silently rewrites one, so it is proved on every target, not only the ones taking interrupts yet.
pub fn ring_selftest() -> Result<u32, (u32, &'static str)> {
    kernel_core::conring::ring_suite(&mut |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    })
}
