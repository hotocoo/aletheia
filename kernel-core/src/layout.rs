//! The address-space layout, stated once (REQ-MM-008, ALET-P1-006).
//!
//! Each target knew its own layout as scattered literals: a RAM base here, a peripheral window there, a
//! user region index in a third place. Nothing said what the layout *is*, so nothing could check the
//! properties a layout must have — that regions do not overlap, that the user region cannot reach kernel
//! addresses, that a guard band separates things that grow toward each other. A layout you cannot check
//! is a layout that drifts.
//!
//! This module is the statement and the check. A target declares its regions; [`Layout::validate`]
//! refuses a declaration that violates the rules, so an impossible layout fails at the point of
//! declaration rather than as a mystery fault later. The contract is
//! `docs/INVARIANT-CONTRACTS.md` §INV-LAYOUT.
//!
//! ## KASLR posture (stated, not implied)
//!
//! There is **no** kernel address-space randomization, and this is a deliberate current position rather
//! than an oversight:
//!
//! * every target identity-maps (VA == PA), which is what makes the DMA story simple and auditable — a
//!   driver hands the device the address it writes through (ADR-036/037). Randomizing the kernel's
//!   virtual base breaks that identity, so KASLR is not a knob to flip; it is a different memory model.
//! * KASLR defends against an attacker who can *read* a pointer and use it. Aletheia's containment is a
//!   capability check on every effect, so a leaked kernel pointer is not itself authority to act.
//! * randomization without a guarded, non-identity layout would add entropy to the log and nothing else.
//!
//! What it would take is recorded in the ADR rather than pretended here: a higher-half split (TTBR1 /
//! `KERNEL_BASE`), an offset-mapped physical window for DMA translation, and PIE kernel images.
use core::fmt;

/// A named half-open region of the virtual address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub name: &'static str,
    pub start: usize,
    /// Exclusive end. `start == end` means an empty region (declared but unused on this target).
    pub end: usize,
    /// May unprivileged code reach it?
    pub user: bool,
}

impl Region {
    pub const fn new(name: &'static str, start: usize, end: usize, user: bool) -> Self {
        Region {
            name,
            start,
            end,
            user,
        }
    }
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
    pub const fn contains(&self, va: usize) -> bool {
        va >= self.start && va < self.end
    }
    pub fn overlaps(&self, other: &Region) -> bool {
        !self.is_empty() && !other.is_empty() && self.start < other.end && other.start < self.end
    }
}

/// Why a declared layout was refused. Each names a property a layout must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutFault {
    /// A region ends before it starts.
    Inverted(&'static str),
    /// Two regions overlap: one address would belong to two things.
    Overlap(&'static str, &'static str),
    /// A region is not page-aligned at both ends.
    Unaligned(&'static str),
    /// A user-reachable region and a kernel-only region touch with no guard band between them.
    NoGuardBetween(&'static str, &'static str),
    /// The null page is inside a region: it must stay permanently unmapped.
    IncludesNullPage(&'static str),
}

impl fmt::Display for LayoutFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutFault::Inverted(n) => write!(f, "region {n} ends before it starts"),
            LayoutFault::Overlap(a, b) => write!(f, "regions {a} and {b} overlap"),
            LayoutFault::Unaligned(n) => write!(f, "region {n} is not page-aligned"),
            LayoutFault::NoGuardBetween(a, b) => {
                write!(f, "no guard band between {a} and {b}")
            }
            LayoutFault::IncludesNullPage(n) => write!(f, "region {n} includes the null page"),
        }
    }
}

/// Page size every target uses.
pub const PAGE: usize = 4096;
/// The smallest gap that counts as a guard band between a user region and a kernel region.
pub const GUARD_BAND: usize = PAGE;
/// Regions a layout may declare. Small and fixed so this is `no_std`-friendly and cheap to check.
pub const MAX_REGIONS: usize = 8;

/// A target's declared layout.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub arch: &'static str,
    pub regions: [Region; MAX_REGIONS],
    pub count: usize,
}

impl Layout {
    pub const fn new(arch: &'static str) -> Self {
        Layout {
            arch,
            regions: [Region::new("", 0, 0, false); MAX_REGIONS],
            count: 0,
        }
    }

    /// Declare a region. Silently ignores anything past [`MAX_REGIONS`] — `validate` then sees fewer
    /// regions than intended, which is why the count is asserted by the tests rather than trusted.
    pub const fn with(mut self, r: Region) -> Self {
        if self.count < MAX_REGIONS {
            self.regions[self.count] = r;
            self.count += 1;
        }
        self
    }

    pub fn live(&self) -> impl Iterator<Item = &Region> {
        self.regions[..self.count].iter().filter(|r| !r.is_empty())
    }

    /// Which region contains `va`, if any.
    pub fn region_of(&self, va: usize) -> Option<&Region> {
        self.live().find(|r| r.contains(va))
    }

    /// Check every property a layout must have. Returns the FIRST violation, so a target's boot can
    /// report exactly which rule its declaration broke.
    pub fn validate(&self) -> Result<(), LayoutFault> {
        for r in self.regions[..self.count].iter() {
            if r.end < r.start {
                return Err(LayoutFault::Inverted(r.name));
            }
            if r.is_empty() {
                continue;
            }
            if !r.start.is_multiple_of(PAGE) || !r.end.is_multiple_of(PAGE) {
                return Err(LayoutFault::Unaligned(r.name));
            }
            if r.contains(0) {
                return Err(LayoutFault::IncludesNullPage(r.name));
            }
        }
        // No two regions may claim one address, and a user region must never merely ABUT a kernel one:
        // something that grows (a stack, a heap) would cross the boundary without ever being unmapped.
        for (i, a) in self.regions[..self.count].iter().enumerate() {
            for b in self.regions[i + 1..self.count].iter() {
                if a.overlaps(b) {
                    return Err(LayoutFault::Overlap(a.name, b.name));
                }
                if a.is_empty() || b.is_empty() || a.user == b.user {
                    continue;
                }
                // Whichever region is lower, the gap is the distance from its end to the other's start;
                // overlap was already refused above, so exactly one subtraction is non-zero.
                let gap = b.start.saturating_sub(a.end) + a.start.saturating_sub(b.end);
                if gap < GUARD_BAND {
                    return Err(LayoutFault::NoGuardBetween(a.name, b.name));
                }
            }
        }
        Ok(())
    }

    /// Total bytes declared (for a boot log line).
    pub fn declared_bytes(&self) -> usize {
        self.live().map(|r| r.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound() -> Layout {
        Layout::new("test")
            .with(Region::new("user", 0x1000, 0x4000_0000, true))
            .with(Region::new("kernel-image", 0x4000_1000, 0x4020_0000, false))
    }

    #[test]
    fn a_sound_layout_validates() {
        assert_eq!(sound().validate(), Ok(()));
    }

    #[test]
    fn overlap_unaligned_null_and_missing_guards_are_all_refused() {
        let overlapping = Layout::new("t")
            .with(Region::new("a", 0x1000, 0x3000, false))
            .with(Region::new("b", 0x2000, 0x4000, false));
        assert_eq!(overlapping.validate(), Err(LayoutFault::Overlap("a", "b")));

        let unaligned = Layout::new("t").with(Region::new("a", 0x1001, 0x3000, false));
        assert_eq!(unaligned.validate(), Err(LayoutFault::Unaligned("a")));

        let null = Layout::new("t").with(Region::new("a", 0x0, 0x2000, false));
        assert_eq!(null.validate(), Err(LayoutFault::IncludesNullPage("a")));

        // User region ABUTS a kernel region: no overlap, but nothing between them either.
        let abutting = Layout::new("t")
            .with(Region::new("user", 0x1000, 0x2000, true))
            .with(Region::new("kern", 0x2000, 0x3000, false));
        assert_eq!(
            abutting.validate(),
            Err(LayoutFault::NoGuardBetween("user", "kern"))
        );
    }
}
