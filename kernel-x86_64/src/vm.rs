//! Virtual-memory selftest (x86-64 first-class target) — the P5 MMU brick, the AMD64 twin of
//! `kernel/src/vm.rs`. It proves the kernel can read and edit its own address space: resolve the
//! identity map, map a fresh physical frame at a new virtual address, route a write through that
//! mapping into the backing frame, then unmap it.
//!
//! HONEST DIFFERENCE FROM AARCH64 (contract-honest, ADR-010/019): the aarch64 backend proves an
//! "MMU off -> build tables -> MMU on" transition because it boots with translation disabled.
//! x86-64 cannot: long mode REQUIRES paging, so OVMF hands us a machine already translating, and
//! after `ExitBootServices` we OWN that live page-table hierarchy. So this suite proves the honest
//! x86-64 property — that we can *walk and edit the live hierarchy* — rather than an off->on flip:
//!   * the existing identity map resolves (translation is real, not a no-op),
//!   * a chosen high VA is unmapped to begin with,
//!   * `map_to` installs a fresh frame there (pulling intermediate page-table frames from our own
//!     `frames` allocator — exercising that allocator as a page-table source),
//!   * the newly mapped VA resolves to exactly that frame,
//!   * a write through the VA lands in the frame's physical bytes (the mapping actually routes),
//!   * `unmap` removes it and the VA stops resolving.
//!
//! We build an `OffsetPageTable` with phys_offset = 0: OVMF identity-maps the RAM we touch
//! (phys == virt), so every page-table frame and mapped frame is reachable at its own address.
//!
//! CR0.WP: writing a new entry into the *pre-existing* top-level table can fault if the firmware
//! left its page-table pages read-only (a ring-0 write to a RO page with CR0.WP set #PFs). We
//! clear CR0.WP for the duration of the map/unmap and restore it after — a standard, localized
//! kernel technique; the window is single-core with no preemption.
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::mapper::{Mapper, Translate};
use x86_64::structures::paging::{
    Page, PageTable, PageTableFlags, PhysFrame as X86PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::frames;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_core::deadva;
use kernel_core::frameown::Owner;
use kernel_core::memattr::{self, AttrOps, MemKind, PageAttrs};
use kernel_core::ptreclaim::{self, PathStep, TableOps};
use kernel_core::teardown::{self, SpaceOps, Teardown};
use kernel_core::vmaddr::{self, AddrPlan};

/// A canonical lower-half virtual address far above any RAM OVMF identity-maps at `-m 256M`
/// (which tops out in the low GiB): PML4 slot 0xA0. Its paging entries are absent at boot, so
/// `map_to` must allocate fresh intermediate tables — proving the frame allocator feeds paging.
const TEST_VA: u64 = 0x0000_5000_0000_0000;

/// The 64-bit pattern written through the test mapping and read back from the backing frame.
const PATTERN: u64 = 0xA1E7_2026_00FF_00FF;

/// Borrow the active top-level page table (the one CR3 points at) as an `OffsetPageTable`.
///
/// SAFETY: CR3 holds the physical address of the live PML4; under OVMF's identity map that
/// address is directly readable/writable at its own value (phys_offset = 0). Single-core with no
/// preemption means no other agent mutates the hierarchy while this borrow is live.
unsafe fn active_mapper() -> x86_64::structures::paging::OffsetPageTable<'static> {
    let (l4_frame, _) = Cr3::read();
    let l4_phys = l4_frame.start_address().as_u64();
    let l4: &'static mut PageTable = &mut *(l4_phys as *mut PageTable);
    x86_64::structures::paging::OffsetPageTable::new(l4, VirtAddr::new(0))
}

/// Bits of a 4-level x86-64 paging entry this module needs: the present bit and the physical
/// address field. (`PageTableFlags::PRESENT` is bit 0; the address occupies bits 51:12.)
const PRESENT: u64 = 1;
const ENTRY_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Index of `va` at each of the four paging levels, PML4 first.
#[inline]
fn indices4(va: u64) -> [usize; 4] {
    [
        ((va >> 39) & 0x1ff) as usize,
        ((va >> 30) & 0x1ff) as usize,
        ((va >> 21) & 0x1ff) as usize,
        ((va >> 12) & 0x1ff) as usize,
    ]
}

/// The x86-64 paging-entry view the shared reclamation policy walks (REQ-MM-003, ADR-031). The only
/// architecture knowledge here is the present bit and the 8-byte entry width; the rules live once
/// in `kernel_core::ptreclaim`, identically to aarch64 and RISC-V.
struct Tables;

impl TableOps for Tables {
    fn read(&self, table: usize, index: usize) -> u64 {
        // SAFETY: paging structures are identity-accessible under OVMF's map; index < 512.
        unsafe { core::ptr::read_volatile((table + index * 8) as *const u64) }
    }
    fn write(&mut self, table: usize, index: usize, value: u64) {
        // SAFETY: as above. Callers clear CR0.WP first, because an upper-level table may be a
        // firmware page OVMF left read-only.
        unsafe { core::ptr::write_volatile((table + index * 8) as *mut u64, value) }
    }
    fn is_present(&self, entry: u64) -> bool {
        entry & PRESENT != 0
    }
    fn free_table(&mut self, table: usize) -> bool {
        frames::free_addr_as(table, Owner::PAGETABLE)
    }
}

/// The same entry view, extended with what address-space DESTRUCTION needs (REQ-MM-004, ADR-032).
///
/// x86-64 is the target where `is_private` earns its existence: `build_space` builds a per-process
/// PML4 by COPYING the live one, so almost every top-level slot points at firmware and kernel
/// tables the running kernel still needs. Teardown therefore descends only into PML4 slot 0 (whose
/// PDPT this space privatized) and, within it, only [`USER_REGION_PDPT_INDEX`] — the 1 GiB region
/// this space actually owns. Everything else is left exactly as found.
impl SpaceOps for Tables {
    fn levels(&self) -> usize {
        4
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        level == 3 || entry & PageTableFlags::HUGE_PAGE.bits() != 0
    }
    fn entry_addr(&self, entry: u64) -> usize {
        (entry & ENTRY_ADDR_MASK) as usize
    }
    fn is_private(&self, level: usize, index: usize) -> bool {
        match level {
            0 => index == 0,                      // the privatized PDPT
            1 => index == USER_REGION_PDPT_INDEX, // this space's own 1 GiB user region
            _ => true,
        }
    }
    fn free_leaf(&mut self, pa: usize) -> bool {
        frames::free_addr_as(pa, Owner::USER)
    }
}

/// Destroy the address space rooted at `root`: free the user pages and tables of its private
/// region, then the root PML4 (REQ-MM-004, ADR-032). Returns `None` — refusing outright — when
/// asked to destroy the space CR3 currently points at.
///
/// CR0.WP is cleared across the walk for the same reason the map path clears it: an upper-level
/// table may be a firmware page OVMF left read-only, and clearing its entry is a ring-0 write.
pub fn destroy_space(root: u64) -> Option<Teardown> {
    if root == active_root() {
        return None;
    }
    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: clearing WP only relaxes ring-0 write protection; restored below. Single-core.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }
    let out = teardown::destroy_address_space(root as usize, &mut Tables);
    if wp_was_set {
        // SAFETY: re-arm the write-protect bit exactly as found.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
    Some(out)
}

/// Decode x86-64 paging bits into the arch-neutral permission model, and audit live trees against
/// it (REQ-MM-006, ADR-034). x86-64 has one NX bit and no separate user/kernel execute control, so
/// a USER page without NX is executable at BOTH levels — which is exactly the ret2usr mapping the
/// rules refuse (hardware SMEP is the mitigation of last resort; not creating the mapping is the
/// first). Memory type comes from PAT/MTRRs rather than a leaf field, so every leaf is modelled as
/// Normal and that is stated rather than silently assumed.
impl AttrOps for Tables {
    fn levels(&self) -> usize {
        4
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        <Self as SpaceOps>::is_leaf(self, entry, level)
    }
    fn entry_addr(&self, entry: u64) -> usize {
        (entry & ENTRY_ADDR_MASK) as usize
    }
    fn in_scope(&self, level: usize, index: usize) -> bool {
        // Audit only what WE mapped: the private user region of a space we built. The live root is
        // OVMF's, and its firmware leaves are inherited rather than created here.
        <Self as SpaceOps>::is_private(self, level, index)
    }
    fn decode(&self, entry: u64, _level: usize) -> PageAttrs {
        let user = entry & PageTableFlags::USER_ACCESSIBLE.bits() != 0;
        let exec = entry & PageTableFlags::NO_EXECUTE.bits() == 0;
        PageAttrs {
            kind: MemKind::Normal,
            write: entry & PageTableFlags::WRITABLE.bits() != 0,
            exec_user: exec && user,
            // A USER page with NX clear IS fetchable at ring 0 as far as paging is concerned —
            // x86-64 has no per-privilege execute bit. What forbids it is CR4.SMEP, enabled by
            // `enable_exec_protections` below, so ring-0 execution of a user page faults in
            // hardware. Modelling `exec_kernel` as `exec && !user` states that division of labour
            // honestly: paging expresses "executable", SMEP expresses "not by the kernel". Without
            // SMEP this target cannot enforce that rule by paging at all, and the boot says so.
            exec_kernel: exec && !user,
            user,
        }
    }
}

/// Turn on the two hardware features W^X depends on here, and report what the CPU actually allows
/// (REQ-MM-006, ADR-034):
///
/// * **EFER.NXE** — without it the NO_EXECUTE bit is a *reserved* bit that faults rather than a
///   permission, so every writable page would have to stay executable.
/// * **CR4.SMEP** — x86-64 has no per-privilege execute bit, so a USER page with NX clear is
///   fetchable at ring 0 by paging alone. SMEP is what makes that fault, and it is therefore the
///   mechanism that enforces the "a user page is never kernel-executable" rule on this target.
///
/// Returns `(nx, smep)`. Neither is guaranteed by firmware; a missing one is printed at boot rather
/// than silently degrading the invariant.
pub fn enable_exec_protections() -> (bool, bool) {
    // CPUID is unprivileged and side-effect free (safe in current core).
    let nx = core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 20) != 0;
    if nx {
        // SAFETY: setting EFER.NXE only gives bit 63 its permission meaning; every mapping we
        // create afterwards is built with that meaning, and pre-existing entries have it clear.
        unsafe { Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE)) };
    }
    // SMEP is CPUID leaf 7, subleaf 0, EBX[7].
    let smep = core::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 7) != 0;
    if smep {
        // SAFETY: enabling SMEP only makes ring-0 instruction fetches from USER pages fault. The
        // kernel never executes from a user page — every kernel mapping is supervisor-only.
        unsafe {
            x86_64::registers::control::Cr4::update(|f| {
                f.insert(x86_64::registers::control::Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION)
            })
        };
    }
    (nx, smep)
}

/// The same decode with NO scoping, for auditing the firmware tree we inherited (informational).
struct UnscopedTables;

impl TableOps for UnscopedTables {
    fn read(&self, table: usize, index: usize) -> u64 {
        Tables.read(table, index)
    }
    fn write(&mut self, table: usize, index: usize, value: u64) {
        Tables.write(table, index, value)
    }
    fn is_present(&self, entry: u64) -> bool {
        Tables.is_present(entry)
    }
    fn free_table(&mut self, _table: usize) -> bool {
        false // never frees: this view exists only to look
    }
}

impl AttrOps for UnscopedTables {
    fn levels(&self) -> usize {
        4
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        <Tables as AttrOps>::is_leaf(&Tables, entry, level)
    }
    fn entry_addr(&self, entry: u64) -> usize {
        <Tables as AttrOps>::entry_addr(&Tables, entry)
    }
    fn decode(&self, entry: u64, level: usize) -> PageAttrs {
        <Tables as AttrOps>::decode(&Tables, entry, level)
    }
}

/// Audit every mapping reachable from `root` against the W^X and attribute rules (REQ-MM-006).
pub fn audit_attrs(root: u64) -> memattr::AuditReport {
    memattr::audit(root as usize, &Tables)
}

/// The same audit with NO scoping — every leaf in the tree, not just the region a space privatized.
/// Used for the inherited OVMF tree (informational) and for the map the kernel builds for ITSELF
/// (`kmap`), where scoping would defeat the point: that whole tree is ours to answer for
/// (ALET-P1-031, REQ-MM-006).
pub fn audit_all(root: u64) -> memattr::AuditReport {
    memattr::audit(root as usize, &UnscopedTables)
}

/// Refuse a leaf whose permissions break W^X or the attribute rules (REQ-MM-006, ADR-034). Called
/// at the entry of every mapping API, because caller-supplied flags are untrusted input exactly
/// like `va`/`pa`.
fn attrs_ok(flags: PageTableFlags) -> bool {
    Tables.decode(flags.bits(), 3).validate().is_ok()
}

/// Total intermediate tables reclaimed since boot, so the boot gate proves reclamation actually ran.
static TABLES_RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// Intermediate page tables freed since boot (REQ-MM-003).
pub fn tables_reclaimed() -> usize {
    TABLES_RECLAIMED.load(Ordering::Relaxed)
}

/// Walk `root` for `va` and return the four-step path (PML4 → PDPT → PD → PT) reclamation needs, or
/// `None` when the walk hits an absent entry or a huge-page leaf (nothing to reclaim there).
fn walk_path(root: u64, va: u64) -> Option<[PathStep; 4]> {
    let idx = indices4(va);
    let ops = Tables;
    let mut table = root as usize;
    let mut path = [PathStep::new(0, 0); 4];
    for level in 0..4 {
        path[level] = PathStep::new(table, idx[level]);
        if level == 3 {
            break;
        }
        let e = ops.read(table, idx[level]);
        if e & PRESENT == 0 || e & PageTableFlags::HUGE_PAGE.bits() != 0 {
            return None;
        }
        table = (e & ENTRY_ADDR_MASK) as usize;
    }
    Some(path)
}

/// Prove the virtual-memory invariants against the live page-table hierarchy. `Ok(n)` = all n
/// passed; `Err((idx,name))` = check idx failed. x86-64-specific (NOT in the shared selftest).
/// This target's declared address-space layout (REQ-MM-008, ALET-P1-006). See the aarch64 twin: stating
/// the layout in one place is what makes its properties checkable, and the boot suite runs that check.
pub fn layout() -> kernel_core::layout::Layout {
    use kernel_core::layout::{Layout, Region};
    let (img_base, img_size) = crate::kmap::image_span();
    Layout::new("x86-64")
        // The image the firmware loaded us at (text/rodata/data/bss, including the guarded ring-0 stack).
        .with(Region::new(
            "kernel-image",
            img_base,
            img_base + img_size,
            false,
        ))
        // Where the ring-3 suite maps unprivileged code and stack.
        .with(Region::new("user", 0x4000_0000, 0x4000_2000, true))
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

    // SAFETY: see `active_mapper`; single-core, no preemption for the whole selftest.
    let mut mapper = unsafe { active_mapper() };
    let test_va = VirtAddr::new(TEST_VA);

    // 1 — translation is real: a known-mapped low RAM address resolves under the identity map.
    // Frame 0x10_0000 (1 MiB) is inside the conventional RAM OVMF identity-maps.
    let known = VirtAddr::new(0x10_0000);
    check!(
        mapper.translate_addr(known) == Some(PhysAddr::new(0x10_0000)),
        "vm: identity map resolves (translation is live, phys == virt)"
    );

    // 2 — the chosen high VA is unmapped before we map it.
    check!(
        mapper.translate_addr(test_va).is_none(),
        "vm: dynamic-test VA is unmapped before mapping"
    );

    // Allocate the frame we will map, and remember its physical address for the write-through check.
    let frame = match frames::alloc_zeroed() {
        Some(f) => f,
        None => return Err((n + 1, "vm: no free frame for the dynamic mapping")),
    };
    let frame_pa = frame.addr() as u64;
    let x86_frame = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(frame_pa));
    let page = Page::<Size4KiB>::containing_address(test_va);
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    // Clear CR0.WP so a write into a firmware-owned (possibly read-only) top-level table cannot
    // #PF; restore it after unmap. SAFETY: single-core, and we restore the exact prior flags.
    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: clearing WP only relaxes ring-0 write protection; restored below.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }

    let mut fa = frames::GlobalFrames;
    // 3 — install a fresh mapping (allocates intermediate tables from our frame allocator).
    // SAFETY: `page` is a currently-unmapped canonical VA; `x86_frame` is a real unused frame we
    // just allocated; `fa` supplies real zeroed frames for intermediate tables.
    let mapped = unsafe { mapper.map_to(page, x86_frame, flags, &mut fa) };
    let map_ok = match mapped {
        Ok(flush) => {
            flush.flush();
            true
        }
        Err(_) => false,
    };
    // Don't early-return while WP is cleared — record results, restore WP, then assert.

    // 4 — the newly mapped VA resolves to exactly the frame we chose.
    let resolves = mapper.translate_addr(test_va) == Some(PhysAddr::new(frame_pa));

    // 5 — a write through the VA lands in the frame's physical bytes (mapping actually routes).
    let mut write_through = false;
    if map_ok {
        // SAFETY: `test_va` is now mapped RW to `frame`; the frame's phys addr is identity-readable.
        unsafe {
            core::ptr::write_volatile(TEST_VA as *mut u64, PATTERN);
            write_through = core::ptr::read_volatile(frame_pa as *const u64) == PATTERN;
        }
    }

    // 6 — unmap removes the mapping and the VA stops resolving.
    let mut unmapped = false;
    if map_ok {
        if let Ok((_f, flush)) = mapper.unmap(page) {
            flush.flush();
            unmapped = mapper.translate_addr(test_va).is_none();
        }
    }

    // Restore CR0.WP exactly as found before asserting (a failed assert must not leave WP off).
    if wp_was_set {
        // SAFETY: re-arming the write-protect bit we cleared above.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
    // Reclaim the mapped frame regardless of outcome.
    frames::free(frame);

    check!(map_ok, "vm: map fresh frame at a new high virtual address");
    check!(
        resolves,
        "vm: mapped VA resolves to the frame (translation follows the new entry)"
    );
    check!(
        write_through,
        "vm: write via VA lands in the mapped physical frame"
    );
    check!(
        unmapped,
        "vm: unmap removes the page; VA no longer resolves"
    );

    // 7 — MAPPING-API ADMISSION CHECK (ALET-P1-001, REQ-MM-001): raw addresses are untrusted input.
    //     On x86-64 the two failure modes are not merely logical: `Page::containing_address`
    //     TRUNCATES a misaligned VA to its page base (mapping a page the caller never named), and
    //     `VirtAddr::new` PANICS on a non-canonical address. Both become a fail-closed `false` here.
    //     A fresh frame is held for the whole block, so each refusal is attributable to the address
    //     rather than to allocator exhaustion.
    let probe = match frames::alloc_zeroed() {
        Some(f) => f,
        None => return Err((n + 1, "vm: no free frame for the admission-check probe")),
    };
    let probe_pa = probe.addr() as u64;
    let root = active_root();
    let non_canonical: u64 = 0x0001_0000_0000_0000; // inside 48 bits, outside the canonical form
    let refusals = [
        (
            !map_kernel_frame(root, non_canonical, probe_pa),
            "vm: mapping a non-canonical VA is refused (would #GP, not page-fault)",
        ),
        (
            !map_kernel_frame(root, TEST_VA + 1, probe_pa),
            "vm: mapping an unaligned VA is refused (would silently map its page base)",
        ),
        (
            !map_kernel_frame(root, TEST_VA, probe_pa + 1),
            "vm: mapping an unaligned PA is refused",
        ),
        (
            !map_kernel_frame(root, TEST_VA, 0),
            "vm: mapping a PA outside the frame-allocator window is refused",
        ),
        (
            !map_kernel_frame(root, 0, probe_pa),
            "vm: mapping the null page is refused (null dereferences keep faulting)",
        ),
        (
            !unmap_user(root, non_canonical),
            "vm: unmapping a non-canonical VA is refused",
        ),
    ];
    // The check is a filter, not a blanket denial: a legal request still succeeds afterwards.
    let legal_still_works = map_kernel_frame(root, TEST_VA, probe_pa)
        && translate_in(root, TEST_VA) == Some(probe_pa)
        && unmap_user(root, TEST_VA);
    frames::free(probe);
    for (ok, name) in refusals {
        check!(ok, name);
    }
    check!(
        legal_still_works,
        "vm: a legal map/translate/unmap still succeeds after the refusals"
    );

    // 8 — PAGE-TABLE RECLAMATION (ALET-P1-002, REQ-MM-003, ADR-031). `Mapper::unmap` clears the
    //     leaf entry and stops, leaving every intermediate table allocated AND still referenced: a
    //     task that maps and unmaps across a wide VA range drains the pool in proportion to
    //     addresses VISITED. TEST_VA sits in a PML4 slot that is absent at boot, so mapping it
    //     allocates a fresh PDPT+PD+PT from our pool — three levels here against two on the
    //     3-level aarch64/RISC-V walks (an honest architectural difference, same rule).
    {
        const R_SIBLING: u64 = TEST_VA + 4096; // same PT as TEST_VA
        let f1 = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "vm: no frame for the reclamation checks")),
        };
        let f2 = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "vm: no second frame for the reclamation checks")),
        };
        let reclaimed0 = tables_reclaimed();
        let free_before = frames::free_count();
        check!(
            map_kernel_frame(root, TEST_VA, f1.addr() as u64)
                && translate_in(root, TEST_VA) == Some(f1.addr() as u64),
            "vm: map a page whose intermediate tables are freshly allocated"
        );
        let free_mapped = frames::free_count();
        check!(
            free_mapped < free_before,
            "vm: mapping consumed frames for the intermediate tables"
        );
        check!(
            map_kernel_frame(root, R_SIBLING, f2.addr() as u64),
            "vm: map a sibling page in the same leaf table"
        );
        let free_two = frames::free_count();
        check!(
            unmap_user(root, TEST_VA) && tables_reclaimed() == reclaimed0,
            "vm: unmapping one of two pages in a leaf table reclaims NO table (sibling still mapped)"
        );
        check!(
            translate_in(root, R_SIBLING) == Some(f2.addr() as u64),
            "vm: the sibling mapping still resolves after the neighbour was unmapped"
        );
        check!(
            frames::free_count() == free_two,
            "vm: no table frame was returned while the leaf table was still in use"
        );
        check!(
            unmap_user(root, R_SIBLING),
            "vm: unmap the last page in the leaf table"
        );
        let freed = tables_reclaimed() - reclaimed0;
        check!(
            freed == 3,
            "vm: emptying the leaf table reclaimed all three intermediate tables (PT, PD, PDPT)"
        );
        check!(
            frames::free_count() == free_two + freed,
            "vm: the reclaimed table frames came back to the allocator"
        );
        check!(
            translate_in(root, TEST_VA).is_none() && translate_in(root, R_SIBLING).is_none(),
            "vm: neither VA resolves after reclamation"
        );
        check!(
            map_kernel_frame(root, TEST_VA, f1.addr() as u64)
                && translate_in(root, TEST_VA) == Some(f1.addr() as u64),
            "vm: the address space rebuilds the reclaimed chain (root intact, frames reusable)"
        );
        check!(
            unmap_user(root, TEST_VA) && translate_in(root, TEST_VA).is_none(),
            "vm: reclamation left the rest of the address space untouched"
        );
        frames::free(f1);
        frames::free(f2);
    }

    // 9 — ADDRESS-SPACE DESTRUCTION (ALET-P1-004, REQ-MM-004, ADR-032). A space that DIES — a task
    //     that faults, is killed, or exits without unmapping — used to keep every page, every table
    //     and its root forever. Teardown frees what the space owns and nothing else, which on
    //     x86-64 is the hard part: `build_space` COPIES the live PML4, so almost every top-level
    //     slot points at firmware and kernel tables the running kernel still needs. The privacy
    //     predicate scopes the walk to this space's own 1 GiB user region, and the ownership model
    //     refuses anything else even if that predicate were wrong.
    {
        let free_before_space = frames::free_count();
        let victim = match build_space() {
            Some(r) => r,
            None => return Err((n + 1, "vm: no frames to build a victim address space")),
        };
        check!(
            victim != active_root() && frames::free_count() < free_before_space,
            "vm: built a second address space with its own PML4/PDPT"
        );
        // Two user pages, 2 MiB apart so they land in DIFFERENT leaf tables.
        const V1: u64 = 0x4000_0000; // inside the private user region (1..2 GiB)
        const V2: u64 = V1 + 0x20_0000;
        let ok = map_user(victim, V1, true).is_some() && map_user(victim, V2, true).is_some();
        check!(
            ok,
            "vm: mapped two user pages into the victim address space"
        );
        check!(
            destroy_space(active_root()).is_none(),
            "vm: destroying the ACTIVE address space is refused (the kernel is running in it)"
        );
        // Find a COPIED kernel slot: a present PML4 entry other than slot 0 (slot 0 is the PDPT
        // this space privatized). Teardown must not touch the table it points at, because the
        // running kernel translates through the very same table in the live root.
        //
        // Compared against the tree the space was DERIVED from, not `active_root()`: since
        // ALET-P2-033 the source is the kernel's own map, and this suite runs before
        // `kmap::activate()`, so the two are deliberately different roots here.
        //
        // The sharing lives one level DOWN now. The kernel's own map covers 4 GiB, so every one of
        // its mappings hangs off PML4[0] and slots 1..512 are empty — searching them found nothing.
        // What the victim actually shares is the rest of the private PDPT: every slot except the
        // user region still points at the kernel's own PDs.
        let ops = Tables;
        let src_pdpt = (ops.read(space_source_root() as usize, 0) & PTE_ADDR_MASK) as usize;
        let vic_pdpt = (ops.read(victim as usize, 0) & PTE_ADDR_MASK) as usize;
        let mut shared_slot = None;
        for i in 0..512 {
            if i == USER_REGION_PDPT_INDEX {
                continue;
            }
            let live = ops.read(src_pdpt, i);
            if live != 0 && ops.read(vic_pdpt, i) == live {
                shared_slot = Some((i, live));
                break;
            }
        }
        let (shared_index, shared_entry) = match shared_slot {
            Some(v) => v,
            None => return Err((n + 1, "vm: the victim shares no kernel slot to protect")),
        };
        let t = match destroy_space(victim) {
            Some(t) => t,
            None => return Err((n + 1, "vm: teardown refused a non-active address space")),
        };
        check!(
            t.leaves_freed == 2,
            "vm: teardown freed exactly the pages the space owned"
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
            ops.read(src_pdpt, shared_index) == shared_entry,
            "vm: the shared kernel mapping the victim copied is untouched by the teardown"
        );
        // The kernel is still running, still translating, and its own space is untouched — proved
        // by the fact that this very check executes and the live root still maps the kernel.
        check!(
            translate_in(active_root(), TEST_VA).is_none(),
            "vm: the surviving address space is intact after the teardown"
        );
    }

    // 10 — W^X AND ATTRIBUTE VALIDATION (ALET-P1-007/008, REQ-MM-006, ADR-034). A writable AND
    //      executable page turns any memory-corruption bug into code execution. The mapping APIs
    //      refuse such a request; the audit then walks the LIVE tree, so the property is checked
    //      against what is actually mapped. On x86-64 the honest exception is everything OVMF
    //      mapped before we took over — the firmware's own identity map is writable and executable
    //      and we inherited it — so the audit counts violations by class and the gate requires zero
    //      among the pages OUR mapping APIs created.
    {
        let probe2 = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "vm: no frame for the W^X checks")),
        };
        let root = active_root();
        check!(
            !map_user_frame_raw(
                root,
                TEST_VA,
                probe2.addr() as u64,
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE
            ),
            "wx: mapping a writable+executable page is refused (W^X)"
        );
        check!(
            !map_user_frame_raw(
                root,
                TEST_VA,
                probe2.addr() as u64,
                PageTableFlags::PRESENT
                    | PageTableFlags::USER_ACCESSIBLE
                    | PageTableFlags::WRITABLE
            ),
            "wx: mapping a writable+executable user page is refused"
        );
        check!(
            map_kernel_frame(root, TEST_VA, probe2.addr() as u64) && unmap_user(root, TEST_VA),
            "wx: a legal non-executable writable mapping still succeeds"
        );
        // Audit a space WE built: the live root is OVMF's, whose half-million firmware leaves are
        // writable+executable and inherited, not created by our APIs. Auditing our own space is the
        // property we can honestly assert; the firmware's tree is reported below for scale.
        let space = match build_space() {
            Some(r) => r,
            None => return Err((n + 1, "vm: no space for the W^X audit")),
        };
        let code = map_user(space, 0x4000_0000, false); // RX user page
        let data = map_user(space, 0x4000_1000, true); // RW+NX user page
        check!(
            code.is_some() && data.is_some(),
            "wx: mapped an executable-read-only and a writable-non-executable user page"
        );
        let report = audit_attrs(space);
        kprintln!(
            "  [info  ] attr audit (our space): {} leaves, {} dynamic violations, {} block violations",
            report.leaves,
            report.dynamic_violations,
            report.bootstrap_violations
        );
        let firmware = memattr::audit(active_root() as usize, &UnscopedTables);
        kprintln!(
            "  [info  ] attr audit (inherited OVMF tree, informational): {} leaves, {} W^X violations",
            firmware.leaves,
            firmware.dynamic_violations + firmware.bootstrap_violations
        );
        check!(
            report.leaves >= 2,
            "wx: the attribute audit actually walked the live address space"
        );
        check!(
            report.dynamic_violations == 0 && report.bootstrap_violations == 0,
            "wx: NO dynamically mapped page in the live tree is writable+executable"
        );
        if let Some(t) = destroy_space(space) {
            check!(
                t.leaves_freed == 2,
                "wx: the audited space was destroyed and its pages returned"
            );
        }
        frames::free(probe2);
    }

    // 11 — THE KERNEL'S OWN MAP (ALET-P1-031, REQ-MM-006, ADR-034). Everything above audits trees
    //      this kernel MAPPED INTO; the tree it RUNS ON is still OVMF's, and the firmware's leaves
    //      are writable+executable by the half-million. `kmap` builds the replacement from the PE
    //      image's own section table — the x86-64 answer to the linker symbols the other two targets
    //      read. These checks assert the built map is W^X-correct everywhere and that each class of
    //      kernel address is mapped AS THE RIGHT THING, not merely that nothing was flagged.
    //      HONEST SCOPE: the map is built and proved, NOT activated — CR3 still holds OVMF's root,
    //      so the inherited violations remain live and are reported for comparison below.
    {
        let kroot = crate::kmap::root();
        check!(
            kroot != 0 && kroot != active_root(),
            "kmap: the kernel built its OWN address map, distinct from the firmware's live root"
        );
        let ours = audit_all(kroot);
        let firmware = audit_all(active_root());
        kprintln!(
            "  [info  ] kernel map: {} leaves, {} dynamic + {} block violations   |   inherited OVMF tree: {} leaves, {} violations",
            ours.leaves,
            ours.dynamic_violations,
            ours.bootstrap_violations,
            firmware.leaves,
            firmware.dynamic_violations + firmware.bootstrap_violations
        );
        check!(
            ours.leaves > 512,
            "kmap: the audit walked a whole identity map, not a stub"
        );
        check!(
            ours.dynamic_violations == 0 && ours.bootstrap_violations == 0,
            "kmap: NO leaf of the kernel's own map is writable+executable — W^X, page AND block"
        );
        // Each class of kernel address, mapped as what it is. `leaf_for` returns `(entry, level)`;
        // level 3 is a 4 KiB page (the image split), level 2 a 2 MiB huge page (bulk RAM/MMIO).
        let text = match crate::kmap::text_probe().and_then(|va| crate::kmap::leaf_for(kroot, va)) {
            Some(v) => v,
            None => return Err((n + 1, "kmap: the image declares no executable section")),
        };
        check!(
            text.1 == 3,
            "kmap: kernel text is mapped at 4 KiB granularity (a 2 MiB block cannot be RO+X alone)"
        );
        check!(
            text.0 & PageTableFlags::NO_EXECUTE.bits() == 0
                && text.0 & PageTableFlags::WRITABLE.bits() == 0,
            "kmap: kernel text is executable AND read-only"
        );
        let data = match crate::kmap::data_probe().and_then(|va| crate::kmap::leaf_for(kroot, va)) {
            Some(v) => v,
            None => return Err((n + 1, "kmap: the image declares no writable section")),
        };
        check!(
            data.1 == 3
                && data.0 & PageTableFlags::WRITABLE.bits() != 0
                && data.0 & PageTableFlags::NO_EXECUTE.bits() != 0,
            "kmap: kernel data is writable and NEVER executable"
        );
        if let Some((ro, level)) =
            crate::kmap::rodata_probe().and_then(|va| crate::kmap::leaf_for(kroot, va))
        {
            check!(
                level == 3
                    && ro & PageTableFlags::WRITABLE.bits() == 0
                    && ro & PageTableFlags::NO_EXECUTE.bits() != 0,
                "kmap: kernel read-only data is neither writable nor executable"
            );
        }
        // Bulk RAM: the frame pool's own base, far from the image, is a huge page — writable so the
        // allocator works, never executable so no data page is a code page.
        let pool = match crate::kmap::leaf_for(kroot, frames::base()) {
            Some(v) => v,
            None => {
                return Err((
                    n + 1,
                    "kmap: the frame pool is not mapped by the kernel's map",
                ))
            }
        };
        check!(
            pool.1 == 2
                && pool.0 & PageTableFlags::WRITABLE.bits() != 0
                && pool.0 & PageTableFlags::NO_EXECUTE.bits() != 0,
            "kmap: RAM outside the image is a 2 MiB RW+NX block — writable, never executable"
        );
        check!(
            crate::kmap::leaf_for(kroot, 0x8000).is_some(),
            "kmap: low memory (the SMP trampoline's home) is identity-mapped by the kernel's map"
        );
        // The split cuts both ways, exactly as on aarch64 and RISC-V: it replaces the huge-page
        // descriptor that made these addresses undescendable with real page tables, so the mapping
        // APIs must refuse the image span explicitly (REQ-MM-006, ALET-P2-032) — otherwise a caller
        // could map a fresh WRITABLE page over kernel text, the write-to-code path W^X exists to
        // close, or unmap `.data` from under the running kernel. The rule lives once in
        // `kernel_core::vmaddr`; this target only declares WHERE its image is, and these two checks
        // prove the declaration is wired into both APIs. Asserted against the LIVE root, whichever
        // tree that is: the refusal must not depend on the map having been activated yet.
        let text_va = match crate::kmap::text_probe() {
            Some(v) => v,
            None => return Err((n + 1, "kmap: the image declares no executable section")),
        };
        let (guard_start, guard_end) = crate::kmap::protected_span();
        check!(
            guard_end > guard_start && text_va >= guard_start && text_va < guard_end,
            "kmap: the image's block-aligned span is declared to the address plan and covers text"
        );
        let spare = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "kmap: no frame for the image-refusal checks")),
        };
        check!(
            !map_kernel_frame(root, text_va as u64, spare.addr() as u64),
            "wx: mapping over the split kernel image is refused (text still maps to itself)"
        );
        check!(
            !unmap_user(root, text_va as u64),
            "wx: unmapping the split kernel image is refused (text still read-only + executable)"
        );
        frames::free(spare);
    }

    // Device mapping (REQ-DRV-005, ADR-037). A driver must be able to map registers the boot map does
    // not cover — a PCI BAR above 4 GiB — but the SAME API must never be usable to alias RAM as MMIO,
    // which would give a task's frame a second mapping with different cacheability and side effects,
    // invisible to the ownership model. The physical rule is therefore INVERTED, not dropped, and both
    // directions are proved here: RAM is refused, and a plainly non-RAM address is accepted.
    {
        let plan = addr_plan();
        let ram_page = match frames::alloc_zeroed() {
            Some(f) => f,
            None => return Err((n + 1, "device: no frame for the device-admission checks")),
        };
        let ram_pa = ram_page.addr();
        check!(
            plan.validate_map_device(ram_pa, ram_pa) == Err(vmaddr::MapFault::PhysIsRam),
            "device: mapping RAM the allocator owns as device memory is refused (no MMIO alias of a frame)"
        );
        check!(
            plan.validate_map(ram_pa, ram_pa).is_ok(),
            "device: the same page is still a legal RAM mapping (the rule is inverted, not stricter)"
        );
        check!(
            !map_device_range(ram_pa, 0x1000),
            "device: the mapping API itself refuses a RAM range, not just the check"
        );
        frames::free(ram_page);
    }

    // Fault classification + re-entrancy (REQ-FAULT-001/002, ADR-039). The model lives in kernel-core
    // and is proved exhaustively on the host; these three invariants prove it is COMPILED INTO this
    // kernel and behaves here — a classification that only holds in `cargo test` protects nothing.
    {
        use kernel_core::faultclass::{
            classify, from_x86_error_code, verdict, FaultKind, FaultVerdict,
        };
        use kernel_core::reentry::ReentryGuard;

        // A user write to a present page: routine, and the only class of fault a supervisor could
        // survive by killing the task.
        let user_write = from_x86_error_code(0b111);
        check!(
            classify(&user_write) == FaultKind::UserPermission
                && verdict(classify(&user_write)) == FaultVerdict::KillTask,
            "fault: a user write to a present page classifies as a user permission fault (kill-task)"
        );
        // A reserved bit set anywhere in the walk means the page tables are corrupt: never survivable,
        // whatever the other bits say.
        let corrupt = from_x86_error_code(0b1111);
        check!(
            classify(&corrupt) == FaultKind::CorruptTranslation
                && verdict(classify(&corrupt)) == FaultVerdict::Panic,
            "fault: a reserved-bit fault classifies as corrupt translation and is never survivable"
        );
        // The guard the fault-reporting path uses really refuses a nested entry on this CPU.
        let guard = ReentryGuard::new();
        let token = guard.enter();
        let nested_refused = token.is_some() && guard.enter().is_none() && guard.refusals() == 1;
        drop(token);
        check!(
            nested_refused && !guard.active(),
            "fault: the re-entrancy guard refuses a nested entry and reopens after leaving"
        );
    }

    // The ring-0 stack's guard page (REQ-MM-007, ALET-P1-012). The stack the CPU loads on every
    // ring3->ring0 transition must fault on overflow rather than continue into `.bss`.
    {
        let guard = crate::gdt::kernel_stack_guard();
        let low = crate::gdt::kernel_stack_low();
        // Against the KERNEL's own map (`kmap::root()`), not `active_root()`: by this point the suite has
        // built and torn down per-process spaces, so CR3 may hold one of those. The guard is a property of
        // the map the kernel built for itself.
        let root = crate::kmap::root();
        check!(
            translate_in(root, guard as u64).is_none(),
            "guard: the ring-0 stack's guard page has NO translation (an overflow faults)"
        );
        check!(
            crate::kmap::leaf_for(root, guard).is_none(),
            "guard: the guard page has no leaf at any level (not merely a bad one)"
        );
        check!(
            translate_in(root, low as u64).is_some()
                && translate_in(root, (low + 0x1000) as u64).is_some(),
            "guard: the stack's own pages are still mapped (the guard cost nothing usable)"
        );
        check!(
            crate::gdt::kernel_stack_top() as usize > low
                && crate::gdt::kernel_stack_top() as usize - low <= 16 * 1024,
            "guard: RSP0 points above the guard, into the usable stack only"
        );
        check!(
            crate::kmap::guard_pages() == 2,
            "guard: the map builder recorded exactly two deliberately-unmapped pages (stack guard + VA 0)"
        );
    }

    // The dead pages are dead in a DERIVED space too (REQ-MM-007/008, ALET-P2-033). Everything above
    // is a property of the map the kernel built for ITSELF; a per-process root is a different tree,
    // and on this target it is built by COPYING the live one — which is how a space that mapped the
    // guard region as a 2 MiB huge page came to exist. A user space reaching an address the kernel's
    // own map deliberately cannot is the guard inverted, so it is checked where it can break.
    {
        let kroot = crate::kmap::root();
        check!(
            audit_dead(kroot).clean() && dead_set().pages() == 2,
            "dead: this map has both dead pages absent, and the declaration names exactly two"
        );
        // A set that declares nothing must FAIL rather than pass vacuously: the audit's own
        // fail-closed posture, proved live rather than only on the host.
        check!(
            !deadva::audit(
                &deadva::DeadSet::new("x86_64"),
                |va| translate_in(kroot, va as u64).map(|pa| pa as usize),
                |va| crate::kmap::leaf_for(kroot, va).is_some(),
            )
            .clean(),
            "dead: an empty declaration is refused, not reported clean (the audit cannot pass vacuously)"
        );
        match build_space() {
            Some(derived) => {
                let report = audit_dead(derived);
                check!(
                    report.clean() && report.pages == 2 && report.spans == 2,
                    "dead: a DERIVED per-process space has both dead pages absent (walked, not assumed)"
                );
                check!(
                    translate_in(derived, 0).is_none()
                        && crate::kmap::leaf_for(derived, crate::gdt::kernel_stack_guard()).is_none(),
                    "dead: unprivileged code cannot reach VA 0 or the kernel stack guard through its own root"
                );
                destroy_space(derived);
            }
            None => {
                check!(
                    false,
                    "dead: a DERIVED per-process space has both dead pages absent (walked, not assumed)"
                );
                check!(
                    false,
                    "dead: unprivileged code cannot reach VA 0 or the kernel stack guard through its own root"
                );
            }
        }
    }

    // The declared layout (REQ-MM-008, ALET-P1-006).
    {
        let l = layout();
        check!(
            l.validate().is_ok(),
            "layout: the declared address-space layout validates (disjoint, aligned, guarded, no null page)"
        );
        let (img_base, _) = crate::kmap::image_span();
        check!(
            l.region_of(img_base).is_some_and(|r| r.name == "kernel-image" && !r.user)
                && l.region_of(0x4000_0000).is_some_and(|r| r.user),
            "layout: kernel text is kernel-only and the user window is user-reachable (no address is both)"
        );
        let kroot = crate::kmap::root();
        check!(
            translate_in(kroot, 0).is_none() && crate::kmap::leaf_for(kroot, 0).is_none(),
            "layout: VA 0 has NO translation in the live map (a kernel null dereference faults)"
        );
    }

    Ok(n)
}

/// Map with EXACTLY the given leaf flags, running the same attribute admission check as the public
/// APIs. Test-only seam: the public APIs choose their own (already legal) flags, so this is how the
/// gate proves an ILLEGAL combination is refused rather than silently corrected.
fn map_user_frame_raw(root: u64, va: u64, pa: u64, flags: PageTableFlags) -> bool {
    if addr_plan().validate_map(va as usize, pa as usize).is_err() {
        return false;
    }
    if !attrs_ok(flags) {
        return false;
    }
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: clearing WP only relaxes ring-0 write protection; restored below.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }
    // SAFETY: `root` is a live PML4; `pa` a real frame; tables come from our own allocator.
    let ok = unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let x86f = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(pa));
        let mut fa = frames::GlobalFrames;
        match mapper.map_to_with_table_flags(page, x86f, flags, parent, &mut fa) {
            Ok(flush) => {
                flush.flush();
                true
            }
            Err(_) => false,
        }
    };
    if wp_was_set {
        // SAFETY: re-arm the write-protect bit exactly as found.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
    ok
}

// ---------------------------------------------------------------------------
// Per-process address spaces + user (ring-3) mappings — the paging half of the user-mode brick
// (`usermode.rs`). The aarch64 twin builds a fresh TTBR0 tree per process; here a fresh PML4 shares
// every KERNEL mapping (so the kernel, handlers, GDT/IDT/TSS, and frame pool stay reachable while
// the task's CR3 is active) yet keeps ONE dedicated PML4 slot private, so mapping a user page in
// space A is invisible to space B — genuine per-process isolation at the same virtual address.
// ---------------------------------------------------------------------------

/// PDPT slot (1 GiB region) reserved for user mappings: 1..2 GiB. It MUST be below 4 GiB — QEMU's
/// OVMF firmware sets the ring-3 code segment with a 4 GiB limit that the CPU enforces on the
/// ring0->ring3 `iret` target, so a user RIP >= 4 GiB faults. 1..2 GiB is below the machine's RAM
/// ceiling usage and below the framebuffer (2 GiB), so clearing it disturbs nothing the kernel uses.
pub const USER_REGION_PDPT_INDEX: usize = 1;

const PTE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;

/// Physical address of the active top-level page table (the PML4 that CR3 points at).
pub fn active_root() -> u64 {
    Cr3::read().0.start_address().as_u64()
}

/// Borrow an arbitrary PML4 (by physical address) as an `OffsetPageTable`.
///
/// SAFETY: `pml4_phys` must be a real, 4 KiB-aligned PML4 frame; under OVMF's identity map it is
/// readable/writable at its own value (phys_offset = 0). Single-core, no preemption.
unsafe fn mapper_for(pml4_phys: u64) -> x86_64::structures::paging::OffsetPageTable<'static> {
    let l4: &'static mut PageTable = &mut *(pml4_phys as *mut PageTable);
    x86_64::structures::paging::OffsetPageTable::new(l4, VirtAddr::new(0))
}

/// Build a fresh address space that shares every kernel mapping yet keeps the user region private.
///
/// The whole live PML4 is copied (shares all high-slot mappings). Then, because our user region
/// lives below 4 GiB inside PML4[0], we give this process a PRIVATE copy of the live low PDPT and
/// clear its `USER_REGION_PDPT_INDEX` slot — so mapping a user page there allocates per-space
/// tables (invisible to other processes) while the shared kernel/RAM/framebuffer identity mappings
/// (the other PDPT slots) are preserved. PML4[0] is pointed at the private PDPT and marked
/// USER-accessible so ring 3 can walk to its pages (kernel leaves stay supervisor, so the U/S AND
/// keeps them ring-0-only). Returns the new PML4 physical address, or `None` on exhaustion.
pub fn build_space() -> Option<u64> {
    let pml4 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr() as u64;
    let pdpt = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr() as u64;
    // Copy the KERNEL's own map, not merely whatever CR3 holds (REQ-MM-007/008, ALET-P2-033). This
    // is the line the hole came in through: `kmap::activate()` runs AFTER the virtual-memory suite,
    // so a space built during it copied OVMF's tree — which maps VA 0 as RAM and covers the ring-0
    // stack guard with a 2 MiB huge page. The derived space inherited both, and ring 3 could reach
    // two addresses the kernel's own map deliberately cannot. Sourcing from `kmap::root()` makes the
    // property hold by CONSTRUCTION rather than by activation order; `active_root()` remains the
    // fallback for the window before the kernel's map exists at all.
    let cur = space_source_root();
    // SAFETY: PML4/PDPT frames are 4 KiB and identity-accessible under OVMF's map; single-core.
    unsafe {
        let src = cur as *const u64;
        let dst = pml4 as *mut u64;
        for i in 0..512 {
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        }
        // PML4[0] always points to a PDPT (there are no 512 GiB pages). Copy it, then privatize.
        let live_pdpt = (core::ptr::read_volatile(src) & PTE_ADDR_MASK) as *const u64;
        let new_pdpt = pdpt as *mut u64;
        for i in 0..512 {
            core::ptr::write_volatile(new_pdpt.add(i), core::ptr::read_volatile(live_pdpt.add(i)));
        }
        core::ptr::write_volatile(new_pdpt.add(USER_REGION_PDPT_INDEX), 0);
        core::ptr::write_volatile(dst, pdpt | PTE_PRESENT | PTE_WRITABLE | PTE_USER);
    }
    // REQ-MM-007/008, ALET-P2-033: the dead pages must be dead HERE too, and this is the target the
    // property actually escaped from — the copy above inherits whatever the live tree holds, and a
    // space copied before `kmap` was active mapped the guard region as one 2 MiB huge page, so ring 3
    // could reach an address the kernel's own map deliberately cannot.
    //
    // The rule is a CHECKED PRECONDITION rather than a patch, because below the private PDPT this
    // space SHARES the kernel's tables: clearing a descriptor here would clear it in the kernel's map
    // too. So inheritance is the mechanism and the audit is what stops it being an assumption — a
    // source tree that does not have these pages dead yields no space at all instead of a space that
    // can reach them.
    if !audit_dead(pml4).clean() {
        frames::free_addr_as(pdpt as usize, Owner::PAGETABLE);
        frames::free_addr_as(pml4 as usize, Owner::PAGETABLE);
        return None;
    }
    Some(pml4)
}

/// The tree [`build_space`] derives a per-process space FROM: the kernel's own map once it exists,
/// and only otherwise whatever CR3 holds.
///
/// The distinction is the substance of ALET-P2-033. `kmap::activate()` runs AFTER the virtual-memory
/// suite, so a space built during it used to copy OVMF's tree — which maps VA 0 as RAM and covers the
/// ring-0 stack guard with a 2 MiB huge page. The derived space inherited both, and ring 3 could
/// reach two addresses the kernel's own map deliberately cannot: the guard inverted, protecting the
/// less privileged tree and not the more privileged one. Sourcing from `kmap::root()` makes the dead
/// pages a property of CONSTRUCTION rather than of activation order.
pub fn space_source_root() -> u64 {
    match crate::kmap::root() {
        0 => active_root(),
        r => r,
    }
}

/// The virtual addresses this target declares permanently dead, in EVERY address space
/// (REQ-MM-007/008, ALET-P2-033): VA 0 and the ring-0 stack guard — exactly the two pages
/// [`crate::kmap::guard_pages`] counts as deliberately skipped, named here so the audit and the
/// builder cannot drift apart.
pub fn dead_set() -> deadva::DeadSet {
    deadva::DeadSet::new("x86_64")
        .with(deadva::DeadSpan::page("null", 0))
        .with(deadva::DeadSpan::page(
            "kernel-stack-guard",
            crate::gdt::kernel_stack_guard(),
        ))
}

/// Audit any address space — the kernel's own root or a derived per-process one — for the dead
/// pages. Asks both questions the property needs: the page must not TRANSLATE, and no descriptor at
/// any level may still cover it (a live block descriptor is one split away from reviving it).
pub fn audit_dead(root: u64) -> deadva::DeadReport {
    deadva::audit(
        &dead_set(),
        |va| translate_in(root, va as u64).map(|pa| pa as usize),
        |va| crate::kmap::leaf_for(root, va).is_some(),
    )
}

/// This target's address-space geometry, for the arch-independent mapping-API admission check
/// (ALET-P1-001, REQ-MM-001). 4-level paging decodes 48 virtual-address bits and the ISA requires
/// bits 63:47 to repeat bit 47; an address in the non-canonical hole is a #GP, not a page fault.
/// Both matter here beyond correctness: `Page::containing_address` silently TRUNCATES a misaligned
/// VA to its page base (mapping a different page than the caller named), and `VirtAddr::new`
/// PANICS on a non-canonical address — so the check converts two crash/corruption paths into a
/// fail-closed `false`. The physical window is read from the frame allocator itself.
fn addr_plan() -> AddrPlan {
    // The image split is the same rule the other two targets declare (REQ-MM-006, ALET-P2-032) — it
    // matters here only once the kernel's own map is ACTIVE, because until then the level above the
    // image is one of OVMF's descriptors, not a table this kernel split. Declaring it
    // unconditionally is the fail-closed order: the refusal exists before the map that needs it.
    let (protected_start, protected_end) = crate::kmap::protected_span();
    AddrPlan::new(
        48,
        true,
        frames::base(),
        frames::total_count() * vmaddr::PAGE_SIZE,
    )
    .with_protected(protected_start, protected_end)
}

/// Software translate `va` in address space `root` (or `None` if unmapped). Used to assert the user
/// slot is empty before trusting it as private, and to prove per-process isolation.
pub fn translate_in(root: u64, va: u64) -> Option<u64> {
    use x86_64::structures::paging::mapper::Translate;
    // SAFETY: `root` is a valid identity-accessible PML4; single-core.
    unsafe {
        mapper_for(root)
            .translate_addr(VirtAddr::new(va))
            .map(|p| p.as_u64())
    }
}

/// Map `va -> pa` in `root` as a ring-3 (user-accessible) page; `writable` sets RW+NX vs
/// read/execute. Every intermediate table is created USER_ACCESSIBLE (else ring 3 would fault on
/// its OWN pages).
///
/// W^X (REQ-MM-006, ADR-034): a writable page is mapped NO_EXECUTE, so a ring-3 write primitive
/// cannot become ring-3 (or ring-0) code execution. This requires EFER.NXE, which firmware does not
/// guarantee — [`enable_nx`] turns it on at boot after checking CPUID, and reports whether it could.
pub fn map_user_frame(root: u64, va: u64, pa: u64, writable: bool) -> bool {
    if addr_plan().validate_map(va as usize, pa as usize).is_err() {
        return false;
    }
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    }
    if !attrs_ok(flags) {
        return false;
    }
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    // SAFETY: `root` is a PML4 we built; `pa` is a real frame; intermediate tables come from our
    // own allocator via `GlobalFrames`. Single-core, no preemption.
    unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let x86f = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(pa));
        let mut fa = frames::GlobalFrames;
        match mapper.map_to_with_table_flags(page, x86f, flags, parent, &mut fa) {
            Ok(flush) => {
                flush.flush();
                true
            }
            Err(_) => false,
        }
    }
}

/// Allocate a fresh zeroed frame and map it USER at `va` in `root`. Returns the backing frame (so
/// the caller can reclaim it) or `None` on exhaustion/failure.
pub fn map_user(root: u64, va: u64, writable: bool) -> Option<frames::Frame> {
    let f = frames::alloc_zeroed_as(Owner::USER)?;
    if map_user_frame(root, va, f.addr() as u64, writable) {
        Some(f)
    } else {
        frames::free_as(f, Owner::USER);
        None
    }
}

/// Map `bytes` into a fresh USER (read/execute) code page at `va` in `root`: copy the bytes into a
/// zeroed frame (x86 caches are coherent for I/D, so no explicit sync is needed), then map it.
pub fn map_stub_frame(root: u64, va: u64, bytes: &[u8]) -> Option<frames::Frame> {
    let f = frames::alloc_zeroed_as(Owner::USER)?;
    let pa = f.addr();
    // SAFETY: `f` is a fresh, identity-accessible frame we hold; `bytes` fits in one 4 KiB page.
    unsafe {
        let dst = pa as *mut u8;
        for (i, b) in bytes.iter().enumerate() {
            core::ptr::write_volatile(dst.add(i), *b);
        }
    }
    if map_user_frame(root, va, pa as u64, false) {
        Some(f)
    } else {
        frames::free_as(f, Owner::USER);
        None
    }
}

/// Map a fresh PRESENT but SUPERVISOR (no USER bit) page at `va` in `root`. A ring-3 read of it is
/// a guaranteed U/S-violation `#PF` (the parents are USER, only the leaf is supervisor) — the
/// OVMF-independent way to prove the ring-3 -> kernel-memory isolation boundary. Returns the frame.
pub fn map_supervisor(root: u64, va: u64) -> Option<frames::Frame> {
    if addr_plan().validate_unmap(va as usize).is_err() {
        return None;
    }
    let f = frames::alloc_zeroed_as(Owner::USER)?;
    // Deliberately NO user bit; writable ⇒ NO_EXECUTE (REQ-MM-006, ADR-034).
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    // SAFETY: see `map_user_frame`; single-core.
    unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let x86f = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(f.addr() as u64));
        let mut fa = frames::GlobalFrames;
        match mapper.map_to_with_table_flags(page, x86f, flags, parent, &mut fa) {
            Ok(flush) => {
                flush.flush();
                Some(f)
            }
            Err(_) => {
                frames::free_as(f, Owner::USER);
                None
            }
        }
    }
}

/// Map `va -> pa` in `root` as a SUPERVISOR (ring-0-only, no USER bit) PRESENT|WRITABLE page — the
/// kernel-owned counterpart to [`map_user_frame`]. Used by the SMP TLB-shootdown suite for a page
/// the kernel (ring 0) reads on every core regardless of SMAP, without exposing it to ring 3.
/// Returns `false` if the VA is already mapped (remap = [`unmap_user`] then map) or on exhaustion.
pub fn map_kernel_frame(root: u64, va: u64, pa: u64) -> bool {
    if addr_plan().validate_map(va as usize, pa as usize).is_err() {
        return false;
    }
    // Writable ⇒ never executable (REQ-MM-006, ADR-034).
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    if !attrs_ok(flags) {
        return false;
    }
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    // When mapping into the LIVE (firmware-owned) root, `map_to` must write a new entry into an
    // upper-level table OVMF mapped READ-ONLY — a ring-0 write there #PFs (PROTECTION_VIOLATION)
    // unless CR0.WP is cleared. Mirror `selftest`'s dance: clear WP across the map, restore it after.
    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: clearing WP only relaxes ring-0 write protection; restored below. Single-core.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }
    // SAFETY: `root` is a live/own PML4; `pa` is a real frame; intermediate tables come from our
    // own allocator via `GlobalFrames`. `flush.flush()` invalidates the BSP's own TLB entry.
    let ok = unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let x86f = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(pa));
        let mut fa = frames::GlobalFrames;
        match mapper.map_to_with_table_flags(page, x86f, flags, parent, &mut fa) {
            Ok(flush) => {
                flush.flush();
                true
            }
            Err(_) => false,
        }
    };
    if wp_was_set {
        // SAFETY: re-arm the write-protect bit exactly as found.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
    ok
}

/// Identity-map `[base, base+len)` as **device** memory (RW, never executable) in the ACTIVE root, so a
/// driver can reach registers the kernel's boot-time map does not cover (REQ-DRV-005, ADR-037).
///
/// This exists because a PCI BAR is wherever the firmware put it — on q35, above 4 GiB, outside both
/// the kernel's MMIO coverage and the frame-allocator window. Every page goes through
/// `AddrPlan::validate_map_device`, which applies all the ordinary VA rules but **inverts** the
/// physical rule: the address must NOT be RAM the allocator owns, so this API can never be used to
/// alias a task's frame as MMIO. Pages already mapped are left alone (the boot map already covers
/// sub-4 GiB MMIO); a refusal anywhere returns `false` having mapped nothing further.
pub fn map_device_range(base: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let plan = addr_plan();
    let first = base & !0xFFF;
    let last = (base + len - 1) & !0xFFF;
    // Device memory is writable, so W^X makes it non-executable — and `memattr` refuses an executable
    // device mapping outright (AttrFault::ExecutableDevice).
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    if !attrs_ok(flags) {
        return false;
    }
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let root = active_root();

    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: clearing WP only relaxes ring-0 write protection; restored below. Single-core here.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }
    let mut ok = true;
    let mut pa = first;
    while pa <= last {
        if plan.validate_map_device(pa, pa).is_err() {
            ok = false;
            break;
        }
        if translate_in(root, pa as u64).is_none() {
            // SAFETY: `root` is the live PML4; `pa` is device memory outside the allocator window
            // (checked above); intermediate tables come from our own allocator.
            let mapped = unsafe {
                let mut mapper = mapper_for(root);
                let page = Page::<Size4KiB>::containing_address(VirtAddr::new(pa as u64));
                let frame = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(pa as u64));
                let mut fa = frames::GlobalFrames;
                match mapper.map_to_with_table_flags(page, frame, flags, parent, &mut fa) {
                    Ok(flush) => {
                        flush.flush();
                        true
                    }
                    Err(_) => false,
                }
            };
            if !mapped {
                ok = false;
                break;
            }
        }
        pa += 0x1000;
    }
    if wp_was_set {
        // SAFETY: re-arm the write-protect bit exactly as found.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
    ok
}

/// Remove the mapping for `va` in `root` (ignoring an already-absent mapping). Returns `false` when
/// the request is refused by the admission check or no mapping was present, so a caller that cares
/// (the selftest) can observe a refusal; the ordinary teardown callers ignore it.
pub fn unmap_user(root: u64, va: u64) -> bool {
    if addr_plan().validate_unmap(va as usize).is_err() {
        return false;
    }
    // The walk is captured BEFORE the unmap, while the chain is still intact — afterwards the leaf
    // entry is gone but the tables are exactly the ones that may now be empty.
    let path = walk_path(root, va);
    // SAFETY: `root` is identity-accessible; single-core. An unmapped `va` yields `Err`.
    let unmapped = unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        match mapper.unmap(page) {
            Ok((_f, flush)) => {
                flush.flush();
                true
            }
            Err(_) => false,
        }
    };
    if unmapped {
        if let Some(path) = path {
            // REQ-MM-003 / ADR-031: `Mapper::unmap` clears the leaf entry and stops, leaving every
            // intermediate table allocated AND still referenced. Reclaim upward, never the root.
            // CR0.WP must be down for the same reason the map path clears it: an upper-level table
            // may be a firmware page OVMF left read-only, and clearing its entry is a ring-0 write.
            let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
            if wp_was_set {
                // SAFETY: clearing WP only relaxes ring-0 write protection; restored below.
                unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
            }
            if let Ok(r) = ptreclaim::reclaim_empty_tables(&path, &mut Tables) {
                TABLES_RECLAIMED.fetch_add(r.tables_freed, Ordering::Relaxed);
            }
            if wp_was_set {
                // SAFETY: re-arm the write-protect bit exactly as found.
                unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
            }
            // The detached ancestors can leave stale paging-structure cache entries for this VA.
            x86_64::instructions::tlb::flush(VirtAddr::new(va));
        }
    }
    unmapped
}

/// Switch the active address space by writing CR3 (preserving the current CR3 flags).
///
/// # Safety
/// `root` must be a PML4 that maps the currently-executing kernel (guaranteed by `build_space`,
/// which copies every kernel slot); otherwise the next instruction fetch faults. Single-core.
pub unsafe fn switch_to(root: u64) {
    let (_frame, flags) = Cr3::read();
    let f = X86PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(root));
    Cr3::write(f, flags);
}
