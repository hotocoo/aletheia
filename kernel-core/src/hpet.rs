//! The HPET's declaration and capabilities, judged without touching hardware (ADR-200).
//!
//! x86-64 takes the HPET as its clock when the TSC is not invariant. Whether a firmware's ACPI
//! `HPET` table and the timer's capability register describe a counter this kernel can keep time
//! with is a pure question about bytes, answered here and proved on the host; the target only reads
//! the bytes and the register.

/// Offset of the 64-bit base address in the ACPI `HPET` table: the Generic Address Structure starts
/// at 40, its address at 4 into it.
const BASE_OFFSET: usize = 44;
/// Offset of the GAS address-space id (0 = system memory).
const SPACE_OFFSET: usize = 40;
/// The table must hold at least the header, the block id and the whole GAS.
pub const MIN_TABLE_LEN: usize = 52;
/// GCAP_ID bit 13: the main counter is 64 bits wide.
const COUNT_SIZE_CAP: u64 = 1 << 13;
/// The spec's longest legal tick period, 100 ns, in femtoseconds.
const MAX_PERIOD_FS: u64 = 100_000_000;

/// The register block's physical address, or `None` when the table is short, the block is not in
/// system memory, or it sits at 0 or above 4 GiB (outside what the kernel's map covers).
pub fn base_from_table(table: &[u8]) -> Option<u64> {
    if table.len() < MIN_TABLE_LEN || table[SPACE_OFFSET] != 0 {
        return None;
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&table[BASE_OFFSET..BASE_OFFSET + 8]);
    let base = u64::from_le_bytes(raw);
    (base != 0 && base < 0x1_0000_0000).then_some(base)
}

/// The main counter's frequency from the capability register, or `None` when the counter is only
/// 32 bits wide (it would wrap in minutes with nothing here to extend it) or the period is outside
/// the spec's (0, 100 ns].
pub fn counter_hz(caps: u64) -> Option<u64> {
    let period_fs = caps >> 32;
    if caps & COUNT_SIZE_CAP == 0 || period_fs == 0 || period_fs > MAX_PERIOD_FS {
        return None;
    }
    Some(1_000_000_000_000_000 / period_fs)
}
