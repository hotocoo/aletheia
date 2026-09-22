//! The PL031 real-time clock on QEMU `virt` (aarch64): the platform's wall clock (ADR-148).
//!
//! The PL031 is a PrimeCell: one 32-bit data register holding seconds since the Unix epoch, and the
//! standard PrimeCell identification registers at the top of its 4 KiB page. This driver reads the
//! identification FIRST and trusts the data register only once the page has said it is a PL031:
//! a fixed address is a fact about QEMU `virt`, and the part behind it is checked, not assumed.
//! The peripheral GiB is identity-mapped with device attributes (`vm::build_identity`), so volatile
//! loads are the whole of the access.

use kernel_core::clock::{plausible, ClockRefusal, UnixSeconds, WallClock};

/// Where QEMU `virt` places the PL031 (DTB-verified: `/pl031@9010000`, `compatible = "arm,pl031"`).
pub const PL031_BASE: usize = 0x0901_0000;

/// RTCDR: the current time, seconds since the epoch.
const DR: usize = 0x000;
/// PeriphID0..3 and PCellID0..3, one byte each in the low bits of consecutive words.
const PERIPH_ID0: usize = 0xFE0;
const PCELL_ID0: usize = 0xFF0;
/// What a PL031 answers: part number 0x031 (PeriphID0 = 0x31, PeriphID1 low nibble = 0x0), and the
/// PrimeCell signature 0xB105F00D spread over PCellID0..3.
const PERIPH_ID0_PL031: u32 = 0x31;
const PERIPH_ID1_PL031_LOW: u32 = 0x0;
const PCELL_ID: [u32; 4] = [0x0D, 0xF0, 0x05, 0xB1];

/// The platform clock. One instance; nothing here is shared across cores.
pub struct Pl031 {
    base: usize,
}

impl Pl031 {
    pub const fn new() -> Self {
        Pl031 { base: PL031_BASE }
    }

    fn reg(&self, offset: usize) -> u32 {
        // SAFETY: `base` is the PL031 page QEMU `virt` declares in its device tree, inside the
        // device-attributed peripheral GiB the boot identity map covers; `offset` is one of the
        // register offsets named above, all inside that 4 KiB page. A 32-bit volatile load is
        // the architectural access, and every register read here is side-effect free.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    /// Whether the page at `base` identifies itself as a PL031.
    pub fn identity_holds(&self) -> bool {
        let pcell_ok = (0..4).all(|i| self.reg(PCELL_ID0 + 4 * i) & 0xFF == PCELL_ID[i]);
        pcell_ok
            && self.reg(PERIPH_ID0) & 0xFF == PERIPH_ID0_PL031
            && self.reg(PERIPH_ID0 + 4) & 0x0F == PERIPH_ID1_PL031_LOW
    }
}

impl WallClock for Pl031 {
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
        if !self.identity_holds() {
            return Err(ClockRefusal::Absent);
        }
        plausible(self.reg(DR) as i64)
    }
}
