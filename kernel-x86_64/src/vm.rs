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
use x86_64::structures::paging::mapper::{Mapper, Translate};
use x86_64::structures::paging::{
    Page, PageTable, PageTableFlags, PhysFrame as X86PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::frames;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_core::frameown::Owner;
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
        let ops = Tables;
        let mut shared_slot = None;
        for i in 1..512 {
            let live = ops.read(active_root() as usize, i);
            if live != 0 && ops.read(victim as usize, i) == live {
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
            ops.read(active_root() as usize, shared_index) == shared_entry,
            "vm: the shared kernel mapping the victim copied is untouched by the teardown"
        );
        // The kernel is still running, still translating, and its own space is untouched — proved
        // by the fact that this very check executes and the live root still maps the kernel.
        check!(
            translate_in(active_root(), TEST_VA).is_none(),
            "vm: the surviving address space is intact after the teardown"
        );
    }

    Ok(n)
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
    let cur = active_root();
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
    Some(pml4)
}

/// This target's address-space geometry, for the arch-independent mapping-API admission check
/// (ALET-P1-001, REQ-MM-001). 4-level paging decodes 48 virtual-address bits and the ISA requires
/// bits 63:47 to repeat bit 47; an address in the non-canonical hole is a #GP, not a page fault.
/// Both matter here beyond correctness: `Page::containing_address` silently TRUNCATES a misaligned
/// VA to its page base (mapping a different page than the caller named), and `VirtAddr::new`
/// PANICS on a non-canonical address — so the check converts two crash/corruption paths into a
/// fail-closed `false`. The physical window is read from the frame allocator itself.
fn addr_plan() -> AddrPlan {
    AddrPlan::new(
        48,
        true,
        frames::base(),
        frames::total_count() * vmaddr::PAGE_SIZE,
    )
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

/// Map `va -> pa` in `root` as a ring-3 (user-accessible) page; `writable` sets RW vs read/execute.
/// Every intermediate table is created USER_ACCESSIBLE (else ring 3 would fault on its OWN pages).
/// We do NOT set the NX bit (EFER.NXE is not guaranteed by firmware), so pages are effectively RWX
/// to ring 3 — W^X is not one of the invariants this milestone proves.
pub fn map_user_frame(root: u64, va: u64, pa: u64, writable: bool) -> bool {
    if addr_plan().validate_map(va as usize, pa as usize).is_err() {
        return false;
    }
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE;
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
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE; // deliberately NO user bit
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
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE; // supervisor leaf (no USER)
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
