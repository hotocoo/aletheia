//! ACPI table discovery — how an x86-64 machine says what hardware it actually has.
//!
//! On this architecture you cannot ask the hardware what exists by reading it: probing an absent
//! device's I/O port returns `0xFF` on some machines, hangs the bus on others, and on a legacy-free
//! platform touching the i8042's ports at all is undefined. The firmware's ACPI tables are the
//! enumeration, and a driver that skips them and pokes the port anyway is guessing.
//!
//! The MADT walk lived inside `smp.rs` because SMP was the only consumer. It has two now — SMP wants
//! `APIC`, the keyboard wants `FACP` (the FADT, whose `IAPC_BOOT_ARCH` word states whether this
//! machine has an 8042 at all) — so the walk lives here, once.
//!
//! **Checksums are verified**, which the MADT walk did not do. Every ACPI system description table
//! sums to zero over its own length; a table that does not is one the firmware did not finish
//! writing, and enumerating hardware from it is worse than finding no table at all.

use core::sync::atomic::{AtomicUsize, Ordering};

/// RSDP physical address, captured from the UEFI configuration table BEFORE `ExitBootServices`.
/// The tables themselves live in ACPI-reclaim memory, which persists and stays identity-mapped, but
/// the *pointer* to them is a boot-services structure — so it is taken while that is still alive.
static RSDP_PA: AtomicUsize = AtomicUsize::new(0);

/// Called from `efi_main` while boot services are alive.
pub fn stash_rsdp(pa: usize) {
    RSDP_PA.store(pa, Ordering::Release);
}

#[inline]
unsafe fn read_u8(pa: usize) -> u8 {
    core::ptr::read_unaligned(pa as *const u8)
}
#[inline]
unsafe fn read_u16(pa: usize) -> u16 {
    core::ptr::read_unaligned(pa as *const u16)
}
#[inline]
unsafe fn read_u32(pa: usize) -> u32 {
    core::ptr::read_unaligned(pa as *const u32)
}
#[inline]
unsafe fn read_u64(pa: usize) -> u64 {
    core::ptr::read_unaligned(pa as *const u64)
}

/// Does the table at `base` sum to zero over `len` bytes, as ACPI requires?
///
/// # Safety
/// `base..base+len` must be readable identity-mapped memory.
unsafe fn checksum_ok(base: usize, len: usize) -> bool {
    // A length that is absurd for a description table is itself corruption; refuse rather than walk
    // megabytes of whatever happens to follow.
    const MAX_TABLE: usize = 1 << 20;
    if !(36..=MAX_TABLE).contains(&len) {
        return false;
    }
    let mut sum: u8 = 0;
    for i in 0..len {
        sum = sum.wrapping_add(read_u8(base + i));
    }
    sum == 0
}

/// Find an ACPI description table by its 4-byte signature. Returns `(physical base, length)`.
///
/// `None` means: no RSDP was captured, the RSDP is not ACPI 2.0+, the XSDT is missing or fails its
/// checksum, the table is absent, or the table fails ITS checksum. All five are the same answer to
/// the caller — this machine does not credibly declare that table — and every one of them is a
/// reason to conclude the hardware is absent rather than to go looking for it anyway.
pub fn find_table(signature: &[u8; 4]) -> Option<(usize, usize)> {
    let rsdp = RSDP_PA.load(Ordering::Acquire);
    if rsdp == 0 {
        return None;
    }
    // SAFETY: the RSDP came from the UEFI config table; ACPI-reclaim memory is identity-mapped and
    // every read below is bounded by a length this function has checksum-validated first.
    unsafe {
        if core::ptr::read_unaligned(rsdp as *const [u8; 8]) != *b"RSD PTR " {
            return None;
        }
        // The first 20 bytes carry their own checksum in every revision; the extended checksum
        // covers the whole structure from revision 2.
        if !rsdp_checksum(rsdp, 20) {
            return None;
        }
        if read_u8(rsdp + 15) < 2 {
            return None; // ACPI 1.0 has no XSDT; every UEFI machine is >= 2
        }
        let rsdp_len = read_u32(rsdp + 20) as usize;
        if rsdp_len < 33 || !rsdp_checksum(rsdp, rsdp_len) {
            return None;
        }
        let xsdt = read_u64(rsdp + 24) as usize;
        if xsdt == 0 || core::ptr::read_unaligned(xsdt as *const [u8; 4]) != *b"XSDT" {
            return None;
        }
        let xsdt_len = read_u32(xsdt + 4) as usize;
        if !checksum_ok(xsdt, xsdt_len) {
            return None;
        }
        let mut off = 36; // past the 36-byte SDT header: an array of 8-byte table pointers
        while off + 8 <= xsdt_len {
            let table = read_u64(xsdt + off) as usize;
            if table != 0 && core::ptr::read_unaligned(table as *const [u8; 4]) == *signature {
                let len = read_u32(table + 4) as usize;
                return if checksum_ok(table, len) {
                    Some((table, len))
                } else {
                    None
                };
            }
            off += 8;
        }
        None
    }
}

/// The RSDP is not an SDT — it has its own two-part checksum and no `length` at offset 4.
///
/// # Safety
/// `base..base+len` must be readable.
unsafe fn rsdp_checksum(base: usize, len: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..len {
        sum = sum.wrapping_add(read_u8(base + i));
    }
    sum == 0
}

/// What the FADT's `IAPC_BOOT_ARCH` word says this machine has. `None` when the field does not
/// exist — FADT revision 1 (ACPI 1.0) predates it.
///
/// The distinction matters and is not pedantry: **absent is not the same as zero.** A zero word
/// means "the firmware states there is no 8042"; an absent word means "the firmware is too old to
/// have an opinion", and on those machines an 8042 is universally present. Collapsing the two would
/// make every ACPI 1.0 machine keyboardless.
pub fn iapc_boot_arch() -> Option<u16> {
    let (fadt, len) = find_table(b"FACP")?;
    // SAFETY: `find_table` validated the length against the table's own checksum.
    unsafe {
        let revision = read_u8(fadt + 8);
        if revision < 3 || len < 111 {
            return None; // IAPC_BOOT_ARCH is at offset 109, ACPI 2.0+ / FADT revision 3+
        }
        Some(read_u16(fadt + 109))
    }
}

/// `IAPC_BOOT_ARCH` bit 1 — "8042 present". Set means the machine really has (or the firmware
/// emulates) a PS/2 controller at ports 0x60/0x64.
const IAPC_8042: u16 = 1 << 1;

/// Does this machine declare an i8042 keyboard controller?
///
/// Fail-safe rather than fail-closed, and the direction is chosen deliberately: an absent FADT field
/// means an ACPI 1.0 machine, where the controller is universally present, so the answer is yes and
/// the driver's own self-test becomes the real gate. A firmware that DOES have an opinion is
/// believed either way — on a legacy-free platform the ports may not be wired at all, and probing
/// them is undefined behavior on the bus rather than a read that returns nothing.
pub fn declares_i8042() -> bool {
    match iapc_boot_arch() {
        Some(w) => w & IAPC_8042 != 0,
        None => true,
    }
}

/// Human-readable provenance for the boot log — which of the three answers this machine gave.
pub fn i8042_provenance() -> &'static str {
    match iapc_boot_arch() {
        Some(w) if w & IAPC_8042 != 0 => "ACPI FADT IAPC_BOOT_ARCH declares 8042 present",
        Some(_) => "ACPI FADT IAPC_BOOT_ARCH declares NO 8042 (legacy-free platform)",
        None => "no FADT IAPC_BOOT_ARCH field (pre-ACPI-2.0) — 8042 assumed, self-test decides",
    }
}
