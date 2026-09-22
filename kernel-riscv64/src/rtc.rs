//! The goldfish real-time clock on QEMU `virt` (RISC-V): the platform's wall clock (ADR-148).
//!
//! The goldfish RTC holds nanoseconds since the Unix epoch as a 64-bit count read through two
//! 32-bit registers: reading TIME_LOW latches the matching high word, so a LOW-then-HIGH pair is
//! one consistent reading. The part has no identification registers, so the plausibility rule in
//! `kernel_core::clock` is the whole of the check that something answered: a page that is not a
//! clock reads zero or nonsense, and both are refused by name.
//! The peripheral GiB is identity-mapped as one device gigapage (`vm::build_identity`).

use kernel_core::clock::{plausible, ClockRefusal, UnixSeconds, WallClock};

/// Where QEMU `virt` places the goldfish RTC (`/soc/rtc@101000`, `compatible = "google,goldfish-rtc"`).
pub const GOLDFISH_RTC_BASE: usize = 0x0010_1000;

/// TIME_LOW: the low word of the nanosecond count; reading it latches TIME_HIGH.
const TIME_LOW: usize = 0x00;
/// TIME_HIGH: the high word latched by the last TIME_LOW read.
const TIME_HIGH: usize = 0x04;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The platform clock. One instance; nothing here is shared across cores.
pub struct GoldfishRtc {
    base: usize,
}

impl GoldfishRtc {
    pub const fn new() -> Self {
        GoldfishRtc {
            base: GOLDFISH_RTC_BASE,
        }
    }

    fn reg(&self, offset: usize) -> u32 {
        // SAFETY: `base` is the goldfish RTC page QEMU `virt` declares in its device tree, inside
        // the device gigapage the boot identity map covers; `offset` is TIME_LOW or TIME_HIGH,
        // both inside that page. A 32-bit volatile load is the architectural access; the only
        // side effect (TIME_LOW latching TIME_HIGH) is the one this driver relies on.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }
}

impl WallClock for GoldfishRtc {
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
        let low = self.reg(TIME_LOW) as u64;
        let high = self.reg(TIME_HIGH) as u64;
        let nanos = (high << 32) | low;
        plausible((nanos / NANOS_PER_SECOND) as i64)
    }
}
