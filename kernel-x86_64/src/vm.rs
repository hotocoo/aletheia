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
    check!(resolves, "vm: mapped VA resolves to the frame (translation follows the new entry)");
    check!(write_through, "vm: write via VA lands in the mapped physical frame");
    check!(unmapped, "vm: unmap removes the page; VA no longer resolves");

    Ok(n)
}
