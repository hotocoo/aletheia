//! PCI on x86-64: the TARGET half of the seam (REQ-DRV-005, ADR-037). The transport itself -
//! capability walk, region resolution, the modern register protocol - lives ONCE in
//! \`kernel_core::virtiopci\` (ADR-074): a second consumer (ARM's SMMUv3 rung behind ECAM) made
//! a single copy the cheaper shape, exactly as ADR-036 did for the MMIO driver.
//!
//! What remains here is what is genuinely x86-64:
//!
//! * **Config space through the legacy ports** (\`0xCF8\` address / \`0xCFC\` data). ECAM would
//!   require finding the MCFG table first; the ports work on every x86 machine QEMU emulates
//!   and need nothing from ACPI.
//! * **The mapping policy**: q35 places 64-bit BARs ABOVE 4 GiB, outside the kernel's boot-time
//!   map, so each resolved region goes through \`vm::map_device_range\`, whose admission check
//!   refuses RAM the frame allocator owns (\`MapFault::PhysIsRam\`, ADR-037) - a driver maps its
//!   own registers and cannot alias a task's frame as MMIO while doing it. Pages the boot map
//!   already covers are left as they are.
//! * **Named lookups**: bus-0 enumeration and per-kind finders with the signatures the rest of
//!   this crate already speaks.

use crate::vm;
use kernel_core::virtiopci::{self, PciEnv};

/// Legacy configuration-space access ports.
const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;
#[cfg(feature = "input-msix")]
const CAP_ID_MSIX: u8 = 0x11;
#[cfg(feature = "input-msix")]
const MSIX_FLAGS: u8 = 0x02;
#[cfg(feature = "input-msix")]
const MSIX_TABLE: u8 = 0x04;
#[cfg(feature = "input-msix")]
const MSIX_ENABLE: u16 = 1 << 15;
#[cfg(feature = "input-msix")]
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
#[cfg(feature = "input-msix")]
const PCI_COMMAND_INTX_DISABLE: u32 = 1 << 10;

#[inline]
unsafe fn outl(port: u16, value: u32) {
    // SAFETY: caller names a port it owns; \`out dx, eax\` has no memory effects.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline]
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags))
    };
    value
}

/// The bus seam: legacy ports + this target's mapping rule.
pub struct Ports;

impl PciEnv for Ports {
    unsafe fn read32(&self, bdf: virtiopci::Bdf, reg: u8) -> u32 {
        let addr = 1u32 << 31
            | (bdf.bus as u32) << 16
            | (bdf.device as u32) << 11
            | (bdf.function as u32) << 8
            | (reg as u32 & 0xFC);
        // SAFETY: the caller names a configuration register on a function it may touch.
        unsafe {
            outl(CONFIG_ADDRESS, addr);
            inl(CONFIG_DATA)
        }
    }

    unsafe fn write32(&self, bdf: virtiopci::Bdf, reg: u8, value: u32) {
        let addr = 1u32 << 31
            | (bdf.bus as u32) << 16
            | (bdf.device as u32) << 11
            | (bdf.function as u32) << 8
            | (reg as u32 & 0xFC);
        // SAFETY: the caller knows the register is writable on this function.
        unsafe {
            outl(CONFIG_ADDRESS, addr);
            outl(CONFIG_DATA, value);
        }
    }

    fn map_region(&self, pa: u64, len: usize) -> Option<usize> {
        // SAFETY: map_device_range admits only device memory (it refuses allocator-owned RAM)
        // and is idempotent over pages already mapped.
        if vm::map_device_range(pa as usize, len) {
            Some(pa as usize)
        } else {
            None
        }
    }
}

/// A bus/device/function triple.
pub type Bdf = virtiopci::Bdf;
/// The virtio-pci transport, resolved through [Ports].
pub type PciTransport = virtiopci::PciTransport;

/// Scan bus 0 for a virtio block function. Returns its BDF, or None when none is attached —
/// the same graceful-skip path the MMIO targets have.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_blk() -> Option<Bdf> {
    unsafe { find_virtio_blk_nth(0) }
}

/// Scan bus 0 for the nth (0-based) virtio block function — the PCI twin of
/// \`virtioblk::probe_nth\`: a scratch disk AND a persistent one ride the same bus (REQ-STOR-003).
/// Function order on the bus is the ordering; QEMU assigns slots in command order.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_blk_nth(nth: usize) -> Option<Bdf> {
    unsafe {
        virtiopci::find_virtio_nth(
            &Ports,
            &[
                virtiopci::DEVICE_BLK_MODERN,
                virtiopci::DEVICE_BLK_TRANSITIONAL,
            ],
            nth,
        )
    }
}

/// Scan bus 0 for the nth virtio NETWORK function.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_net_nth(nth: usize) -> Option<Bdf> {
    unsafe {
        virtiopci::find_virtio_nth(
            &Ports,
            &[
                virtiopci::DEVICE_NET_MODERN,
                virtiopci::DEVICE_NET_TRANSITIONAL,
            ],
            nth,
        )
    }
}

/// Scan bus 0 for the nth virtio GPU function (REQ-GFX-001).
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_gpu_nth(nth: usize) -> Option<Bdf> {
    unsafe { virtiopci::find_virtio_nth(&Ports, &[virtiopci::DEVICE_GPU_MODERN], nth) }
}

/// Scan bus 0 for the nth virtio INPUT function (ALET-P2-021 hardware rung, ADR-080).
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn find_virtio_input_nth(nth: usize) -> Option<Bdf> {
    unsafe { virtiopci::find_virtio_nth(&Ports, &[virtiopci::DEVICE_INPUT_MODERN], nth) }
}

/// Enumerate EVERY present function on bus 0 — \`(bdf, vendor, device id)\`, slot order. The
/// VT-d programming path needs the whole picture: a context table is per-FUNCTION, and a
/// function the programmer never saw is a function DMA-ing outside the contract.
///
/// # Safety
/// Touches the PCI configuration ports.
pub unsafe fn enumerate_bus0() -> alloc::vec::Vec<(Bdf, u16, u16)> {
    // SAFETY: touches only the configuration ports.
    unsafe { virtiopci::enumerate_bus0(&Ports) }
}

/// Bring up one virtio-pci function's transport: enable memory + bus master, walk the vendor
/// capabilities, and map every region this target's rule admits.
///
/// # Safety
/// bdf must name a virtio-modern function.
pub unsafe fn transport_new(bdf: Bdf) -> Result<PciTransport, &'static str> {
    // SAFETY: regions resolve through [Ports], whose map_region applies ADR-037's admission rule.
    unsafe { PciTransport::new(&Ports, bdf) }
}

#[cfg(feature = "input-msix")]
fn cap_ptr(env: &Ports, bdf: Bdf) -> Option<u8> {
    let mut ptr = unsafe { virtiopci::cfg_read8(env, bdf, virtiopci::CFG_CAP_PTR) } & 0xfc;
    let mut hops = 0;
    while ptr != 0 && hops < 48 {
        hops += 1;
        let id = unsafe { virtiopci::cfg_read8(env, bdf, ptr) };
        if id == CAP_ID_MSIX {
            return Some(ptr);
        }
        ptr = unsafe { virtiopci::cfg_read8(env, bdf, ptr + 1) } & 0xfc;
    }
    None
}

#[cfg(feature = "input-msix")]
unsafe fn bar_base(env: &Ports, bdf: Bdf, index: u8) -> Option<u64> {
    if index > 5 {
        return None;
    }
    let lo = unsafe { env.read32(bdf, virtiopci::CFG_BAR0 + index * 4) };
    if lo == 0xffff_ffff || lo & 1 != 0 {
        return None;
    }
    let ty = (lo >> 1) & 0x3;
    if ty == 0x2 {
        if index == 5 {
            return None;
        }
        let hi = unsafe { env.read32(bdf, virtiopci::CFG_BAR0 + (index + 1) * 4) };
        Some(((hi as u64) << 32) | (lo as u64 & 0xffff_fff0))
    } else {
        Some((lo as u64) & 0xffff_fff0)
    }
}

/// Program one MSI-X table entry to deliver to the BSP LAPIC and return the table/vector pair
/// that was actually selected.  The device stays masked until the complete message is visible,
/// then MSI-X is enabled and legacy INTx is disabled.  No guessed BAR or capability offset is
/// accepted: a missing/cyclic capability list, I/O BAR, short table, or unmappable table refuses.
///
/// The vector is allocated by the kernel's fixed interrupt domain (0x51 for the input wake path),
/// while the MSI-X table index is independently validated against the device-advertised size.
#[cfg(feature = "input-msix")]
pub unsafe fn enable_msix(bdf: Bdf, vector: u8) -> Result<usize, &'static str> {
    let env = Ports;
    let cap = cap_ptr(&env, bdf).ok_or("PCI function has no MSI-X capability")?;
    let ctrl = unsafe { virtiopci::cfg_read16(&env, bdf, cap + MSIX_FLAGS) };
    let count = ((ctrl & 0x07ff) as usize) + 1;
    if count == 0 {
        return Err("MSI-X advertises an empty table");
    }
    let table = unsafe { env.read32(bdf, cap + MSIX_TABLE) };
    let bir = (table & 0x7) as u8;
    let offset = (table & 0xffff_fff8) as u64;
    let bar = unsafe { bar_base(&env, bdf, bir) }.ok_or("MSI-X table names an invalid BAR")?;
    let table_bytes = count
        .checked_mul(16)
        .ok_or("MSI-X table size overflow")?;
    if offset > u64::MAX - table_bytes as u64 {
        return Err("MSI-X table address overflow");
    }
    let pa = bar.checked_add(offset).ok_or("MSI-X table base overflow")?;
    if pa > usize::MAX as u64 {
        return Err("MSI-X table is outside the target address space");
    }
    if !vm::map_device_range(pa as usize, table_bytes) {
        return Err("MSI-X table could not be admitted as device memory");
    }

    // Mask the function before touching its table entry. The per-vector mask is then also kept
    // set until all four DWORDs have been written, avoiding a partially programmed message.
    let mut flags = ctrl | MSIX_FUNCTION_MASK;
    let cap_word = unsafe { env.read32(bdf, cap) };
    let cap_word = (cap_word & 0xffff) | ((flags as u32) << 16);
    unsafe { env.write32(bdf, cap, cap_word) };
    let entry = pa as usize;
    unsafe {
        core::ptr::write_volatile(entry as *mut u32, 0xfee0_0000);
        core::ptr::write_volatile((entry + 4) as *mut u32, 0);
        core::ptr::write_volatile((entry + 8) as *mut u32, vector as u32);
        core::ptr::write_volatile((entry + 12) as *mut u32, 0);
    }

    // Message delivery is now authoritative; prevent the legacy pin from generating a second
    // interrupt path for the same device.
    let command_status = unsafe { env.read32(bdf, virtiopci::CFG_COMMAND) };
    let command = (command_status & 0xffff) | PCI_COMMAND_INTX_DISABLE;
    // Config-space status contains RW1C bits; writing the previously read status value back here
    // could acknowledge unrelated device faults. Preserve only the command half and write zeroes
    // into the status half instead.
    unsafe { env.write32(bdf, virtiopci::CFG_COMMAND, command) };

    flags |= MSIX_ENABLE;
    flags &= !MSIX_FUNCTION_MASK;
    let cap_word = unsafe { env.read32(bdf, cap) };
    let cap_word = (cap_word & 0xffff) | ((flags as u32) << 16);
    unsafe { env.write32(bdf, cap, cap_word) };
    Ok(count)
}
