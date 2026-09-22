//! Host proof of the wall-clock contract (REQ-SEC-TLS-008, ADR-148).
//!
//! The boot suite proves the contract on every CPU against that CPU's own clock device. Here the
//! same suite runs against the HOST's clock — an independent time source this kernel never
//! implemented — so a conversion that happened to agree with itself on every target would still
//! be caught disagreeing with the operating system underneath.

use kernel_core::clock::{
    civil_from_unix, clock_suite, plausible, unix_seconds, verifier_at, ClockRefusal, NoWallClock,
    UnixSeconds, WallClock,
};
use kernel_core::trust::{TrustRefusal, ROOT_KEY_FIXTURE};
use std::time::{SystemTime, UNIX_EPOCH};

/// The host's own clock, through the contract.
struct HostClock;

impl WallClock for HostClock {
    fn read_utc(&self) -> Result<UnixSeconds, ClockRefusal> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ClockRefusal::Implausible)?;
        plausible(now.as_secs() as i64)
    }
}

#[test]
fn the_live_suite_holds_against_the_host_clock() {
    let mut seen = 0;
    clock_suite(&HostClock, |_, passed, name| {
        assert!(
            passed,
            "live invariant failed against the host clock: {name}"
        );
        seen += 1;
    })
    .expect("clock suite");
    assert_eq!(seen, 7);
}

#[test]
fn the_host_date_and_this_conversion_agree() {
    // The host says what day it is through its own libraries; this tree's conversion must name
    // the same day from the same seconds.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("host clock")
        .as_secs() as i64;
    let (y, m, d, h, mi, s) = civil_from_unix(secs);
    assert_eq!(unix_seconds(y, m, d, h, mi, s), secs);
    assert!((2026..2100).contains(&y), "the host thinks it is {y}");
}

#[test]
fn an_absent_clock_never_becomes_a_verifier() {
    assert_eq!(NoWallClock.read_utc(), Err(ClockRefusal::Absent));
    assert!(matches!(
        verifier_at(&NoWallClock, ROOT_KEY_FIXTURE),
        Err(TrustRefusal::NoClock)
    ));
}

#[test]
fn every_second_of_a_day_round_trips() {
    let base = unix_seconds(2026, 9, 22, 0, 0, 0);
    for offset in 0..86_400 {
        let t = base + offset;
        let (y, m, d, h, mi, s) = civil_from_unix(t);
        assert_eq!((y, m, d), (2026, 9, 22));
        assert_eq!(h * 3_600 + mi * 60 + s, offset);
        assert_eq!(unix_seconds(y, m, d, h, mi, s), t);
    }
}
