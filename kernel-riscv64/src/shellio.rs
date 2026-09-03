//! The RISC-V seam for the interactive console (REQ-CON-001, ADR-044).
//!
//! `kernel_core::shell` owns the editor, the commands, the refusals and the loop. What is genuinely
//! this target's: where a typed byte comes from (the NS16550A receive register), the machine's facts
//! (frame allocator, `rdtime`, current privilege), and how to stop (the SiFive test device).
use kernel_core::device::DeviceGuard;
use kernel_core::fs;
#[cfg(feature = "interactive")]
use kernel_core::fs::Filesystem;
use kernel_core::shell::{self, ShellHost};
use kernel_core::spine::{CapEngine, CapToken, Constraints, Scope};
#[cfg(feature = "interactive")]
use kernel_core::storage::BlockDevice;
use kernel_core::storage::MemBlockDevice;
use kernel_core::Hal;

#[cfg(feature = "interactive")]
use crate::console;
use crate::frames;
use crate::hal::ActiveHal;

/// This target's answers to the questions a command may ask.
pub struct Host {
    authority: CapEngine,
    offered: [CapToken; 2],
}

impl Host {
    /// Construct initial console subject with explicit, attenuable authority. This is still a root
    /// console by policy, but command effects now require real capability evaluation rather than a
    /// boolean ambient-privilege hook.
    fn privileged() -> Self {
        let mut authority = CapEngine::new(0xA11E_7B02, 0);
        let console = authority.mint(
            "human:console",
            "console.*",
            Scope::All,
            Constraints::none(),
        );
        let system = authority.mint("human:console", "system.*", Scope::All, Constraints::none());
        Host {
            authority,
            offered: [console, system],
        }
    }
}

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
    fn supervisor_terminated(&self) -> usize {
        crate::usermode::supervisor().terminated()
    }
    fn supervisor_escalations(&self) -> usize {
        crate::usermode::supervisor().escalations()
    }
    fn authorize(&self, action: shell::ShellAction) -> bool {
        shell::authorize_with_capabilities(&self.authority, &self.offered, action)
    }
    #[cfg(feature = "interactive")]
    fn input_dropped(&self) -> u64 {
        crate::conirq::dropped()
    }
    /// The live desktop's session facts (ADR-080/084/085), read from the model this machine is
    /// running. `None` = no desktop was installed, and the command says so rather than
    /// inventing zeros.
    #[cfg(feature = "interactive")]
    fn input_facts(&self) -> Option<shell::InputFacts> {
        crate::desktop::facts()
    }
    /// `wfi` — safe here because this target's console is interrupt-driven through the PLIC
    /// (REQ-CON-002), and the UART's external interrupt is what ends the wait. On RISC-V `wfi` is
    /// permitted to return spuriously, which the surrounding `loop` already handles: a spurious wake
    /// simply asks the ring again.
    fn idle(&self) {
        // SAFETY: `wfi` is a hint with no memory effects. `sstatus.SIE` is set in the console loop's
        // context, so the PLIC's external interrupt resumes execution.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) }
    }
    /// Reset through the SBI System Reset extension — the firmware call, because on RISC-V there is
    /// no reset register a supervisor may write and OpenSBI is the thing that owns the platform.
    /// SRST is optional in the spec, so a firmware without it returns an error and the console says
    /// the machine did not restart instead of hanging in a loop pretending it did.
    fn reboot(&self) -> bool {
        crate::sbi::system_reset()
    }
}

/// Blocks for the console's scratch namespace when no disk is attached.
const SCRATCH_BLOCKS: usize = fs::FILE_DATA_START + 64;

/// The console under a merciless storm (ADR-089): the dispatcher at command volume, measured on
/// THIS machine's own heap. Failure returns `(index, name)` → the caller exits `820 + index`.
pub fn storm() -> Result<u32, (u32, &'static str)> {
    let host = Host::privileged();
    kernel_core::shellstorm::storm_suite(
        &host,
        &mut || crate::heap::used_bytes(),
        |n, passed, name| {
            if passed {
                kprintln!("  [pass {:>2}] {}", n, name);
            } else {
                kprintln!("  [FAIL {:>2}] {}", n, name);
            }
        },
    )
}

/// Prove the console on this target, in kernel space, over a real namespace. Failure returns
/// `(index, name)` → the caller exits `250 + index`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut disk = MemBlockDevice::new(SCRATCH_BLOCKS);
    let host = Host::privileged();
    let mut guard = DeviceGuard::new_with_actions(
        &mut disk,
        "console.inspect",
        "console.write",
        "console.flush",
    );
    let mut device = guard.authorized_device(&host.authority, &host.offered);
    shell::console_suite(&host, &mut device, &mut |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    })
}

/// Print one string to the serial console, and to the live desktop's terminal window when this
/// machine brought one up (ADR-083/085) — one shell, two surfaces. No desktop, no-op.
#[cfg(feature = "interactive")]
fn emit(s: &str) {
    console::puts(s);
    crate::desktop::term_write(s.as_bytes());
}

/// The console's input: the UART ring first, then the terminal window's queue — the virtio
/// keyboard's keystrokes that the input session routed to the focused window (ADR-083/085).
#[cfg(feature = "interactive")]
fn getc() -> Option<u8> {
    crate::conirq::pop().or_else(crate::desktop::term_getc)
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
    let host = Host::privileged();
    let mut guard =
        DeviceGuard::new_with_actions(dev, "console.inspect", "console.write", "console.flush");
    let mut device = guard.authorized_device(&host.authority, &host.offered);
    let Some(mut fs) = mount_or_format(&mut device) else {
        kprintln!("[console] FATAL: no usable namespace");
        ActiveHal::exit(251)
    };
    // Interrupt-driven from here (REQ-CON-002, ADR-045). The trap vector is re-installed first: the
    // user-mode suite points `stvec` at its own entry, and a console interrupt arriving at THAT
    // handler would be read as an unexpected user trap.
    crate::trap::init();
    crate::conirq::init();
    shell::run_loop(&host, &mut fs, &mut device, &mut getc, &mut emit);
    ActiveHal::exit(0)
}

/// Enter the interactive console and never come back. The PERSISTENT disk is preferred when one is
/// attached, so what a user writes survives the reboot; a RAM disk otherwise, so a bare boot still
/// gives a usable console.
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
