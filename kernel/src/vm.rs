//! Virtual memory — the aarch64 MMU brought up on real page tables (PRD P5, mm brick 2/2).
//!
//! WHY THIS MATTERS: until now the kernel ran with the MMU off, in a flat physical space. An
//! operating system isolates programs by giving each its own *virtual* address space, which
//! requires the CPU's translation hardware. This module is the first live translation regime:
//! it builds page tables out of `frames::` frames, identity-maps the running kernel + peripherals
//! so nothing breaks when translation turns on, enables the MMU, and then proves *dynamic*
//! virtual memory — mapping a fresh frame at a brand-new virtual address, writing through the VA,
//! and observing the bytes land in the different physical frame the VA now points at.
//!
//! SCOPE (contract-honest, ADR-010/ADR-019): aarch64 dev backend, TTBR0 only, 4 KiB granule,
//! 39-bit VA, identity map. Higher-half (TTBR1) split, per-process address spaces, and the
//! x86-64/RISC-V MMU backends are the follow-on bricks. Every line here executes under QEMU and
//! is asserted by `scripts/vm-e2e.sh`; a wrong table faults to `exit 102`, never a silent hang.
//!
//! REGISTER SETUP (encodings verified against the ARMv8-A architecture reference, AArch64
//! stage-1 EL1 translation): `MAIR_EL1` attr0 = Normal Write-Back R/W-allocate (0xFF), attr1 =
//! Device-nGnRnE (0x00). `TCR_EL1` = T0SZ 25 (39-bit VA) · 4 KiB granule (TG0=0) · inner-shareable
//! WB-WA walks · TTBR1 walks disabled (EPD1=1, higher-half deferred) · IPS 40-bit. Every block and
//! page descriptor sets the **Access Flag (bit 10)** — an unset AF faults on first access.
use crate::frames;
use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_core::frameown::Owner;
use kernel_core::layout;
use kernel_core::memattr::{self, AttrOps, MemKind, PageAttrs};
use kernel_core::ptreclaim::{self, PathStep, TableOps};
use kernel_core::teardown::{self, SpaceOps, Teardown};
use kernel_core::vmaddr::{self, AddrPlan};

// --- Fixed platform addresses (QEMU virt) -------------------------------------------------
const RAM_BASE: usize = 0x4000_0000;
const UART_BASE: usize = 0x0900_0000;
const BLOCK_2M: usize = 0x20_0000;
const GIB: usize = 0x4000_0000;

// --- Translation-control register values ---------------------------------------------------
/// MAIR_EL1: attr0 = Normal WB R/W-alloc (0xFF), attr1 = Device-nGnRnE (0x00).
const MAIR_VALUE: u64 = 0xFF;
/// TCR_EL1: T0SZ=25 | IRGN0=WBWA | ORGN0=WBWA | SH0=inner | TG0=4KiB (=0b00) | EPD1=1 | IPS=40-bit.
const TCR_VALUE: u64 = 25 | (1 << 8) | (1 << 10) | (0b11 << 12) | (1 << 23) | (0b010 << 32);

// --- Descriptor bit fields (AArch64 stage-1, 4 KiB granule) --------------------------------
const VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 0b11; // table (L1/L2) or page (L3) descriptor
const DESC_BLOCK: u64 = 0b01; // block descriptor (L1/L2)
const AF: u64 = 1 << 10; // Access Flag — MUST be set or first access faults
const SH_INNER: u64 = 0b11 << 8; // inner shareable (Normal cacheable memory)
const AP_RW_EL1: u64 = 0b00 << 6; // EL1 read/write, no EL0 access
const AP_RW_EL0: u64 = 0b01 << 6; // EL1 + EL0 read/write (user-accessible)
const AP_RO_EL0: u64 = 0b11 << 6; // EL1 + EL0 read-only (user-accessible, not writable)
const AP_RO_EL1: u64 = 0b10 << 6; // EL1 read-only, no EL0 access
const PXN: u64 = 1 << 53; // privileged execute-never
const UXN: u64 = 1 << 54; // unprivileged execute-never
const ATTR_NORMAL: u64 = 0 << 2; // AttrIndx = 0 (MAIR attr0, Normal)
const ATTR_DEVICE: u64 = 1 << 2; // AttrIndx = 1 (MAIR attr1, Device)

/// Output-address mask for a next-level table / L3 page: bits[47:12].
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Attributes for a Normal-memory 2 MiB RAM block OUTSIDE the kernel image: read/write, executable
/// at neither level. Blocks overlapping the image are split into 4 KiB pages instead (W^X,
/// REQ-MM-006), so no block descriptor is ever both writable and executable.
const NORMAL_BLOCK: u64 = DESC_BLOCK | ATTR_NORMAL | AP_RW_EL1 | SH_INNER | AF | UXN | PXN;
/// Attributes for a Device-memory 2 MiB block (MMIO — never executable).
const DEVICE_BLOCK: u64 = DESC_BLOCK | ATTR_DEVICE | AP_RW_EL1 | AF | UXN | PXN;
/// The same, as a 4 KiB PAGE descriptor. Needed only for the first 2 MiB of the peripheral window, which
/// is split so VA 0 can be left unmapped (REQ-MM-008): a block cannot have a hole in it.
const DEVICE_PAGE: u64 = DESC_TABLE | ATTR_DEVICE | AP_RW_EL1 | AF | UXN | PXN;
/// Attributes for a Normal-memory 4 KiB page (dynamic mappings): kernel read/write, executable at
/// NEITHER level (UXN | PXN). W^X (REQ-MM-006, ADR-034): a dynamically mapped writable page must
/// never be executable, or any kernel write primitive becomes code execution.
pub const NORMAL_PAGE: u64 = DESC_TABLE | ATTR_NORMAL | AP_RW_EL1 | SH_INNER | AF | UXN | PXN;
/// EL0-executable user code page: EL0 read-only + executable (AP_RO_EL0, UXN clear), EL1
/// execute-never (PXN). Read-ONLY by W^X (REQ-MM-006, ADR-034): a page EL0 can both write and
/// execute is the mapping that turns any user-space write primitive into code execution. The stub
/// is written through the frame's kernel identity address BEFORE it is mapped here, so nothing
/// needs to write it through this mapping.
pub const USER_CODE: u64 = DESC_TABLE | ATTR_NORMAL | AP_RO_EL0 | SH_INNER | AF | PXN;
/// EL0 data/stack page: EL0 RW, never executable at either level.
pub const USER_DATA: u64 = DESC_TABLE | ATTR_NORMAL | AP_RW_EL0 | SH_INNER | AF | UXN | PXN;

// --- Kernel-image bounds, for the page-granular W^X split (REQ-MM-006, ADR-034) -------------
// A 2 MiB block descriptor carries ONE permission set, and the blocks that hold the kernel image
// hold text (executable, read-only) and data/stack/heap (writable, never executable) together — so
// while the identity map used blocks throughout, every block over the image had to be both writable
// and kernel-executable. `linker.ld` exports the section boundaries and `build_identity` splits each
// overlapping block into 4 KiB pages, which is what makes W^X a GLOBAL property of the address space
// instead of a pinned exception (ALET-P1-007).
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __stack_guard: u8;
}

#[inline]
fn page_up(a: usize) -> usize {
    (a + 0xFFF) & !0xFFF
}

/// `(text_start, text_end, rodata_end)` of the running image, the last two rounded UP to a page.
/// `.rodata` and `.data` both carry `ALIGN(0x1000)` in `linker.ld`, so rounding can never merge a
/// text page with a rodata page or a rodata page with a data page.
/// The kernel stack's GUARD page (REQ-MM-007, ALET-P1-012): one page below `__stack_bottom`, reserved
/// by `linker.ld` and deliberately left UNMAPPED, so a stack overflow faults at the first byte past the
/// stack instead of walking into `.bss`.
pub fn stack_guard_page() -> usize {
    core::ptr::addr_of!(__stack_guard) as usize
}

fn image_spans() -> (usize, usize, usize) {
    // Only the ADDRESSES of these linker-defined symbols are taken; their contents are never read,
    // which is why no `unsafe` is needed here (`addr_of!` does not create a reference).
    (
        core::ptr::addr_of!(__text_start) as usize,
        page_up(core::ptr::addr_of!(__text_end) as usize),
        page_up(core::ptr::addr_of!(__rodata_end) as usize),
    )
}

/// Descriptor for the 4 KiB identity page at `pa` inside a split block: kernel text is read-only +
/// EL1-executable, `.rodata` is read-only + execute-never, and everything else (data, bss, stack,
/// heap, and any RAM sharing the block) is writable + execute-never. No page is ever both writable
/// and executable at either privilege level, and none is EL0-accessible.
fn image_page_desc(pa: usize) -> u64 {
    let (text_start, text_end, rodata_end) = image_spans();
    let base = DESC_TABLE | ATTR_NORMAL | SH_INNER | AF;
    if pa >= text_start && pa < text_end {
        base | AP_RO_EL1 | UXN
    } else if pa >= text_end && pa < rodata_end {
        base | AP_RO_EL1 | UXN | PXN
    } else {
        base | AP_RW_EL1 | UXN | PXN
    }
}

/// How many 2 MiB RAM blocks the image's text+rodata span touches — the blocks `build_identity`
/// builds as page tables instead. Derived from the linker symbols, so it tracks the image rather
/// than restating a number that can go stale.
pub fn image_split_blocks() -> usize {
    let (text_start, _, rodata_end) = image_spans();
    let first = (text_start - RAM_BASE) / BLOCK_2M;
    let last = (rodata_end - 1 - RAM_BASE) / BLOCK_2M;
    last + 1 - first
}

/// The block-aligned RAM span that the image split maps with 4 KiB pages — and, declared through
/// [`AddrPlan::with_protected`] in [`addr_plan`], the span the dynamic mapping APIs refuse
/// (REQ-MM-006). The whole aligned span is refused, not just the image, because the same tables also
/// map RAM that merely shares the image's blocks. The refusal ITSELF lives once in
/// `kernel_core::vmaddr` (ALET-P2-032); this target only says where its image is.
fn image_split_span() -> (usize, usize) {
    let (text_start, _, rodata_end) = image_spans();
    (
        text_start & !(BLOCK_2M - 1),
        (rodata_end + BLOCK_2M - 1) & !(BLOCK_2M - 1),
    )
}

#[inline]
unsafe fn read_entry(table: usize, idx: usize) -> u64 {
    core::ptr::read_volatile((table + idx * 8) as *const u64)
}
#[inline]
unsafe fn write_entry(table: usize, idx: usize, val: u64) {
    core::ptr::write_volatile((table + idx * 8) as *mut u64, val);
}

/// The aarch64 descriptor view the shared reclamation policy walks (REQ-MM-003, ADR-031). The only
/// architecture knowledge here is the VALID bit and the 8-byte entry width; the rules — empty
/// tables only, parent cleared before the free, never the root, stop at the first table still in
/// use, restore on a refused free — live once in `kernel_core::ptreclaim`.
struct Tables;

impl TableOps for Tables {
    fn read(&self, table: usize, index: usize) -> u64 {
        // SAFETY: page tables are identity-accessible; `index` < 512 by construction of the walk.
        unsafe { read_entry(table, index) }
    }
    fn write(&mut self, table: usize, index: usize, value: u64) {
        // SAFETY: as above; writing a descriptor (or zero) into a live table is sound.
        unsafe { write_entry(table, index, value) }
    }
    fn is_present(&self, entry: u64) -> bool {
        entry & VALID != 0
    }
    fn free_table(&mut self, table: usize) -> bool {
        // The frame must be one this kernel holds AS A PAGE TABLE — the ownership model refuses a
        // user page or an already-free frame, and reclamation then restores the parent entry.
        frames::free_addr_as(table, Owner::PAGETABLE)
    }
}

/// The same descriptor view, extended with what address-space DESTRUCTION needs (REQ-MM-004,
/// ADR-032): the level count, how to tell a block/page leaf from a table pointer, and how to hand a
/// leaf page back. Every slot of a TTBR0 tree built by `build_identity` belongs to that space, so
/// the default `is_private` (everything) is correct here — unlike x86-64, whose per-process root
/// copies the live kernel's top-level entries.
impl SpaceOps for Tables {
    fn levels(&self) -> usize {
        3
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        if level == 2 {
            entry & VALID != 0 // L3 entries are always pages
        } else {
            entry & 0b11 == DESC_BLOCK // 1 GiB / 2 MiB block descriptors
        }
    }
    fn entry_addr(&self, entry: u64) -> usize {
        (entry & ADDR_MASK) as usize
    }
    fn free_leaf(&mut self, pa: usize) -> bool {
        // USER-owned pages only. The identity map's RAM/device BLOCK descriptors name addresses the
        // allocator either does not own or holds under another tag, so the ownership model refuses
        // them and teardown counts them as skipped — the block mappings are what keep this safe.
        frames::free_addr_as(pa, Owner::USER)
    }
}

/// Destroy the address space rooted at `root`: free its user pages, every table below the root, and
/// the root itself (REQ-MM-004, ADR-032). Returns `None` — refusing outright — when asked to
/// destroy the address space the CPU is currently translating through, which would pull the ground
/// out from under the running kernel.
pub fn destroy_space(root: usize) -> Option<Teardown> {
    if root == active_root() {
        return None;
    }
    Some(teardown::destroy_address_space(root, &mut Tables))
}

/// Decode aarch64 descriptor bits into the arch-neutral permission model, and audit live trees
/// against it (REQ-MM-006, ADR-034).
impl AttrOps for Tables {
    fn levels(&self) -> usize {
        3
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        <Self as SpaceOps>::is_leaf(self, entry, level)
    }
    fn entry_addr(&self, entry: u64) -> usize {
        (entry & ADDR_MASK) as usize
    }
    fn decode(&self, entry: u64, _level: usize) -> PageAttrs {
        let ap = entry & (0b11 << 6);
        let user = ap == AP_RW_EL0 || ap == AP_RO_EL0;
        let write = ap == AP_RW_EL0 || ap == AP_RW_EL1;
        PageAttrs {
            kind: if entry & ATTR_DEVICE != 0 {
                MemKind::Device
            } else {
                MemKind::Normal
            },
            write,
            exec_user: user && entry & UXN == 0,
            exec_kernel: entry & PXN == 0,
            user,
        }
    }
}

/// Audit every mapping reachable from `root` against the W^X and attribute rules (REQ-MM-006).
pub fn audit_attrs(root: usize) -> memattr::AuditReport {
    memattr::audit(root, &Tables)
}

/// Total intermediate tables reclaimed since boot, so the VM gate can prove reclamation actually
/// ran rather than inferring it from a frame count that other allocations also move.
static TABLES_RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// Intermediate page tables freed since boot (REQ-MM-003).
pub fn tables_reclaimed() -> usize {
    TABLES_RECLAIMED.load(Ordering::Relaxed)
}

#[inline]
fn indices(va: usize) -> (usize, usize, usize) {
    ((va >> 30) & 0x1ff, (va >> 21) & 0x1ff, (va >> 12) & 0x1ff)
}

/// Build an identity-mapping table tree from fresh frames and return the L1 root physical
/// address. Maps the peripheral GiB (0..1 GiB, Device) and the 128 MiB of RAM (Normal). Returns
/// `None` if the frame allocator is exhausted. Tables live in RAM, so they remain reachable at
/// their identity address once the MMU is on.
pub fn build_identity() -> Option<usize> {
    let l1 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();

    // L1[0] -> peripheral GiB (0..1 GiB), 2 MiB Device blocks (covers the PL011 UART).
    let l2_dev = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
    // The FIRST block is split into 4 KiB pages so VA 0 can be left with NO descriptor (REQ-MM-008,
    // ALET-P1-006). `vmaddr` already refuses mapping the null page through the mapping APIs, but the boot
    // identity map covered it as device memory — so a kernel null dereference read or WROTE an MMIO
    // register instead of faulting. That is the one address that must never translate.
    let l3_dev0 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
    for j in 1..512 {
        let page = (j * vmaddr::PAGE_SIZE) as u64;
        // SAFETY: `l3_dev0` is a fresh, in-RAM, identity-accessible frame; `j` < 512 entries.
        unsafe { write_entry(l3_dev0, j, page | DEVICE_PAGE) };
    }
    // SAFETY: entry 0 of the device L2 becomes a table pointer; the rest stay 2 MiB blocks.
    unsafe { write_entry(l2_dev, 0, (l3_dev0 as u64) | DESC_TABLE) };
    for i in 1..512 {
        let pa = (i * BLOCK_2M) as u64;
        // SAFETY: `l2_dev` is a fresh, in-RAM, identity-accessible frame; `i` < 512 entries.
        unsafe { write_entry(l2_dev, i, pa | DEVICE_BLOCK) };
    }
    // SAFETY: `l1` fresh table; entry 0 points at the device L2.
    unsafe { write_entry(l1, 0, (l2_dev as u64) | DESC_TABLE) };

    // L1[1] -> the RAM GiB (1..2 GiB). RAM occupies 0x4000_0000..RAM_END => L2 blocks 0..N.
    let l2_ram = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
    let ram_blocks = (frames::RAM_END - RAM_BASE) / BLOCK_2M;
    let (text_start, _, rodata_end) = image_spans();
    let guard = stack_guard_page();
    for i in 0..ram_blocks {
        let pa = RAM_BASE + i * BLOCK_2M;
        if pa <= guard && guard < pa + BLOCK_2M {
            // The block holding the stack guard page becomes a table of 4 KiB pages so that ONE page can
            // be left invalid (REQ-MM-007). A 2 MiB block cannot have a hole in it, which is why the
            // split is necessary rather than merely tidy.
            let l3 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
            for j in 0..512 {
                let page = pa + j * vmaddr::PAGE_SIZE;
                let desc = if page == guard {
                    0 // invalid: no descriptor at all — the guard page has no translation
                } else {
                    page as u64 | image_page_desc(page)
                };
                // SAFETY: `l3` is a fresh, in-RAM, identity-accessible frame; `j` < 512 entries.
                unsafe { write_entry(l3, j, desc) };
            }
            // SAFETY: `l2_ram` fresh in-RAM frame; `i` < ram_blocks <= 512.
            unsafe { write_entry(l2_ram, i, (l3 as u64) | DESC_TABLE) };
        } else if pa < rodata_end && pa + BLOCK_2M > text_start {
            // This block spans kernel text and/or rodata, so it becomes a table of 4 KiB pages: one
            // block descriptor cannot be read-only+executable for text and writable for data at the
            // same time, and being both is exactly the W^X violation (REQ-MM-006, ADR-034).
            let l3 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
            for j in 0..512 {
                let page = pa + j * vmaddr::PAGE_SIZE;
                // SAFETY: `l3` is a fresh, in-RAM, identity-accessible frame; `j` < 512 entries.
                unsafe { write_entry(l3, j, page as u64 | image_page_desc(page)) };
            }
            // SAFETY: `l2_ram` fresh in-RAM frame; `i` < ram_blocks <= 512.
            unsafe { write_entry(l2_ram, i, (l3 as u64) | DESC_TABLE) };
        } else {
            // SAFETY: as above; RAM outside the image keeps its single writable, NX block.
            unsafe { write_entry(l2_ram, i, pa as u64 | NORMAL_BLOCK) };
        }
    }
    // SAFETY: RAM_BASE is in L1 index 1 (0x4000_0000 >> 30 == 1).
    unsafe { write_entry(l1, 1, (l2_ram as u64) | DESC_TABLE) };

    Some(l1)
}

/// This target's address-space geometry, for the arch-independent mapping-API admission check
/// (ALET-P1-001, REQ-MM-001). TTBR0 is configured with T0SZ=25, so the 3-level 4 KiB walk decodes
/// 39 virtual-address bits and there is no high canonical half: a `va` with any bit above 39 set is
/// an ALIAS of a lower address' page-table entry, not a distinct page. The physical window is read
/// from the frame allocator itself, so the check can never drift from the pool it protects.
fn addr_plan() -> AddrPlan {
    let (protected_start, protected_end) = image_split_span();
    AddrPlan::new(
        39,
        false,
        frames::base(),
        frames::total_count() * vmaddr::PAGE_SIZE,
    )
    .with_protected(protected_start, protected_end)
}

/// Software page-table walk: translate `va` to its physical address using `root`, or `None` if
/// unmapped. Used to *assert the map is correct before the MMU is enabled* (turning a would-be
/// silent hang into a testable pre-check) and to verify dynamic map/unmap afterwards.
pub fn translate(root: usize, va: usize) -> Option<usize> {
    let (l1i, l2i, l3i) = indices(va);
    // SAFETY: all reads are of 8-byte-aligned entries inside identity-accessible RAM tables.
    unsafe {
        let l1e = read_entry(root, l1i);
        if l1e & VALID == 0 {
            return None;
        }
        if l1e & 0b11 == DESC_BLOCK {
            let base = (l1e & 0x0000_FFFF_C000_0000) as usize;
            return Some(base | (va & (GIB - 1)));
        }
        let l2t = (l1e & ADDR_MASK) as usize;
        let l2e = read_entry(l2t, l2i);
        if l2e & VALID == 0 {
            return None;
        }
        if l2e & 0b11 == DESC_BLOCK {
            let base = (l2e & 0x0000_FFFF_FFE0_0000) as usize;
            return Some(base | (va & (BLOCK_2M - 1)));
        }
        let l3t = (l2e & ADDR_MASK) as usize;
        let l3e = read_entry(l3t, l3i);
        if l3e & VALID == 0 {
            return None;
        }
        Some(((l3e & ADDR_MASK) as usize) | (va & 0xFFF))
    }
}

/// The leaf descriptor that maps `va` in `root`, with the level it was found at (1 = 1 GiB block,
/// 2 = 2 MiB block, 3 = 4 KiB page), or `None` if `va` is unmapped. `translate` answers *where* a VA
/// points; this answers *how* it is mapped, so a gate can assert the kernel image's GRANULARITY as
/// well as its permissions — a page-granular W^X split is only real if the leaves really are pages.
pub fn leaf_of(root: usize, va: usize) -> Option<(u64, usize)> {
    let (l1i, l2i, l3i) = indices(va);
    // SAFETY: all reads are of 8-byte-aligned entries inside identity-accessible RAM tables.
    unsafe {
        let l1e = read_entry(root, l1i);
        if l1e & VALID == 0 {
            return None;
        }
        if l1e & 0b11 == DESC_BLOCK {
            return Some((l1e, 1));
        }
        let l2e = read_entry((l1e & ADDR_MASK) as usize, l2i);
        if l2e & VALID == 0 {
            return None;
        }
        if l2e & 0b11 == DESC_BLOCK {
            return Some((l2e, 2));
        }
        let l3e = read_entry((l2e & ADDR_MASK) as usize, l3i);
        if l3e & VALID == 0 {
            return None;
        }
        Some((l3e, 3))
    }
}

/// Permissions of the leaf that maps `va`, decoded into the arch-neutral model (REQ-MM-006).
fn attrs_of(root: usize, va: usize) -> Option<PageAttrs> {
    leaf_of(root, va).map(|(entry, level)| Tables.decode(entry, level))
}

/// Map a single 4 KiB `va -> pa` with `flags`, creating intermediate tables from fresh frames as
/// needed. Returns `false` on allocator exhaustion or if an intermediate level is a block (this
/// wave never splits blocks). Invalidates the TLB entry for `va`.
pub fn map_page(root: usize, va: usize, pa: usize, flags: u64) -> bool {
    if addr_plan().validate_map(va, pa).is_err() {
        return false;
    }
    // W^X and attribute admission (REQ-MM-006, ADR-034): a writable+executable page, an executable
    // device mapping, or a user page that is executable at EL1 is refused here rather than created
    // and audited later. `flags` is caller-supplied, so it is untrusted input like `va`/`pa`.
    if Tables.decode(flags, 2).validate().is_err() {
        return false;
    }
    let (l1i, l2i, l3i) = indices(va);
    // SAFETY: table entries are identity-accessible; new tables come from `frames::alloc_zeroed`.
    unsafe {
        let l1e = read_entry(root, l1i);
        let l2t = if l1e & VALID == 0 {
            let t = match frames::alloc_zeroed_as(Owner::PAGETABLE) {
                Some(f) => f.addr(),
                None => return false,
            };
            write_entry(root, l1i, (t as u64) | DESC_TABLE);
            t
        } else if l1e & 0b11 == DESC_BLOCK {
            return false;
        } else {
            (l1e & ADDR_MASK) as usize
        };

        let l2e = read_entry(l2t, l2i);
        let l3t = if l2e & VALID == 0 {
            let t = match frames::alloc_zeroed_as(Owner::PAGETABLE) {
                Some(f) => f.addr(),
                None => return false,
            };
            write_entry(l2t, l2i, (t as u64) | DESC_TABLE);
            t
        } else if l2e & 0b11 == DESC_BLOCK {
            return false;
        } else {
            (l2e & ADDR_MASK) as usize
        };

        write_entry(l3t, l3i, (pa as u64 & ADDR_MASK) | flags);
        tlbi_va(va);
    }
    true
}

/// Unmap the 4 KiB page at `va` (clear its L3 entry) and invalidate its TLB entry. Returns
/// `false` if the page was not present as a 4 KiB mapping.
pub fn unmap_page(root: usize, va: usize) -> bool {
    if addr_plan().validate_unmap(va).is_err() {
        return false;
    }
    let (l1i, l2i, l3i) = indices(va);
    // SAFETY: identity-accessible table walk; writing a zero (invalid) entry is always sound.
    unsafe {
        let l1e = read_entry(root, l1i);
        if l1e & VALID == 0 || l1e & 0b11 == DESC_BLOCK {
            return false;
        }
        let l2t = (l1e & ADDR_MASK) as usize;
        let l2e = read_entry(l2t, l2i);
        if l2e & VALID == 0 || l2e & 0b11 == DESC_BLOCK {
            return false;
        }
        let l3t = (l2e & ADDR_MASK) as usize;
        if read_entry(l3t, l3i) & VALID == 0 {
            return false;
        }
        write_entry(l3t, l3i, 0);
        // REQ-MM-003 / ADR-031: the leaf entry is gone, so any table this emptied is pure leak —
        // still allocated AND still referenced. Reclaim upward (never the root) BEFORE the
        // invalidation below, so one `tlbi vae1` covers the leaf and the ancestors it detaches.
        let path = [
            PathStep::new(root, l1i),
            PathStep::new(l2t, l2i),
            PathStep::new(l3t, l3i),
        ];
        if let Ok(r) = ptreclaim::reclaim_empty_tables(&path, &mut Tables) {
            TABLES_RECLAIMED.fetch_add(r.tables_freed, Ordering::Relaxed);
        }
        tlbi_va(va);
    }
    true
}

/// Invalidate the TLB entry for one VA (page-granular), ordered by barriers.
#[inline]
unsafe fn tlbi_va(va: usize) {
    let page = (va >> 12) as u64;
    asm!(
        "dsb ishst",
        "tlbi vae1, {page}",
        "dsb ish",
        "isb",
        page = in(reg) page,
        options(nostack),
    );
}

/// Broadcast TLB invalidation of one VA to EVERY core in the inner-shareable domain (SMP TLB
/// shootdown, REQ-SMP-004): `tlbi vaae1is` (all-ASID, inner-shareable) + `dsb ish` for completion +
/// `isb`. On aarch64 the `…is` variant is the REAL cross-core mechanism — the hardware propagates
/// the invalidation to all cores and `dsb ish` waits for it to complete everywhere — so an
/// initiator that calls this has already invalidated the stale entry on every core. The
/// `kernel_core::shootdown` barrier layered on top proves the per-core completion is acknowledged
/// before the initiator reclaims (uniform with x86-64/RISC-V, whose invalidation is core-local).
///
/// SAFETY: `tlbi`/`dsb`/`isb` at EL1 are always sound; over-invalidating (VA not mapped) is
/// harmless. The caller must have already published the page-table edit this invalidates.
#[inline]
pub unsafe fn tlbi_va_broadcast(va: usize) {
    let page = (va >> 12) as u64;
    asm!(
        "dsb ishst",
        "tlbi vaae1is, {page}",
        "dsb ish",
        "isb",
        page = in(reg) page,
        options(nostack),
    );
}

/// Core-LOCAL TLB invalidation of one VA (`tlbi vaae1` — no `is`, this core only) + local barriers.
/// This is the per-core half of a software shootdown: the remote core, on servicing a shootdown
/// request, drops its own cached translation. Uniform with the x86-64 `invlpg` and RISC-V
/// `sfence.vma` per-core paths; on aarch64 it is belt-and-suspenders behind the initiator's
/// broadcast, and it is what the `service` callback runs so the acknowledgement means real work.
///
/// SAFETY: `tlbi`/`dsb`/`isb` at EL1 are always sound; over-invalidating is harmless.
#[inline]
pub unsafe fn tlbi_va_local(va: usize) {
    let page = (va >> 12) as u64;
    asm!(
        "dsb nshst",
        "tlbi vaae1, {page}",
        "dsb nsh",
        "isb",
        page = in(reg) page,
        options(nostack),
    );
}

/// Enable the MMU (stage-1 EL1 translation) with `root` as TTBR0. Programs MAIR/TCR/TTBR0, does
/// the invalidate-then-enable barrier dance, and sets `SCTLR_EL1.M`. Caches (`SCTLR.C/I`) are
/// left as-is this wave. Precondition: `root`'s tables identity-map the code, stack, and heap
/// currently in use (asserted by a software walk before this is called).
///
/// SAFETY: enabling translation with tables that do not cover the running kernel would fault
/// immediately. The caller guarantees an identity map built by `build_identity` + a pre-enable
/// `translate` assertion.
pub unsafe fn enable(root: usize) {
    asm!("msr mair_el1, {v}", v = in(reg) MAIR_VALUE, options(nostack));
    asm!("msr tcr_el1,  {v}", v = in(reg) TCR_VALUE, options(nostack));
    asm!("msr ttbr0_el1,{v}", v = in(reg) root as u64, options(nostack));
    asm!(
        "dsb ish",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        options(nostack)
    );
    let mut sctlr: u64;
    asm!("mrs {v}, sctlr_el1", v = out(reg) sctlr, options(nostack));
    sctlr |= 1 << 0; // M — enable stage-1 MMU
    asm!("msr sctlr_el1, {v}", "isb", v = in(reg) sctlr, options(nostack));
}

/// The live page-table root in use by the CPU (`TTBR0_EL1`, base address masked). Lets a later
/// brick (EL0 user-mode) map fresh user pages into the *active* address space rather than a
/// throwaway table built for a test. Only meaningful after `enable`.
pub fn active_root() -> usize {
    let ttbr0: u64;
    // SAFETY: reading TTBR0_EL1 is always sound at EL1.
    unsafe { asm!("mrs {v}, ttbr0_el1", v = out(reg) ttbr0, options(nomem, nostack)) };
    (ttbr0 & ADDR_MASK) as usize
}

/// Switch the active user address space by pointing `TTBR0_EL1` at `root`, then flushing the
/// TLB so no stale translation from the previous space survives. This is what gives each
/// process its own view of memory: after the switch, the SAME virtual address resolves through
/// `root`'s tables (or faults if `root` does not map it).
///
/// PRECONDITION (load-bearing): `root` MUST replicate the kernel identity map (code, stack,
/// `exc_vectors`, UART, and all kernel statics at their identity PAs) — otherwise the `isb`
/// after the write faults, because the very instruction stream doing the switch would become
/// unmapped. `build_identity()` guarantees this. The `tlbi vmalle1` is mandatory: reusing one
/// user VA across processes backed by different frames would otherwise resolve to a stale entry.
///
/// SAFETY: caller guarantees `root` identity-maps the running kernel (see precondition).
pub unsafe fn switch_address_space(root: usize) {
    asm!("msr ttbr0_el1, {v}", v = in(reg) root as u64, options(nostack));
    asm!(
        "dsb ish",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        options(nostack)
    );
}

/// Whether the MMU is currently enabled (`SCTLR_EL1.M`).
pub fn mmu_enabled() -> bool {
    let sctlr: u64;
    // SAFETY: reading SCTLR_EL1 is always sound at EL1.
    unsafe { asm!("mrs {v}, sctlr_el1", v = out(reg) sctlr, options(nomem, nostack)) };
    sctlr & 1 != 0
}

// ---------------------------------------------------------------------------
// Selftest — virtual-memory invariants, aarch64-only (NOT in the shared `selftest.rs`).
// Order matters: the identity map is proved by a *software* walk BEFORE the MMU is enabled, so a
// construction bug is caught as a failed assertion rather than a hang. After enable, dynamic
// map/unmap is proved by writing through a fresh VA and observing the bytes in a different frame.
// ---------------------------------------------------------------------------

/// Dynamic-mapping test VA: 0x4800_0000 sits just past the identity-mapped RAM (block 64), so it
/// is guaranteed unmapped until we map it — proving translation, not identity.
const TEST_VA: usize = frames::RAM_END;
const PATTERN: u64 = 0x5EED_2026_A1E7_0001;

/// Prove the virtual-memory invariants live. `Ok(n)` all passed; `Err((idx,name))` = failure.
/// This target's declared address-space layout (REQ-MM-008, ALET-P1-006). Stating it in one place is what
/// makes the properties checkable: the regions must not overlap, none may include the null page, and a
/// user-reachable region must never merely ABUT a kernel one (something that grows would cross the
/// boundary without ever being unmapped). `layout::Layout::validate` refuses a declaration that breaks
/// any of those, and the boot suite runs that check — a layout nobody validates is a layout that drifts.
pub fn layout() -> layout::Layout {
    let (text_start, _, rodata_end) = image_spans();
    layout::Layout::new("aarch64")
        // The peripheral window: device MMIO, kernel-only.
        .with(layout::Region::new(
            "device-mmio",
            0x0000_1000,
            0x4000_0000,
            false,
        ))
        // Kernel image (text + rodata; data/bss/stack/heap follow inside the RAM window below).
        .with(layout::Region::new(
            "kernel-image",
            text_start,
            rodata_end,
            false,
        ))
        // The RAM the frame allocator owns, above the image.
        .with(layout::Region::new(
            "kernel-ram",
            rodata_end,
            frames::RAM_END,
            false,
        ))
        // Where the user-mode suite maps unprivileged code and stack.
        .with(layout::Region::new("user", 0x5000_0000, 0x5000_2000, true))
}

pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            if !($cond) {
                kprintln!("  [FAIL {:>2}] {}", n, $name);
                return Err((n, $name));
            }
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }

    // 1 — build the identity table tree from real frames.
    let root = match build_identity() {
        Some(r) => r,
        None => return Err((1, "vm: identity tables (frame allocator exhausted)")),
    };
    check!(
        root % frames::FRAME_SIZE == 0,
        "vm: identity page tables built from frames"
    );

    // 2..4 — SOFTWARE-WALK ASSERT before enabling the MMU (catches a bad map without a hang).
    check!(
        translate(root, RAM_BASE) == Some(RAM_BASE),
        "vm: RAM identity-maps (pre-enable walk)"
    );
    check!(
        translate(root, UART_BASE) == Some(UART_BASE),
        "vm: device MMIO identity-maps (UART)"
    );
    {
        let probe = 0u64;
        let sp_ish = core::ptr::addr_of!(probe) as usize; // a live stack address (in RAM)
        check!(
            translate(root, sp_ish) == Some(sp_ish),
            "vm: running-stack address identity-maps"
        );
    }
    check!(
        translate(root, TEST_VA).is_none(),
        "vm: dynamic-test VA is unmapped before mapping"
    );

    // 5 — ENABLE THE MMU. A faulty identity map faults here -> exit 102 (clean fail, not a hang).
    check!(!mmu_enabled(), "vm: MMU off before enable");
    // SAFETY: `root` identity-maps the running code/stack/heap/UART, asserted by checks 2..4.
    unsafe { enable(root) };
    // If we reach this line, translation is live and the kernel is still executing under it.
    kprintln!("  [info  ] MMU enabled — kernel still executing under translation");
    check!(
        mmu_enabled(),
        "vm: MMU enabled and kernel survives translation"
    );

    // 6 — DYNAMIC virtual memory: map a fresh frame at a new VA, write via the VA, observe the
    //     bytes in the (different) physical frame. This is real address translation.
    let frame = match frames::alloc_zeroed() {
        Some(f) => f,
        None => return Err((n + 1, "vm: no frame for dynamic mapping")),
    };
    check!(
        frame.addr() != TEST_VA,
        "vm: test frame PA differs from its VA (translation, not identity)"
    );
    check!(
        map_page(root, TEST_VA, frame.addr(), NORMAL_PAGE),
        "vm: map fresh frame at a new virtual address"
    );
    check!(
        translate(root, TEST_VA) == Some(frame.addr()),
        "vm: mapped VA resolves to the frame"
    );
    // SAFETY: TEST_VA is now a valid Normal-memory mapping; the hardware walk will translate it.
    unsafe { core::ptr::write_volatile(TEST_VA as *mut u64, PATTERN) };
    // Read back through the identity-mapped physical frame — proves the write was redirected.
    // SAFETY: `frame` is held; its identity address is Normal-mapped RAM.
    let seen = unsafe { core::ptr::read_volatile(frame.addr() as *const u64) };
    check!(
        seen == PATTERN,
        "vm: write via VA lands in the mapped physical frame"
    );

    // 7 — UNMAP: the VA no longer resolves.
    check!(unmap_page(root, TEST_VA), "vm: unmap the dynamic page");
    check!(
        translate(root, TEST_VA).is_none(),
        "vm: unmapped VA no longer resolves"
    );

    // 8 — MAPPING-API ADMISSION CHECK (ALET-P1-001, REQ-MM-001): raw addresses are untrusted
    //     input. Each rejection below is a real corruption the check prevents on live tables:
    //     an aliasing VA silently overwrites another page's entry, a misaligned address maps a
    //     different page than the caller named, and a PA outside the allocator's window maps
    //     memory the kernel does not own. The reference frame is still allocated, so every
    //     rejection is attributable to the address, not to allocator exhaustion.
    let alias_va = TEST_VA + (1usize << 39); // bit 39 is not decoded by this 39-bit walk
    check!(
        !map_page(root, alias_va, frame.addr(), NORMAL_PAGE),
        "vm: mapping a VA above the decoded width is refused (would alias another entry)"
    );
    check!(
        translate(root, TEST_VA).is_none(),
        "vm: the refused aliasing map left the aliased VA untouched"
    );
    check!(
        !map_page(root, TEST_VA + 1, frame.addr(), NORMAL_PAGE),
        "vm: mapping an unaligned VA is refused"
    );
    check!(
        !map_page(root, TEST_VA, frame.addr() + 1, NORMAL_PAGE),
        "vm: mapping an unaligned PA is refused"
    );
    check!(
        !map_page(root, TEST_VA, 0, NORMAL_PAGE),
        "vm: mapping a PA outside the frame-allocator window is refused"
    );
    check!(
        !map_page(root, 0, frame.addr(), NORMAL_PAGE),
        "vm: mapping the null page is refused (null dereferences keep faulting)"
    );
    check!(
        !unmap_page(root, alias_va),
        "vm: unmapping an aliasing VA is refused (cannot tear down another page)"
    );
    // The check is a filter, not a blanket denial: a legal request still succeeds afterwards.
    check!(
        map_page(root, TEST_VA, frame.addr(), NORMAL_PAGE) && unmap_page(root, TEST_VA),
        "vm: a legal map/unmap still succeeds after the refusals"
    );

    // 9 — PAGE-TABLE RECLAMATION (ALET-P1-002, REQ-MM-003, ADR-031). An unmap that clears only the
    //     leaf leaves every intermediate table allocated AND still referenced: a task that maps and
    //     unmaps across a wide VA range drains the pool in proportion to addresses VISITED. These
    //     checks prove the tables come back, that a sibling mapping protects them, and that the
    //     address space's root is never freed.
    {
        // A VA far from TEST_VA so its L2/L3 tables are freshly allocated for this check alone.
        const R_VA: usize = TEST_VA + (1usize << 30); // a different L1 slot entirely
        const R_SIBLING: usize = R_VA + 4096; // same L3 table as R_VA
        let f1 = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "vm: no frame for the reclamation checks")),
        };
        let reclaimed0 = tables_reclaimed();
        let free_before = frames::free_count();
        check!(
            map_page(root, R_VA, f1.addr(), NORMAL_PAGE)
                && translate(root, R_VA) == Some(f1.addr()),
            "vm: map a page whose L2/L3 tables are freshly allocated"
        );
        let free_mapped = frames::free_count();
        check!(
            free_mapped < free_before,
            "vm: mapping consumed frames for the intermediate tables"
        );
        // A second page in the SAME L3 table: unmapping the first must free nothing.
        let f2 = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "vm: no second frame for the reclamation checks")),
        };
        check!(
            map_page(root, R_SIBLING, f2.addr(), NORMAL_PAGE),
            "vm: map a sibling page in the same leaf table"
        );
        let free_two = frames::free_count();
        check!(
            unmap_page(root, R_VA) && tables_reclaimed() == reclaimed0,
            "vm: unmapping one of two pages in a leaf table reclaims NO table (sibling still mapped)"
        );
        check!(
            translate(root, R_SIBLING) == Some(f2.addr()),
            "vm: the sibling mapping still resolves after the neighbour was unmapped"
        );
        check!(
            frames::free_count() == free_two,
            "vm: no table frame was returned while the leaf table was still in use"
        );
        // Now the last page goes: the whole chain below the root must be reclaimed.
        check!(
            unmap_page(root, R_SIBLING),
            "vm: unmap the last page in the leaf table"
        );
        check!(
            tables_reclaimed() == reclaimed0 + 2,
            "vm: emptying the leaf table reclaimed both intermediate tables (L3 and L2)"
        );
        check!(
            frames::free_count() == free_two + 2,
            "vm: the reclaimed table frames came back to the allocator"
        );
        check!(
            translate(root, R_VA).is_none() && translate(root, R_SIBLING).is_none(),
            "vm: neither VA resolves after reclamation"
        );
        // The root survived, and the address space still works: map/translate/unmap again, which
        // also proves the freed tables were genuinely reusable rather than corrupt.
        check!(
            map_page(root, R_VA, f1.addr(), NORMAL_PAGE)
                && translate(root, R_VA) == Some(f1.addr()),
            "vm: the address space rebuilds the reclaimed chain (root intact, frames reusable)"
        );
        check!(
            unmap_page(root, R_VA) && translate(root, TEST_VA).is_none(),
            "vm: reclamation left the rest of the address space untouched"
        );
        frames::free(f1);
        frames::free(f2);
    }

    // 10 — ADDRESS-SPACE DESTRUCTION (ALET-P1-004, REQ-MM-004, ADR-032). Reclamation (above) only
    //      helps a space that tidies up page by page. A space that DIES — a task that faults, is
    //      killed, or exits without unmapping — used to keep every page, every table and its root
    //      forever, so a crashed process was a permanent physical-memory loss. Teardown frees what
    //      the space owns and, critically, nothing else: the identity map's RAM/device BLOCK
    //      descriptors are addresses the allocator does not hold under this space's tag, so the
    //      ownership model refuses them and they are reported as SKIPPED rather than freed.
    {
        let free_before_space = frames::free_count();
        let victim = match build_identity() {
            Some(r) => r,
            None => return Err((n + 1, "vm: no frames to build a victim address space")),
        };
        check!(
            victim != root && frames::free_count() < free_before_space,
            "vm: built a second address space with its own tables"
        );
        // Give it two user pages, as a live process would have.
        let p1 = match frames::alloc_zeroed_as(Owner::USER) {
            Some(f) => f,
            None => return Err((n + 1, "vm: no user page for the teardown check")),
        };
        let p2 = match frames::alloc_zeroed_as(Owner::USER) {
            Some(f) => f,
            None => return Err((n + 1, "vm: no second user page for the teardown check")),
        };
        // Above RAM_END, so neither VA collides with the identity map's block descriptors, and
        // 2 MiB apart, so the two pages land in DIFFERENT leaf tables — teardown must walk more
        // than one branch.
        const V1: usize = frames::RAM_END + 0x20_0000;
        const V2: usize = frames::RAM_END + 0x60_0000;
        check!(
            map_page(victim, V1, p1.addr(), NORMAL_PAGE)
                && map_page(victim, V2, p2.addr(), NORMAL_PAGE),
            "vm: mapped two user pages into the victim address space"
        );
        check!(
            destroy_space(root).is_none(),
            "vm: destroying the ACTIVE address space is refused (the kernel is running in it)"
        );
        let t = match destroy_space(victim) {
            Some(t) => t,
            None => return Err((n + 1, "vm: teardown refused a non-active address space")),
        };
        check!(
            t.leaves_freed == 2,
            "vm: teardown freed exactly the pages the space owned"
        );
        check!(
            t.leaves_skipped > 0,
            "vm: teardown SKIPPED the identity map's block descriptors (not this space's frames)"
        );
        check!(
            t.tables_refused == 0,
            "vm: every table in the tree was one this space owned"
        );
        check!(
            frames::free_count() == free_before_space,
            "vm: destroying the space returned every frame it held, including its root"
        );
        check!(
            frames::owner_of(p1.addr()).is_none() && frames::owner_of(p2.addr()).is_none(),
            "vm: the destroyed space's pages have no owner (they are genuinely back in the pool)"
        );
        // The kernel's own address space is untouched and still works.
        check!(
            translate(root, TEST_VA).is_none() && active_root() == root,
            "vm: the surviving address space is intact after the teardown"
        );
    }

    // 11 — W^X AND ATTRIBUTE VALIDATION (ALET-P1-007/008, REQ-MM-006, ADR-034). A page that is
    //      both writable and executable turns any memory-corruption bug into code execution. The
    //      mapping API refuses such a request outright, and the audit then walks the LIVE tree so
    //      the property is checked against what is actually mapped, not against what the API was
    //      asked for. The bootstrap identity map used to be the honest exception — it covered kernel
    //      text, rodata, data, stack and heap in single 2 MiB blocks, which have one permission set
    //      and so had to be writable AND kernel-executable. Those blocks are now built as 4 KiB
    //      pages from the linker's section symbols (ALET-P1-007), so BOTH violation classes must be
    //      zero: W^X is a property of the whole live address space, not of the dynamic paths only.
    {
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                DESC_TABLE | ATTR_NORMAL | AP_RW_EL1 | SH_INNER | AF
            ),
            "wx: mapping a writable+executable page is refused (W^X)"
        );
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                DESC_TABLE | ATTR_DEVICE | AP_RW_EL1 | SH_INNER | AF | UXN
            ),
            "wx: mapping executable device memory is refused"
        );
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                DESC_TABLE | ATTR_NORMAL | AP_RW_EL0 | SH_INNER | AF | UXN
            ),
            "wx: mapping a user page that is kernel-executable is refused (ret2usr)"
        );
        check!(
            map_page(root, TEST_VA, frame.addr(), NORMAL_PAGE) && unmap_page(root, TEST_VA),
            "wx: a legal non-executable writable mapping still succeeds"
        );
        let report = audit_attrs(root);
        kprintln!(
            "  [info  ] attr audit: {} leaves, {} dynamic violations, {} bootstrap violations",
            report.leaves,
            report.dynamic_violations,
            report.bootstrap_violations
        );
        check!(
            report.leaves > 0,
            "wx: the attribute audit actually walked the live address space"
        );
        check!(
            report.dynamic_violations == 0,
            "wx: NO dynamically mapped page in the live tree is writable+executable"
        );
        check!(
            report.bootstrap_violations == 0,
            "wx: NO block descriptor in the bootstrap identity map is writable+executable either"
        );
        // The image split itself, asserted from the linker symbols rather than from a constant: the
        // permissions above are only trustworthy if the leaves covering the image really are pages.
        let (text_start, text_end, rodata_end) = image_spans();
        check!(
            image_split_blocks() >= 1
                && matches!(leaf_of(root, text_start), Some((_, 3)))
                && translate(root, text_start) == Some(text_start),
            "wx: the kernel image is identity-mapped at 4 KiB granularity, not by 2 MiB blocks"
        );
        check!(
            attrs_of(root, text_start)
                .is_some_and(|a| a.exec_kernel && !a.write && !a.exec_user && !a.user),
            "wx: kernel text is EL1-executable and READ-ONLY (never writable, never EL0)"
        );
        check!(
            text_end < rodata_end
                && attrs_of(root, rodata_end - 1)
                    .is_some_and(|a| !a.write && !a.exec_kernel && !a.exec_user),
            "wx: .rodata is mapped read-only and execute-never at both privilege levels"
        );
        check!(
            attrs_of(root, &TABLES_RECLAIMED as *const _ as usize)
                .is_some_and(|a| a.write && !a.exec_kernel && !a.exec_user)
                && attrs_of(root, &n as *const _ as usize)
                    .is_some_and(|a| a.write && !a.exec_kernel && !a.exec_user),
            "wx: kernel data and the running stack are writable and execute-never"
        );
        // The split cuts both ways: it replaced the block descriptors that made these VAs
        // unmappable-over (the mapping API refuses to descend into a block) with real tables, so the
        // API must refuse the image span explicitly. Otherwise a caller could map a fresh WRITABLE
        // page over kernel text — the write-to-code path W^X exists to close — or unmap kernel .data
        // from under the running kernel.
        check!(
            !map_page(root, text_start, frame.addr(), NORMAL_PAGE)
                && translate(root, text_start) == Some(text_start),
            "wx: mapping over the split kernel image is refused (text still maps to itself)"
        );
        check!(
            !unmap_page(root, text_start)
                && attrs_of(root, text_start).is_some_and(|a| a.exec_kernel && !a.write),
            "wx: unmapping the split kernel image is refused (text still read-only + executable)"
        );
    }

    // The kernel stack's guard page (REQ-MM-007, ALET-P1-012). An overflow must FAULT, so the page below
    // the stack must have no translation at all — and the pages the stack actually uses must still work,
    // or the guard would have cost the kernel its stack.
    {
        let guard = stack_guard_page();
        let stack_low = guard + vmaddr::PAGE_SIZE;
        check!(
            translate(root, guard).is_none(),
            "guard: the page below the kernel stack has NO translation (an overflow faults)"
        );
        check!(
            leaf_of(root, guard).is_none(),
            "guard: the guard page has no leaf descriptor at any level (not merely a bad one)"
        );
        check!(
            translate(root, stack_low) == Some(stack_low)
                && translate(root, stack_low + vmaddr::PAGE_SIZE)
                    == Some(stack_low + vmaddr::PAGE_SIZE),
            "guard: the stack's own first pages are still mapped (the guard cost nothing usable)"
        );
        check!(
            attrs_of(root, stack_low).is_some_and(|a| a.write && !a.exec_kernel),
            "guard: the stack itself is writable and never executable (W^X holds across the split)"
        );
    }

    frames::free(frame);
    // The declared layout (REQ-MM-008, ALET-P1-006).
    {
        let l = layout();
        check!(
            l.validate().is_ok(),
            "layout: the declared address-space layout validates (disjoint, aligned, guarded, no null page)"
        );
        let (text_start, _, _) = image_spans();
        check!(
            l.region_of(text_start).is_some_and(|r| r.name == "kernel-image" && !r.user)
                && l.region_of(0x5000_0000).is_some_and(|r| r.user),
            "layout: kernel text is kernel-only and the user window is user-reachable (no address is both)"
        );
        check!(
            translate(root, 0).is_none() && leaf_of(root, 0).is_none(),
            "layout: VA 0 has NO translation in the live map (a kernel null dereference faults)"
        );
    }

    Ok(n)
}
