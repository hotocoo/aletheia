//! The wall clock: a time this kernel reads for itself (REQ-SEC-TLS-008, ADR-148).
//!
//! ADR-147's verifier takes the time as an argument and refuses a time of zero, because this kernel
//! had no clock. This module is the contract a platform clock must meet, and the proof that the
//! platform's own reading is one the verifier can be handed.
//!
//! ## What a clock reading must be
//!
//! A number of seconds since the Unix epoch, in UTC, that is PLAUSIBLE: not before this tree's own
//! first commit year and not in the next century. A clock that says 1970 is not a clock that
//! happens to be wrong; it is the absence of a clock reporting itself as the epoch, and a verifier
//! handed that number would find every certificate not yet valid. So implausible readings are a
//! named refusal here, before any caller can treat them as a time.
//!
//! Every target reads a different device (PL031 on aarch64, the goldfish RTC on RISC-V, the CMOS
//! RTC on x86-64), and each is written in its own crate. This module knows only the contract.

use crate::trust::{PinnedRoot, TrustRefusal};

/// Seconds since the Unix epoch, UTC.
pub type UnixSeconds = i64;

/// A civil UTC date and time: `(year, month, day, hour, minute, second)`.
pub type Civil = (i64, i64, i64, i64, i64, i64);

/// Why a clock did not give a time. Never zero, never "unknown": each is a fact about the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockRefusal {
    /// No clock device is present, or the one at the expected address is not the expected part.
    Absent,
    /// The device was mid-update for longer than a reader will wait, or two reads never agreed.
    Unsettled,
    /// A reading outside the window this tree considers a time (before 2026, or from 2100 on), or a
    /// civil field outside its range.
    Implausible,
}

/// A source of wall-clock time.
pub trait WallClock {
    /// The current time, or the named reason there is none.
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal>;
}

/// The platform without a clock. Exists so the absence has a type and a refusal rather than a zero.
pub struct NoWallClock;

impl WallClock for NoWallClock {
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
        Err(ClockRefusal::Absent)
    }
}

/// 2026-01-01T00:00:00Z: no reading before this is a time this tree can have been built at.
pub const PLAUSIBLE_FLOOR: UnixSeconds = 1_767_225_600;
/// 2100-01-01T00:00:00Z: no reading from here on is a time this tree will be running at.
pub const PLAUSIBLE_CEILING: UnixSeconds = 4_102_444_800;

/// Accept a reading only inside the plausible window.
pub fn plausible(t: UnixSeconds) -> Result<UnixSeconds, ClockRefusal> {
    if (PLAUSIBLE_FLOOR..PLAUSIBLE_CEILING).contains(&t) {
        Ok(t)
    } else {
        Err(ClockRefusal::Implausible)
    }
}

/// Seconds since the epoch for a civil UTC date and time, by Howard Hinnant's days-from-civil
/// algorithm: no tables, no leap-year cases written out, correct for every date a certificate or
/// a clock can carry. Fields are NOT range-checked here; see [`checked_unix_seconds`].
pub fn unix_seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3_600 + minute * 60 + second
}

/// [`unix_seconds`] for a reading whose fields came from a device: a month of 13 or an hour of 24
/// is refused rather than wrapped into a different, plausible-looking day.
pub fn checked_unix_seconds(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Result<UnixSeconds, ClockRefusal> {
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return Err(ClockRefusal::Implausible);
    }
    Ok(unix_seconds(year, month, day, hour, minute, second))
}

/// The civil UTC date and time `(year, month, day, hour, minute, second)` of a reading, by the
/// inverse of the algorithm above. For printing a time a person can check against their own
/// watch; nothing decides on the output of this.
pub fn civil_from_unix(t: UnixSeconds) -> Civil {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, rem / 3_600, (rem % 3_600) / 60, rem % 60)
}

/// Build a verifier from a clock: the one way a platform's time reaches [`PinnedRoot`]. A clock
/// that refuses gives a verifier that does not exist, never one judging at time zero.
pub fn verifier_at(clock: &dyn WallClock, root: [u8; 32]) -> Result<PinnedRoot, TrustRefusal> {
    match clock.read_utc() {
        Ok(t) => PinnedRoot::new(root, t),
        Err(_) => Err(TrustRefusal::NoClock),
    }
}

/// How far apart two back-to-back reads may be and still be one clock.
const AGREEMENT_WINDOW: i64 = 5;

/// The wall-clock contract, proved on every CPU at boot against that CPU's own clock device.
pub fn clock_suite(
    clock: &dyn WallClock,
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::trust::{
        certificate_message, FIXTURE_NAME, LEAF_FIXTURE, LEAF_KEY_FIXTURE, ROOT_KEY_FIXTURE,
    };
    let mut n: u32 = 0;
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

    // 1 — the platform has a clock, and it reads a plausible time. Zero, the epoch and the far
    //     future are each the absence of a clock wearing a number.
    let first = clock.read_utc();
    check!(
        matches!(first, Ok(t) if (PLAUSIBLE_FLOOR..PLAUSIBLE_CEILING).contains(&t)),
        "clock: the platform clock is present and reads a plausible time, never zero"
    );

    // 2 — a second read never runs backwards and agrees with the first to within a few seconds:
    //     one clock, not two devices or a counter that wrapped.
    {
        let second = clock.read_utc();
        let ok = match (first, second) {
            (Ok(a), Ok(b)) => b >= a && b - a < AGREEMENT_WINDOW,
            _ => false,
        };
        check!(
            ok,
            "clock: two reads never run backwards and agree to within a few seconds"
        );
    }

    // 3 — the absence of a clock is a refusal with a name, and a refusal builds no verifier. The
    //     zero that ADR-147 refuses can never be handed to it from here.
    {
        check!(
            NoWallClock.read_utc() == Err(ClockRefusal::Absent)
                && verifier_at(&NoWallClock, ROOT_KEY_FIXTURE).is_err(),
            "clock: an absent clock is a named refusal, and a refusal builds no verifier"
        );
    }

    // 4 — the platform's own time makes a verifier, and that verifier accepts the pinned fixture:
    //     the first certificate this kernel judges at a time it read itself.
    {
        let mut message = [0u8; 1024];
        let len = certificate_message(&[&LEAF_FIXTURE], &mut message).unwrap_or(0);
        let ok = match verifier_at(clock, ROOT_KEY_FIXTURE) {
            Ok(v) => v.check(FIXTURE_NAME, &message[..len]) == Ok(LEAF_KEY_FIXTURE),
            Err(_) => false,
        };
        check!(
            ok,
            "clock: the platform's own time builds a verifier that accepts the pinned fixture"
        );
    }

    // 5 — implausible readings are refused by name before anyone can call them a time.
    {
        check!(
            plausible(0) == Err(ClockRefusal::Implausible)
                && plausible(946_684_800) == Err(ClockRefusal::Implausible)
                && plausible(PLAUSIBLE_FLOOR - 1) == Err(ClockRefusal::Implausible)
                && plausible(PLAUSIBLE_FLOOR) == Ok(PLAUSIBLE_FLOOR)
                && plausible(PLAUSIBLE_CEILING - 1) == Ok(PLAUSIBLE_CEILING - 1)
                && plausible(PLAUSIBLE_CEILING) == Err(ClockRefusal::Implausible)
                && plausible(i64::MIN) == Err(ClockRefusal::Implausible),
            "clock: the epoch, the year 2000 and the year 2100 are refused as no time at all"
        );
    }

    // 6 — civil dates convert to the seconds they mean and back, leap days and the 2038 boundary
    //     included; the certificate reader and every clock driver share this one conversion.
    {
        let cases: [(Civil, i64); 6] = [
            ((1970, 1, 1, 0, 0, 0), 0),
            ((2000, 2, 29, 0, 0, 0), 951_782_400),
            ((2026, 1, 1, 0, 0, 0), 1_767_225_600),
            ((2036, 1, 1, 0, 0, 0), 2_082_758_400),
            ((2038, 1, 19, 3, 14, 8), 2_147_483_648),
            ((2099, 12, 31, 23, 59, 59), 4_102_444_799),
        ];
        let ok = cases.iter().all(|&((y, mo, d, h, mi, s), t)| {
            unix_seconds(y, mo, d, h, mi, s) == t && civil_from_unix(t) == (y, mo, d, h, mi, s)
        });
        check!(
            ok,
            "clock: civil dates convert to the seconds they mean and back, leap days included"
        );
    }

    // 7 — a device reading with a field out of range is refused, never wrapped into a different
    //     day that happens to look plausible.
    {
        check!(
            checked_unix_seconds(2026, 13, 1, 0, 0, 0) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 1, 32, 0, 0, 0) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 1, 1, 24, 0, 0) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 1, 1, 0, 60, 0) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 1, 1, 0, 0, 60) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 1, 0, 0, 0, 0) == Err(ClockRefusal::Implausible)
                && checked_unix_seconds(2026, 9, 22, 10, 0, 0) == Ok(1_790_071_200),
            "clock: a civil reading with a field out of range is refused rather than wrapped"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that reads a fixed, plausible time.
    struct Fixed(i64);
    impl WallClock for Fixed {
        fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
            plausible(self.0)
        }
    }

    #[test]
    fn the_boot_suite_holds_against_a_plausible_clock() {
        let mut seen = 0;
        let n = clock_suite(&Fixed(1_790_078_400), |_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the clock suite should hold");
        assert_eq!(n, 7);
        assert_eq!(seen, 7);
    }

    #[test]
    fn a_clock_stuck_at_the_epoch_fails_the_first_invariant_by_name() {
        let verdict = clock_suite(&Fixed(0), |_, _, _| {});
        assert_eq!(
            verdict,
            Err((
                1,
                "clock: the platform clock is present and reads a plausible time, never zero"
            ))
        );
    }

    #[test]
    fn the_civil_round_trip_holds_for_every_day_of_a_leap_century() {
        // Every day from 2000-01-01 to 2100-01-01, forwards and back.
        let mut t = unix_seconds(2000, 1, 1, 0, 0, 0);
        let end = unix_seconds(2100, 1, 1, 0, 0, 0);
        let mut days = 0;
        while t < end {
            let (y, m, d, h, mi, s) = civil_from_unix(t);
            assert_eq!(unix_seconds(y, m, d, h, mi, s), t);
            assert!((1..=12).contains(&m) && (1..=31).contains(&d) && h == 0 && mi == 0 && s == 0);
            t += 86_400;
            days += 1;
        }
        assert_eq!(days, 36_525, "a century of 25 leap years is 36,525 days");
    }
}
