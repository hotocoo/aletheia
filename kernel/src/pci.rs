//! PCI on aarch64: the TARGET half of the seam (ADR-074). The transport lives once in
//! `kernel_core::virtiopci`; this file supplies what is genuinely this machine:
//!
//! * **ECAM configuration space** at the base the device tree DECLARES for the host bridge
//!   (0x3f00_0000 on the virt machine with highmem-ecam off) - no ports, no MCFG hunt.
//!
//! * **BAR assignment by the kernel itself.** Bare-metal `-kernel` boot runs NO PCI firmware,
//!   so nothing programmed the BARs: this kernel sizes each memory BAR (the all-ones probe),
//!   assigns addresses from the platform's MMIO window, and only then enables decoding +
//!   bus master. A bare-metal kernel is its own BAR allocator or nothing uses the bus.
//!   Assignment lands inside the Device-mapped GiB the identity map already covers, so the
//!   resolved regions need no dynamic mapping - map_region is identity here.
use crate::dtb::PcieDt;
use kernel_core::virtioblk;
use kernel_core::virtiopci::{self, Bdf, PciEnv};

/// One function's view of ECAM space. Bus 0 only: the virt root complex has no root ports,
/// so every present device is bus 0 and a deeper walk would be code with no machine to run on.
pub struct Ecam {
    base: usize,
}

impl Ecam {
    pub const fn new(base: usize) -> Self {
        Ecam { base }
    }

    fn addr(bdf: Bdf, reg: u8) -> usize {
        ((bdf.bus as usize) << 20)
            | ((bdf.device as usize) << 15)
            | ((bdf.function as usize) << 12)
            | (reg as usize & 0xFC)
    }
}

impl PciEnv for Ecam {
    unsafe fn read32(&self, bdf: Bdf, reg: u8) -> u32 {
        if bdf.bus != 0 {
            return 0xFFFF_FFFF;
        }
        // SAFETY: the ECAM window was declared by the device tree and bounds-checked at
        // construction; every bus-0 offset lies inside it.
        unsafe { core::ptr::read_volatile((self.base + Self::addr(bdf, reg)) as *const u32) }
    }

    unsafe fn write32(&self, bdf: Bdf, reg: u8, value: u32) {
        if bdf.bus != 0 {
            return;
        }
        // SAFETY: as above; callers know the register is writable.
        unsafe { core::ptr::write_volatile((self.base + Self::addr(bdf, reg)) as *mut u32, value) }
    }

    fn map_region(&self, pa: u64, len: usize) -> Option<usize> {
        // Assigned BARs live in the peripheral GiB the identity map covers; anything else is
        // refused rather than dynamically mapped - this rung needs no second mapping path.
        let pa = pa as usize;
        let end = pa.checked_add(len)?;
        (end <= crate::vm::GIB && crate::vm::is_mapped_identity(pa)).then_some(pa)
    }
}

/// Where assigned BARs land: inside the PCIe MMIO window the DT ranges declare (its first
/// megabyte is left alone - QEMU reserves nothing there, but distance is cheap insurance).
pub const BAR_ASSIGN_BASE: usize = 0x1000_0000 + 0x10_0000;

/// Size and assign EVERY memory BAR of one function from a bump cursor. Returns the new
/// cursor. Refuses when the window cannot hold a BAR or a BAR names I/O space (fail closed).
/// # Safety
/// The caller owns this function's configuration space at this moment.
pub unsafe fn assign_bars(env: &Ecam, bdf: Bdf, cursor: &mut usize) -> Result<(), &'static str> {
    for index in 0..6u8 {
        let raw = unsafe { env.read32(bdf, virtiopci::CFG_BAR0 + index * 4) };
        if raw == 0 {
            continue; // unimplemented BAR
        }
        if raw & 1 != 0 {
            return Err("I/O-space BAR - this driver speaks MMIO only");
        }
        let size = unsafe { virtiopci::bar_size(env, bdf, index)? };
        if size == 0 {
            continue;
        }
        if size > (1 << 30) {
            return Err("a memory BAR larger than 1 GiB does not fit this window");
        }
        let align = size as usize;
        let assigned = (*cursor + align - 1) & !(align - 1);
        let end = assigned.checked_add(size as usize).ok_or("BAR overflow")?;
        if end >= crate::vm::GIB {
            return Err("the MMIO window cannot hold this BAR");
        }
        unsafe { virtiopci::bar_assign(env, bdf, index, assigned as u64) };
        *cursor = end;
        if (raw >> 1) & 3 == 2 {
            // 64-bit BAR: the pair's second half is consumed by the probe above; leave it
            // zero - our assignments are below 4 GiB by construction.
            unsafe { env.write32(bdf, virtiopci::CFG_BAR0 + index * 4 + 4, 0) };
        }
    }
    Ok(())
}

/// The concrete PCI-transport block device this target hands its suites.
pub type PciBlkDevice = virtioblk::VirtioBlk<crate::virtio::Aarch64Virtio, virtiopci::PciTransport>;

/// A block device brought up over virtio-pci, plus what its boot log line needs.
pub struct PciBlk {
    pub dev: PciBlkDevice,
    pub bdf: Bdf,
}

/// Find the FIRST virtio block function behind the host bridge, give it BARs, and initialize
/// the shared driver on it. None = none attached (graceful skip).
/// # Safety
/// Walks ECAM and programs the found function's BARs/command register exclusively.
pub unsafe fn open_block(pcie: &PcieDt) -> Option<PciBlk> {
    let env = Ecam::new(pcie.ecam_base);
    let bdf = unsafe {
        virtiopci::find_virtio_nth(
            &env,
            &[
                virtiopci::DEVICE_BLK_MODERN,
                virtiopci::DEVICE_BLK_TRANSITIONAL,
            ],
            0,
        )
    }?;
    let mut cursor = BAR_ASSIGN_BASE;
    unsafe { assign_bars(&env, bdf, &mut cursor).ok()? };
    let transport = unsafe { virtiopci::PciTransport::new(&env, bdf).ok()? };
    kprintln!(
        "[smmu] pci {:02x}:{:02x}.{} blk regions common@{:#x} notify@{:#x} device@{:#x} mult={}",
        bdf.bus,
        bdf.device,
        bdf.function,
        transport.regions().0,
        transport.regions().1,
        transport.regions().2,
        transport.regions().3
    );
    let (dev, _report) = unsafe { virtioblk::VirtioBlk::init(transport).ok()? };
    Some(PciBlk { dev, bdf })
}

/// Where the NVMe controller's BARs land: 16 MiB above the virtio-blk-pci assignments, so the two
/// bump cursors can never hand out overlapping windows.
const NVME_BAR_BASE: usize = BAR_ASSIGN_BASE + 0x0100_0000;

/// This target's NVMe driver (ADR-223): the shared driver over BAR0 in the identity window.
pub type Nvme = kernel_core::nvme::Nvme<crate::virtio::Aarch64Virtio, kernel_core::nvme::MmioRegs>;

/// Find the first NVM Express function (by class code, any vendor), give it BARs, enable decoding
/// and bus master, and initialize the shared driver. `None` = no controller (graceful skip);
/// `Some(Err)` = a controller is present and was refused.
/// # Safety
/// Walks ECAM and programs the found function's BARs and command register exclusively.
pub unsafe fn open_nvme(
    pcie: &PcieDt,
) -> Option<Result<(Nvme, kernel_core::nvme::NvmeReport, Bdf), &'static str>> {
    let env = Ecam::new(pcie.ecam_base);
    let bdf = unsafe { virtiopci::find_class_nth(&env, virtiopci::CLASS_NVME, 0) }?;
    let mut cursor = NVME_BAR_BASE;
    Some((|| {
        unsafe { assign_bars(&env, bdf, &mut cursor)? };
        unsafe { virtiopci::enable_bus_master(&env, bdf) };
        let base = virtiopci::bar_base_pa(&env, bdf, 0)? as usize;
        // BAR0 must hold the registers and the first doorbells (two pages at the minimum stride).
        let base = env
            .map_region(base as u64, 0x2000)
            .ok_or("nvme BAR0 is outside the identity window")?;
        let (dev, report) = unsafe { Nvme::init(kernel_core::nvme::MmioRegs::new(base))? };
        Ok((dev, report, bdf))
    })())
}

/// Blocks on the NVMe image the VM gate attaches (1 MiB).
const NVME_GATE_BLOCKS: usize = 256;

/// Prove the shared NVMe driver against this machine's controller. `Ok(0)` = none attached.
pub fn nvme_selftest() -> Result<u32, (u32, &'static str)> {
    let Some(disc) = crate::smmu::discovery() else {
        kprintln!("[nvme] no device tree PCIe declaration (skipped)");
        return Ok(0);
    };
    // SAFETY: nothing else owns the NVMe function's configuration space during boot.
    let opened = unsafe { open_nvme(&disc.pcie) };
    let (mut dev, rep, bdf) = match opened {
        None => {
            kprintln!("[nvme] no controller (skipped)");
            return Ok(0);
        }
        Some(Err(e)) => {
            kprintln!("[nvme] init refused: {}", e);
            return Err((0, "nvme controller initialization"));
        }
        Some(Ok(t)) => t,
    };
    kernel_core::nvme::log_report(&rep, bdf.bus, bdf.device, bdf.function, &mut |s| {
        kprintln!("{}", s)
    });
    match kernel_core::nvme::device_suite(&mut dev, NVME_GATE_BLOCKS, &mut |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => Ok(n as u32),
        Err((i, name)) => Err((i as u32, name)),
    }
}
