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
    let mut upper_half = false;
    for index in 0..6u8 {
        if core::mem::take(&mut upper_half) {
            continue; // the high dword of the 64-bit BAR handled on the previous index
        }
        let raw = unsafe { env.read32(bdf, virtiopci::CFG_BAR0 + index * 4) };
        // A raw 0 is NOT "unimplemented": a 32-bit non-prefetchable memory BAR at base 0 reads
        // 0 too (the e1000's BAR0, ADR-226). Only the size probe below can tell them apart.
        if raw & 1 != 0 {
            // I/O-space BAR (legacy ports on e1000 and ich9-ahci, ADR-226): left unassigned, and
            // never decoded - `enable_bus_master` sets memory space and bus master, not I/O space.
            continue;
        }
        let size = unsafe { virtiopci::bar_size(env, bdf, index)? };
        upper_half = (raw >> 1) & 3 == 2;
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

/// Where e1000 and AHCI BARs land: 32 and 48 MiB above the virtio-blk-pci window (ADR-226).
const E1000_BAR_BASE: usize = BAR_ASSIGN_BASE + 0x0200_0000;
const AHCI_BAR_BASE: usize = BAR_ASSIGN_BASE + 0x0300_0000;

/// Give `bdf` its memory BARs from `cursor`, enable decoding and bus master, return BAR `bar`'s
/// base inside the identity window (at least `len` bytes).
/// # Safety
/// The caller owns this function's configuration space.
unsafe fn open_bar(
    env: &Ecam,
    bdf: Bdf,
    cursor: usize,
    bar: u8,
    len: usize,
) -> Result<usize, &'static str> {
    let mut c = cursor;
    unsafe { assign_bars(env, bdf, &mut c)? };
    unsafe { virtiopci::enable_bus_master(env, bdf) };
    let pa = virtiopci::bar_base_pa(env, bdf, bar)?;
    env.map_region(pa, len)
        .ok_or("BAR is outside the identity window")
}

fn suite_log(n: usize, passed: bool, name: &str) {
    if passed {
        kprintln!("  [pass {:>2}] {}", n, name);
    } else {
        kprintln!("  [FAIL {:>2}] {}", n, name);
    }
}

/// The shared e1000 driver on aarch64 (ADR-224, ADR-226). `Ok(0)` = no controller.
pub fn e1000_selftest() -> Result<u32, (u32, &'static str)> {
    use kernel_core::e1000;
    let Some(disc) = crate::smmu::discovery() else {
        return Ok(0);
    };
    let env = Ecam::new(disc.pcie.ecam_base);
    // SAFETY: walks ECAM only.
    let found = unsafe { virtiopci::enumerate_bus0(&env) }
        .into_iter()
        .find(|&(_, v, d)| v == e1000::VENDOR_INTEL && e1000::DEVICE_IDS.contains(&d));
    let Some((bdf, _, id)) = found else {
        kprintln!("[e1000] no controller (skipped)");
        return Ok(0);
    };
    // SAFETY: this boot owns the function; BAR0 is mapped device memory in the identity window.
    let opened = unsafe {
        open_bar(&env, bdf, E1000_BAR_BASE, 0, 0x6000).and_then(|b| {
            e1000::E1000::<crate::virtio::Aarch64Virtio, _>::init(e1000::MmioRegs::new(b))
        })
    };
    let dev = match opened {
        Ok(d) => d,
        Err(e) => {
            kprintln!("[e1000] init refused: {}", e);
            return Err((0, "e1000 controller initialization"));
        }
    };
    let m = dev.mac();
    kprintln!(
        "[e1000] controller @ PCI {:02x}:{:02x}.{} id {:#06x} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bdf.bus, bdf.device, bdf.function, id, m[0], m[1], m[2], m[3], m[4], m[5]
    );
    e1000::device_suite(&dev, &mut suite_log)
        .map(|n| n as u32)
        .map_err(|(i, name)| (i as u32, name))
}

/// The shared AHCI driver on aarch64 (ADR-225, ADR-226). `Ok(0)` = no controller.
pub fn ahci_selftest() -> Result<u32, (u32, &'static str)> {
    use kernel_core::ahci;
    let Some(disc) = crate::smmu::discovery() else {
        return Ok(0);
    };
    let env = Ecam::new(disc.pcie.ecam_base);
    // SAFETY: walks ECAM only.
    let Some(bdf) = (unsafe { virtiopci::find_class_nth(&env, 0x01_06_01, 0) }) else {
        kprintln!("[ahci] no controller (skipped)");
        return Ok(0);
    };
    // SAFETY: this boot owns the function; ABAR (BAR5) is mapped device memory.
    let opened = unsafe {
        open_bar(&env, bdf, AHCI_BAR_BASE, 5, 0x1100).and_then(|b| {
            let regs = ahci::MmioRegs::new(b);
            let mut disks = alloc::vec::Vec::new();
            for p in ahci::disk_ports(&regs) {
                let d = match ahci::AhciDisk::<crate::virtio::Aarch64Virtio, _>::open(regs, p) {
                    Err(ahci::NOT_ATA) => continue, // a CD-ROM or other non-disk port
                    r => r?,
                };
                kprintln!(
                    "[ahci] port {} serial \"{}\" {} x {} B{}",
                    p,
                    core::str::from_utf8(d.serial()).unwrap_or("?").trim(),
                    d.sectors(),
                    d.sector_bytes(),
                    if d.is_scratch() { " (scratch)" } else { "" }
                );
                disks.push(d);
            }
            Ok(disks)
        })
    };
    let mut disks = match opened {
        Ok(d) => d,
        Err(e) => {
            kprintln!("[ahci] init refused: {}", e);
            return Err((0, "ahci controller initialization"));
        }
    };
    ahci::device_suite(&mut disks, 256, &mut suite_log)
        .map(|n| n as u32)
        .map_err(|(i, name)| (i as u32, name))
}
