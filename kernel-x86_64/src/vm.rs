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
    let pml4 = frames::alloc_zeroed()?.addr() as u64;
    let pdpt = frames::alloc_zeroed()?.addr() as u64;
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
    let f = frames::alloc_zeroed()?;
    if map_user_frame(root, va, f.addr() as u64, writable) {
        Some(f)
    } else {
        frames::free(f);
        None
    }
}

/// Map `bytes` into a fresh USER (read/execute) code page at `va` in `root`: copy the bytes into a
/// zeroed frame (x86 caches are coherent for I/D, so no explicit sync is needed), then map it.
pub fn map_stub_frame(root: u64, va: u64, bytes: &[u8]) -> Option<frames::Frame> {
    let f = frames::alloc_zeroed()?;
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
        frames::free(f);
        None
    }
}

/// Map a fresh PRESENT but SUPERVISOR (no USER bit) page at `va` in `root`. A ring-3 read of it is
/// a guaranteed U/S-violation `#PF` (the parents are USER, only the leaf is supervisor) — the
/// OVMF-independent way to prove the ring-3 -> kernel-memory isolation boundary. Returns the frame.
pub fn map_supervisor(root: u64, va: u64) -> Option<frames::Frame> {
    let f = frames::alloc_zeroed()?;
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
                frames::free(f);
                None
            }
        }
    }
}

/// Remove the mapping for `va` in `root` (ignoring an already-absent mapping).
pub fn unmap_user(root: u64, va: u64) {
    // SAFETY: `root` is identity-accessible; single-core. An unmapped `va` yields `Err`, ignored.
    unsafe {
        let mut mapper = mapper_for(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        if let Ok((_f, flush)) = mapper.unmap(page) {
            flush.flush();
        }
    }
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
