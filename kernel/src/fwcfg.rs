//! The fw_cfg MMIO transport for QEMU virt — layout taken from the machine's OWN firmware code
//! path (fw_cfg_init_mem_dma): SELECTOR is a 16-BIG-ENDIAN-BYTES write at base+8 (only a
//! two-byte write is accepted; anything else is an unassigned access and faults), and DATA
//! streams one byte per 8-bit load at base+0 (region width 8). The DMA pair lives at base+16
//! and stays unused — the legacy select/read path crosses no DMA descriptor. Device-nGnRnE
//! attributes order every access strictly, so volatile operations suffice.
use kernel_core::fwcfg::FwCfgBus;

/// Where QEMU virt puts the firmware configuration window (DTB-verified base; behavior verified
/// by live probes recorded in ADR-072).
pub const FW_CFG_BASE: usize = 0x0902_0000;

/// The platform bus handle. One instance; nothing here is shared across cores.
// The DATA window (loads stream item bytes at base+0) is read in place; it needs no named
// offset constant - the transport never stores through it (a store would push phantom bytes).
const SELECTOR: usize = 0x08;

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
        // SAFETY: the control register sits at base+8 (fw_cfg_init_mem_dma maps ctl@+8,
        // data@+0, dma@+16) and accepts ONLY a two-byte write, whose value must arrive
        // BIG-ENDIAN on the wire — verified live: native-LE 0x0001 selected item 0x0100, the
        // byte-swapped store selected item 0x0001. A store into the DATA window instead would
        // push phantom bytes and shift every later read.
        unsafe {
            core::ptr::write_volatile((self.base + SELECTOR) as *mut u16, selector.swap_bytes())
        };
    }
    fn read_byte(&mut self) -> u8 {
        // SAFETY: the data window streams ONE byte per 8-bit load, advancing the device's own
        // cursor — verified live against the signature item, the directory head, and names.
        unsafe { core::ptr::read_volatile(self.base as *const u8) }
    }
}
