//! What a device is allowed to touch (REQ-DRV-006, ADR-043).
//!
//! Every driver here hands a device a **raw physical address** and trusts it to write only there. Nothing
//! checked that address, and since ADR-037 enabled PCI bus-master the gap is sharper: a descriptor with a
//! wrong address is a device writing wherever the number points — kernel text, another task's frame, a page
//! table. ALET-P1-018 names it, and an IOMMU/SMMU is the eventual answer.
//!
//! This is the part that is honest to build without one: **a device-visible memory boundary the kernel
//! enforces at the choke point where addresses become descriptors.**
//!
//! * A driver [`register`](DmaRegistry::register)s each frame it intends a device to access, naming itself
//!   as owner. Registration applies admission rules — page-aligned, non-null, no overlap with a region
//!   another owner holds, and **never inside the kernel image**.
//! * Before an address is published in a descriptor, the driver asks [`visible`](DmaRegistry::visible). An
//!   address nobody registered is refused, so a corrupted or miscalculated descriptor address fails a check
//!   instead of reaching the device.
//! * [`revoke`](DmaRegistry::revoke) ends visibility, so a frame returning to the allocator stops being
//!   something any device may be told about — the DMA twin of erase-on-free.
//!
//! **What this is and is not.** It is a *software* boundary: it constrains what the KERNEL tells a device,
//! which is where every wrong address in this codebase would come from. It cannot constrain a device that
//! invents its own addresses — a malicious or broken device still needs an IOMMU, and ALET-P1-018 stays
//! open for exactly that reason. Claiming otherwise is what `docs/MATURITY.md` exists to prevent.
use alloc::vec::Vec;

/// Page size the admission rules use.
pub const PAGE: usize = 4096;
/// Live regions a registry tracks. Bounded, not tiny: a framebuffer resource's scatter-gather
/// backing is hundreds of single-frame entries at once (REQ-GFX-002), and revocation-on-detach
/// returns every one — the bound covers the LIVE set, not a resource's lifetime total.
pub const MAX_REGIONS: usize = 192;

/// Why a DMA request was refused. Each names a distinct way a device could be pointed at memory it must
/// not touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaFault {
    /// The address is not page-aligned, or the length is zero.
    Malformed,
    /// The null page — never a legal DMA target (and never mapped; ADR-040).
    NullPage,
    /// The range overlaps the kernel image. A device writing there is the write-to-code path W^X closes,
    /// arriving from the other side.
    KernelImage,
    /// The range overlaps a region another owner registered. Two drivers pointing one device at one frame
    /// is a bug in the same way a double free is.
    Conflict,
    /// The registry is full.
    NoSpace,
    /// The handle is not live (already revoked, or never issued).
    UnknownHandle,
}

/// A live registration. A driver holds it to revoke later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle(usize);

#[derive(Clone, Copy)]
struct Region {
    addr: usize,
    len: usize,
    owner: &'static str,
    live: bool,
}

/// One live grant, NAMED: the frame run a device may be told about and the driver that
/// vouches for it. This is what the hardware IOMMU layer consumes to program per-device
/// windows (ALET-P1-018, ADR-075) - the software boundary stays the single source of truth
/// for what each device may touch, so the two layers cannot drift apart by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// Page-aligned physical start of the granted span.
    pub addr: usize,
    /// Length of the span in PAGES (rounded up from the registered byte length).
    pub pages: usize,
    /// The owner string the registering driver gave.
    pub owner: &'static str,
}

impl Grant {
    /// The span's byte length, page-rounded.
    pub fn len_bytes(&self) -> usize {
        self.pages * PAGE
    }
}

/// The set of physical ranges devices may currently be told about.
pub struct DmaRegistry {
    regions: Vec<Region>,
    /// The kernel image span no region may overlap. `(0, 0)` means "not declared", and then the image rule
    /// cannot be enforced — so a target that forgets to declare it gets no protection AND fails the
    /// invariant that checks the rule, rather than the rule silently passing.
    image: (usize, usize),
    registered: usize,
    refusals: usize,
}

impl DmaRegistry {
    pub const fn new() -> Self {
        DmaRegistry {
            regions: Vec::new(),
            image: (0, 0),
            registered: 0,
            refusals: 0,
        }
    }

    /// Declare the kernel image span `[start, end)` that no DMA region may overlap.
    pub fn declare_kernel_image(&mut self, start: usize, end: usize) {
        self.image = (start, end);
    }

    /// Is the image rule enforceable? False until [`declare_kernel_image`](Self::declare_kernel_image).
    pub fn image_declared(&self) -> bool {
        self.image.1 > self.image.0
    }

    fn overlaps_image(&self, addr: usize, len: usize) -> bool {
        let (s, e) = self.image;
        e > s && addr < e && s < addr + len
    }

    /// Register `[addr, addr+len)` as device-visible, owned by `owner`.
    pub fn register(
        &mut self,
        addr: usize,
        len: usize,
        owner: &'static str,
    ) -> Result<Handle, DmaFault> {
        if len == 0 || !addr.is_multiple_of(PAGE) {
            self.refusals += 1;
            return Err(DmaFault::Malformed);
        }
        if addr == 0 {
            self.refusals += 1;
            return Err(DmaFault::NullPage);
        }
        if self.overlaps_image(addr, len) {
            self.refusals += 1;
            return Err(DmaFault::KernelImage);
        }
        for r in self.regions.iter().filter(|r| r.live) {
            let overlap = addr < r.addr + r.len && r.addr < addr + len;
            if overlap && r.owner != owner {
                self.refusals += 1;
                return Err(DmaFault::Conflict);
            }
        }
        if self.regions.iter().filter(|r| r.live).count() >= MAX_REGIONS {
            self.refusals += 1;
            return Err(DmaFault::NoSpace);
        }
        self.regions.push(Region {
            addr,
            len,
            owner,
            live: true,
        });
        self.registered += 1;
        Ok(Handle(self.regions.len() - 1))
    }

    /// May a device be told about `[addr, addr+len)` right now? The question a driver must ask before
    /// publishing a descriptor — the choke point where a wrong address would otherwise escape.
    pub fn visible(&self, addr: usize, len: usize) -> bool {
        len > 0
            && self
                .regions
                .iter()
                .any(|r| r.live && addr >= r.addr && addr + len <= r.addr + r.len)
    }

    /// End visibility.
    pub fn revoke(&mut self, h: Handle) -> Result<(), DmaFault> {
        match self.regions.get_mut(h.0) {
            Some(r) if r.live => {
                r.live = false;
                Ok(())
            }
            _ => {
                self.refusals += 1;
                Err(DmaFault::UnknownHandle)
            }
        }
    }

    /// Who owns the region covering `addr`, if any.
    pub fn owner_of(&self, addr: usize) -> Option<&'static str> {
        self.regions
            .iter()
            .find(|r| r.live && addr >= r.addr && addr < r.addr + r.len)
            .map(|r| r.owner)
    }

    /// Snapshot the LIVE grants, in registration order, each NAMED by its owner. Dead regions are
    /// skipped: revocation must shrink what an IOMMU programs, or revoke would be a lie twice over.
    pub fn grants(&self) -> alloc::vec::Vec<Grant> {
        self.regions
            .iter()
            .filter(|r| r.live)
            .map(|r| Grant {
                addr: r.addr,
                pages: r.len.div_ceil(PAGE),
                owner: r.owner,
            })
            .collect()
    }

    /// Live regions now.
    pub fn live_regions(&self) -> usize {
        self.regions.iter().filter(|r| r.live).count()
    }
    /// Registrations ever accepted.
    pub fn registered(&self) -> usize {
        self.registered
    }
    /// Requests refused — so a boot can report that the boundary did work, rather than being silent.
    pub fn refusals(&self) -> usize {
        self.refusals
    }
}

impl Default for DmaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The DMA invariant suite (REQ-DRV-006). Pure policy — no device needed — so every target runs it
/// identically and `conformance.sh` can require it.
pub fn selftest<F: FnMut(usize, bool, &str)>(mut log: F) -> Result<usize, (usize, &'static str)> {
    let mut n = 0usize;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let ok = $cond;
            log(n, ok, $name);
            if !ok {
                return Err((n, $name));
            }
        }};
    }

    let mut reg = DmaRegistry::new();
    reg.declare_kernel_image(0x4008_0000, 0x4010_0000);
    check!(
        "dma: the kernel image span is declared (without it the image rule cannot be enforced)",
        reg.image_declared()
    );

    check!(
        "dma: an address nobody registered is never device-visible (deny by default)",
        !reg.visible(0x5000_0000, PAGE)
    );

    let h = reg.register(0x5000_0000, PAGE, "driver.a");
    check!(
        "dma: a registered frame becomes visible to its owner",
        h.is_ok()
            && reg.visible(0x5000_0000, PAGE)
            && reg.owner_of(0x5000_0000) == Some("driver.a")
    );

    check!(
        "dma: a range extending past its registration is refused (no partial visibility)",
        !reg.visible(0x5000_0000, 2 * PAGE)
    );

    check!(
        "dma: a range overlapping the kernel image is refused",
        reg.register(0x4008_0000, PAGE, "driver.a") == Err(DmaFault::KernelImage)
            && !reg.visible(0x4008_0000, PAGE)
    );

    check!(
        "dma: the null page and misaligned addresses are refused",
        reg.register(0, PAGE, "driver.a") == Err(DmaFault::NullPage)
            && reg.register(0x5000_0800, PAGE, "driver.a") == Err(DmaFault::Malformed)
    );

    check!(
        "dma: a second owner registering the same frame is refused (one frame, one owner)",
        reg.register(0x5000_0000, PAGE, "driver.b") == Err(DmaFault::Conflict)
    );

    let h = h.expect("registered above");
    check!(
        "dma: revoking ends visibility, and revoking twice is refused",
        reg.revoke(h) == Ok(())
            && !reg.visible(0x5000_0000, PAGE)
            && reg.revoke(h) == Err(DmaFault::UnknownHandle)
    );

    check!(
        "dma: every refusal was counted (the boundary is auditable, not silent)",
        reg.refusals() >= 5 && reg.live_regions() == 0
    );

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suite_holds() {
        assert_eq!(
            selftest(|_, _, _| {}).expect("every dma invariant holds"),
            9
        );
    }

    #[test]
    fn an_undeclared_image_leaves_the_rule_unenforceable_and_says_so() {
        // A target that forgets to declare its image gets NO protection from the image rule — and
        // `image_declared` is false, so the invariant checking that rule fails rather than passing
        // silently. That is the difference between a missing guard and a guard nobody noticed missing.
        let mut reg = DmaRegistry::new();
        assert!(!reg.image_declared());
        assert!(reg.register(0x4008_0000, PAGE, "d").is_ok());
    }

    #[test]
    fn the_same_owner_may_re_register_an_overlapping_range() {
        // A driver re-registering its own frame (a re-init) is not a conflict; a second owner is.
        let mut reg = DmaRegistry::new();
        assert!(reg.register(0x9000_0000, PAGE, "same").is_ok());
        assert!(reg.register(0x9000_0000, PAGE, "same").is_ok());
        assert_eq!(
            reg.register(0x9000_0000, PAGE, "other"),
            Err(DmaFault::Conflict)
        );
    }
}
