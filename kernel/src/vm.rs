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
const PXN: u64 = 1 << 53; // privileged execute-never
const UXN: u64 = 1 << 54; // unprivileged execute-never
const ATTR_NORMAL: u64 = 0 << 2; // AttrIndx = 0 (MAIR attr0, Normal)
const ATTR_DEVICE: u64 = 1 << 2; // AttrIndx = 1 (MAIR attr1, Device)

/// Output-address mask for a next-level table / L3 page: bits[47:12].
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Attributes for a Normal-memory 2 MiB block that holds kernel code/data (executable at EL1).
const NORMAL_BLOCK: u64 = DESC_BLOCK | ATTR_NORMAL | AP_RW_EL1 | SH_INNER | AF | UXN;
/// Attributes for a Device-memory 2 MiB block (MMIO — never executable).
const DEVICE_BLOCK: u64 = DESC_BLOCK | ATTR_DEVICE | AP_RW_EL1 | AF | UXN | PXN;
/// Attributes for a Normal-memory 4 KiB page (dynamic mappings).
const NORMAL_PAGE: u64 = DESC_TABLE | ATTR_NORMAL | AP_RW_EL1 | SH_INNER | AF | UXN;
/// EL0-executable user code page: EL0 RW+X (AP_RW_EL0, UXN clear), EL1 execute-never (PXN).
pub const USER_CODE: u64 = DESC_TABLE | ATTR_NORMAL | AP_RW_EL0 | SH_INNER | AF | PXN;
/// EL0 data/stack page: EL0 RW, never executable at either level.
pub const USER_DATA: u64 = DESC_TABLE | ATTR_NORMAL | AP_RW_EL0 | SH_INNER | AF | UXN | PXN;

#[inline]
unsafe fn read_entry(table: usize, idx: usize) -> u64 {
    core::ptr::read_volatile((table + idx * 8) as *const u64)
}
#[inline]
unsafe fn write_entry(table: usize, idx: usize, val: u64) {
    core::ptr::write_volatile((table + idx * 8) as *mut u64, val);
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
    let l1 = frames::alloc_zeroed()?.addr();

    // L1[0] -> peripheral GiB (0..1 GiB), 2 MiB Device blocks (covers the PL011 UART).
    let l2_dev = frames::alloc_zeroed()?.addr();
    for i in 0..512 {
        let pa = (i * BLOCK_2M) as u64;
        // SAFETY: `l2_dev` is a fresh, in-RAM, identity-accessible frame; `i` < 512 entries.
        unsafe { write_entry(l2_dev, i, pa | DEVICE_BLOCK) };
    }
    // SAFETY: `l1` fresh table; entry 0 points at the device L2.
    unsafe { write_entry(l1, 0, (l2_dev as u64) | DESC_TABLE) };

    // L1[1] -> the RAM GiB (1..2 GiB). RAM occupies 0x4000_0000..RAM_END => L2 blocks 0..N.
    let l2_ram = frames::alloc_zeroed()?.addr();
    let ram_blocks = (frames::RAM_END - RAM_BASE) / BLOCK_2M;
    for i in 0..ram_blocks {
        let pa = (RAM_BASE + i * BLOCK_2M) as u64;
        // SAFETY: `l2_ram` fresh in-RAM frame; `i` < ram_blocks <= 512.
        unsafe { write_entry(l2_ram, i, pa | NORMAL_BLOCK) };
    }
    // SAFETY: RAM_BASE is in L1 index 1 (0x4000_0000 >> 30 == 1).
    unsafe { write_entry(l1, 1, (l2_ram as u64) | DESC_TABLE) };

    Some(l1)
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

/// Map a single 4 KiB `va -> pa` with `flags`, creating intermediate tables from fresh frames as
/// needed. Returns `false` on allocator exhaustion or if an intermediate level is a block (this
/// wave never splits blocks). Invalidates the TLB entry for `va`.
pub fn map_page(root: usize, va: usize, pa: usize, flags: u64) -> bool {
    let (l1i, l2i, l3i) = indices(va);
    // SAFETY: table entries are identity-accessible; new tables come from `frames::alloc_zeroed`.
    unsafe {
        let l1e = read_entry(root, l1i);
        let l2t = if l1e & VALID == 0 {
            let t = match frames::alloc_zeroed() {
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
            let t = match frames::alloc_zeroed() {
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

    frames::free(frame);
    Ok(n)
}
