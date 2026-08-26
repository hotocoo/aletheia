//! The virtio-pci transport, shared by every target whose virtio devices are PCI functions
//! (REQ-DRV-005, ADR-037; generalized in ADR-074 when ARM's SMMUv3 rung needed the same bus).
//!
//! A split virtqueue and a feature handshake are protocol facts; HOW one reaches a function's
//! configuration space (legacy ports on x86-64, ECAM on aarch64) and HOW a BAR region enters
//! the kernel's address space (a dynamic device mapping under q35, the identity map on virt)
//! are BUS-AND-TARGET facts. This module owns the former through the [PciEnv] seam and leaves
//! the latter to each target - one copy of the capability walk, never two.
//!
//! What resolution does, and deliberately no more:
//!
//! * **Capability walk**: the virtio vendor capability (0x09) carries a cfg_type; we take
//!   COMMON_CFG (1), NOTIFY_CFG (2) and DEVICE_CFG (4), each as (bar, offset, length), plus
//!   the notify capability's notify_off_multiplier. A device missing any of the three is
//!   REFUSED - fail closed rather than poking a guessed offset. The hop bound is the hard
//!   stop for a device that builds a cycle.
//! * **Enable exactly two command bits**: memory space (so the BARs decode) and bus master
//!   (so the device may DMA). I/O space and interrupts stay off; completion is polled, as on
//!   every target.
//! * **BAR sizing/assignment helpers** ([bar_size], [bar_assign]) exist for targets with NO
//!   PCI firmware: a bare-metal kernel is its own BAR allocator or nothing uses the bus.

use crate::virtioblk::Transport;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

/// A bus/device/function triple - pure data; how it becomes an access belongs to the seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// The target seam: configuration-space access plus the region-mapping policy.
pub trait PciEnv {
    /// Read one 32-bit configuration register (an absent function reads all-ones).
    /// # Safety
    /// Touches the configuration mechanism; safe for any BDF.
    unsafe fn read32(&self, bdf: Bdf, reg: u8) -> u32;
    /// Write one 32-bit configuration register.
    /// # Safety
    /// The caller must know the register is writable on this function.
    unsafe fn write32(&self, bdf: Bdf, reg: u8, value: u32);
    /// Map a resolved register region [pa, pa+len) and return its virtual address, or None if
    /// the target's admission rule refuses it (e.g. it names RAM the allocator owns - ADR-037).
    fn map_region(&self, pa: u64, len: usize) -> Option<usize>;
}

/// Configuration-space register offsets (PCI 3.0 section 6.1).
pub const CFG_VENDOR: u8 = 0x00;
pub const CFG_COMMAND: u8 = 0x04;
pub const CFG_HEADER_TYPE: u8 = 0x0C;
pub const CFG_BAR0: u8 = 0x10;
pub const CFG_CAP_PTR: u8 = 0x34;

/// Command-register bits we set: decode the memory BARs, and allow DMA.
const CMD_MEMORY_SPACE: u32 = 1 << 1;
const CMD_BUS_MASTER: u32 = 1 << 2;

/// virtio's PCI identity.
pub const VENDOR_VIRTIO: u16 = 0x1AF4;
/// Modern device ids are 0x1040 + kind; transitional ids are a fixed short list.
pub const DEVICE_NET_MODERN: u16 = 0x1041;
pub const DEVICE_NET_TRANSITIONAL: u16 = 0x1000;
pub const DEVICE_BLK_MODERN: u16 = 0x1042;
pub const DEVICE_BLK_TRANSITIONAL: u16 = 0x1001;
pub const DEVICE_GPU_MODERN: u16 = 0x1050;

/// PCI capability id for vendor-specific caps, and virtio's cfg_type values (VIRTIO 1.1 4.1.4).
pub const CAP_ID_VENDOR: u8 = 0x09;
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// Offsets inside virtio_pci_common_cfg (VIRTIO 1.1 4.1.4.3).
const C_DEVICE_FEATURE_SELECT: usize = 0x00;
const C_DEVICE_FEATURE: usize = 0x04;
const C_DRIVER_FEATURE_SELECT: usize = 0x08;
const C_DRIVER_FEATURE: usize = 0x0C;
const C_DEVICE_STATUS: usize = 0x14;
const C_QUEUE_SELECT: usize = 0x16;
const C_QUEUE_SIZE: usize = 0x18;
const C_QUEUE_ENABLE: usize = 0x1C;
const C_QUEUE_NOTIFY_OFF: usize = 0x1E;
const C_QUEUE_DESC: usize = 0x20;
const C_QUEUE_DRIVER: usize = 0x28;
const C_QUEUE_DEVICE: usize = 0x30;

/// Read a 16-bit field at any byte offset, from two aligned 32-bit reads.
/// # Safety
/// The caller names a present function.
pub unsafe fn cfg_read16(env: &impl PciEnv, bdf: Bdf, reg: u8) -> u16 {
    let word = unsafe { env.read32(bdf, reg & 0xFC) };
    ((word >> ((reg as u32 & 2) * 8)) & 0xFFFF) as u16
}

/// Read an 8-bit field at any byte offset.
/// # Safety
/// As [cfg_read16].
pub unsafe fn cfg_read8(env: &impl PciEnv, bdf: Bdf, reg: u8) -> u8 {
    let word = unsafe { env.read32(bdf, reg & 0xFC) };
    ((word >> ((reg as u32 & 3) * 8)) & 0xFF) as u8
}

/// Enumerate EVERY present function on bus 0 - `(bdf, vendor, device id)`, slot order. The
/// IOMMU programming path needs the whole picture, not one device kind: a stream table is per-
/// FUNCTION, and a function the programmer never saw is a function DMA-ing outside the contract.
///
/// # Safety
/// Touches the configuration space of every bus-0 slot.
pub unsafe fn enumerate_bus0(env: &impl PciEnv) -> Vec<(Bdf, u16, u16)> {
    let mut out = Vec::new();
    for device in 0..32u8 {
        let zero = Bdf {
            bus: 0,
            device,
            function: 0,
        };
        if unsafe { env.read32(zero, CFG_VENDOR) } == 0xFFFF_FFFF {
            continue;
        }
        let multi = unsafe { cfg_read8(env, zero, CFG_HEADER_TYPE + 2) } & 0x80 != 0;
        let functions = if multi { 8 } else { 1 };
        for function in 0..functions {
            let bdf = Bdf {
                bus: 0,
                device,
                function,
            };
            let vendor = unsafe { cfg_read16(env, bdf, CFG_VENDOR) };
            if vendor == 0xFFFF {
                continue;
            }
            let dev_id = unsafe { cfg_read16(env, bdf, CFG_VENDOR + 2) };
            out.push((bdf, vendor, dev_id));
        }
    }
    out
}

/// Scan bus 0 for the nth virtio function whose device id is one of ids.
/// # Safety
/// Touches the configuration space of every bus-0 slot.
pub unsafe fn find_virtio_nth(env: &impl PciEnv, ids: &[u16], nth: usize) -> Option<Bdf> {
    let funcs = unsafe { enumerate_bus0(env) };
    let mut seen = 0usize;
    for (bdf, vendor, dev_id) in funcs {
        if vendor != VENDOR_VIRTIO || !ids.contains(&dev_id) {
            continue;
        }
        if seen == nth {
            return Some(bdf);
        }
        seen += 1;
    }
    None
}

/// One resolved virtio capability region, mapped and ready.
#[derive(Clone, Copy, Debug)]
pub struct CapRegion {
    pub addr: usize,
    pub len: u32,
}

/// The three register regions the modern protocol needs, plus the notify multiplier.
pub struct VirtioPciRegions {
    pub common: CapRegion,
    pub notify: CapRegion,
    pub notify_off_multiplier: u32,
    pub device_cfg: CapRegion,
    /// The PCI device id, so `identity()` can report the VIRTIO kind.
    pub device_id: u16,
}

/// Enable memory-space decoding and bus master. The status half of the register is
/// write-1-to-clear, so it is written back as read rather than zeroed.
/// # Safety
/// The caller knows this function exists.
pub unsafe fn enable_bus_master(env: &impl PciEnv, bdf: Bdf) {
    let command_status = unsafe { env.read32(bdf, CFG_COMMAND) };
    let command = (command_status & 0xFFFF) | CMD_MEMORY_SPACE | CMD_BUS_MASTER;
    unsafe { env.write32(bdf, CFG_COMMAND, command | (command_status & 0xFFFF_0000)) };
}

/// Resolve the device's virtio capabilities into MAPPED regions. Refuses a device missing any
/// of COMMON/NOTIFY/DEVICE config, a cycle-building cap list, or a region the target refuses
/// to map - fail closed, never a guessed offset.
/// # Safety
/// bdf must name a virtio-modern function; each region goes through env.map_region.
pub unsafe fn resolve_virtio_regions(
    env: &impl PciEnv,
    bdf: Bdf,
) -> Result<VirtioPciRegions, &'static str> {
    unsafe { enable_bus_master(env, bdf) };

    let mut common: Option<CapRegion> = None;
    let mut notify: Option<CapRegion> = None;
    let mut device_cfg: Option<CapRegion> = None;
    let mut notify_off_multiplier = 0u32;

    let mut ptr = unsafe { cfg_read8(env, bdf, CFG_CAP_PTR) } & 0xFC;
    let mut hops = 0;
    while ptr != 0 && hops < 48 {
        hops += 1;
        let cap_id = unsafe { cfg_read8(env, bdf, ptr) };
        let next = unsafe { cfg_read8(env, bdf, ptr + 1) } & 0xFC;
        if cap_id == CAP_ID_VENDOR {
            // struct virtio_pci_cap: id, next, cap_len, cfg_type, bar, pad[3], offset, length
            let cfg_type = unsafe { cfg_read8(env, bdf, ptr + 3) };
            let bar = unsafe { cfg_read8(env, bdf, ptr + 4) };
            let offset = unsafe { env.read32(bdf, ptr + 8) };
            let length = unsafe { env.read32(bdf, ptr + 12) };
            if cfg_type == VIRTIO_PCI_CAP_COMMON_CFG
                || cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG
                || cfg_type == VIRTIO_PCI_CAP_DEVICE_CFG
            {
                let pa = bar_base_pa(env, bdf, bar)?;
                let mlen = length.max(1) as usize;
                // Map [pa+offset, pa+offset+len): the capability OFFSET names where this
                // register block lives INSIDE the BAR, so mapping the BAR base alone leaves
                // every non-zero-offset region (device cfg on q35 sits at +0x2000)
                // untranslated - found live when the x86 gate flooded ring-3 #PFs.
                let addr = match env.map_region(pa + offset as u64, mlen) {
                    Some(a) => a,
                    None => return Err("virtio-pci register region could not be mapped"),
                };
                let region = CapRegion {
                    addr,
                    len: length,
                };
                match cfg_type {
                    VIRTIO_PCI_CAP_COMMON_CFG => common = Some(region),
                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                        notify_off_multiplier = unsafe { env.read32(bdf, ptr + 16) };
                        notify = Some(region);
                    }
                    _ => device_cfg = Some(region),
                }
            }
        }
        ptr = next;
    }

    let common = common.ok_or("virtio-pci device has no COMMON_CFG capability - fail closed")?;
    let notify = notify.ok_or("virtio-pci device has no NOTIFY_CFG capability - fail closed")?;
    let device_cfg =
        device_cfg.ok_or("virtio-pci device has no DEVICE_CFG capability - fail closed")?;
    if (common.len as usize) < C_QUEUE_DEVICE + 8 {
        return Err("virtio-pci COMMON_CFG region is too short for the queue registers");
    }
    Ok(VirtioPciRegions {
        common,
        notify,
        notify_off_multiplier,
        device_cfg,
        device_id: unsafe { cfg_read16(env, bdf, CFG_VENDOR + 2) },
    })
}

/// A BAR's physical base as programmed (I/O-space BARs refuse: this driver speaks MMIO only).
fn bar_base_pa(env: &impl PciEnv, bdf: Bdf, index: u8) -> Result<u64, &'static str> {
    if index > 5 {
        return Err("virtio-pci capability names a BAR index > 5");
    }
    let low = unsafe { env.read32(bdf, CFG_BAR0 + index * 4) };
    if low & 1 != 0 {
        return Err("virtio-pci capability points at an I/O-space BAR - fail closed");
    }
    let is_64 = (low >> 1) & 3 == 2;
    let base_lo = (low & 0xFFFF_FFF0) as u64;
    let base = if is_64 {
        let high = unsafe { env.read32(bdf, CFG_BAR0 + index * 4 + 4) } as u64;
        (high << 32) | base_lo
    } else {
        base_lo
    };
    if base == 0 {
        return Err("virtio-pci BAR is unassigned (nothing programmed it)");
    }
    Ok(base)
}

/// Size ONE memory BAR by the write-all-ones probe, restoring an UNASSIGNED bar afterwards
/// (assignment is a separate decision). Returns the natural size in bytes.
/// # Safety
/// The caller owns this function's configuration space at this moment.
pub unsafe fn bar_size(env: &impl PciEnv, bdf: Bdf, index: u8) -> Result<u64, &'static str> {
    if index > 5 {
        return Err("BAR index > 5");
    }
    let reg = CFG_BAR0 + index * 4;
    let raw = unsafe { env.read32(bdf, reg) };
    if raw & 1 != 0 {
        return Err("I/O-space BAR - this driver speaks MMIO only");
    }
    let is_64 = (raw >> 1) & 3 == 2;
    unsafe { env.write32(bdf, reg, 0xFFFF_FFF0 | (raw & 0xF)) };
    let probed = unsafe { env.read32(bdf, reg) };
    unsafe { env.write32(bdf, reg, raw & 0xFFFF_FFF0) };
    if !is_64 {
        let mask = probed & 0xFFFF_FFF0;
        return Ok(if mask == 0 {
            0
        } else {
            (!mask).wrapping_add(1) as u64
        });
    }
    // High half probes the same way; restore it unassigned as well.
    let hi_reg = reg + 4;
    let raw_hi = unsafe { env.read32(bdf, hi_reg) };
    unsafe { env.write32(bdf, hi_reg, 0xFFFF_FFFF) };
    let probed_hi = unsafe { env.read32(bdf, hi_reg) };
    unsafe { env.write32(bdf, hi_reg, raw_hi) };
    let combined = ((probed_hi as u64) << 32) | (probed & 0xFFFF_FFF0) as u64;
    let mask = combined & 0xFFFF_FFFF_FFFF_FFF0;
    Ok(if mask == 0 {
        0
    } else {
        (!mask).wrapping_add(1)
    })
}

/// Program a memory BAR's base. Caller picks addresses inside the window the platform declares.
/// # Safety
/// The caller owns the assignment decision.
pub unsafe fn bar_assign(env: &impl PciEnv, bdf: Bdf, index: u8, addr: u64) {
    let reg = CFG_BAR0 + index * 4;
    unsafe { env.write32(bdf, reg, addr as u32 & 0xFFFF_FFF0) };
}

/// The transport: the three mapped register regions plus the latched notify offset.
pub struct PciTransport {
    common: CapRegion,
    notify: CapRegion,
    notify_off_multiplier: u32,
    device_cfg: CapRegion,
    notify_off: u16,
    device_id: u16,
}

impl PciTransport {
    /// Enable the function, resolve and map its regions.
    ///
    /// # Safety
    /// bdf must name a virtio function; regions go through the target's admission rule.
    pub unsafe fn new(env: &impl PciEnv, bdf: Bdf) -> Result<Self, &'static str> {
        let r = unsafe { resolve_virtio_regions(env, bdf)? };
        Ok(PciTransport {
            common: r.common,
            notify: r.notify,
            notify_off_multiplier: r.notify_off_multiplier,
            device_cfg: r.device_cfg,
            notify_off: 0,
            device_id: r.device_id,
        })
    }

    /// The resolved region addresses + multiplier, for the caller's boot log.
    pub fn regions(&self) -> (usize, usize, usize, u32) {
        (
            self.common.addr,
            self.notify.addr,
            self.device_cfg.addr,
            self.notify_off_multiplier,
        )
    }

    /// Assemble a transport from ALREADY-RESOLVED regions - lets a target's gate resolve first,
    /// log what it resolved, and still hand the standard transport to the driver.
    pub fn from_parts(r: VirtioPciRegions) -> Self {
        PciTransport {
            common: r.common,
            notify: r.notify,
            notify_off_multiplier: r.notify_off_multiplier,
            device_cfg: r.device_cfg,
            notify_off: 0,
            device_id: r.device_id,
        }
    }

    /// Latch the selected queue's notify offset - called once after queue select (via
    /// Transport::after_queue_select) so notify never re-selects mid-request.
    /// # Safety
    /// The COMMON_CFG region must be mapped and a queue already selected.
    pub unsafe fn latch_notify_off(&mut self) {
        self.notify_off = unsafe { self.c16(C_QUEUE_NOTIFY_OFF) };
    }

    #[inline]
    unsafe fn c8(&self, off: usize) -> u8 {
        read_volatile((self.common.addr + off) as *const u8)
    }
    #[inline]
    unsafe fn w8(&self, off: usize, v: u8) {
        write_volatile((self.common.addr + off) as *mut u8, v);
    }
    #[inline]
    unsafe fn c16(&self, off: usize) -> u16 {
        read_volatile((self.common.addr + off) as *const u16)
    }
    #[inline]
    unsafe fn w16(&self, off: usize, v: u16) {
        write_volatile((self.common.addr + off) as *mut u16, v);
    }
    #[inline]
    unsafe fn c32(&self, off: usize) -> u32 {
        read_volatile((self.common.addr + off) as *const u32)
    }
    #[inline]
    unsafe fn w32(&self, off: usize, v: u32) {
        write_volatile((self.common.addr + off) as *mut u32, v);
    }
    #[inline]
    unsafe fn w64(&self, off: usize, v: u64) {
        // Two 32-bit stores: the halves are defined separately, and a single 64-bit store to a
        // device register is not guaranteed to be seen as one access.
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

impl Transport for PciTransport {
    fn identity(&self) -> (u32, u32) {
        // Report the VIRTIO device KIND, not the PCI device id - identity must mean the same
        // thing on both buses, or a driver asking "is this a network device?" reads 0x1041 and
        // refuses a good NIC.
        let kind = match self.device_id {
            0x1000 => 1, // transitional net
            0x1001 => 2, // transitional blk
            id if id >= 0x1040 => (id - 0x1040) as u32,
            other => other as u32,
        };
        (2, kind)
    }

    unsafe fn device_features(&self, sel: u32) -> u32 {
        self.w32(C_DEVICE_FEATURE_SELECT, sel);
        self.c32(C_DEVICE_FEATURE)
    }

    unsafe fn set_driver_features(&self, sel: u32, value: u32) {
        self.w32(C_DRIVER_FEATURE_SELECT, sel);
        self.w32(C_DRIVER_FEATURE, value);
    }

    unsafe fn status(&self) -> u32 {
        self.c8(C_DEVICE_STATUS) as u32
    }

    unsafe fn set_status(&self, value: u32) {
        self.w8(C_DEVICE_STATUS, value as u8);
    }

    unsafe fn select_queue(&self, queue: u16) {
        self.w16(C_QUEUE_SELECT, queue);
    }

    unsafe fn after_queue_select(&mut self) {
        // The notify address depends on the SELECTED queue's queue_notify_off; latch it now so
        // notify never has to touch queue_select while a request is in flight.
        self.latch_notify_off();
    }

    unsafe fn queue_num_max(&self) -> u32 {
        self.c16(C_QUEUE_SIZE) as u32
    }

    unsafe fn set_queue_num(&self, size: u32) {
        self.w16(C_QUEUE_SIZE, size as u16);
    }

    unsafe fn set_queue_addrs(&self, desc: u64, avail: u64, used: u64) {
        self.w64(C_QUEUE_DESC, desc);
        self.w64(C_QUEUE_DRIVER, avail);
        self.w64(C_QUEUE_DEVICE, used);
    }

    unsafe fn queue_ready(&self) {
        self.w16(C_QUEUE_ENABLE, 1);
    }

    unsafe fn notify(&self, queue: u16) {
        let off = self.notify_off as usize * self.notify_off_multiplier as usize;
        write_volatile((self.notify.addr + off) as *mut u16, queue);
    }

    unsafe fn config_u64(&self, off: usize) -> u64 {
        let lo = read_volatile((self.device_cfg.addr + off) as *const u32) as u64;
        let hi = read_volatile((self.device_cfg.addr + off + 4) as *const u32) as u64;
        (hi << 32) | lo
    }
}
