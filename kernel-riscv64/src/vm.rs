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
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_core::frameown::Owner;
use kernel_core::memattr::{self, AttrOps, MemKind, PageAttrs};
use kernel_core::ptreclaim::{self, PathStep, TableOps};
use kernel_core::teardown::{self, SpaceOps, Teardown};
use kernel_core::vmaddr::{self, AddrPlan};

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
/// U-mode executable code page: user R+X, NOT writable. Read-only by W^X (REQ-MM-006, ADR-034) —
/// a page U-mode can both write and execute is the mapping that turns any user write primitive into
/// code execution. The stub is copied through the frame's kernel identity address before this
/// mapping exists, so nothing writes it through this PTE.
pub const USER_CODE: u64 = PTE_V | PTE_R | PTE_X | PTE_U | PTE_A;
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

/// The Sv39 PTE view the shared reclamation policy walks (REQ-MM-003, ADR-031). The only
/// architecture knowledge here is the V bit and the 8-byte entry width; the rules live once in
/// `kernel_core::ptreclaim`, identically to aarch64 and x86-64.
struct Tables;

impl TableOps for Tables {
    fn read(&self, table: usize, index: usize) -> u64 {
        // SAFETY: page tables are identity-accessible; `index` < 512 by construction of the walk.
        unsafe { read_entry(table, index) }
    }
    fn write(&mut self, table: usize, index: usize, value: u64) {
        // SAFETY: as above; writing a PTE (or zero) into a live table is sound.
        unsafe { write_entry(table, index, value) }
    }
    fn is_present(&self, entry: u64) -> bool {
        entry & PTE_V != 0
    }
    fn free_table(&mut self, table: usize) -> bool {
        // Must be a frame this kernel holds AS A PAGE TABLE; the ownership model refuses anything
        // else and reclamation then restores the parent entry (REQ-MM-002).
        frames::free_addr_as(table, Owner::PAGETABLE)
    }
}

/// The same PTE view, extended with what address-space DESTRUCTION needs (REQ-MM-004, ADR-032). An
/// Sv39 entry is a LEAF when any of R/W/X is set (`is_leaf`), which covers 4 KiB pages, 2 MiB
/// megapages and 1 GiB gigapages alike. Every slot of a per-process Sv39 tree belongs to that
/// space, so the default `is_private` (everything) is correct here.
impl SpaceOps for Tables {
    fn levels(&self) -> usize {
        3
    }
    fn is_leaf(&self, entry: u64, _level: usize) -> bool {
        entry & PTE_RWX != 0
    }
    fn entry_addr(&self, entry: u64) -> usize {
        pte_pa(entry)
    }
    fn free_leaf(&mut self, pa: usize) -> bool {
        // USER-owned pages only; the identity map's RAM/device megapage leaves are refused by the
        // ownership model and counted as skipped.
        frames::free_addr_as(pa, Owner::USER)
    }
}

/// Destroy the address space rooted at `root` (REQ-MM-004, ADR-032). Refuses — returning `None` —
/// to destroy the space `satp` is currently translating through.
pub fn destroy_space(root: usize) -> Option<Teardown> {
    if root == active_root() {
        return None;
    }
    Some(teardown::destroy_address_space(root, &mut Tables))
}

/// Decode Sv39 PTE bits into the arch-neutral permission model, and audit live trees against it
/// (REQ-MM-006, ADR-034). RISC-V has no separate user/kernel execute bits: a leaf is executable by
/// whichever privilege level may access it, so `PTE_U` decides which of the two exec flags is set —
/// which is exactly why the ret2usr rule matters here (S-mode execution of a U page is prevented by
/// SUM/hardware, and by refusing to create the mapping at all).
impl AttrOps for Tables {
    fn levels(&self) -> usize {
        3
    }
    fn is_leaf(&self, entry: u64, level: usize) -> bool {
        <Self as SpaceOps>::is_leaf(self, entry, level)
    }
    fn entry_addr(&self, entry: u64) -> usize {
        pte_pa(entry)
    }
    fn decode(&self, entry: u64, _level: usize) -> PageAttrs {
        let user = entry & PTE_U != 0;
        let exec = entry & PTE_X != 0;
        PageAttrs {
            // Sv39 PTEs carry no cacheability field (memory type comes from the platform's PMAs),
            // so every leaf is modelled as Normal and the device rule is enforced by the addresses
            // the device map uses rather than by a bit. Stated rather than silently assumed.
            kind: MemKind::Normal,
            write: entry & PTE_W != 0,
            exec_user: exec && user,
            exec_kernel: exec && !user,
            user,
        }
    }
}

/// Audit every mapping reachable from `root` against the W^X and attribute rules (REQ-MM-006).
pub fn audit_attrs(root: usize) -> memattr::AuditReport {
    memattr::audit(root, &Tables)
}

/// Total intermediate tables reclaimed since boot, so the VM gate proves reclamation actually ran.
static TABLES_RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// Intermediate page tables freed since boot (REQ-MM-003).
pub fn tables_reclaimed() -> usize {
    TABLES_RECLAIMED.load(Ordering::Relaxed)
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
    let root = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();

    // Root[0] -> peripheral GiB (0..1 GiB) as a Device gigapage leaf.
    // SAFETY: `root` is a fresh, in-RAM, identity-accessible table; index 0 < 512.
    unsafe { write_entry(root, 0, pte(0, DEV_LEAF)) };

    // Root[2] -> a level-1 table of RAM megapages. RAM occupies 0x8000_0000.., which lands in
    // level-2 index 2 (0x8000_0000 >> 30 == 2); within that gigabyte the RAM megapages start at
    // level-1 index 0.
    let l1 = frames::alloc_zeroed_as(Owner::PAGETABLE)?.addr();
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

/// This target's address-space geometry, for the arch-independent mapping-API admission check
/// (ALET-P1-001, REQ-MM-001). Sv39 decodes 39 virtual-address bits INCLUDING the sign bit: bits
/// 63:39 must repeat bit 38, so the low half is `[0, 2^38)` and anything between the halves is a
/// non-addressable hole, not a mappable page. A `va` with bit 39 set aliases a low-half entry. The
/// physical window is read from the frame allocator itself, so the check cannot drift from the pool.
fn addr_plan() -> AddrPlan {
    AddrPlan::new(
        39,
        true,
        frames::base(),
        frames::total_count() * vmaddr::PAGE_SIZE,
    )
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
    if addr_plan().validate_map(va, pa).is_err() {
        return false;
    }
    // W^X and attribute admission (REQ-MM-006, ADR-034): caller-supplied flags are untrusted input
    // exactly like `va`/`pa`, so a writable+executable or mis-encoded leaf is refused here.
    if Tables.decode(flags, 2).validate().is_err() {
        return false;
    }
    let (i2, i1, i0) = indices(va);
    // SAFETY: table entries are identity-accessible; new tables come from `frames::alloc_zeroed`.
    unsafe {
        let e2 = read_entry(root, i2);
        let t1 = if e2 & PTE_V == 0 {
            let t = match frames::alloc_zeroed_as(Owner::PAGETABLE) {
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
            let t = match frames::alloc_zeroed_as(Owner::PAGETABLE) {
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
    if addr_plan().validate_unmap(va).is_err() {
        return false;
    }
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
        // REQ-MM-003 / ADR-031: reclaim the tables this emptied (never the root) BEFORE the fence,
        // so one `sfence.vma` covers the leaf and the ancestors it detaches.
        let path = [
            PathStep::new(root, i2),
            PathStep::new(t1, i1),
            PathStep::new(t0, i0),
        ];
        if let Ok(r) = ptreclaim::reclaim_empty_tables(&path, &mut Tables) {
            TABLES_RECLAIMED.fetch_add(r.tables_freed, Ordering::Relaxed);
        }
        sfence_va(va);
    }
    true
}

/// Core-LOCAL TLB fence for one VA (`sfence.vma va, x0`, this hart only) — the per-hart half of a
/// software TLB shootdown (REQ-SMP-004): the SBI RFENCE path fences remote harts via firmware, and
/// this is what a hart runs itself when servicing a shootdown request (uniform with the aarch64
/// local `tlbi` and the x86 `invlpg` service callbacks).
///
/// SAFETY: `sfence.vma` is always sound at S-mode; over-fencing an unmapped VA is harmless.
#[inline]
pub unsafe fn sfence_page(va: usize) {
    sfence_va(va);
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

    // 8 — MAPPING-API ADMISSION CHECK (ALET-P1-001, REQ-MM-001): raw addresses are untrusted
    //     input. Each rejection below is a real corruption the check prevents on live tables:
    //     an aliasing VA silently overwrites another page's entry, a misaligned address maps a
    //     different page than the caller named, and a PA outside the allocator's window maps
    //     memory the kernel does not own. The reference frame is still allocated, so every
    //     rejection is attributable to the address, not to allocator exhaustion.
    let alias_va = TEST_VA + (1usize << 39); // bit 39 set: non-canonical under Sv39, aliases TEST_VA
    check!(
        !map_page(root, alias_va, frame.addr(), NORMAL_PAGE),
        "vm: mapping a VA outside the Sv39 canonical form is refused (would alias another entry)"
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
    //      asked for. The bootstrap identity map is the honest exception: it covers kernel text,
    //      rodata, data, stack and heap in single 2 MiB blocks, so those blocks are writable AND
    //      kernel-executable until the image is split at page granularity. The audit counts that
    //      class separately and the gate PINS it, so it is measured, not hidden.
    {
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D
            ),
            "wx: mapping a writable+executable page is refused (W^X)"
        );
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D
            ),
            "wx: mapping executable device memory is refused"
        );
        // Sv39 has ONE execute bit and lets PTE_U decide whose execution it authorizes, so the
        // aarch64/x86-64 case "user-accessible AND kernel-executable" is unrepresentable here — an
        // honest architectural difference, not a missing check. The U-mode analogue of the same
        // attack surface is a user page that is writable and executable, which is refused:
        check!(
            !map_page(
                root,
                TEST_VA,
                frame.addr(),
                PTE_V | PTE_R | PTE_W | PTE_X | PTE_U | PTE_A | PTE_D
            ),
            "wx: mapping a user page that is writable+executable is refused"
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
            report.bootstrap_violations == 64,
            "wx: the bootstrap identity blocks are the only W^X exception, and their count is pinned"
        );
    }

    frames::free(frame);
    Ok(n)
}
