//! The kernel's OWN x86-64 address map (ALET-P1-031, REQ-MM-006, ADR-034).
//!
//! WHY THIS EXISTS: long mode requires paging, so OVMF hands this kernel a machine already
//! translating and `ExitBootServices` makes us the owner of *the firmware's* hierarchy — roughly
//! 524 795 of its ~524 799 leaves are writable AND executable. Attribute validation (ALET-P1-008)
//! stops US from creating such a mapping, but it cannot un-map what we inherited: the only fix is
//! for the kernel to build its own map, exactly as the aarch64 and RISC-V backends do when they
//! turn the MMU on. `kernel/src/vm.rs` gets its section bounds from `linker.ld`; a UEFI PE image has
//! no such symbols, so the bounds come from `LoadedImage` (base + size, captured before
//! ExitBootServices) and the image's own PE section table, which carries the same information the
//! linker script does — where text ends, what is writable, what is executable.
//!
//! SHAPE OF THE MAP: identity, so every physical address keeps the value the running kernel, the
//! frame allocator, the SMP trampoline and the framebuffer already assume.
//!   * RAM/MMIO outside the image — 2 MiB huge pages, read/write, NEVER executable.
//!   * every 2 MiB region overlapping the image — split into 4 KiB pages: an executable section is
//!     read-only + executable, a writable section is read/write + NX, everything else (headers,
//!     read-only data, inter-section padding) is read-only + NX, and RAM merely sharing the region
//!     is read/write + NX.
//!
//! No descriptor in the resulting tree is both writable and executable, at any level.
//!
//! SCOPE, HONESTLY (this commit): the tree is BUILT and AUDITED; CR3 still points at OVMF's. A map
//! that is wrong shows up as an audit count and a failed invariant rather than as a triple fault,
//! which is why construction lands before activation. Until CR3 moves, the firmware's inherited
//! leaves remain live and ALET-P1-031 stays open — the boot log reports both trees side by side so
//! the difference is measured, not claimed.

use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

use crate::frames;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use kernel_core::frameown::Owner;

/// 4 KiB page.
const PAGE: usize = 0x1000;
/// 2 MiB region — one PD entry, and the granularity at which the image forces a split.
const BLOCK_2M: usize = 0x20_0000;
/// 1 GiB — one PDPT entry, and the rounding unit for the mapped ceiling.
const GIB: usize = 0x4000_0000;

/// Paging-entry bits used here. Deliberately spelled out rather than pulled from `PageTableFlags`,
/// because this module writes raw `u64` descriptors into frames it owns.
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Physical-address field of any entry, bits 51:12.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// PE section characteristics (`IMAGE_SCN_MEM_*`).
const SCN_EXECUTE: u32 = 0x2000_0000;
const SCN_WRITE: u32 = 0x8000_0000;

/// Most sections a UEFI PE image is expected to carry (`.text`, `.rdata`, `.data`, `.reloc`, …).
/// A larger table is refused rather than silently truncated: [`sections`] reports a count of zero
/// and [`build`] then refuses to build a map at all.
const MAX_SECTIONS: usize = 16;

/// Image base and size, captured from `LoadedImage` while boot services are still alive.
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);
static IMAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
/// Physical address of the PML4 [`build`] produced, or 0 before it runs / on failure.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// One PE section, reduced to what a page map needs.
#[derive(Clone, Copy)]
pub struct Section {
    /// First byte, page-aligned down.
    pub start: usize,
    /// One past the last byte, page-aligned up.
    pub end: usize,
    pub write: bool,
    pub exec: bool,
}

impl Section {
    const EMPTY: Section = Section {
        start: 0,
        end: 0,
        write: false,
        exec: false,
    };
}

/// Record where the firmware loaded this image. MUST be called before `ExitBootServices`, while
/// the `LoadedImage` protocol is still openable; the image memory itself survives the call.
pub fn capture_image(base: usize, size: usize) {
    IMAGE_BASE.store(base, Ordering::Relaxed);
    IMAGE_SIZE.store(size, Ordering::Relaxed);
}

/// `(base, end)` of the loaded image, page-aligned outward. `(0, 0)` if it was never captured.
pub fn image_span() -> (usize, usize) {
    let base = IMAGE_BASE.load(Ordering::Relaxed);
    let size = IMAGE_SIZE.load(Ordering::Relaxed);
    if base == 0 || size == 0 {
        return (0, 0);
    }
    (align_down(base, PAGE), align_up(base + size, PAGE))
}

/// The PML4 this kernel built for itself, or 0 if [`build`] has not run or failed.
pub fn root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// The 2 MiB-aligned span the image split covers — what the dynamic mapping APIs refuse
/// (REQ-MM-006, ALET-P2-032), declared to `kernel_core::vmaddr` by [`crate::vm::addr_plan`].
///
/// The whole aligned span is refused, not just the image, because the same page tables map RAM that
/// merely shares the image's 2 MiB regions. Without it, `map_page` could install a fresh writable
/// page over kernel text — the write-to-code path W^X closes — precisely because the split replaced
/// the huge-page descriptor that had made those addresses undescendable.
pub fn protected_span() -> (usize, usize) {
    let (start, end) = image_span();
    if start == 0 {
        return (0, 0);
    }
    (align_down(start, BLOCK_2M), align_up(end, BLOCK_2M))
}

/// Make the kernel's own map the LIVE one: point CR3 at it (ALET-P1-031).
///
/// Everything the kernel touches keeps its address — the map is identity — so the switch does not
/// move code, stack, page tables, MMIO or the framebuffer. What it does move is the RULE: from the
/// firmware's tree, where a half-million leaves are writable and executable, to one where none is.
///
/// CR4.PGE is cleared across the write and restored after. A global TLB entry survives a CR3 load by
/// definition, and OVMF marks its mappings global — without this, the firmware's permissions would
/// stay live in the TLB for pages this map deliberately narrowed, which is exactly the silent
/// half-switch that makes W^X unprovable.
///
/// Returns whether CR3 now holds our root.
pub fn activate() -> bool {
    use x86_64::registers::control::{Cr3, Cr3Flags, Cr4, Cr4Flags};
    use x86_64::structures::paging::PhysFrame;
    use x86_64::PhysAddr;

    let root = root();
    if root == 0 {
        return false;
    }
    let pge = Cr4::read().contains(Cr4Flags::PAGE_GLOBAL);
    // SAFETY: single-core at this point (the APs are not yet awake) with interrupts enabled but no
    // handler that touches page tables. Clearing PGE only stops entries being treated as global;
    // the CR3 load then flushes them. The new root is a complete identity map covering every
    // address the kernel executes from, reads, writes, or has mapped as a device — proved by the
    // nine `kmap` invariants that ran before this call.
    unsafe {
        if pge {
            Cr4::update(|f| f.remove(Cr4Flags::PAGE_GLOBAL));
        }
        Cr3::write(
            PhysFrame::containing_address(PhysAddr::new(root)),
            Cr3Flags::empty(),
        );
        if pge {
            Cr4::update(|f| f.insert(Cr4Flags::PAGE_GLOBAL));
        }
    }
    Cr3::read().0.start_address().as_u64() == root
}

#[inline]
const fn align_down(a: usize, to: usize) -> usize {
    a & !(to - 1)
}
#[inline]
const fn align_up(a: usize, to: usize) -> usize {
    (a + to - 1) & !(to - 1)
}

/// Parse the loaded image's PE section table into `(sections, count)`.
///
/// The layout is the one every PE file uses: `MZ` at offset 0, the offset of the PE signature at
/// 0x3C, then the COFF header (section count at +6, optional-header size at +20) and the section
/// table right after the optional header. Each 40-byte entry gives a virtual address and size
/// relative to the image base plus the `IMAGE_SCN_MEM_*` characteristics that say whether it is
/// writable or executable — the same facts `linker.ld` exports as `__text_end` / `__rodata_end` on
/// the other targets, read from the image instead of from the linker.
///
/// Returns `(_, 0)` when no image was captured or the headers do not parse, which makes [`build`]
/// refuse rather than map the kernel's own text as writable by default.
pub fn sections() -> ([Section; MAX_SECTIONS], usize) {
    let mut out = [Section::EMPTY; MAX_SECTIONS];
    let base = IMAGE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return (out, 0);
    }
    // SAFETY: the image is loaded, identity-mapped, and stays resident across ExitBootServices
    // (UEFI loader code/data memory is not reclaimed). Only header bytes are read, never written.
    unsafe {
        let rd16 = |off: usize| core::ptr::read_unaligned((base + off) as *const u16);
        let rd32 = |off: usize| core::ptr::read_unaligned((base + off) as *const u32);
        if rd16(0) != 0x5A4D {
            return (out, 0); // not "MZ"
        }
        let pe = rd32(0x3C) as usize;
        if rd32(pe) != 0x0000_4550 {
            return (out, 0); // not "PE\0\0"
        }
        let count = rd16(pe + 6) as usize;
        let opt_size = rd16(pe + 20) as usize;
        if count == 0 || count > MAX_SECTIONS {
            return (out, 0); // unparsable, or more sections than this table models
        }
        let table = pe + 24 + opt_size;
        for (i, s) in out.iter_mut().enumerate().take(count) {
            let e = table + i * 40;
            let va = rd32(e + 12) as usize;
            let vsize = rd32(e + 8) as usize;
            let chars = rd32(e + 36);
            *s = Section {
                start: align_down(base + va, PAGE),
                end: align_up(base + va + vsize, PAGE),
                write: chars & SCN_WRITE != 0,
                exec: chars & SCN_EXECUTE != 0,
            };
        }
        (out, count)
    }
}

/// Leaf flags for the 4 KiB page at `pa` inside the image split.
///
/// An executable section is mapped read-only + executable; a writable section read/write + NX;
/// anything else — headers, read-only data, padding between sections — read-only + NX, except
/// addresses outside the image entirely, which stay read/write + NX like the rest of RAM. No page
/// is ever writable and executable.
fn image_page_flags(pa: usize, secs: &[Section; MAX_SECTIONS], count: usize) -> u64 {
    let (img_start, img_end) = image_span();
    for s in secs.iter().take(count) {
        if pa >= s.start && pa < s.end {
            return if s.exec {
                PRESENT // read-only, executable
            } else if s.write {
                PRESENT | WRITABLE | NO_EXECUTE
            } else {
                PRESENT | NO_EXECUTE // read-only data
            };
        }
    }
    if pa >= img_start && pa < img_end {
        PRESENT | NO_EXECUTE // headers / inter-section padding: readable, nothing more
    } else {
        PRESENT | WRITABLE | NO_EXECUTE // ordinary RAM sharing the image's 2 MiB region
    }
}

/// Highest physical address the map covers: past the end of the highest RAM the UEFI map describes,
/// never below 4 GiB, rounded up to a whole GiB.
///
/// The 4 GiB floor is what covers the platform's memory-mapped devices — LAPIC at 0xFEE0_0000,
/// IOAPIC, HPET, the GOP framebuffer, and any firmware-`RESERVED` range — which the map does not
/// always describe as ranges an OS should map.
///
/// Only entries that describe actual STORAGE raise the ceiling ([`is_ram`]). This is an allowlist,
/// not a denylist, because it decides how large a tree the kernel builds: on QEMU q35 the firmware
/// describes an aperture reaching 1 TiB, and taking the map's raw maximum made the kernel build a
/// 1 TiB identity map (524 283 huge leaves, 1032 table frames = 4 MiB of page tables) to reach
/// device registers it never touches. NOT CLAIMED, therefore: memory-mapped devices ABOVE 4 GiB
/// (64-bit PCI BARs) are unmapped by this tree — nothing this kernel drives lives there, and a
/// driver that needs one must map it explicitly rather than find it pre-mapped.
fn phys_ceiling(map: &MemoryMapOwned) -> usize {
    let mut top = 4 * GIB;
    for d in map.entries() {
        if !is_ram(d.ty) {
            continue;
        }
        let end = (d.phys_start + d.page_count * PAGE as u64) as usize;
        if end > top {
            top = end;
        }
    }
    align_up(top, GIB)
}

/// Does this UEFI memory type describe real storage the kernel may need reachable? Free RAM, the
/// image and anything it allocated, the boot/runtime services regions, and the ACPI/persistent
/// ranges. Everything else — `RESERVED`, `MMIO`, `MMIO_PORT_SPACE`, `UNUSABLE`, `UNACCEPTED` — is
/// either a device aperture, an error region, or firmware's business.
fn is_ram(ty: MemoryType) -> bool {
    matches!(
        ty,
        MemoryType::CONVENTIONAL
            | MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NON_VOLATILE
            | MemoryType::PERSISTENT_MEMORY
    )
}

/// Read one entry of a table this module owns.
///
/// SAFETY: `table` is a frame from our own allocator, identity-mapped in the live map; `index`
/// < 512 by construction of every caller.
#[inline]
unsafe fn read_entry(table: usize, index: usize) -> u64 {
    core::ptr::read_volatile((table + index * 8) as *const u64)
}

/// Write one entry of a table this module owns.
///
/// SAFETY: as [`read_entry`], and the frame is ordinary conventional RAM the firmware map leaves
/// writable — no CR0.WP window is needed, unlike edits to the firmware's own table pages.
#[inline]
unsafe fn write_entry(table: usize, index: usize, value: u64) {
    core::ptr::write_volatile((table + index * 8) as *mut u64, value);
}

/// The next-level table under `table[index]`, allocating a zeroed frame for it on first use.
/// Intermediate entries are present + writable and never USER — the leaf bits, not these, decide
/// what a mapping actually permits.
fn ensure_table(table: usize, index: usize) -> Option<usize> {
    // SAFETY: see `read_entry`.
    let existing = unsafe { read_entry(table, index) };
    if existing & PRESENT != 0 {
        return Some((existing & ADDR_MASK) as usize);
    }
    let frame = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
    // SAFETY: see `write_entry`; `frame` is 4 KiB-aligned so it occupies only the address field.
    unsafe { write_entry(table, index, frame as u64 | PRESENT | WRITABLE) };
    Some(frame)
}

#[inline]
const fn idx(level: usize, pa: usize) -> usize {
    (pa >> (39 - level * 9)) & 0x1FF
}

/// What [`build`] produced, for a boot line that states the map rather than asserting it is fine.
#[derive(Clone, Copy, Default)]
pub struct BuildReport {
    /// Physical bytes the map covers, identity.
    pub covered: usize,
    /// 2 MiB huge-page leaves (RAM and MMIO outside the image).
    pub huge_leaves: usize,
    /// 4 KiB leaves (the image split).
    pub page_leaves: usize,
    /// 2 MiB regions split into 4 KiB pages because the image overlaps them.
    pub split_blocks: usize,
    /// Frames consumed by the tree itself.
    pub tables: usize,
}

/// Build the kernel's own identity map and return `(root, report)`.
///
/// Fails — returning `None`, so the caller reports it rather than running on a half-built tree —
/// when the image bounds were never captured, the PE section table does not parse, or the frame
/// allocator runs dry mid-build. CR3 is NOT touched: the tree is inert until something activates it.
pub fn build(map: &MemoryMapOwned) -> Option<(u64, BuildReport)> {
    let (img_start, img_end) = image_span();
    let (secs, count) = sections();
    if img_start == 0 || count == 0 {
        return None;
    }
    let ceiling = phys_ceiling(map);
    let free_before = frames::free_count();
    let pml4 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
    let mut report = BuildReport {
        covered: ceiling,
        ..BuildReport::default()
    };

    let mut pa = 0usize;
    while pa < ceiling {
        let pdpt = ensure_table(pml4, idx(0, pa))?;
        let pd = ensure_table(pdpt, idx(1, pa))?;
        // Does the image reach into this 2 MiB region? If so it cannot be one permission set.
        if pa < img_end && pa + BLOCK_2M > img_start {
            let pt = ensure_table(pd, idx(2, pa))?;
            for page in (pa..pa + BLOCK_2M).step_by(PAGE) {
                let flags = image_page_flags(page, &secs, count);
                // SAFETY: see `write_entry`; `page` is 4 KiB-aligned.
                unsafe { write_entry(pt, idx(3, page), page as u64 | flags) };
                report.page_leaves += 1;
            }
            report.split_blocks += 1;
        } else {
            // SAFETY: see `write_entry`; `pa` is 2 MiB-aligned, so it fits the address field of a
            // huge-page descriptor with its low bits clear.
            unsafe {
                write_entry(
                    pd,
                    idx(2, pa),
                    pa as u64 | PRESENT | WRITABLE | HUGE | NO_EXECUTE,
                )
            };
            report.huge_leaves += 1;
        }
        pa += BLOCK_2M;
    }

    report.tables = free_before - frames::free_count();
    KERNEL_ROOT.store(pml4 as u64, Ordering::Relaxed);
    Some((pml4 as u64, report))
}

/// Resolve `va` in the tree rooted at `root`, returning `(entry, level)` of the leaf that maps it —
/// level 2 for a 2 MiB huge page, 3 for a 4 KiB page — or `None` if nothing maps it. The gate uses
/// it to assert what a specific kernel address is mapped AS, not merely that the audit found no
/// violations anywhere.
pub fn leaf_for(root: u64, va: usize) -> Option<(u64, usize)> {
    let mut table = root as usize;
    for level in 0..4 {
        // SAFETY: see `read_entry`; every table in this tree is identity-accessible.
        let entry = unsafe { read_entry(table, idx(level, va)) };
        if entry & PRESENT == 0 {
            return None;
        }
        if level == 3 || (level >= 1 && entry & HUGE != 0) {
            return Some((entry, level));
        }
        table = (entry & ADDR_MASK) as usize;
    }
    None
}

/// First address inside an executable section, or `None` if the image declares none (which would
/// itself be a failure the gate reports).
pub fn text_probe() -> Option<usize> {
    let (secs, count) = sections();
    secs.iter().take(count).find(|s| s.exec).map(|s| s.start)
}

/// First address inside a writable section — `.data`/`.bss`, the span that must be RW+NX.
pub fn data_probe() -> Option<usize> {
    let (secs, count) = sections();
    secs.iter()
        .take(count)
        .find(|s| s.write && !s.exec)
        .map(|s| s.start)
}

/// First address inside a read-only, non-executable section (`.rdata`), or `None` if the image has
/// no such section.
///
/// Unlike [`text_probe`] and [`data_probe`], whose `None` is a gate FAILURE, this one is genuinely
/// optional and the gate skips its invariant instead: an image with no read-only section is unusual
/// but not broken, and failing a boot over a section the linker was free not to emit would assert
/// something about the toolchain rather than about the map. Text and data are different — an image
/// claiming neither executable nor writable memory is not this kernel, so those hard-fail.
pub fn rodata_probe() -> Option<usize> {
    let (secs, count) = sections();
    secs.iter()
        .take(count)
        .find(|s| !s.write && !s.exec)
        .map(|s| s.start)
}
