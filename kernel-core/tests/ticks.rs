//! The tick-to-nanosecond conversion every CPU's clock goes through (`ticks_to_ns_at`).

use kernel_core::ticks_to_ns_at;

/// x86-64's HPET under QEMU runs at 100 MHz. A 64-bit `ticks * 10^9` overflows past 184.47 s of
/// uptime; the clock must keep moving there, or every wait bounded by it becomes unbounded.
#[test]
fn the_clock_keeps_moving_past_the_64_bit_product() {
    const HPET_HZ: u64 = 100_000_000;
    let edge = u64::MAX / 1_000_000_000;
    let before = ticks_to_ns_at(edge, HPET_HZ);
    let after = ticks_to_ns_at(edge + HPET_HZ, HPET_HZ);
    assert_eq!(
        after - before,
        1_000_000_000,
        "one more second of ticks is one more second"
    );
    // A day of a 3 GHz TSC, far past the 6 s where the old product froze.
    const TSC_HZ: u64 = 3_000_000_000;
    let day = 86_400 * TSC_HZ;
    assert_eq!(ticks_to_ns_at(day, TSC_HZ), 86_400 * 1_000_000_000);
    assert_eq!(ticks_to_ns_at(day + 3, TSC_HZ), 86_400 * 1_000_000_000 + 1);
}

#[test]
fn the_conversion_is_exact_at_the_frequencies_the_targets_run() {
    assert_eq!(ticks_to_ns_at(62_500_000, 62_500_000), 1_000_000_000); // aarch64 generic timer
    assert_eq!(ticks_to_ns_at(10_000_000, 10_000_000), 1_000_000_000); // riscv64 timebase
    assert_eq!(
        ticks_to_ns_at(7, 0),
        7,
        "a zero frequency means the counter is already in ns"
    );
}
