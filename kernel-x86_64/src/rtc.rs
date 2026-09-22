//! The CMOS real-time clock (x86-64): the platform's wall clock (ADR-148).
//!
//! The MC146818-compatible RTC every PC carries, reached through the index port 0x70 and the data
//! port 0x71. It keeps civil time (seconds through year, plus a century byte) in either BCD or
//! binary and either 24-hour or 12-hour form, as its Status B register says, and it must not be
//! read while an update is in progress. So a reading here is: wait for the update flag to clear,
//! take every field, take them all again, and accept only two identical snapshots — then decode
//! by the format the device declares, and hand the civil fields to the range-checked conversion in
//! `kernel_core::clock`. QEMU and VirtualBox both keep this clock in UTC by default.

use kernel_core::clock::{checked_unix_seconds, plausible, ClockRefusal, UnixSeconds, WallClock};
use x86_64::instructions::port::Port;

const INDEX_PORT: u16 = 0x70;
const DATA_PORT: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
/// The century byte QEMU's firmware (and VirtualBox's) keep at the ACPI-declared location.
const REG_CENTURY: u8 = 0x32;

/// Status A bit 7: update in progress; the time registers are not to be read.
const STATUS_A_UIP: u8 = 0x80;
/// Status B bit 1: 24-hour mode (clear = 12-hour with the PM flag in bit 7 of the hour).
const STATUS_B_24H: u8 = 0x02;
/// Status B bit 2: binary fields (clear = BCD).
const STATUS_B_BINARY: u8 = 0x04;
const HOUR_PM: u8 = 0x80;

/// How many polls of the update flag before the clock is declared unsettled. An update takes
/// under two milliseconds once a second; a device that never clears the flag is not a clock.
const UIP_POLL_BUDGET: u32 = 1_000_000;
/// How many snapshot pairs to try before declaring the reading unsettled.
const SNAPSHOT_BUDGET: u32 = 8;

/// One reading of every field this driver decodes, in the raw form the device gave.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    seconds: u8,
    minutes: u8,
    hours: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
    status_b: u8,
}

/// The platform clock. Ports are constructed per access; nothing here is shared across cores.
pub struct CmosRtc;

impl CmosRtc {
    pub const fn new() -> Self {
        CmosRtc
    }

    fn read(&self, register: u8) -> u8 {
        let mut index = Port::<u8>::new(INDEX_PORT);
        let mut data = Port::<u8>::new(DATA_PORT);
        // SAFETY: 0x70/0x71 are the CMOS/RTC index and data ports on every PC-compatible
        // platform; writing the register index (bit 7 clear, so NMI stays as it was) followed by
        // one data read is the architectural access sequence, reads only the RTC/CMOS byte
        // selected, and changes nothing but the index latch this driver itself owns.
        unsafe {
            index.write(register);
            data.read()
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            seconds: self.read(REG_SECONDS),
            minutes: self.read(REG_MINUTES),
            hours: self.read(REG_HOURS),
            day: self.read(REG_DAY),
            month: self.read(REG_MONTH),
            year: self.read(REG_YEAR),
            century: self.read(REG_CENTURY),
            status_b: self.read(REG_STATUS_B),
        }
    }

    fn wait_settled(&self) -> Result<(), ClockRefusal> {
        for _ in 0..UIP_POLL_BUDGET {
            if self.read(REG_STATUS_A) & STATUS_A_UIP == 0 {
                return Ok(());
            }
        }
        Err(ClockRefusal::Unsettled)
    }

    /// Two identical snapshots taken with no update in progress, or a refusal.
    fn stable_snapshot(&self) -> Result<Snapshot, ClockRefusal> {
        let mut previous: Option<Snapshot> = None;
        for _ in 0..SNAPSHOT_BUDGET {
            self.wait_settled()?;
            let current = self.snapshot();
            if previous == Some(current) {
                return Ok(current);
            }
            previous = Some(current);
        }
        Err(ClockRefusal::Unsettled)
    }
}

/// Decode one field by the format Status B declares.
fn field(raw: u8, status_b: u8) -> u8 {
    if status_b & STATUS_B_BINARY != 0 {
        raw
    } else {
        (raw & 0x0F) + ((raw >> 4) * 10)
    }
}

/// The hour, decoded from either 24-hour or 12-hour-with-PM form.
fn hour(raw: u8, status_b: u8) -> u8 {
    if status_b & STATUS_B_24H != 0 {
        return field(raw, status_b);
    }
    let pm = raw & HOUR_PM != 0;
    let h = field(raw & !HOUR_PM, status_b) % 12;
    if pm {
        h + 12
    } else {
        h
    }
}

impl WallClock for CmosRtc {
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
        let s = self.stable_snapshot()?;
        let b = s.status_b;
        // A century byte of zero is firmware that never wrote one; the only century a two-digit
        // year can mean in this tree's plausible window is the twenty-first.
        let century = match field(s.century, b) {
            0 => 20,
            c => c as i64,
        };
        let t = checked_unix_seconds(
            century * 100 + field(s.year, b) as i64,
            field(s.month, b) as i64,
            field(s.day, b) as i64,
            hour(s.hours, b) as i64,
            field(s.minutes, b) as i64,
            field(s.seconds, b) as i64,
        )?;
        plausible(t)
    }
}
