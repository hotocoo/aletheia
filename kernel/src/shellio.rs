//! The aarch64 seam for the interactive console (REQ-CON-001, ADR-044).
//!
//! `kernel_core::shell` owns the editor, the commands, the refusals and the loop. What is genuinely
//! this target's is only: where a typed byte comes from (PL011 RX), how a line is printed, what the
//! machine's facts are (frame allocator, generic timer, current EL), and how to stop.
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
#[cfg(feature = "interactive")]
use crate::uart;

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
}

/// Blocks for the console's scratch namespace when no disk is attached. Small: this is a RAM disk
/// carved out of the kernel heap, and the console's own suite needs only a handful of objects.
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

/// Print one string to the serial console (`puts` already translates `\n` to CRLF).
#[cfg(feature = "interactive")]
fn emit(s: &str) {
    uart::puts(s);
}

#[cfg(feature = "interactive")]
/// Mount the console's namespace, formatting only a device that carries none. A device that mounts
/// is NEVER reformatted — an interactive session must not be the thing that eats the disk.
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
    shell::run_loop(&Host, &mut fs, dev, &mut uart::getc, &mut emit);
    ActiveHal::exit(0)
}

#[cfg(feature = "interactive")]
/// Enter the interactive console and never come back: the machine now belongs to whoever is typing.
///
/// Storage preference is deliberate — the PERSISTENT disk when one is attached, so what a user
/// writes survives the reboot (that is the difference between a demo and an OS); a RAM disk
/// otherwise, so the console still works on a bare `cargo run`.
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
