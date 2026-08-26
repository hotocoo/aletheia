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
use kernel_core::virtiogpu::{self, VirtioGpu};
use kernel_core::virtionet::{self, VirtioNet};

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

/// Bring up the `nth` virtio block function (0-based) WITHOUT running any suite. Devices come up
/// BEFORE the VT-d gate turns enforcement on and stay live across it — a device's queues must be
/// published while the machine is still quiet (see `vtd::dmar_suite`, ADR-073).
///
/// # Safety
/// None: construction only maps register regions and publishes owned frames.
pub fn open_block(nth: usize) -> Option<VirtioBlk> {
    // SAFETY: the BDF names a virtio block function; `PciTransport::new` resolves its capability
    // regions (refusing a device missing any of them) and enables memory decoding + bus master, and
    // `X86Virtio::alloc_frame` hands out identity-mapped frames this kernel owns exclusively.
    unsafe {
        let bdf = pci::find_virtio_blk_nth(nth)?;
        let transport = pci::transport_new(bdf).ok()?;
        let regions = transport.regions();
        let (dev, report) = VirtioBlk::init(transport).ok()?;
        log_report(bdf, regions, &report);
        Some(dev)
    }
}

/// Prove the shared driver against this target's real emulated device, on an instance the CALLER
/// brought up (`open_block`). Failure returns `(index, name)` → the caller exits `180 + index`.
pub fn block_suite(dev: &mut VirtioBlk) -> Result<u32, (u32, &'static str)> {
    // The device's own answer about its DMA gate: the suite asserts what the DRIVER can vouch for, never a
    // default (REQ-DRV-006, ADR-043).
    let dma_gate_ok = dev.dma_gate_refuses_unregistered() && dev.dma_regions() == 2;
    match virtioblk::device_suite_gated(
        dev,
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

/// This target's concrete network device (REQ-NET-001, ADR-041): the shared driver, over PCI.
pub type Net = VirtioNet<X86Virtio, PciTransport>;

/// Bring up a virtio-net function if one is attached. `None` = no NIC (the graceful-skip path).
pub fn network_device() -> Option<Result<Net, virtionet::NetError>> {
    // SAFETY: the BDF names a virtio network function; `PciTransport::new` resolves and MAPS its register
    // regions (refusing RAM), and the frames handed to the device are identity-mapped and ours.
    unsafe {
        let bdf = pci::find_virtio_net_nth(0)?;
        let transport = match pci::transport_new(bdf) {
            Ok(t) => t,
            Err(e) => {
                kprintln!("[net] transport setup failed: {}", e);
                return Some(Err(virtionet::NetError::Unsupported("transport")));
            }
        };
        kprintln!(
            "[net] virtio-net @ PCI {:02x}:{:02x}.{}",
            bdf.bus,
            bdf.device,
            bdf.function
        );
        Some(VirtioNet::init(transport))
    }
}

/// This target's concrete GPU device (REQ-GFX-001): the shared driver, over PCI.
pub type Gpu = VirtioGpu<X86Virtio, PciTransport>;

/// Bring up a virtio-gpu function if one is attached. `None` = no GPU (the graceful-skip path),
/// so a boot without `-device virtio-gpu-pci` still passes; the VM gate attaches one and requires
/// the marker.
pub fn graphics_device() -> Option<Result<Gpu, virtiogpu::GpuError>> {
    // SAFETY: the BDF names a virtio GPU function; `PciTransport::new` resolves and MAPS its
    // register regions (refusing RAM), and the frames handed to the device are identity-mapped.
    unsafe {
        let bdf = pci::find_virtio_gpu_nth(0)?;
        let transport = match pci::transport_new(bdf) {
            Ok(t) => t,
            Err(e) => {
                kprintln!("[gpu] transport setup failed: {}", e);
                return Some(Err(virtiogpu::GpuError::Unsupported("transport")));
            }
        };
        kprintln!(
            "[gpu] virtio-gpu @ PCI {:02x}:{:02x}.{}",
            bdf.bus,
            bdf.device,
            bdf.function
        );
        Some(VirtioGpu::init(transport))
    }
}
