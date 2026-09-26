//! The HPET judgement (ADR-200): which ACPI declarations and capability registers make a clock.

use kernel_core::hpet::{base_from_table, counter_hz, MIN_TABLE_LEN};

fn table(space: u8, base: u64) -> Vec<u8> {
    let mut t = vec![0u8; 56];
    t[40] = space;
    t[44..52].copy_from_slice(&base.to_le_bytes());
    t
}

#[test]
fn qemu_q35_declaration_is_accepted() {
    assert_eq!(base_from_table(&table(0, 0xFED0_0000)), Some(0xFED0_0000));
}

#[test]
fn a_declaration_the_kernel_cannot_reach_is_refused() {
    assert_eq!(base_from_table(&table(1, 0xFED0_0000)), None, "I/O space");
    assert_eq!(base_from_table(&table(0, 0)), None, "no base");
    assert_eq!(
        base_from_table(&table(0, 0x1_0000_0000)),
        None,
        "above the 4 GiB map"
    );
    assert_eq!(
        base_from_table(&table(0, 0xFED0_0000)[..MIN_TABLE_LEN - 1]),
        None,
        "short"
    );
}

#[test]
fn qemu_hpet_capabilities_give_100_mhz() {
    // QEMU: period 10 ns (10_000_000 fs), 64-bit counter.
    let caps = (10_000_000u64 << 32) | (1 << 13);
    assert_eq!(counter_hz(caps), Some(100_000_000));
    // A typical chipset HPET: 69.841279 ns (14.318 MHz, truncated).
    assert_eq!(
        counter_hz((69_841_279u64 << 32) | (1 << 13)),
        Some(14_318_179)
    );
}

#[test]
fn a_counter_that_cannot_keep_time_is_refused() {
    assert_eq!(counter_hz(10_000_000u64 << 32), None, "32-bit counter");
    assert_eq!(counter_hz(1 << 13), None, "zero period");
    assert_eq!(
        counter_hz((100_000_001u64 << 32) | (1 << 13)),
        None,
        "period over 100 ns"
    );
    assert_eq!(
        counter_hz((100_000_000u64 << 32) | (1 << 13)),
        Some(10_000_000),
        "exactly 100 ns"
    );
}
