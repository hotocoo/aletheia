//! Minimal PCI enumeration + the virtio-pci (modern) transport for x86-64 (REQ-DRV-005, ADR-037).
//!
//! The other two targets reach virtio through a fixed MMIO window the platform documents. x86-64's
//! q35 has no such window: virtio devices are **PCI** functions, so the registers the shared driver
//! needs (`kernel_core::virtioblk::Transport`) live inside BAR regions that a *capability list* in
//! configuration space points at. That is a difference in the bus, not in the protocol — so this file
//! implements the transport trait and the driver stays shared (ADR-036 / ADR-037).
//!
//! ## What it does, and deliberately no more
//!
//! * **Config space through the legacy ports** (`0xCF8` address / `0xCFC` data). ECAM would require
//!   finding the MCFG table first; the ports work on every x86 machine QEMU emulates and need nothing
//!   from ACPI. Enumeration is bus 0 only, 32 devices × up to 8 functions.
//! * **One device kind**: vendor `0x1AF4` with a block device id (`0x1042` modern, `0x1001`
//!   transitional). Anything else is skipped, never guessed at.
//! * **Capability walk**: the virtio vendor capability (`0x09`) carries a `cfg_type`; we take
//!   COMMON_CFG (1), NOTIFY_CFG (2) and DEVICE_CFG (4), each as `(bar, offset, length)`, plus the
//!   notify capability's `notify_off_multiplier`. A device missing any of the three is **refused** —
//!   fail closed rather than poking a guessed offset.
//! * **Enable exactly two command bits**: memory space (so the BARs decode) and bus master (so the
//!   device may DMA). I/O space and interrupts stay off; completion is polled, as on every target.
//!
//! ## Honest limits
//!
//! A 64-bit BAR is read as a pair, and on q35 it lands **above 4 GiB** — outside the kernel's own
//! boot-time map, which deliberately covers only sub-4 GiB MMIO. Rather than widening that map to some
//! arbitrary bound, each resolved region is mapped here through `vm::map_device_range`, whose admission
//! check refuses RAM the frame allocator owns (`MapFault::PhysIsRam`, ADR-037) — a driver maps its own
//! registers, and cannot alias a task's frame as MMIO while doing it. Regions are mapped 4 KiB at a
//! time, which suits the ~16 KiB virtio window and is not a general MMIO strategy.
//!
//! There is no bus/bridge recursion (QEMU q35 puts virtio functions on bus 0), no MSI/MSI-X (nothing
//! here uses interrupts), and no BAR *assignment* — the firmware already programmed them, and
//! re-assigning addresses is a different, larger decision.
use core::ptr::{read_volatile, write_volatile};

use kernel_core::virtioblk::Transport;

/// Legacy configuration-space access ports.
const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Configuration-space register offsets (PCI 3.0 §6.1).
const CFG_VENDOR: u8 = 0x00; // vendor (low half) + device (high half)
const CFG_COMMAND: u8 = 0x04; // command (low half) + status (high half)
const CFG_HEADER_TYPE: u8 = 0x0C; // cache line / latency / header type / BIST
const CFG_BAR0: u8 = 0x10;
const CFG_CAP_PTR: u8 = 0x34;

/// Command-register bits we set: decode the memory BARs, and allow DMA.
const CMD_MEMORY_SPACE: u32 = 1 << 1;
const CMD_BUS_MASTER: u32 = 1 << 2;

/// virtio's PCI identity.
const VENDOR_VIRTIO: u16 = 0x1AF4;
const DEVICE_BLK_MODERN: u16 = 0x1042;
const DEVICE_BLK_TRANSITIONAL: u16 = 0x1001;
/// virtio-net's PCI ids: modern (0x1041) and transitional (0x1000). A second device KIND is two more ids,
/// not a second driver framework (ADR-041).
const DEVICE_NET_MODERN: u16 = 0x1041;
const DEVICE_NET_TRANSITIONAL: u16 = 0x1000;

/// PCI capability ids and the virtio capability's `cfg_type` values (VIRTIO 1.1 §4.1.4).
const CAP_ID_VENDOR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// Offsets inside `virtio_pci_common_cfg` (VIRTIO 1.1 §4.1.4.3).
const C_DEVICE_FEATURE_SELECT: usize = 0x00; // u32
const C_DEVICE_FEATURE: usize = 0x04; // u32
const C_DRIVER_FEATURE_SELECT: usize = 0x08; // u32
const C_DRIVER_FEATURE: usize = 0x0C; // u32
const C_DEVICE_STATUS: usize = 0x14; // u8
const C_QUEUE_SELECT: usize = 0x16; // u16
const C_QUEUE_SIZE: usize = 0x18; // u16
const C_QUEUE_ENABLE: usize = 0x1C; // u16
const C_QUEUE_NOTIFY_OFF: usize = 0x1E; // u16
const C_QUEUE_DESC: usize = 0x20; // u64
const C_QUEUE_DRIVER: usize = 0x28; // u64
const C_QUEUE_DEVICE: usize = 0x30; // u64

#[inline]
unsafe fn outl(port: u16, value: u32) {
    // SAFETY: caller names a port it owns; `out dx, eax` has no memory effects.
    core::arch::asm!(
        "out dx, eax",
        in("dx") port,
        in("eax") value,
        options(nomem, nostack, preserves_flags)
    );
}

#[inline]
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: as above.
    core::arch::asm!(
        "in eax, dx",
        out("eax") value,
        in("dx") port,
        options(nomem, nostack, preserves_flags)
    );
    value
}

/// A bus/device/function triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Bdf {
    fn address(&self, reg: u8) -> u32 {
        1 << 31
            | (self.bus as u32) << 16
            | (self.device as u32) << 11
            | (self.function as u32) << 8
            | (reg as u32 & 0xFC)
    }

    /// Read one 32-bit configuration register.
    ///
    /// # Safety
    /// Touches the configuration ports; safe for any BDF (an absent function reads all-ones).
    pub unsafe fn read32(&self, reg: u8) -> u32 {
        outl(CONFIG_ADDRESS, self.address(reg));
        inl(CONFIG_DATA)
    }

    /// Write one 32-bit configuration register.
    ///
    /// # Safety
    /// As above; the caller must know the register is writable on this function.
    pub unsafe fn write32(&self, reg: u8, value: u32) {
        outl(CONFIG_ADDRESS, self.address(reg));
        outl(CONFIG_DATA, value);
    }

    unsafe fn read16(&self, reg: u8) -> u16 {
        let word = self.read32(reg & 0xFC);
        ((word >> ((reg as u32 & 2) * 8)) & 0xFFFF) as u16
    }

    unsafe fn read8(&self, reg: u8) -> u8 {
        let word = self.read32(reg & 0xFC);
        ((word >> ((reg as u32 & 3) * 8)) & 0xFF) as u8
    }
}

/// One virtio capability region, resolved to an absolute MMIO address.
#[derive(Clone, Copy, Debug)]
struct CapRegion {
    addr: usize,
    len: u32,
}

/// Read a BAR as a usable MMIO base. Refuses an I/O-space BAR, an unassigned one, and one that does not
/// fit an address — but NOT a high one: a 64-bit BAR above 4 GiB is normal, and the caller maps it.
unsafe fn bar_base(bdf: &Bdf, index: u8) -> Result<usize, &'static str> {
    if index > 5 {
        return Err("virtio-pci capability names a BAR index > 5");
    }
    let reg = CFG_BAR0 + index * 4;
    let low = bdf.read32(reg);
    if low & 1 != 0 {
        return Err("virtio-pci capability points at an I/O-space BAR — fail closed");
    }
    let is_64 = (low >> 1) & 3 == 2;
    let base_lo = (low & 0xFFFF_FFF0) as u64;
    let base = if is_64 {
        let high = bdf.read32(reg + 4) as u64;
        (high << 32) | base_lo
    } else {
        base_lo
    };
    if base == 0 {
        return Err("virtio-pci BAR is unassigned (firmware did not program it)");
    }
    if base > usize::MAX as u64 {
        return Err("virtio-pci BAR does not fit an address");
    }
    Ok(base as usize)
}

/// Scan bus 0 for a virtio block function. Returns its BDF, or `None` when none is attached — the same
/// graceful-skip path the MMIO targets have.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_blk() -> Option<Bdf> {
    find_virtio_blk_nth(0)
}

/// Scan bus 0 for the `nth` (0-based) virtio block function — the PCI twin of
/// `virtioblk::probe_nth`, for a target that attaches a scratch disk and a PERSISTENT one
/// (REQ-STOR-003). Function order on the bus is the ordering; QEMU assigns slots in command order.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_blk_nth(nth: usize) -> Option<Bdf> {
    find_virtio_nth(&[DEVICE_BLK_MODERN, DEVICE_BLK_TRANSITIONAL], nth)
}

/// Scan bus 0 for the `nth` virtio NETWORK function.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_net_nth(nth: usize) -> Option<Bdf> {
    find_virtio_nth(&[DEVICE_NET_MODERN, DEVICE_NET_TRANSITIONAL], nth)
}

/// Scan bus 0 for the `nth` virtio function whose device id is one of `ids`.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_nth(ids: &[u16], nth: usize) -> Option<Bdf> {
    let mut seen = 0usize;
    for device in 0..32u8 {
        // Function 0 decides whether the slot is populated and whether it is multi-function.
        let zero = Bdf {
            bus: 0,
            device,
            function: 0,
        };
        if zero.read32(CFG_VENDOR) == 0xFFFF_FFFF {
            continue; // no such device
        }
        let multi = zero.read8(CFG_HEADER_TYPE + 2) & 0x80 != 0;
        let functions = if multi { 8 } else { 1 };
        for function in 0..functions {
            let bdf = Bdf {
                bus: 0,
                device,
                function,
            };
            let vendor = bdf.read16(CFG_VENDOR);
            let dev_id = bdf.read16(CFG_VENDOR + 2);
            if vendor != VENDOR_VIRTIO {
                continue;
            }
            if ids.contains(&dev_id) {
                if seen == nth {
                    return Some(bdf);
                }
                seen += 1;
            }
        }
    }
    None
}

/// The virtio-pci transport: the three register regions the protocol needs, already resolved from the
/// device's BARs, plus the notify multiplier.
pub struct PciTransport {
    common: CapRegion,
    notify: CapRegion,
    notify_off_multiplier: u32,
    device_cfg: CapRegion,
    /// `queue_notify_off` of the queue the driver selected (queue 0 is all this driver uses), latched
    /// so `notify` never has to touch `queue_select` while a request is in flight.
    notify_off: u16,
    device_id: u32,
}

impl PciTransport {
    /// Resolve the device's virtio capabilities into register regions and enable memory + bus-master.
    /// Refuses a device missing COMMON, NOTIFY or DEVICE config — fail closed, never a guessed offset.
    ///
    /// # Safety
    /// `bdf` must name a virtio block function (as returned by [`find_virtio_blk`]), and its BARs must
    /// be mapped in the active address space (the kernel's map covers sub-4 GiB MMIO).
    pub unsafe fn new(bdf: Bdf) -> Result<Self, &'static str> {
        // Enable memory-space decoding and DMA before touching any BAR region. The status half of the
        // register is write-1-to-clear, so it is written back as read to avoid clearing live bits.
        let command_status = bdf.read32(CFG_COMMAND);
        let command = (command_status & 0xFFFF) | CMD_MEMORY_SPACE | CMD_BUS_MASTER;
        bdf.write32(CFG_COMMAND, command | (command_status & 0xFFFF_0000));

        let mut common: Option<CapRegion> = None;
        let mut notify: Option<CapRegion> = None;
        let mut device_cfg: Option<CapRegion> = None;
        let mut notify_off_multiplier = 0u32;

        // Walk the capability list. The hop bound is the hard stop for a device that builds a cycle.
        let mut ptr = bdf.read8(CFG_CAP_PTR) & 0xFC;
        let mut hops = 0;
        while ptr != 0 && hops < 48 {
            hops += 1;
            let cap_id = bdf.read8(ptr);
            let next = bdf.read8(ptr + 1) & 0xFC;
            if cap_id == CAP_ID_VENDOR {
                // struct virtio_pci_cap: id, next, cap_len, cfg_type, bar, pad[3], offset, length
                let cfg_type = bdf.read8(ptr + 3);
                let bar = bdf.read8(ptr + 4);
                let offset = bdf.read32(ptr + 8);
                let length = bdf.read32(ptr + 12);
                if cfg_type == VIRTIO_PCI_CAP_COMMON_CFG
                    || cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG
                    || cfg_type == VIRTIO_PCI_CAP_DEVICE_CFG
                {
                    let region = CapRegion {
                        addr: bar_base(&bdf, bar)? + offset as usize,
                        len: length,
                    };
                    // Map it. On q35 the BAR sits above 4 GiB, which the kernel's boot-time map does
                    // not cover — so a driver does what a driver should: it maps its own registers,
                    // through the device-admission check that refuses RAM (ADR-037). Pages the boot
                    // map already covers are left as they are.
                    if !crate::vm::map_device_range(region.addr, region.len.max(1) as usize) {
                        return Err(
                            "virtio-pci register region could not be mapped as device memory",
                        );
                    }
                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => common = Some(region),
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            // The notify capability is longer: it appends notify_off_multiplier.
                            notify_off_multiplier = bdf.read32(ptr + 16);
                            notify = Some(region);
                        }
                        _ => device_cfg = Some(region),
                    }
                }
            }
            ptr = next;
        }

        let common =
            common.ok_or("virtio-pci device has no COMMON_CFG capability — fail closed")?;
        let notify =
            notify.ok_or("virtio-pci device has no NOTIFY_CFG capability — fail closed")?;
        let device_cfg =
            device_cfg.ok_or("virtio-pci device has no DEVICE_CFG capability — fail closed")?;
        if (common.len as usize) < C_QUEUE_DEVICE + 8 {
            return Err("virtio-pci COMMON_CFG region is too short for the queue registers");
        }

        Ok(PciTransport {
            common,
            notify,
            notify_off_multiplier,
            device_cfg,
            notify_off: 0,
            device_id: bdf.read16(CFG_VENDOR + 2) as u32,
        })
    }

    /// The resolved region addresses + notify multiplier, for the caller's boot log.
    pub fn regions(&self) -> (usize, usize, usize, u32) {
        (
            self.common.addr,
            self.notify.addr,
            self.device_cfg.addr,
            self.notify_off_multiplier,
        )
    }

    /// Latch the selected queue's notify offset. Called once, after the driver has selected queue 0,
    /// because `notify` must not touch `queue_select` (it runs with a request in flight).
    ///
    /// # Safety
    /// The COMMON_CFG region must be mapped, and a queue must already be selected.
    pub unsafe fn latch_notify_off(&mut self) {
        self.notify_off = self.c16(C_QUEUE_NOTIFY_OFF);
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
        // Written as two 32-bit stores: the common-config layout defines the halves separately, and a
        // 64-bit store to a device register is not guaranteed to be seen as a single access.
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

impl Transport for PciTransport {
    fn identity(&self) -> (u32, u32) {
        // Report the VIRTIO device kind, not the PCI device id — `identity` must mean the same thing on
        // both buses, or a driver that checks "am I talking to a network device?" reads 0x1041 and refuses.
        // Modern virtio-pci ids are 0x1040 + kind; the transitional ids are a fixed short list.
        let kind = match self.device_id as u16 {
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
        // virtio-pci's notify address depends on the SELECTED queue's `queue_notify_off`; latch it now
        // so `notify` never has to re-select a queue mid-request.
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
        // The notify address is per-queue: notify_base + queue_notify_off * notify_off_multiplier.
        let off = self.notify_off as usize * self.notify_off_multiplier as usize;
        write_volatile((self.notify.addr + off) as *mut u16, queue);
    }

    unsafe fn config_u64(&self, off: usize) -> u64 {
        let lo = read_volatile((self.device_cfg.addr + off) as *const u32) as u64;
        let hi = read_volatile((self.device_cfg.addr + off + 4) as *const u32) as u64;
        (hi << 32) | lo
    }
}
