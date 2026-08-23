//! virtio-blk backend for the aarch64 dev target (REQ-DRV-003, ADR-023 / ADR-036).
//!
//! The driver itself moved to `kernel_core::virtioblk` when the RISC-V target needed the same one
//! (ADR-036): a split virtqueue and a feature handshake are bus facts, not CPU facts. What is left
//! here is exactly what is genuinely aarch64-specific, and nothing else:
//!
//! * **where QEMU `virt` puts the transports** — 32 slots, 0x200 apart, from `0x0a00_0000`, inside the
//!   Device-mapped peripheral GiB `vm::build_identity` already covers;
//! * **a DMA-able frame** — `frames::alloc_zeroed`, whose pages are identity-mapped (VA == PA);
//! * **the barrier** — `dsb sy`, ordering Normal-memory ring writes before the Device-memory notify.
//!
//! **Graceful probe.** Under bare `cargo run` (no `-drive`) no block transport is present, so `probe`
//! returns `None`, the kernel logs `[virtio] no device (skipped)` and boots green. The VM gate
//! (`scripts/vm-e2e.sh`) attaches a 1 MiB disk and asserts the invariant marker.
use kernel_core::virtioblk::{self, InitReport, MmioLayout, MmioTransport, VirtioHal};
use kernel_core::virtiogpu::{self, VirtioGpu, VIRTIO_ID_GPU};
use kernel_core::virtionet::{self, VirtioNet, VIRTIO_ID_NET};

use crate::frames;

/// QEMU `virt` (aarch64) virtio-mmio window: 32 transport slots, 0x200 apart.
const LAYOUT: MmioLayout = MmioLayout {
    base: 0x0a00_0000,
    stride: 0x200,
    slots: 32,
};

/// The blocks the VM gate's attached image holds (1 MiB = 2048 sectors = 256 4 KiB blocks). Asserted
/// by the suite, so a wrong sector/block mapping fails before any data is trusted.
const GATE_IMAGE_BLOCKS: usize = 256;

/// The aarch64 seam: this target's frame allocator and barrier instruction.
pub struct Aarch64Virtio;

impl VirtioHal for Aarch64Virtio {
    fn alloc_frame() -> Option<usize> {
        frames::alloc_zeroed().map(|f| f.addr())
    }

    fn barrier() {
        // SAFETY: `dsb sy` has no operands and only enforces memory ordering.
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    }
}

/// This target's concrete block device: the shared driver, on the shared virtio-mmio transport.
pub type VirtioBlk = virtioblk::VirtioBlk<Aarch64Virtio, MmioTransport>;

/// Scan for a block transport. `None` = none attached (the graceful-skip path).
pub fn probe() -> Option<usize> {
    // SAFETY: every slot address is inside the Device-mapped peripheral GiB.
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
/// block device is attached, so bare `cargo run` still boots; the VM gate attaches a disk and asserts
/// the invariant marker. Failure returns `(index, name)` → the caller exits `120 + index`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let base = match probe() {
        Some(b) => b,
        None => {
            kprintln!("[virtio] no device (skipped)");
            return Ok(0);
        }
    };

    // SAFETY: `base` came from `probe`, so it is a mapped virtio-mmio transport; `MmioTransport::new`
    // refuses anything that is not a modern block device, and `Aarch64Virtio::alloc_frame` hands out
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
pub type Net = VirtioNet<Aarch64Virtio, MmioTransport>;

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

/// This target's concrete GPU device (REQ-GFX-001): the shared driver, same transport seam.
pub type Gpu = VirtioGpu<Aarch64Virtio, MmioTransport>;

/// Bring up a virtio-gpu device if one is attached. `None` = no GPU (the graceful-skip path), so
/// a boot without `-device virtio-gpu-device` still passes; the VM gate attaches one and requires
/// the marker.
pub fn graphics_device() -> Option<Result<Gpu, virtiogpu::GpuError>> {
    // SAFETY: the slot addresses are mapped device memory; `new_for` refuses anything that is not
    // a modern GPU device, and the frames handed to the device are identity-mapped and ours.
    unsafe {
        let base = virtioblk::probe_nth_kind(&LAYOUT, VIRTIO_ID_GPU, 0)?;
        let transport = match MmioTransport::new_for(base, VIRTIO_ID_GPU) {
            Ok(t) => t,
            Err(e) => {
                kprintln!("[gpu] transport setup failed: {}", e);
                return Some(Err(virtiogpu::GpuError::Unsupported("transport")));
            }
        };
        kprintln!("[gpu] virtio-gpu @ {:#x}", base);
        Some(VirtioGpu::init(transport))
    }
}
