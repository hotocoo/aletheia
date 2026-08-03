//! Virtual-memory address validation — the arch-independent admission check every mapping API runs
//! before it touches a page table (GAPS4 ALET-P1-001).
//!
//! # Why a raw address is untrusted input
//!
//! A page-table walker decodes a fixed number of virtual-address bits: 39 on aarch64 with a 3-level
//! 4 KiB TTBR0 map, 39 for RISC-V Sv39, 48 for x86-64 4-level paging. Bits above that width are not
//! part of the walk. If a mapping API accepts a raw `va` and indexes with the low bits only, two
//! *different* virtual addresses that differ solely above the decoded width silently resolve to the
//! **same** page-table entry: the second map overwrites the first, and an unmap of one address
//! tears down the other's mapping. Same class of defect on the physical side — a `pa` that is not
//! page-aligned has its low bits swallowed by the entry's address mask, so the caller believes it
//! mapped `pa` while the hardware maps `pa & !0xFFF`; a `pa` outside the frame allocator's window
//! maps memory the kernel does not own (firmware tables, MMIO, another agent's frames).
//!
//! None of these fail loudly. They corrupt an address space and surface much later as a wrong-page
//! read — which is why validation belongs at the boundary, fail-closed, and is proved on the host
//! rather than discovered in a VM.
//!
//! # What this module is (and is not)
//!
//! It is pure arithmetic over addresses: no allocation, no architecture registers, no `unsafe`. Each
//! target declares its [`AddrPlan`] once (decoded VA width, whether the ISA demands canonical
//! sign-extension, and the physical window the frame allocator actually owns) and calls
//! [`AddrPlan::validate_map`] / [`AddrPlan::validate_unmap`] on entry to its mapping API. Because
//! the rule set is arch-independent, it is proved once here under `cargo test`
//! (`kernel-core/tests/vmaddr.rs`) and each target's VM gate then proves its own plan is wired in.
//!
//! It is deliberately NOT the frame-ownership model (who owns a frame; double-free defense) nor the
//! W^X invariant checker — those are separate findings (ALET-P1-003 and ALET-P1-007) and remain
//! open in `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md` rather than being implied by this one.

/// Architectural page size every Aletheia target maps at. Block/huge mappings are built by the
/// identity-map bootstrap, not by the dynamic mapping APIs this module guards.
pub const PAGE_SIZE: usize = 4096;

/// Why a mapping request was refused. Each variant names a distinct corruption the check prevents,
/// so a failing target reports which rule it broke instead of a bare `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapFault {
    /// `va` is not 4 KiB-aligned: its low bits would be dropped by the walk, mapping a different
    /// page than the caller named.
    UnalignedVirt,
    /// `pa` is not 4 KiB-aligned: its low bits would be swallowed by the entry's address mask.
    UnalignedPhys,
    /// `va` sets bits the walker never decodes, so it ALIASES a lower address' page-table entry.
    VirtOutOfRange,
    /// `va` is within the decoded width but is not canonically sign-extended, which the ISA
    /// (x86-64) rejects as a general-protection fault rather than a page fault.
    NonCanonicalVirt,
    /// `va` is the null page. Mapping it defeats null-pointer detection for every task in the
    /// address space; Aletheia keeps VA 0 permanently unmapped.
    NullVirt,
    /// `pa` lies outside the physical window the frame allocator owns — firmware tables, MMIO, or
    /// memory belonging to something else.
    PhysOutOfRange,
    /// `va` is inside the span the kernel image occupies (REQ-MM-006). Mapping there would install a
    /// fresh, writable page over kernel TEXT — exactly the write-to-code path W^X closes — and
    /// unmapping there would pull `.data` or the stack out from under the running kernel.
    ProtectedVirt,
    /// `pa` lies INSIDE the frame-allocator window on a request to map **device** memory (REQ-DRV-005).
    /// The device rule is the mirror image of [`MapFault::PhysOutOfRange`]: RAM must not be mapped as
    /// MMIO. Doing so would give one physical page two mappings with different cacheability and side
    /// effects, and would let a "device" mapping reach a frame some task owns — through a path the
    /// ownership model (ADR-030) never sees.
    PhysIsRam,
}

impl MapFault {
    /// Stable short name, for invariant logs on targets that have no formatter.
    pub const fn as_str(self) -> &'static str {
        match self {
            MapFault::UnalignedVirt => "unaligned-va",
            MapFault::UnalignedPhys => "unaligned-pa",
            MapFault::VirtOutOfRange => "va-out-of-range",
            MapFault::NonCanonicalVirt => "va-non-canonical",
            MapFault::NullVirt => "va-null-page",
            MapFault::PhysOutOfRange => "pa-out-of-window",
            MapFault::ProtectedVirt => "va-kernel-image",
            MapFault::PhysIsRam => "pa-is-ram",
        }
    }
}

/// The address-space geometry of one target: what its page-table walker actually decodes, and what
/// physical memory its frame allocator actually owns.
///
/// `va_bits` is the number of virtual-address bits the walk consumes (39 for aarch64 TTBR0 with
/// T0SZ=25 and for RISC-V Sv39; 48 for x86-64 4-level paging). `canonical` selects the rule applied
/// above that width, and the two are genuinely different architectures, not a stylistic choice:
///
/// * `false` — the whole `va_bits` range is one flat window and every higher bit must be zero.
///   That is aarch64 TTBR0: T0SZ=25 makes TTBR0 cover `[0, 2^39)` outright, and TTBR1 (the higher
///   half) is a separate register this kernel disables.
/// * `true` — the ISA requires bits above `va_bits - 1` to repeat bit `va_bits - 1`, splitting the
///   space into a low and a high canonical half with a non-addressable hole between them. That is
///   x86-64 4-level paging (sign-extended from bit 47) **and RISC-V Sv39** (sign-extended from bit
///   38 — Sv39's 39 bits include the sign bit, so its low half is `[0, 2^38)`, not `[0, 2^39)`).
///   An address in the hole faults differently from an unmapped page, so it must be refused here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrPlan {
    va_bits: u32,
    canonical: bool,
    ram_base: usize,
    ram_len: usize,
    protected_start: usize,
    protected_end: usize,
}

impl AddrPlan {
    /// Declare a target's plan. `ram_base`/`ram_len` describe the frame allocator's window; pass
    /// the allocator's own `base()` and `total_count() * PAGE_SIZE` so the check can never drift
    /// from the pool it is protecting.
    pub const fn new(va_bits: u32, canonical: bool, ram_base: usize, ram_len: usize) -> Self {
        Self {
            va_bits,
            canonical,
            ram_base,
            ram_len,
            protected_start: 0,
            protected_end: 0,
        }
    }

    /// Refuse `[start, end)` at the mapping APIs — the span the kernel IMAGE occupies (REQ-MM-006).
    ///
    /// Every target that maps its own image at 4 KiB granularity to make text read-only needs this,
    /// and needs it for the same reason: before the split, those addresses were unreachable because
    /// the level above was a block/huge descriptor and the walkers refuse to descend into one. The
    /// split turns that level into a table, so the refusal has to become explicit or the API would
    /// let a caller map a fresh writable page over kernel text (the write-to-code path W^X closes)
    /// or unmap `.data` out from under the running kernel. Pass the BLOCK-ALIGNED span the split
    /// actually covers, not just the image: the same tables map RAM that merely shares those blocks.
    ///
    /// A zero-length span (the default) protects nothing, which is what a target that has not split
    /// its image wants.
    pub const fn with_protected(self, start: usize, end: usize) -> Self {
        Self {
            protected_start: start,
            protected_end: end,
            ..self
        }
    }

    /// The refused span, `(0, 0)` when the target protects nothing.
    pub const fn protected(&self) -> (usize, usize) {
        (self.protected_start, self.protected_end)
    }

    /// Virtual-address bits the walker decodes.
    pub const fn va_bits(&self) -> u32 {
        self.va_bits
    }

    /// First physical address the frame allocator owns.
    pub const fn ram_base(&self) -> usize {
        self.ram_base
    }

    /// Length in bytes of the physical window the frame allocator owns.
    pub const fn ram_len(&self) -> usize {
        self.ram_len
    }

    /// Is `va` a legal, unambiguous 4 KiB page base in this address space?
    ///
    /// Order matters for diagnosis, not for safety: alignment is reported before range so a caller
    /// passing a byte address inside a valid page learns the real problem.
    pub fn validate_unmap(&self, va: usize) -> Result<(), MapFault> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapFault::UnalignedVirt);
        }
        if va == 0 {
            return Err(MapFault::NullVirt);
        }
        // The page must fit entirely below the top of the address space: a base one page short of
        // 2^64 would wrap when the walker adds the page length.
        if va.checked_add(PAGE_SIZE).is_none() {
            return Err(MapFault::VirtOutOfRange);
        }
        if self.canonical {
            if !is_canonical(va, self.va_bits) {
                return Err(MapFault::NonCanonicalVirt);
            }
        } else if self.va_bits < usize::BITS && (va >> self.va_bits) != 0 {
            // Any bit above the decoded width is an alias of the same entry a lower VA reaches.
            return Err(MapFault::VirtOutOfRange);
        }
        // Last, because it is the only rule about WHAT lives at the address rather than whether the
        // address is well-formed: the kernel's own image is not remappable through this API.
        if self.protected_end > self.protected_start
            && va >= self.protected_start
            && va < self.protected_end
        {
            return Err(MapFault::ProtectedVirt);
        }
        Ok(())
    }

    /// Is `va -> pa` a legal mapping request? Applies every [`validate_unmap`](Self::validate_unmap)
    /// rule to `va`, then requires `pa` to be a page-aligned frame fully inside the allocator's
    /// window.
    pub fn validate_map(&self, va: usize, pa: usize) -> Result<(), MapFault> {
        self.validate_unmap(va)?;
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(MapFault::UnalignedPhys);
        }
        let end = match pa.checked_add(PAGE_SIZE) {
            Some(e) => e,
            None => return Err(MapFault::PhysOutOfRange),
        };
        let window_end = match self.ram_base.checked_add(self.ram_len) {
            Some(e) => e,
            None => return Err(MapFault::PhysOutOfRange),
        };
        if pa < self.ram_base || end > window_end {
            return Err(MapFault::PhysOutOfRange);
        }
        Ok(())
    }

    /// Is `va -> pa` a legal mapping request for **device** memory (MMIO)? Every
    /// [`validate_unmap`](Self::validate_unmap) rule applies to `va`, and `pa` must be page-aligned —
    /// but where [`validate_map`](Self::validate_map) requires the physical page to be INSIDE the
    /// frame-allocator window, this requires it to be OUTSIDE (REQ-DRV-005).
    ///
    /// That inversion is the whole point. A driver legitimately needs to reach physical addresses the
    /// allocator does not own (a PCI BAR sits wherever the firmware put it, above 4 GiB on q35), so the
    /// RAM rule cannot apply. What must NOT happen is the reverse: mapping RAM as MMIO, which would
    /// alias a frame some task owns under different cacheability and side-effect rules, through a path
    /// the ownership model never sees. Neither call can express the other's mistake.
    pub fn validate_map_device(&self, va: usize, pa: usize) -> Result<(), MapFault> {
        self.validate_unmap(va)?;
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(MapFault::UnalignedPhys);
        }
        let end = match pa.checked_add(PAGE_SIZE) {
            Some(e) => e,
            None => return Err(MapFault::PhysOutOfRange),
        };
        let window_end = match self.ram_base.checked_add(self.ram_len) {
            Some(e) => e,
            None => return Err(MapFault::PhysOutOfRange),
        };
        // Any overlap with the allocator window at all — not merely containment — is a refusal.
        if pa < window_end && end > self.ram_base {
            return Err(MapFault::PhysIsRam);
        }
        Ok(())
    }

    /// Do two virtual addresses resolve to the same page-table entry under this plan? Used by the
    /// host proof to state the aliasing property directly, rather than inferring it from a range
    /// check: for a plan whose validation is correct, no two ACCEPTED addresses may alias.
    pub fn aliases(&self, a: usize, b: usize) -> bool {
        a != b && self.decoded(a) == self.decoded(b)
    }

    /// The bits of `va` the walker actually consumes.
    fn decoded(&self, va: usize) -> usize {
        if self.va_bits >= usize::BITS {
            va
        } else {
            va & ((1usize << self.va_bits) - 1)
        }
    }
}

/// Is `va` canonically sign-extended from bit `va_bits - 1` (the x86-64 rule)?
fn is_canonical(va: usize, va_bits: u32) -> bool {
    if va_bits == 0 || va_bits >= usize::BITS {
        return true;
    }
    let shift = usize::BITS - va_bits;
    // Sign-extend from bit `va_bits - 1` and require the result to be unchanged.
    ((((va as i64) << shift) >> shift) as usize) == va
}

#[cfg(test)]
mod tests {
    use super::*;

    // aarch64 TTBR0 (T0SZ=25) and RISC-V Sv39 low half: 39 decoded bits, no high canonical half.
    const LOW39: AddrPlan = AddrPlan::new(39, false, 0x4000_0000, 0x1000_0000);
    // x86-64 4-level paging: 48 decoded bits, sign-extended canonical form.
    const CANON48: AddrPlan = AddrPlan::new(48, true, 0x10_0000, 0x1000_0000);

    #[test]
    fn accepts_an_aligned_page_inside_the_window() {
        assert_eq!(LOW39.validate_map(0x20_0000, 0x4000_1000), Ok(()));
        assert_eq!(CANON48.validate_map(0x20_0000, 0x11_0000), Ok(()));
    }

    #[test]
    fn rejects_a_virtual_address_that_is_not_a_page_base() {
        assert_eq!(
            LOW39.validate_map(0x20_0001, 0x4000_1000),
            Err(MapFault::UnalignedVirt)
        );
    }

    #[test]
    fn rejects_a_physical_address_that_is_not_a_frame_base() {
        assert_eq!(
            LOW39.validate_map(0x20_0000, 0x4000_1001),
            Err(MapFault::UnalignedPhys)
        );
    }

    #[test]
    fn rejects_the_null_page_so_null_dereferences_still_fault() {
        assert_eq!(LOW39.validate_map(0, 0x4000_1000), Err(MapFault::NullVirt));
        assert_eq!(LOW39.validate_unmap(0), Err(MapFault::NullVirt));
    }

    #[test]
    fn rejects_a_virtual_address_above_the_decoded_width() {
        // Bit 39 is not decoded by a 39-bit walk: this address aliases 0x20_0000.
        let aliasing = (1usize << 39) | 0x20_0000;
        assert!(LOW39.aliases(aliasing, 0x20_0000));
        assert_eq!(
            LOW39.validate_map(aliasing, 0x4000_1000),
            Err(MapFault::VirtOutOfRange)
        );
    }

    #[test]
    fn rejects_a_non_canonical_address_on_a_canonical_target() {
        // Inside 48 bits when truncated, but bits 63:47 are neither all-0 nor all-1.
        let non_canonical = 0x0001_8000_0000_0000usize;
        assert_eq!(
            CANON48.validate_map(non_canonical, 0x11_0000),
            Err(MapFault::NonCanonicalVirt)
        );
    }

    #[test]
    fn accepts_both_canonical_halves_on_a_canonical_target() {
        assert_eq!(CANON48.validate_unmap(0x0000_7FFF_FFFF_F000), Ok(()));
        assert_eq!(CANON48.validate_unmap(0xFFFF_8000_0000_0000), Ok(()));
    }

    #[test]
    fn rejects_a_frame_outside_the_allocator_window() {
        assert_eq!(
            LOW39.validate_map(0x20_0000, 0x3FFF_F000),
            Err(MapFault::PhysOutOfRange)
        );
        let past_end = LOW39.ram_base() + LOW39.ram_len();
        assert_eq!(
            LOW39.validate_map(0x20_0000, past_end),
            Err(MapFault::PhysOutOfRange)
        );
        // The last frame fully inside the window is still legal.
        assert_eq!(LOW39.validate_map(0x20_0000, past_end - PAGE_SIZE), Ok(()));
    }

    #[test]
    fn rejects_a_page_base_that_would_wrap_the_address_space() {
        let top = usize::MAX - (PAGE_SIZE - 1);
        assert!(matches!(
            LOW39.validate_unmap(top),
            Err(MapFault::VirtOutOfRange) | Err(MapFault::NonCanonicalVirt)
        ));
    }
}
