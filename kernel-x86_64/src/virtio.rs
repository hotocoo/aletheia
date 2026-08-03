//! virtio-blk backend for the x86-64 target (REQ-DRV-005, ADR-037).
//!
//! AMD64 is the OTHER first-class target (ADR-019), and it was the last one with no real block device
//! — not because the driver was missing (ADR-036 made it shared) but because its bus is different: q35
//! exposes virtio as **PCI** functions, so there is no MMIO window to scan. `pci.rs` implements the
//! `Transport` seam over the device's capability-described BAR regions, and this file supplies the two
//! CPU facts the driver needs:
//!
//! * **a DMA-able frame** — `frames::alloc_zeroed`, identity-mapped by `kmap` (VA == PA), which is what
//!   lets the address the driver writes through be the address the device reads;
//! * **the barrier** — `mfence`. x86-64's TSO already keeps stores ordered between themselves, so this
//!   is stricter than strictly required; it is used anyway because the fence also orders the *loads*
//!   of the used ring against the store that notified the device, and because a driver that is correct
//!   only under TSO is a trap for the next reader.
//!
//! **Graceful probe.** With no virtio-blk device attached, `probe` finds no function, the kernel logs
//! `[virtio] no device (skipped)` and boots green; `kernel-x86_64/scripts/smoke-test.sh` attaches a
//! scratch disk and requires the invariant marker.
use kernel_core::virtioblk::{self, InitReport, VirtioHal};

use crate::frames;
use crate::pci::{self, Bdf, PciTransport};

/// The blocks the boot gate's scratch disk holds (1 MiB = 2048 sectors = 256 4 KiB blocks). Asserted
/// by the shared suite, so a wrong sector/block mapping fails before any data is trusted.
const GATE_IMAGE_BLOCKS: usize = 256;

/// The x86-64 seam: this target's frame allocator and fence.
pub struct X86Virtio;

impl VirtioHal for X86Virtio {
    fn alloc_frame() -> Option<usize> {
        frames::alloc_zeroed().map(|f| f.addr())
    }

    fn barrier() {
        // SAFETY: `mfence` has no operands and only enforces memory ordering.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    }
}

/// This target's concrete block device: the shared driver, over the PCI transport.
pub type VirtioBlk = virtioblk::VirtioBlk<X86Virtio, PciTransport>;

/// Find a virtio block function on bus 0. `None` = none attached (the graceful-skip path).
pub fn probe() -> Option<Bdf> {
    // SAFETY: reads PCI configuration space through the legacy ports; an absent function reads
    // all-ones, which the scan treats as "no device".
    unsafe { pci::find_virtio_blk() }
}

fn log_report(bdf: Bdf, regions: (usize, usize, usize, u32), r: &InitReport) {
    kprintln!(
        "[virtio] block device @ PCI {:02x}:{:02x}.{} (transport v{}, id {:#x})",
        bdf.bus,
        bdf.device,
        bdf.function,
        r.version,
        r.device_id
    );
    kprintln!(
        "[virtio] cfg regions: common={:#x} notify={:#x} device={:#x} notify_mult={}",
        regions.0,
        regions.1,
        regions.2,
        regions.3
    );
    kprintln!(
        "[virtio] features dev_lo={:#010x} dev_hi={:#010x} -> version1=true flush={}",
        r.features_lo,
        r.features_hi,
        r.flush_ok
    );
    kprintln!(
        "[virtio] queue0 num_max={} using N={}",
        r.queue_num_max,
        r.qsize
    );
    kprintln!(
        "[virtio] DRIVER_OK; capacity = {} sectors ({} x {}-byte blocks)",
        r.capacity_sectors,
        r.capacity_sectors / virtioblk::SECTORS_PER_BLOCK,
        kernel_core::storage::BLOCK_SIZE
    );
}

/// Prove the shared driver against this target's real emulated device. Skips green (`Ok(0)`) when no
/// block device is attached. Failure returns `(index, name)` → the caller exits `180 + index`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let bdf = match probe() {
        Some(b) => b,
        None => {
            kprintln!("[virtio] no device (skipped)");
            return Ok(0);
        }
    };

    // SAFETY: `bdf` names a virtio block function; `PciTransport::new` resolves its capability regions
    // (refusing a device missing any of them) and enables memory decoding + bus master, and
    // `X86Virtio::alloc_frame` hands out identity-mapped frames this kernel owns exclusively.
    let built = unsafe { PciTransport::new(bdf) };
    let transport = match built {
        Ok(t) => t,
        Err(e) => {
            kprintln!("[virtio] transport setup failed: {}", e);
            return Err((0, "virtio-pci transport setup"));
        }
    };
    let regions = transport.regions();
    let (dev, report) = match unsafe { VirtioBlk::init(transport) } {
        Ok(pair) => pair,
        Err(e) => {
            kprintln!("[virtio] init failed: {}", e);
            return Err((0, "virtio-blk device initialization"));
        }
    };
    log_report(bdf, regions, &report);

    match virtioblk::device_suite(dev, GATE_IMAGE_BLOCKS, |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => Ok(n as u32),
        Err((idx, name)) => Err((idx as u32, name)),
    }
}
