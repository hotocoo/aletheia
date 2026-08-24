//! The fw_cfg MMIO transport for QEMU virt (RISC-V) — same device model as the aarch64
//! machine, because both come from the same firmware code path (fw_cfg_init_mem_dma):
//! SELECTOR is a 16-bit write at base+8 whose value must arrive BIG-ENDIAN on the wire (only
//! two-byte writes are accepted; anything else is an unassigned access), and DATA streams one
//! byte per 8-bit load at base+0 (region width 8). The DMA pair lives at base+16 and stays
//! unused — the legacy select/read path crosses no DMA descriptor. Device memory orders every
//! access strictly, so volatile operations suffice.
use kernel_core::fwcfg::FwCfgBus;

/// Where QEMU virt puts the firmware configuration window (DTB-verified base; behavior
/// verified by live probes recorded in ADR-072).
pub const FW_CFG_BASE: usize = 0x1010_0000;

/// Control-register offset within the firmware window.
const SELECTOR: usize = 0x08;
/// Data-register offset (loads stream item bytes here).
const DATA: usize = 0x00;

/// The platform bus handle. One instance; nothing here is shared across cores.
pub struct FwCfgMmio {
    base: usize,
}

impl FwCfgMmio {
    pub const fn new() -> Self {
        FwCfgMmio { base: FW_CFG_BASE }
    }
}

impl FwCfgBus for FwCfgMmio {
    fn select(&mut self, selector: u16) {
        // SAFETY: the control register sits at base+8 and accepts ONLY a two-byte write,
        // whose value must arrive BIG-ENDIAN on the wire — verified live on the aarch64 twin:
        // native-LE 0x0001 selected item 0x0100; the byte-swapped store selected 0x0001.
        unsafe {
            core::ptr::write_volatile(
                (self.base + SELECTOR) as *mut u16,
                selector.swap_bytes(),
            )
        };
    }
    fn read_byte(&mut self) -> u8 {
        // SAFETY: the data window streams ONE byte per 8-bit load, advancing the device's own
        // cursor — verified live against the signature item and directory names.
        unsafe { core::ptr::read_volatile((self.base + DATA) as *const u8) }
    }
}
