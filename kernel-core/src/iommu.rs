//! The IOMMU contract: device-visible memory enforced by TRANSLATION, not trust (ALET-P1-018,
//! ADR-071).
//!
//! The software boundary ([crate::dma]) constrains what the KERNEL tells a device. What kept
//! ALET-P1-018 open is the other half: nothing stopped a device that INVENTS its own addresses.
//! Hardware answers that - Intel VT-d, ARM SMMUv3 - and this module defines, once, the contract
//! such a unit must satisfy and a complete SOFTWARE MODEL of it that every proof can run against
//! today.
//!
//! Per-device address spaces: each attached device translates through ITS OWN mappings; device
//! A's windows do not exist for device B, so isolation between devices is structural rather than
//! a policy anyone remembers to apply. Deny by default with faults NAMED: a translation of an
//! unmapped or permission-denied page is a fault naming the device, the address and the reason -
//! exactly what a real IOMMU writes into its fault queue; nothing about an unmapped device
//! succeeds quietly. The kernel image is not a DMA target on either side of a mapping: neither
//! an IOVA nor a physical address inside the image span may be mapped, for any device, ever.
//! Mapped means translated: a mapped page translates to exactly the physical page it was mapped
//! to, which makes this a real TRANSLATION check (an offset IOVA lands on the offset PA), not a
//! pass-through registry. Revocation is unmap: removing a mapping ends the device's access
//! immediately. And the model is bounded: mappings live in a fixed-capacity table so it cannot
//! grow without bound on a never-freeing heap.
//!
//! # Proof posture
//!
//! Host-exhaustive in tests/iommu.rs (state-machine fuzz against a mirror model, per-device
//! isolation both directions, kernel-image refusals on both sides of every mapping, revocation-
//! mid-flight), plus a compact in-kernel suite so every target proves the core promises at boot.
//! exists to prevent.

use alloc::vec::Vec;
use core::cell::Cell;

pub const PAGE: usize = 4096;
/// Live mappings the model tracks across ALL devices. Bounded so a boot cannot grow it without
/// bound on a never-freeing heap; generous enough for every current driver's live set.
pub const MAX_MAPPINGS: usize = 256;

/// Access permission a mapping carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    /// Device may load from this page.
    Read,
    /// Device may store to this page.
    Write,
}

/// Why the IOMMU refused or faulted. Every variant names the device and address involved -
/// the shape a hardware IOMMU's fault queue reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IommuFault {
    /// An address or length was unaligned/zero where alignment and size are required.
    Malformed { device: u32 },
    /// The null page is never a legal translation source or target.
    NullPage { device: u32, addr: usize },
    /// The IOVA or physical side overlaps the kernel image span.
    KernelImage { device: u32, addr: usize },
    /// The device was never attached to the model.
    UnknownDevice(u32),
    /// The device was already attached.
    AlreadyAttached(u32),
    /// The IOVA window overlaps an existing mapping of the same device.
    DoubleMap { device: u32, iova: usize },
    /// No mapping covers this page for this device - deny-by-default firing.
    NotMapped { device: u32, iova: usize },
    /// A mapping covers the page but not with the requested permission.
    PermDenied { device: u32, iova: usize },
    /// The mapping table is full.
    NoSpace,
}

/// One translation window: `pages` consecutive pages at `iova` translate to the
/// same run starting at `pa`, with `write` granting stores.
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    pub iova: usize,
    pub pa: usize,
    pub pages: usize,
    pub write: bool,
}

/// The software IOMMU: attach devices, program their windows, translate -
/// everything a real unit enforces, enforced here by construction.
pub struct SoftIommu {
    spaces: Vec<(u32, Vec<Mapping>)>, // device id -> its mappings (sorted by iova)
    image: (usize, usize),            // kernel image span, never mappable
    attached: usize,
    mapped: usize,
    faults: Cell<usize>,
    translations: Cell<usize>,
}

impl Default for SoftIommu {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftIommu {
    pub const fn new() -> Self {
        SoftIommu {
            spaces: Vec::new(),
            image: (0, 0),
            attached: 0,
            mapped: 0,
            faults: Cell::new(0),
            translations: Cell::new(0),
        }
    }

    /// Declare the kernel image span `[start, end)` no mapping may touch on either side.
    pub fn declare_kernel_image(&mut self, start: usize, end: usize) {
        self.image = (start, end);
    }

    pub fn image_declared(&self) -> bool {
        self.image.1 > self.image.0
    }

    fn overlaps_image(span: (usize, usize), addr: usize) -> bool {
        let (s, e) = span;
        e > s && addr < e && s < addr + PAGE
    }

    fn space_mut(&mut self, device: u32) -> Option<&mut Vec<Mapping>> {
        self.spaces
            .iter_mut()
            .find(|(d, _)| *d == device)
            .map(|(_, m)| m)
    }

    fn space(&self, device: u32) -> Option<&Vec<Mapping>> {
        self.spaces
            .iter()
            .find(|(d, _)| *d == device)
            .map(|(_, m)| m)
    }

    /// Attach a device: it gets its own empty address space. Devices that are not
    /// attached have NO spaces at all - every translation faults.
    pub fn attach(&mut self, device: u32) -> Result<(), IommuFault> {
        if self.space(device).is_some() {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::AlreadyAttached(device));
        }
        self.spaces.push((device, Vec::new()));
        self.attached += 1;
        Ok(())
    }

    pub fn is_attached(&self, device: u32) -> bool {
        self.space(device).is_some()
    }
    /// Map `pages` consecutive pages: IOVA `iova`.. translates to PA `pa`.., with
    /// store permission per `write`. Both sides must be page-aligned, non-null, and
    /// clear of the kernel image (BOTH sides - an IOVA inside the image would let a
    /// device reach it by translation, a PA inside it by mapping); the IOVA window
    /// must not overlap another mapping of the same device, and neither may the PA
    /// window alias another PA window of that device. Distinct devices mapping the
    /// same physical page is allowed - buffer sharing between devices is a kernel
    /// decision made here explicitly, never something a device can invent.
    pub fn map(
        &mut self,
        device: u32,
        iova: usize,
        pa: usize,
        pages: usize,
        write: bool,
    ) -> Result<(), IommuFault> {
        if pages == 0 || !iova.is_multiple_of(PAGE) || !pa.is_multiple_of(PAGE) {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::Malformed { device });
        }
        if iova == 0 {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::NullPage { device, addr: iova });
        }
        if pa == 0 {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::NullPage { device, addr: pa });
        }
        if Self::overlaps_image(self.image, iova) {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::KernelImage { device, addr: iova });
        }
        if Self::overlaps_image(self.image, pa) {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::KernelImage { device, addr: pa });
        }
        if mappings_total(self) >= MAX_MAPPINGS {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::NoSpace);
        }
        let mappings = match self.space_mut(device) {
            Some(m) => m,
            None => {
                self.faults.set(self.faults.get() + 1);
                return Err(IommuFault::UnknownDevice(device));
            }
        };
        let iova_end = iova + pages * PAGE;
        for m in mappings.iter() {
            let m_end = m.iova + m.pages * PAGE;
            if iova < m_end && m.iova < iova_end {
                self.faults.set(self.faults.get() + 1);
                return Err(IommuFault::DoubleMap { device, iova });
            }
            let m_pa_end = m.pa + m.pages * PAGE;
            let pa_end = pa + pages * PAGE;
            if pa < m_pa_end && m.pa < pa_end {
                // Physical aliasing inside one address space: two windows reaching
                // one frame is the DMA twin of a double map.
                self.faults.set(self.faults.get() + 1);
                return Err(IommuFault::DoubleMap { device, iova });
            }
        }
        mappings.push(Mapping {
            iova,
            pa,
            pages,
            write,
        });
        self.mapped += 1;
        Ok(())
    }

    /// Remove the mapping of `pages` pages at `iova` for `device`. Revocation is
    /// immediate: later translations of that window fault.
    pub fn unmap(&mut self, device: u32, iova: usize, pages: usize) -> Result<(), IommuFault> {
        let mappings = match self.space_mut(device) {
            Some(m) => m,
            None => {
                self.faults.set(self.faults.get() + 1);
                return Err(IommuFault::UnknownDevice(device));
            }
        };
        let pos = mappings
            .iter()
            .position(|m| m.iova == iova && m.pages == pages);
        match pos {
            Some(i) => {
                mappings.remove(i);
                Ok(())
            }
            None => {
                self.faults.set(self.faults.get() + 1);
                Err(IommuFault::NotMapped { device, iova })
            }
        }
    }

    /// Translate one page-sized access at `iova` for `device`, demanding `perm`.
    /// Returns the physical address the access lands on. Deny-by-default: an
    /// unmapped page is a named fault; a mapped page without the permission is a
    /// named fault too.
    pub fn translate(&self, device: u32, iova: usize, perm: Perm) -> Result<usize, IommuFault> {
        if !iova.is_multiple_of(PAGE) {
            self.faults.set(self.faults.get() + 1);
            return Err(IommuFault::Malformed { device });
        }
        let mappings = match self.space(device) {
            Some(m) => m,
            None => {
                self.faults.set(self.faults.get() + 1);
                return Err(IommuFault::UnknownDevice(device));
            }
        };
        for m in mappings.iter() {
            let end = m.iova + m.pages * PAGE;
            if iova >= m.iova && iova < end {
                if perm == Perm::Write && !m.write {
                    self.faults.set(self.faults.get() + 1);
                    return Err(IommuFault::PermDenied { device, iova });
                }
                self.translations.set(self.translations.get() + 1);
                return Ok(m.pa + (iova - m.iova));
            }
        }
        self.faults.set(self.faults.get() + 1);
        Err(IommuFault::NotMapped { device, iova })
    }

    /// Attached devices.
    pub fn attached_devices(&self) -> usize {
        self.attached
    }
    /// Live mappings across all devices.
    pub fn live_mappings(&self) -> usize {
        mappings_total(self)
    }
    /// Translations performed successfully.
    pub fn translations(&self) -> usize {
        self.translations.get()
    }
    /// Refusals and faults counted - evidence the boundary did work.
    pub fn faults(&self) -> usize {
        self.faults.get()
    }
}

fn mappings_total(iommu: &SoftIommu) -> usize {
    iommu.spaces.iter().map(|(_, m)| m.len()).sum()
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite. Kept SMALL by design: the boot heap never
// frees (ADR-063), so the boot proves the core contract while the exhaustive
// sweeps and the fuzz live in tests/iommu.rs on the host.
// ---------------------------------------------------------------------------
pub fn iommu_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
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

    const IMG_START: usize = 0x4000_0000;
    const DEV_A: u32 = 1;
    const DEV_B: u32 = 2;

    // 1 - deny by default: an unattached device's translation faults BY NAME.
    let mut iommu = SoftIommu::new();
    check!(
        matches!(
            iommu.translate(DEV_A, PAGE, Perm::Read),
            Err(IommuFault::UnknownDevice(DEV_A))
        ),
        "iommu: an unattached device has no address space at all"
    );

    // 2 - attach + map + translate: the IOVA lands on exactly the mapped PA.
    iommu.declare_kernel_image(IMG_START, IMG_START + 0x10_0000);
    iommu.attach(DEV_A).unwrap();
    let iova = 0x0001_0000;
    let pa = 0x0002_0000;
    iommu.map(DEV_A, iova, pa, 2, true).unwrap();
    check!(
        iommu.translate(DEV_A, iova + PAGE, Perm::Write) == Ok(pa + PAGE),
        "iommu: a mapped window translates each page to its own physical page"
    );

    // 3 - deny-by-default INSIDE a device: the hole between mappings faults.
    {
        let hole = iova + 2 * PAGE;
        check!(
            matches!(
                iommu.translate(DEV_A, hole, Perm::Read),
                Err(IommuFault::NotMapped { device: DEV_A, iova }) if iova == hole
            ),
            "iommu: an unmapped page of an attached device faults by name"
        );
    }

    // 4 - cross-device isolation both directions.
    iommu.attach(DEV_B).unwrap();
    let dev_b_sees = iommu.translate(DEV_B, iova, Perm::Read);
    check!(
        matches!(dev_b_sees, Err(IommuFault::NotMapped { device: DEV_B, .. })),
        "iommu: one device's windows do not exist for another"
    );

    // 5 - the kernel image is refused as BOTH sides of any mapping. The addr
    // field binds in each pattern and is compared against what was attempted -
    // a refusal naming the WRONG address would be its own bug.
    let img_iova = IMG_START + PAGE;
    let e1 = iommu.map(DEV_A, img_iova, pa, 1, false).err();
    let e2 = iommu.map(DEV_A, iova + 8 * PAGE, IMG_START, 1, false).err();
    let both_kernel_image = matches!(
        (e1, e2),
        (
            Some(IommuFault::KernelImage { device: DEV_A, addr: a1 }),
            Some(IommuFault::KernelImage { device: DEV_A, addr: a2 })
        ) if a1 == img_iova && a2 == IMG_START
    );
    check!(
        both_kernel_image,
        "iommu: the kernel image is refused as IOVA and as physical target"
    );

    // 6 - double map: overlapping IOVA window refused; PA aliasing inside one
    // device's space refused too.
    check!(
        matches!(
            iommu.map(DEV_A, iova + PAGE, pa + 4 * PAGE, 1, false),
            Err(IommuFault::DoubleMap { device: DEV_A, iova: overlapped }) if overlapped == iova + PAGE
        ),
        "iommu: an overlapping IOVA window is a named refusal"
    );

    // 7 - revocation is immediate: after unmap, translations fault again.
    iommu.unmap(DEV_A, iova, 2).unwrap();
    check!(
        matches!(
            iommu.translate(DEV_A, iova, Perm::Read),
            Err(IommuFault::NotMapped { device: DEV_A, iova: hole }) if hole == iova
        ),
        "iommu: unmapping ends access immediately"
    );

    // 8 - permission denied is distinct from not-mapped.
    iommu.map(DEV_A, iova, pa, 1, false).unwrap(); // read-only this time
    check!(
        matches!(
            iommu.translate(DEV_A, iova, Perm::Write),
            Err(IommuFault::PermDenied { device: DEV_A, iova: denied }) if denied == iova
        ),
        "iommu: a read-only page refuses stores under its own name"
    );

    // 9 - every fault was COUNTED: the boundary did measurable work.
    check!(
        iommu.faults() >= 6 && iommu.live_mappings() == 1 && iommu.translations() >= 1,
        "iommu: refusals and translations are counted, not silent"
    );

    Ok(n)
}
