//! What the monitor says it can show (ADR-191): a bounded EDID 1.3/1.4 base-block reader.
//!
//! Every operating system asks the display, not a constant, what resolutions and refresh rates it
//! supports, and runs at the best one. The display answers with EDID (VESA E-EDID): a 128-byte
//! base block holding a manufacturer, a name, and four sources of modes —
//!
//! * **detailed timing descriptors** (bytes 54..126, four 18-byte slots; the first is the
//!   PREFERRED mode): pixel clock, active and blanking extents, from which the refresh rate is
//!   computed exactly (`clock / (h_total * v_total)`);
//! * **standard timings** (bytes 38..54, eight 2-byte codes): width, aspect ratio, refresh;
//! * **established timings** (bytes 35..38): a fixed bitmap of classic VESA modes.
//!
//! Reading is fail-closed: a wrong header, a bad checksum, a version before 1.3 or a truncated
//! block is a named refusal, and a descriptor that does not describe a timing is skipped, never
//! guessed. Nothing allocates. Extension blocks (CTA-861, DisplayID) are not read — named below.

/// Modes one EDID can contribute: 4 detailed + 8 standard + 17 established.
pub const MAX_MODES: usize = 32;

/// Where a mode came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Detailed,
    Standard,
    Established,
}

/// One display mode. Refresh is in millihertz so 59.94 Hz is 59_940, not "60".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub interlaced: bool,
    pub source: Source,
}

impl Mode {
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
    /// Whole hertz, rounded.
    pub fn refresh_hz(&self) -> u32 {
        (self.refresh_mhz + 500) / 1000
    }
}

/// Why a block is not an EDID this reader trusts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdidRefusal {
    TooShort,
    BadHeader,
    BadChecksum,
    UnsupportedVersion { version: u8, revision: u8 },
}

/// A read EDID base block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edid {
    /// Three-letter PNP manufacturer id.
    pub manufacturer: [u8; 3],
    pub product: u16,
    pub version: u8,
    pub revision: u8,
    /// The monitor name descriptor (tag 0xFC), trimmed; empty when absent.
    name: [u8; 13],
    name_len: usize,
    modes: [Mode; MAX_MODES],
    n: usize,
    /// Index into `modes` of the preferred timing (the first detailed descriptor), if any.
    preferred: Option<usize>,
    /// Extension blocks the display announced (not read).
    pub extensions: u8,
}

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// The established-timing bitmap (bytes 35, 36 and bit 7 of 37), in bit order from byte 35 bit 7.
/// `(width, height, refresh_hz, interlaced)`.
const ESTABLISHED: [(u32, u32, u32, bool); 17] = [
    (720, 400, 70, false),
    (720, 400, 88, false),
    (640, 480, 60, false),
    (640, 480, 67, false),
    (640, 480, 72, false),
    (640, 480, 75, false),
    (800, 600, 56, false),
    (800, 600, 60, false),
    (800, 600, 72, false),
    (800, 600, 75, false),
    (832, 624, 75, false),
    (1024, 768, 87, true),
    (1024, 768, 60, false),
    (1024, 768, 70, false),
    (1024, 768, 75, false),
    (1280, 1024, 75, false),
    (1152, 870, 75, false),
];

impl Edid {
    pub fn modes(&self) -> &[Mode] {
        &self.modes[..self.n]
    }
    pub fn preferred(&self) -> Option<Mode> {
        self.preferred.map(|i| self.modes[i])
    }
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    fn push(&mut self, m: Mode) -> Option<usize> {
        if let Some(i) = self.modes[..self.n].iter().position(|x| {
            x.width == m.width
                && x.height == m.height
                && x.interlaced == m.interlaced
                && x.refresh_hz() == m.refresh_hz()
        }) {
            // The same mode from a less exact source adds nothing; a detailed timing's exact
            // refresh wins over a standard code's whole-hertz one.
            if m.source == Source::Detailed {
                self.modes[i] = m;
            }
            return Some(i);
        }
        if self.n == MAX_MODES {
            return None;
        }
        self.modes[self.n] = m;
        self.n += 1;
        Some(self.n - 1)
    }

    /// The best mode that fits the limits: the preferred timing if it fits (the display's own
    /// answer to "what should I run at"), else the largest area, then the highest refresh.
    /// Interlaced modes are chosen only when nothing progressive fits.
    pub fn best_fit(&self, max_w: u32, max_h: u32, max_area: u64) -> Option<Mode> {
        let fits = |m: &Mode| m.width <= max_w && m.height <= max_h && m.area() <= max_area;
        if let Some(p) = self.preferred().filter(|p| fits(p) && !p.interlaced) {
            return Some(p);
        }
        let rank = |m: &Mode| (!m.interlaced, m.area(), m.refresh_mhz);
        self.modes()
            .iter()
            .filter(|m| fits(m))
            .max_by_key(|m| rank(m))
            .copied()
    }

    /// Every mode, largest first, then highest refresh — the order a settings list shows.
    pub fn sorted(&self) -> [Mode; MAX_MODES] {
        let mut out = self.modes;
        out[..self.n]
            .sort_unstable_by(|a, b| (b.area(), b.refresh_mhz).cmp(&(a.area(), a.refresh_mhz)));
        out
    }
}

fn detailed(d: &[u8]) -> Option<Mode> {
    let clock_10khz = u16::from_le_bytes([d[0], d[1]]) as u64;
    if clock_10khz == 0 {
        return None; // a display descriptor, not a timing
    }
    let h_active = d[2] as u32 | ((d[4] as u32 >> 4) << 8);
    let h_blank = d[3] as u32 | ((d[4] as u32 & 0xF) << 8);
    let v_active = d[5] as u32 | ((d[7] as u32 >> 4) << 8);
    let v_blank = d[6] as u32 | ((d[7] as u32 & 0xF) << 8);
    let interlaced = d[17] & 0x80 != 0;
    let h_total = (h_active + h_blank) as u64;
    let v_total = (v_active + v_blank) as u64;
    if h_active == 0 || v_active == 0 || h_total == 0 || v_total == 0 {
        return None;
    }
    // Hz = clock / (h_total * v_total); in millihertz with the clock in 10 kHz units.
    let refresh_mhz = (clock_10khz * 10_000 * 1000 + h_total * v_total / 2) / (h_total * v_total);
    // An interlaced timing's descriptor gives one FIELD's lines.
    let (height, refresh_mhz) = if interlaced {
        (v_active * 2, refresh_mhz)
    } else {
        (v_active, refresh_mhz)
    };
    Some(Mode {
        width: h_active,
        height,
        refresh_mhz: refresh_mhz.min(u32::MAX as u64) as u32,
        interlaced,
        source: Source::Detailed,
    })
}

fn standard(b0: u8, b1: u8, revision: u8) -> Option<Mode> {
    if (b0 == 0x01 && b1 == 0x01) || b0 == 0 {
        return None; // unused slot
    }
    let width = (b0 as u32 + 31) * 8;
    let height = match b1 >> 6 {
        // EDID 1.3 and later: 00 is 16:10 (before 1.3 it meant 1:1).
        0 if revision >= 3 => width * 10 / 16,
        0 => width,
        1 => width * 3 / 4,
        2 => width * 4 / 5,
        _ => width * 9 / 16,
    };
    Some(Mode {
        width,
        height,
        refresh_mhz: ((b1 & 0x3F) as u32 + 60) * 1000,
        interlaced: false,
        source: Source::Standard,
    })
}

/// Read an EDID base block.
pub fn parse(block: &[u8]) -> Result<Edid, EdidRefusal> {
    if block.len() < 128 {
        return Err(EdidRefusal::TooShort);
    }
    let b = &block[..128];
    if b[..8] != HEADER {
        return Err(EdidRefusal::BadHeader);
    }
    if b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) != 0 {
        return Err(EdidRefusal::BadChecksum);
    }
    let (version, revision) = (b[18], b[19]);
    if version != 1 || revision < 3 {
        return Err(EdidRefusal::UnsupportedVersion { version, revision });
    }
    let id = u16::from_be_bytes([b[8], b[9]]);
    let letter = |v: u16| b'A' - 1 + (v & 0x1F) as u8;
    let mut e = Edid {
        manufacturer: [letter(id >> 10), letter(id >> 5), letter(id)],
        product: u16::from_le_bytes([b[10], b[11]]),
        version,
        revision,
        name: [0; 13],
        name_len: 0,
        modes: [Mode {
            width: 0,
            height: 0,
            refresh_mhz: 0,
            interlaced: false,
            source: Source::Established,
        }; MAX_MODES],
        n: 0,
        preferred: None,
        extensions: b[126],
    };
    for slot in 0..4 {
        let d = &b[54 + slot * 18..72 + slot * 18];
        if let Some(m) = detailed(d) {
            let i = e.push(m);
            if slot == 0 {
                e.preferred = i;
            }
        } else if d[0] == 0 && d[1] == 0 && d[3] == 0xFC {
            let text = &d[5..18];
            let end = text.iter().position(|&c| c == 0x0A).unwrap_or(13);
            let t = &text[..end];
            let t_end = t.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1);
            e.name[..t_end].copy_from_slice(&t[..t_end]);
            e.name_len = t_end;
        }
    }
    for i in 0..8 {
        if let Some(m) = standard(b[38 + i * 2], b[39 + i * 2], revision) {
            e.push(m);
        }
    }
    let bits = u32::from(b[35]) << 16 | u32::from(b[36]) << 8 | u32::from(b[37]);
    for (i, &(w, h, hz, il)) in ESTABLISHED.iter().enumerate() {
        if bits & (1 << (23 - i)) != 0 {
            e.push(Mode {
                width: w,
                height: h,
                refresh_mhz: hz * 1000,
                interlaced: il,
                source: Source::Established,
            });
        }
    }
    Ok(e)
}

/// Build a valid EDID base block — the boot suite's fixture, and the host tests'. `preferred` is
/// written as detailed timing 0 from CVT-reduced-blanking-shaped numbers.
pub fn fixture(preferred: (u32, u32, u32), name: &[u8]) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[..8].copy_from_slice(&HEADER);
    // "ALT" = A(1) L(12) T(20)
    let id: u16 = (1 << 10) | (12 << 5) | 20;
    b[8..10].copy_from_slice(&id.to_be_bytes());
    b[10] = 0x84;
    b[11] = 0x01;
    b[18] = 1;
    b[19] = 4;
    // Established: 640x480@60, 800x600@60, 1024x768@60.
    b[35] = 0b0010_0001;
    b[36] = 0b0000_1000;
    // Standard: 1280x1024@60 (5:4), 1920x1080@60 (16:9); the rest unused.
    let std_codes = [(1280u32, 2u8), (1920, 3)];
    for i in 0..8 {
        let (b0, b1) = match std_codes.get(i) {
            Some(&(w, aspect)) => ((w / 8 - 31) as u8, aspect << 6),
            None => (1, 1),
        };
        b[38 + i * 2] = b0;
        b[39 + i * 2] = b1;
    }
    let (w, h, hz) = preferred;
    let (hb, vb) = (160u32, 30u32);
    let clock_10khz = ((w + hb) as u64 * (h + vb) as u64 * hz as u64 / 10_000) as u16;
    let d = &mut b[54..72];
    d[..2].copy_from_slice(&clock_10khz.to_le_bytes());
    d[2] = (w & 0xFF) as u8;
    d[3] = (hb & 0xFF) as u8;
    d[4] = (((w >> 8) as u8) << 4) | ((hb >> 8) as u8 & 0xF);
    d[5] = (h & 0xFF) as u8;
    d[6] = (vb & 0xFF) as u8;
    d[7] = (((h >> 8) as u8) << 4) | ((vb >> 8) as u8 & 0xF);
    let n = &mut b[72..90];
    n[3] = 0xFC;
    let len = name.len().min(13);
    n[5..5 + len].copy_from_slice(&name[..len]);
    if len < 13 {
        n[5 + len] = 0x0A;
        for c in n[6 + len..].iter_mut() {
            *c = b' ';
        }
    }
    let sum = b[..127].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b[127] = 0u8.wrapping_sub(sum);
    b
}

/// The EDID contract, proved at boot on every CPU without a display (a built fixture).
pub fn edid_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n = 0u32;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }
    let block = fixture((2560, 1440, 144), b"ALETHEIA TEST");
    let e = parse(&block);
    check!(
        e.is_ok_and(|e| e.manufacturer == *b"ALT" && e.name() == b"ALETHEIA TEST"),
        "edid: a valid block reads, manufacturer and monitor name included"
    );
    let e = e.unwrap_or_else(|_| unreachable!());
    check!(
        e.preferred()
            .is_some_and(|p| p.width == 2560 && p.height == 1440 && p.refresh_hz() == 144),
        "edid: the preferred mode's refresh is COMPUTED from its pixel clock and totals"
    );
    check!(
        e.modes().len() == 6
            && e.modes()
                .iter()
                .any(|m| (m.width, m.height, m.refresh_hz()) == (1920, 1080, 60))
            && e.modes()
                .iter()
                .any(|m| (m.width, m.height) == (1280, 1024))
            && e.modes().iter().any(|m| (m.width, m.height) == (1024, 768)),
        "edid: detailed, standard and established timings are all read, none twice"
    );
    check!(
        e.best_fit(4096, 4096, 4 * 1024 * 1024)
            .is_some_and(|m| m.width == 2560)
            && e.best_fit(2048, 2048, 4 * 1024 * 1024)
                .is_some_and(|m| m.width == 1920 && m.height == 1080)
            && e.best_fit(800, 600, u64::MAX)
                .is_some_and(|m| m.width == 800),
        "edid: the best mode is the preferred one when it fits, else the largest that does"
    );
    let mut bad = block;
    bad[100] ^= 1;
    let mut old = block;
    old[19] = 2;
    let s: u8 = old[..127].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    old[127] = 0u8.wrapping_sub(s);
    check!(
        parse(&bad) == Err(EdidRefusal::BadChecksum)
            && parse(&block[..127]) == Err(EdidRefusal::TooShort)
            && parse(&[0u8; 128]) == Err(EdidRefusal::BadHeader)
            && parse(&old)
                == Err(EdidRefusal::UnsupportedVersion {
                    version: 1,
                    revision: 2
                }),
        "edid: a bad checksum, a short block, a wrong header and EDID 1.2 are refused by name"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_holds_on_the_host() {
        assert_eq!(edid_suite(|_, p, name| assert!(p, "{name}")).unwrap(), 5);
    }

    #[test]
    fn every_single_bit_flip_is_refused_or_still_reads() {
        let block = fixture((1920, 1080, 60), b"X");
        for i in 0..128 * 8 {
            let mut b = block;
            b[i / 8] ^= 1 << (i % 8);
            // A flipped bit anywhere breaks the checksum: nothing corrupted is ever read.
            assert!(parse(&b).is_err(), "bit {i}");
        }
    }

    #[test]
    fn a_59_94_hz_timing_is_not_rounded_away() {
        // CEA 1920x1080@59.94: 148.35 MHz, 2200 x 1125.
        let mut b = fixture((1920, 1080, 60), b"X");
        let d = &mut b[54..72];
        d[..2].copy_from_slice(&14835u16.to_le_bytes());
        d[3] = (280 & 0xFF) as u8;
        d[4] = (((1920 >> 8) as u8) << 4) | (280 >> 8) as u8;
        d[6] = 45;
        d[7] = (((1080 >> 8) as u8) << 4) | 0;
        let s: u8 = b[..127].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        b[127] = 0u8.wrapping_sub(s);
        let p = parse(&b).unwrap().preferred().unwrap();
        assert_eq!(p.refresh_mhz, 59_939);
        assert_eq!(p.refresh_hz(), 60);
    }
}

/// What the machine learned about its display at boot, kept for the console's `display` (ADR-191).
pub mod resident {
    use super::{Edid, EdidRefusal, Mode};
    use crate::sync::SpinLock;

    /// The display as the console reports it.
    #[derive(Clone, Copy, Debug)]
    pub struct DisplayFacts {
        /// The scanout's current geometry, as GET_DISPLAY_INFO reported it.
        pub current: (u32, u32),
        /// The EDID, or why there is none.
        pub edid: Result<Edid, EdidAbsent>,
        /// The best mode that fits the driver's limits, if any.
        pub best: Option<Mode>,
        /// The mode the desktop is running at (ADR-192 onward); `None` = the fixed default.
        pub running: Option<(u32, u32)>,
    }

    /// Why no EDID was read.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum EdidAbsent {
        /// The device did not offer `VIRTIO_GPU_F_EDID`.
        NotOffered,
        /// The device refused or failed the command.
        DeviceError,
        /// The bytes were not an EDID this reader trusts.
        Refused(EdidRefusal),
    }

    static FACTS: SpinLock<Option<DisplayFacts>> = SpinLock::new(None);

    pub fn record(f: DisplayFacts) {
        if let Some(mut slot) = FACTS.try_lock() {
            *slot = Some(f);
        }
    }

    pub fn set_running(w: u32, h: u32) {
        if let Some(mut slot) = FACTS.try_lock() {
            if let Some(f) = slot.as_mut() {
                f.running = Some((w, h));
            }
        }
    }

    pub fn facts() -> Option<DisplayFacts> {
        *FACTS.try_lock()?
    }
}
