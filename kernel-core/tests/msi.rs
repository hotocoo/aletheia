//! Hosted proof of the shared MSI / MSI-X programming (ADR-264) against a simulated configuration
//! space: the doorbell and data land where the specification puts them, the legacy pin is silenced,
//! a controller's admin vector stays masked, and a function that cannot take a message is refused
//! by name rather than armed on a wake that cannot come.

use kernel_core::virtiopci::{
    enable_msi, enable_msix_entry, Bdf, PciEnv, CAP_ID_MSI, CAP_ID_MSIX, CFG_BAR0, CFG_CAP_PTR,
    CFG_COMMAND,
};
use std::cell::RefCell;

const BDF: Bdf = Bdf {
    bus: 0,
    device: 1,
    function: 0,
};
const CAP: u8 = 0x40;
const TABLE_ENTRIES: u16 = 4;
const TABLE_BIR: u32 = 0;
const TABLE_OFFSET: u32 = 0x2000;
const ENTRY_BYTES: usize = 16;

struct Cfg {
    words: RefCell<[u32; 64]>,
    /// The device's BAR0 window: the MSI-X table lives at TABLE_OFFSET inside it.
    bar: Vec<u8>,
}

impl Cfg {
    fn new(cap_id: u8, ctrl: u16) -> Self {
        let mut w = [0u32; 64];
        w[(CFG_CAP_PTR / 4) as usize] = CAP as u32;
        w[(CFG_COMMAND / 4) as usize] = 0x0000_0006; // memory + bus master
        w[(CAP / 4) as usize] = cap_id as u32 | (ctrl as u32) << 16; // next = 0
        let bar = vec![0u8; 0x4000];
        w[(CFG_BAR0 / 4) as usize] = 0x1000_0000; // 32-bit memory BAR at a non-zero base
        if cap_id == CAP_ID_MSIX {
            w[(CAP / 4 + 1) as usize] = TABLE_OFFSET | TABLE_BIR;
        }
        Cfg {
            words: RefCell::new(w),
            bar,
        }
    }
    fn word(&self, reg: u8) -> u32 {
        self.words.borrow()[(reg / 4) as usize]
    }
    fn entry(&self, n: usize, field: usize) -> u32 {
        let at = TABLE_OFFSET as usize + n * ENTRY_BYTES + field;
        u32::from_le_bytes(self.bar[at..at + 4].try_into().unwrap())
    }
}

impl PciEnv for Cfg {
    unsafe fn read32(&self, _: Bdf, reg: u8) -> u32 {
        self.word(reg)
    }
    unsafe fn write32(&self, _: Bdf, reg: u8, value: u32) {
        // A BAR latches only the bits its size decodes (the write-all-ones sizing probe).
        let value = if reg == CFG_BAR0 {
            value & !(self.bar.len() as u32 - 1)
        } else {
            value
        };
        self.words.borrow_mut()[(reg / 4) as usize] = value;
    }
    fn map_region(&self, _pa: u64, len: usize) -> Option<usize> {
        // The table's page inside the simulated BAR window.
        (len <= self.bar.len() - TABLE_OFFSET as usize)
            .then(|| self.bar.as_ptr() as usize + TABLE_OFFSET as usize)
    }
}

const ADDR: u64 = 0xFEE0_3000;
const DATA: u16 = 0x55;

#[test]
fn a_64_bit_maskable_msi_gets_its_message_and_is_enabled_with_the_pin_silenced() {
    // control: 64-bit capable (bit 7), per-vector masking (bit 8), one vector requested (MME = 0).
    let cfg = Cfg::new(CAP_ID_MSI, 1 << 7 | 1 << 8);
    unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.expect("msi");
    assert_eq!(cfg.word(CAP + 4), ADDR as u32);
    assert_eq!(cfg.word(CAP + 8), (ADDR >> 32) as u32);
    assert_eq!(cfg.word(CAP + 12) & 0xFFFF, DATA as u32);
    assert_eq!(cfg.word(CAP + 16), 0, "the vector is unmasked");
    assert_eq!((cfg.word(CAP) >> 16) & 1, 1, "MSI enable");
    assert_eq!((cfg.word(CAP) >> 16) & 0x70, 0, "one vector only (MME = 0)");
    assert_ne!(cfg.word(CFG_COMMAND) & (1 << 10), 0, "INTx disabled");
    assert_eq!(
        cfg.word(CFG_COMMAND) >> 16,
        0,
        "the write-1-to-clear status half is untouched"
    );
    assert_eq!(cfg.word(CFG_COMMAND) & 6, 6, "memory + bus master survive");
}

#[test]
fn a_function_without_msi_is_refused_by_name() {
    let cfg = Cfg::new(0x01, 0); // a power-management capability only
    let e = unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.unwrap_err();
    assert!(e.contains("no MSI capability"), "{e}");
    assert_eq!(
        cfg.word(CFG_COMMAND) & (1 << 10),
        0,
        "the pin is left alone"
    );
}

#[test]
fn a_32_bit_msi_cannot_take_a_doorbell_above_4_gib() {
    let cfg = Cfg::new(CAP_ID_MSI, 0);
    let e = unsafe { enable_msi(&cfg, BDF, 1 << 33, DATA) }.unwrap_err();
    assert!(e.contains("32-bit"), "{e}");
    unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.expect("below 4 GiB is fine");
    assert_eq!(cfg.word(CAP + 4), ADDR as u32);
    assert_eq!(
        cfg.word(CAP + 8) & 0xFFFF,
        DATA as u32,
        "32-bit layout: data at +8"
    );
}

#[test]
fn msix_entry_one_is_programmed_and_the_admin_entry_stays_masked() {
    let cfg = Cfg::new(CAP_ID_MSIX, TABLE_ENTRIES - 1);
    let size = unsafe { enable_msix_entry(&cfg, BDF, 1, ADDR, DATA as u32) }.expect("msix");
    assert_eq!(size, TABLE_ENTRIES);
    assert_eq!(cfg.entry(1, 0), ADDR as u32);
    assert_eq!(cfg.entry(1, 4), (ADDR >> 32) as u32);
    assert_eq!(cfg.entry(1, 8), DATA as u32);
    assert_eq!(cfg.entry(1, 12) & 1, 0, "the used entry is unmasked");
    assert_eq!(
        cfg.entry(0, 12) & 1,
        1,
        "entry 0 (a controller's admin vector) is masked"
    );
    let ctrl = cfg.word(CAP) >> 16;
    assert_ne!(ctrl & (1 << 15), 0, "MSI-X enabled");
    assert_eq!(ctrl & (1 << 14), 0, "function mask released");
    assert_ne!(cfg.word(CFG_COMMAND) & (1 << 10), 0, "INTx disabled");
}

#[test]
fn an_entry_past_the_table_and_a_function_without_msix_are_refused_by_name() {
    let cfg = Cfg::new(CAP_ID_MSIX, TABLE_ENTRIES - 1);
    let e = unsafe { enable_msix_entry(&cfg, BDF, TABLE_ENTRIES, ADDR, 1) }.unwrap_err();
    assert!(e.contains("past the table"), "{e}");
    assert_eq!(cfg.word(CAP) >> 16 & (1 << 15), 0, "nothing was enabled");
    let none = Cfg::new(0x01, 0);
    let e = unsafe { enable_msix_entry(&none, BDF, 1, ADDR, 1) }.unwrap_err();
    assert!(e.contains("no MSI-X capability"), "{e}");
}

#[test]
fn a_table_that_leaves_its_bar_or_names_an_upper_half_is_refused_by_name() {
    // Offset + 4 entries * 16 bytes runs past the 0x4000-byte BAR.
    let cfg = Cfg::new(CAP_ID_MSIX, TABLE_ENTRIES - 1);
    cfg.words.borrow_mut()[(CAP / 4 + 1) as usize] = (0x4000 - 0x20) | TABLE_BIR;
    let e = unsafe { enable_msix_entry(&cfg, BDF, 1, ADDR, 1) }.unwrap_err();
    assert!(e.contains("outside the BAR"), "{e}");
    assert_eq!(cfg.word(CAP) >> 16 & (1 << 15), 0, "nothing was enabled");
    assert_eq!(
        cfg.word(CFG_BAR0),
        0x1000_0000,
        "the probe put the BAR back"
    );
    assert_eq!(cfg.word(CFG_COMMAND) & 6, 6, "the decode is back on");
    // An offset whose sum overflows 32 bits is still outside, not wrapped inside.
    cfg.words.borrow_mut()[(CAP / 4 + 1) as usize] = 0xFFFF_FFF8 | TABLE_BIR;
    let e = unsafe { enable_msix_entry(&cfg, BDF, 1, ADDR, 1) }.unwrap_err();
    assert!(e.contains("outside the BAR"), "{e}");
    // BIR 1 names the upper half of BAR0 when BAR0 is a 64-bit memory BAR.
    let wide = Cfg::new(CAP_ID_MSIX, TABLE_ENTRIES - 1);
    wide.words.borrow_mut()[(CFG_BAR0 / 4) as usize] = 0x1000_0004;
    wide.words.borrow_mut()[(CFG_BAR0 / 4 + 1) as usize] = 0x2000_0000;
    wide.words.borrow_mut()[(CAP / 4 + 1) as usize] = TABLE_OFFSET | 1;
    let e = unsafe { enable_msix_entry(&wide, BDF, 1, ADDR, 1) }.unwrap_err();
    assert!(e.contains("upper half"), "{e}");
}

/// The capability list is the device's own claim: a pointer into the standard header, a capability
/// so near the end that its body would leave configuration space, and a cycle are each refused by
/// name, and nothing is written.
#[test]
fn a_malformed_capability_list_is_refused_by_name_and_nothing_is_written() {
    let reg = |cfg: &Cfg, at: u8, v: u32| cfg.words.borrow_mut()[(at / 4) as usize] = v;
    // Pointer into the standard header (0x10 < 0x40).
    let cfg = Cfg::new(CAP_ID_MSI, 1 << 7 | 1 << 8);
    reg(&cfg, CFG_CAP_PTR, 0x10);
    let e = unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.expect_err("below the header");
    assert!(e.contains("outside the configuration space"), "{e}");
    assert_eq!(cfg.word(CFG_COMMAND), 0x6, "nothing was written");
    // The MSI capability at 0xF8: 20 bytes from there leave the 256-byte space.
    let cfg = Cfg::new(CAP_ID_MSI, 1 << 7 | 1 << 8);
    reg(&cfg, CFG_CAP_PTR, 0xF8);
    reg(&cfg, 0xF8, CAP_ID_MSI as u32 | 1 << 23 | 1 << 24);
    let e = unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.expect_err("body past 0x100");
    assert!(e.contains("does not fit"), "{e}");
    assert_eq!(cfg.word(CFG_COMMAND), 0x6, "nothing was written");
    // The same for MSI-X at 0xF8 (12 bytes).
    let cfg = Cfg::new(CAP_ID_MSIX, TABLE_ENTRIES - 1);
    reg(&cfg, CFG_CAP_PTR, 0xF8);
    reg(&cfg, 0xF8, CAP_ID_MSIX as u32);
    let e = unsafe { enable_msix_entry(&cfg, BDF, 1, ADDR, DATA as u32) }.expect_err("body past");
    assert!(e.contains("does not fit"), "{e}");
    assert_eq!(cfg.word(CFG_COMMAND), 0x6, "nothing was written");
    // A list that loops: the entry's next pointer names itself and its id is not MSI.
    let cfg = Cfg::new(0x01, 0);
    reg(&cfg, CAP, 0x01 | (CAP as u32) << 8);
    let e = unsafe { enable_msi(&cfg, BDF, ADDR, DATA) }.expect_err("a loop");
    assert!(e.contains("does not end"), "{e}");
    assert_eq!(cfg.word(CFG_COMMAND), 0x6, "nothing was written");
}
