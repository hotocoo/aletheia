//! virtio-blk backend for the RISC-V target (REQ-DRV-004, ADR-036).
//!
//! RISC-V is a **first-class** target (ADR-019) that until now had no real storage: the filesystem and
//! the journal were proved only over a RAM disk here, while the aarch64 *dev* backend was the one
//! talking to hardware. That is backwards, and it was an accident of where the driver happened to be
//! written. The driver now lives in `kernel_core::virtioblk` (ADR-036), so this file is only the parts
//! that are genuinely RISC-V:
//!
//! * **where QEMU `virt` (RISC-V) puts the transports** — 8 slots, 0x1000 apart, from `0x1000_1000`,
//!   inside the peripheral GiB that `vm::build_identity` already maps as ONE device gigapage;
//! * **a DMA-able frame** — `frames::alloc_zeroed`, identity-mapped (VA == PA);
//! * **the barrier** — `fence iorw, iorw`, which on RISC-V orders both normal and I/O accesses, so the
//!   ring writes are visible before the `QueueNotify` store to device memory.
//!
//! **Graceful probe.** With no `-drive` attached, `probe` returns `None`, the kernel logs
//! `[virtio] no device (skipped)` and boots green; `scripts/vm-e2e-riscv.sh` attaches a 1 MiB disk and
//! requires the invariant marker.
use kernel_core::virtioblk::{self, InitReport, MmioLayout, VirtioHal};

use crate::frames;

/// QEMU `virt` (RISC-V) virtio-mmio window: 8 transport slots, 0x1000 apart.
const LAYOUT: MmioLayout = MmioLayout {
    base: 0x1000_1000,
    stride: 0x1000,
    slots: 8,
};

/// The blocks the VM gate's attached image holds (1 MiB = 2048 sectors = 256 4 KiB blocks).
const GATE_IMAGE_BLOCKS: usize = 256;

/// The RISC-V seam: this target's frame allocator and fence.
pub struct RiscvVirtio;

impl VirtioHal for RiscvVirtio {
    fn alloc_frame() -> Option<usize> {
        frames::alloc_zeroed().map(|f| f.addr())
    }

    fn barrier() {
        // SAFETY: `fence iorw, iorw` has no operands and only enforces memory ordering — over both
        // normal memory (the rings) and I/O (the notify register), which is exactly the pairing here.
        unsafe { core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags)) };
    }
}

/// This target's concrete block device.
pub type VirtioBlk = virtioblk::VirtioBlk<RiscvVirtio>;

/// Scan for a block transport. `None` = none attached (the graceful-skip path).
pub fn probe() -> Option<usize> {
    // SAFETY: every slot address is inside the device gigapage the identity map covers.
    unsafe { virtioblk::probe(&LAYOUT) }
}

fn log_report(r: &InitReport) {
    kprintln!(
        "[virtio] block device @ {:#x} (version {}, id {})",
        r.base,
        r.version,
        r.device_id
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
    let base = match probe() {
        Some(b) => b,
        None => {
            kprintln!("[virtio] no device (skipped)");
            return Ok(0);
        }
    };

    // SAFETY: `base` came from `probe`, so it is a mapped virtio-mmio block transport, and
    // `RiscvVirtio::alloc_frame` hands out identity-mapped frames this kernel owns exclusively.
    let (dev, report) = match unsafe { VirtioBlk::init(base) } {
        Ok(pair) => pair,
        Err(e) => {
            kprintln!("[virtio] init failed: {}", e);
            return Err((0, "virtio-blk device initialization"));
        }
    };
    log_report(&report);

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
