//! Virtual memory — the RISC-V Sv39 MMU brought up on real page tables (PRD P5), the second
//! memory-management brick on this first-class target, bringing it to parity with the aarch64
//! `kernel/src/vm.rs` and the x86-64 image.
//!
//! WHY THIS MATTERS: until now the RISC-V kernel ran with paging OFF (`satp = Bare`), in a flat
//! physical space. An operating system isolates programs by giving each its own *virtual* address
//! space, which requires the CPU's translation hardware. This module is the first live translation
//! regime on RISC-V: it builds Sv39 page tables out of `frames::` frames, identity-maps the running
//! kernel + peripherals so nothing breaks when translation turns on, enables paging via `satp`, and
//! then proves *dynamic* virtual memory — mapping a fresh frame at a brand-new virtual address,
//! writing through the VA, and observing the bytes land in the different physical frame the VA now
//! points at.
//!
//! SCOPE (contract-honest, ADR-010/ADR-019): riscv64 backend, Sv39 (39-bit VA, 3 levels, 4 KiB
//! granule), identity map. Higher-half split and Sv48 are follow-on bricks. Every line here executes
//! under QEMU and is asserted by `scripts/vm-e2e-riscv.sh`; a wrong table faults to `exit 102`
//! (the trap handler), never a silent hang.
//!
//! Sv39 PTE ENCODING (verified against the RISC-V privileged spec, S-mode Sv39 translation): a PTE
//! is 64-bit; bits [9:0] are flags, the physical page number sits in bits [53:10] (`PPN = pa >> 12`,
//! placed by `<< 10`). Flags: V(valid,0) R(read,1) W(write,2) X(exec,3) U(user,4) G(global,5)
//! A(accessed,6) D(dirty,7). A PTE with any of R/W/X set is a LEAF (a mapping); with none set it is
//! a POINTER to the next-level table. **Every leaf sets A and D** — the RISC-V analogue of the
//! aarch64 Access Flag: leaving them clear lets an implementation fault on first access, so setting
//! them up front is the single highest-leverage anti-hang move (the aarch64 backend makes the same
//! move with AF). A gigapage leaf may sit at level 2 (1 GiB), a megapage leaf at level 1 (2 MiB), a
//! 4 KiB page at level 0.
use crate::frames;
use core::arch::asm;

// --- Fixed platform addresses (QEMU virt, RISC-V) ------------------------------------------
const RAM_BASE: usize = frames::RAM_BASE; // 0x8000_0000
/// QEMU `virt` NS16550A UART base (the console MMIO); inside the peripheral GiB.
const UART_BASE: usize = 0x1000_0000;
const MEG_2M: usize = 0x20_0000;
const GIB: usize = 0x4000_0000;

// --- Sv39 PTE flag bits --------------------------------------------------------------------
const PTE_V: u64 = 1 << 0; // valid
const PTE_R: u64 = 1 << 1; // readable
const PTE_W: u64 = 1 << 2; // writable
const PTE_X: u64 = 1 << 3; // executable
const PTE_U: u64 = 1 << 4; // user-accessible (U-mode)
const PTE_A: u64 = 1 << 6; // accessed — set to avoid fault-on-first-access
const PTE_D: u64 = 1 << 7; // dirty — set for the same reason on writable leaves

/// The R|W|X mask that distinguishes a leaf (mapping) from a pointer (next-level table).
const PTE_RWX: u64 = PTE_R | PTE_W | PTE_X;
/// PPN field mask: bits [53:10] hold a 44-bit physical page number.
const PPN_MASK: u64 = (1 << 44) - 1;

// --- Leaf attribute sets -------------------------------------------------------------------
/// Device MMIO leaf (peripheral GiB): RW, no execute, no user. A/D set.
const DEV_LEAF: u64 = PTE_V | PTE_R | PTE_W | PTE_A | PTE_D;
/// Kernel RAM leaf: RWX (holds kernel code + data + stack + heap). A/D set.
const RAM_LEAF: u64 = PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D;
/// Normal-memory 4 KiB kernel page (dynamic mappings): RW. A/D set.
pub const NORMAL_PAGE: u64 = PTE_V | PTE_R | PTE_W | PTE_A | PTE_D;
/// U-mode executable code page: user R/W/X. A/D set.
pub const USER_CODE: u64 = PTE_V | PTE_R | PTE_W | PTE_X | PTE_U | PTE_A | PTE_D;
/// U-mode data/stack page: user R/W, not executable. A/D set.
pub const USER_DATA: u64 = PTE_V | PTE_R | PTE_W | PTE_U | PTE_A | PTE_D;

/// Sv39 mode nibble for the `satp.MODE` field (bits 63:60).
const SATP_MODE_SV39: u64 = 8 << 60;

/// Build a leaf/pointer PTE from a physical base address + flags. `pa` is assumed frame-aligned.
#[inline]
const fn pte(pa: usize, flags: u64) -> u64 {
    (((pa as u64) >> 12) << 10) | flags
}

/// Physical base address referenced by a PTE (PPN << 12), ignoring flags/offset.
#[inline]
const fn pte_pa(entry: u64) -> usize {
    (((entry >> 10) & PPN_MASK) << 12) as usize
}

#[inline]
const fn is_leaf(entry: u64) -> bool {
    entry & PTE_RWX != 0
}

#[inline]
unsafe fn read_entry(table: usize, idx: usize) -> u64 {
    core::ptr::read_volatile((table + idx * 8) as *const u64)
}
#[inline]
unsafe fn write_entry(table: usize, idx: usize, val: u64) {
    core::ptr::write_volatile((table + idx * 8) as *mut u64, val);
}

/// Sv39 VA -> (VPN[2], VPN[1], VPN[0]).
#[inline]
fn indices(va: usize) -> (usize, usize, usize) {
    ((va >> 30) & 0x1ff, (va >> 21) & 0x1ff, (va >> 12) & 0x1ff)
}

/// Build an identity-mapping Sv39 table tree from fresh frames and return the level-2 root
/// physical address. Maps the peripheral GiB (0..1 GiB) as ONE device gigapage leaf (covers the
/// NS16550A UART, CLINT, PLIC, and SiFive-test) and the 128 MiB of RAM as 2 MiB megapage leaves.
/// Returns `None` if the frame allocator is exhausted. Tables live in RAM, so they stay reachable
/// at their identity address once paging is on.
pub fn build_identity() -> Option<usize> {
    let root = frames::alloc_zeroed()?.addr();

    // Root[0] -> peripheral GiB (0..1 GiB) as a Device gigapage leaf.
    // SAFETY: `root` is a fresh, in-RAM, identity-accessible table; index 0 < 512.
    unsafe { write_entry(root, 0, pte(0, DEV_LEAF)) };

    // Root[2] -> a level-1 table of RAM megapages. RAM occupies 0x8000_0000.., which lands in
    // level-2 index 2 (0x8000_0000 >> 30 == 2); within that gigabyte the RAM megapages start at
    // level-1 index 0.
    let l1 = frames::alloc_zeroed()?.addr();
    let ram_megs = (frames::RAM_END - RAM_BASE) / MEG_2M;
    for i in 0..ram_megs {
        let pa = RAM_BASE + i * MEG_2M;
        // SAFETY: `l1` fresh in-RAM frame; `i` < ram_megs (== 64) < 512.
        unsafe { write_entry(l1, i, pte(pa, RAM_LEAF)) };
    }
    // SAFETY: RAM_BASE is in level-2 index 2; `l1` is a pointer PTE (only V set -> next level).
    unsafe { write_entry(root, (RAM_BASE >> 30) & 0x1ff, pte(l1, PTE_V)) };

    Some(root)
}

/// Software page-table walk: translate `va` to its physical address using `root`, or `None` if
/// unmapped. Used to *assert the map is correct before paging is enabled* (turning a would-be
/// silent hang into a testable pre-check) and to verify dynamic map/unmap afterwards.
pub fn translate(root: usize, va: usize) -> Option<usize> {
    let (i2, i1, i0) = indices(va);
    // SAFETY: all reads are of 8-byte-aligned entries inside identity-accessible RAM tables.
    unsafe {
        let e2 = read_entry(root, i2);
        if e2 & PTE_V == 0 {
            return None;
        }
        if is_leaf(e2) {
            // 1 GiB gigapage.
            return Some(pte_pa(e2) | (va & (GIB - 1)));
        }
        let t1 = pte_pa(e2);
        let e1 = read_entry(t1, i1);
        if e1 & PTE_V == 0 {
            return None;
        }
        if is_leaf(e1) {
            // 2 MiB megapage.
            return Some(pte_pa(e1) | (va & (MEG_2M - 1)));
        }
        let t0 = pte_pa(e1);
        let e0 = read_entry(t0, i0);
        if e0 & PTE_V == 0 {
            return None;
        }
        Some(pte_pa(e0) | (va & 0xFFF))
    }
}

/// Map a single 4 KiB `va -> pa` with `flags`, creating intermediate tables from fresh frames as
/// needed. Returns `false` on allocator exhaustion or if an intermediate level is already a leaf
/// (this wave never splits a giga/megapage). Fences the TLB for `va`.
pub fn map_page(root: usize, va: usize, pa: usize, flags: u64) -> bool {
    let (i2, i1, i0) = indices(va);
    // SAFETY: table entries are identity-accessible; new tables come from `frames::alloc_zeroed`.
    unsafe {
        let e2 = read_entry(root, i2);
        let t1 = if e2 & PTE_V == 0 {
            let t = match frames::alloc_zeroed() {
                Some(f) => f.addr(),
                None => return false,
            };
            write_entry(root, i2, pte(t, PTE_V));
            t
        } else if is_leaf(e2) {
            return false;
        } else {
            pte_pa(e2)
        };

        let e1 = read_entry(t1, i1);
        let t0 = if e1 & PTE_V == 0 {
            let t = match frames::alloc_zeroed() {
                Some(f) => f.addr(),
                None => return false,
            };
            write_entry(t1, i1, pte(t, PTE_V));
            t
        } else if is_leaf(e1) {
            return false;
        } else {
            pte_pa(e1)
        };

        write_entry(t0, i0, pte(pa, flags));
        sfence_va(va);
    }
    true
}

/// Unmap the 4 KiB page at `va` (clear its level-0 entry) and fence its TLB entry. Returns `false`
/// if the page was not present as a 4 KiB mapping.
pub fn unmap_page(root: usize, va: usize) -> bool {
    let (i2, i1, i0) = indices(va);
    // SAFETY: identity-accessible table walk; writing a zero (invalid) entry is always sound.
    unsafe {
        let e2 = read_entry(root, i2);
        if e2 & PTE_V == 0 || is_leaf(e2) {
            return false;
        }
        let t1 = pte_pa(e2);
        let e1 = read_entry(t1, i1);
        if e1 & PTE_V == 0 || is_leaf(e1) {
            return false;
        }
        let t0 = pte_pa(e1);
        if read_entry(t0, i0) & PTE_V == 0 {
            return false;
        }
        write_entry(t0, i0, 0);
        sfence_va(va);
    }
    true
}

/// Fence the TLB for one VA (`sfence.vma va, x0`).
#[inline]
unsafe fn sfence_va(va: usize) {
    asm!("sfence.vma {v}, zero", v = in(reg) va, options(nostack));
}

/// Full TLB + page-walk-cache fence (`sfence.vma x0, x0`) — used on satp writes.
#[inline]
unsafe fn sfence_all() {
    asm!("sfence.vma zero, zero", options(nostack));
}

/// Enable Sv39 paging with `root` as the translation root: write `satp = (MODE=Sv39 | PPN)` and
/// fence. Precondition: `root`'s tables identity-map the code, stack, and heap currently in use
/// (asserted by a software walk before this is called) — otherwise the very next instruction fetch
/// faults.
///
/// SAFETY: enabling translation with tables that do not cover the running kernel would fault
/// immediately. The caller guarantees an identity map built by `build_identity` + a pre-enable
/// `translate` assertion.
pub unsafe fn enable(root: usize) {
    let satp = SATP_MODE_SV39 | ((root as u64) >> 12);
    asm!("csrw satp, {v}", v = in(reg) satp, options(nostack));
    sfence_all();
}

/// The live translation root in use by the CPU (`satp.PPN << 12`). Lets a later brick (U-mode)
/// map fresh user pages into the *active* address space rather than a throwaway test table. Only
/// meaningful after `enable`.
pub fn active_root() -> usize {
    let satp: u64;
    // SAFETY: reading satp is always sound at S-mode.
    unsafe { asm!("csrr {v}, satp", v = out(reg) satp, options(nomem, nostack)) };
    ((satp & PPN_MASK) << 12) as usize
}

/// Switch the active address space by pointing `satp` at `root` (keeping MODE=Sv39), then fencing
/// the whole TLB so no stale translation from the previous space survives. This is what gives each
/// process its own view of memory: after the switch, the SAME virtual address resolves through
/// `root`'s tables (or faults if `root` does not map it).
///
/// PRECONDITION (load-bearing): `root` MUST replicate the kernel identity map (code, stack, trap
/// vector, UART, and all kernel statics at their identity PAs) — otherwise the instruction stream
/// doing the switch becomes unmapped and faults. `build_identity()` guarantees this.
///
/// SAFETY: caller guarantees `root` identity-maps the running kernel (see precondition).
pub unsafe fn switch_address_space(root: usize) {
    let satp = SATP_MODE_SV39 | ((root as u64) >> 12);
    asm!("csrw satp, {v}", v = in(reg) satp, options(nostack));
    sfence_all();
}

/// Whether Sv39 paging is currently enabled (`satp.MODE == 8`).
pub fn mmu_enabled() -> bool {
    let satp: u64;
    // SAFETY: reading satp is always sound at S-mode.
    unsafe { asm!("csrr {v}, satp", v = out(reg) satp, options(nomem, nostack)) };
    (satp >> 60) == 8
}

// ---------------------------------------------------------------------------
// Selftest — virtual-memory invariants, riscv64-only (NOT in the shared `selftest.rs`).
// Order matters: the identity map is proved by a *software* walk BEFORE paging is enabled, so a
// construction bug is caught as a failed assertion rather than a hang. After enable, dynamic
// map/unmap is proved by writing through a fresh VA and observing the bytes in a different frame.
// ---------------------------------------------------------------------------

/// Dynamic-mapping test VA: `RAM_END` (0x8800_0000) sits just past the identity-mapped RAM
/// megapages (level-1 index 64), so it is guaranteed unmapped until we map it — proving translation,
/// not identity.
const TEST_VA: usize = frames::RAM_END;
const PATTERN: u64 = 0x5EED_2026_A1E7_0001;

/// Prove the virtual-memory invariants live. `Ok(n)` all passed; `Err((idx,name))` = failure.
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
        "vm: identity Sv39 page tables built from frames"
    );

    // 2..4 — SOFTWARE-WALK ASSERT before enabling paging (catches a bad map without a hang).
    check!(
        translate(root, RAM_BASE) == Some(RAM_BASE),
        "vm: RAM identity-maps (pre-enable walk)"
    );
    check!(
        translate(root, UART_BASE) == Some(UART_BASE),
        "vm: device MMIO identity-maps (UART, pre-enable walk)"
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

    // 5 — ENABLE PAGING. A faulty identity map faults here -> exit 102 (clean fail, not a hang).
    check!(!mmu_enabled(), "vm: paging off before enable");
    // SAFETY: `root` identity-maps the running code/stack/heap/UART, asserted by checks 2..4.
    unsafe { enable(root) };
    // If we reach this line, translation is live and the kernel is still executing under it.
    kprintln!("  [info  ] Sv39 paging enabled — kernel still executing under translation");
    check!(
        mmu_enabled(),
        "vm: Sv39 paging enabled and kernel survives translation"
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

    frames::free(frame);
    Ok(n)
}
