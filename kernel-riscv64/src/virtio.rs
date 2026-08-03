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
use kernel_core::virtioblk::{self, InitReport, MmioLayout, MmioTransport, VirtioHal};
use kernel_core::virtionet::{self, VirtioNet, VIRTIO_ID_NET};

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

/// This target's concrete block device: the shared driver, on the shared virtio-mmio transport.
pub type VirtioBlk = virtioblk::VirtioBlk<RiscvVirtio, MmioTransport>;

/// Scan for a block transport. `None` = none attached (the graceful-skip path).
pub fn probe() -> Option<usize> {
    // SAFETY: every slot address is inside the device gigapage the identity map covers.
    unsafe { virtioblk::probe(&LAYOUT) }
}

fn log_report(base: usize, r: &InitReport) {
    kprintln!(
        "[virtio] block device @ {:#x} (version {}, id {})",
        base,
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

    // SAFETY: `base` came from `probe`, so it is a mapped virtio-mmio transport; `MmioTransport::new`
    // refuses anything that is not a modern block device, and `RiscvVirtio::alloc_frame` hands out
    // identity-mapped frames this kernel owns exclusively.
    let init = unsafe { MmioTransport::new(base).and_then(|t| VirtioBlk::init(t)) };
    let (dev, report) = match init {
        Ok(pair) => pair,
        Err(e) => {
            kprintln!("[virtio] init failed: {}", e);
            return Err((0, "virtio-blk device initialization"));
        }
    };
    log_report(base, &report);

    // The device's own answer about its DMA gate: the suite asserts what the DRIVER can vouch for, never a
    // default (REQ-DRV-006, ADR-043).
    let dma_gate_ok = dev.dma_gate_refuses_unregistered() && dev.dma_regions() == 2;
    let mut dev = dev;
    match virtioblk::device_suite_gated(
        &mut dev,
        GATE_IMAGE_BLOCKS,
        dma_gate_ok,
        &mut |n, passed, name| {
            if passed {
                kprintln!("  [pass {:>2}] {}", n, name);
            } else {
                kprintln!("  [FAIL {:>2}] {}", n, name);
            }
        },
    ) {
        Ok(n) => Ok(n as u32),
        Err((idx, name)) => Err((idx as u32, name)),
    }
}

/// The PERSISTENT medium: the SECOND block device, if one is attached (REQ-STOR-003, ADR-038).
///
/// Device 0 is the scratch disk the destructive suites reformat; device 1 is the medium the OS keeps its
/// store on and never wipes. Two disks is what makes the cross-reboot claim provable: the boot gate
/// boots the same image twice with the same persistent image file, and the second boot must FIND and
/// verify what the first one wrote.
pub fn persistent_device() -> Option<VirtioBlk> {
    // SAFETY: the slot addresses are mapped device memory; `MmioTransport::new` refuses anything that
    // is not a modern block device, and the frames handed to the device are identity-mapped and ours.
    unsafe {
        let base = virtioblk::probe_nth(&LAYOUT, 1)?;
        let (dev, _report) = MmioTransport::new(base)
            .and_then(|t| VirtioBlk::init(t))
            .ok()?;
        Some(dev)
    }
}

/// This target's concrete network device (REQ-NET-001, ADR-041): the shared driver, same transport.
pub type Net = VirtioNet<RiscvVirtio, MmioTransport>;

/// Bring up a virtio-net device if one is attached. `None` = no NIC (the graceful-skip path), so a boot
/// without `-netdev` still passes; the VM gate attaches one and requires the marker.
pub fn network_device() -> Option<Result<Net, virtionet::NetError>> {
    // SAFETY: the slot addresses are mapped device memory; `new_for` refuses anything that is not a modern
    // network device, and the frames handed to the device are identity-mapped and exclusively ours.
    unsafe {
        let base = virtioblk::probe_nth_kind(&LAYOUT, VIRTIO_ID_NET, 0)?;
        let transport = match MmioTransport::new_for(base, VIRTIO_ID_NET) {
            Ok(t) => t,
            Err(e) => {
                kprintln!("[net] transport setup failed: {}", e);
                return Some(Err(virtionet::NetError::Unsupported("transport")));
            }
        };
        kprintln!("[net] virtio-net @ {:#x}", base);
        Some(VirtioNet::init(transport))
    }
}
