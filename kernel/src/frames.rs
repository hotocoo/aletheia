//! Physical page-frame allocator (aarch64 dev backend) — the first brick of real memory
//! management (PRD P5). It manages the RAM that lies *above* the kernel image, stack, and the
//! static bump heap, handing out fixed 4 KiB physical frames one at a time.
//!
//! WHY THIS EXISTS: the kernel until now ran in a single flat identity space with one static
//! bump heap and never freed. To isolate programs in their own address spaces (the defining
//! trait of an OS) the kernel must first own *physical* memory: allocate a frame, use it as a
//! page table or a program's page, and reclaim it. This module is that ownership — pure software
//! (no MMU yet), so it cannot break the boot path. `vm.rs` builds page tables out of these frames.
//!
//! DESIGN: an intrusive LIFO free-list. Each free frame stores, in its own first 8 bytes, the
//! physical address of the next free frame — so the allocator needs no side table. This works
//! before the MMU is on (physical == effective address) and continues to work afterwards because
//! `vm.rs` identity-maps this RAM. Single-core, no preemption, fail-closed on exhaustion.
//!
//! OWNERSHIP (REQ-MM-002, ADR-030, GAPS4 ALET-P1-003): the free-list alone cannot tell a frame it
//! already holds from one a caller legitimately owns, so `free` used to accept any aligned
//! in-window address — including one already on the list. That pushes the same frame twice, and two
//! later `alloc` calls hand ONE page to two owners. Every alloc/free now goes through the shared
//! arch-independent [`kernel_core::frameown::FrameOwnerTable`], which records who holds each frame
//! and refuses the double free, the free of a never-allocated frame, and the free of someone else's
//! frame. The rules live in `kernel-core` (proved on the host); this file supplies the per-target
//! state array and wires the two calls.
use core::cell::UnsafeCell;
use kernel_core::frameown::Owner;

pub use kernel_core::framealloc::FRAME_SIZE;

/// Base of DRAM on QEMU `virt` (aarch64). With [`RAM_END`] it bounds the largest window this
/// allocator can ever manage, which is what sizes the ownership state array.
pub const RAM_BASE: usize = 0x4000_0000;

/// End of usable RAM on QEMU `virt` with `-m 128M`: DRAM base 0x4000_0000 + 128 MiB. The dev
/// backend hardcodes this (contract-honest: the run command fixes `-m 128M`); a hardware port
/// parses the memory map / DTB instead.
pub const RAM_END: usize = 0x4800_0000;

extern "C" {
    /// End of the linker-reserved region (image + stack + bump heap). Everything below this is
    /// in use by the running kernel and must never be handed out as a free frame.
    static __heap_end: u8;
}

use kernel_core::framealloc::FrameAllocator;
pub use kernel_core::framealloc::PhysFrame;

/// Single-core interior-mutability wrapper: the kernel is uniprocessor with no preemption during
/// allocation, so there is no concurrent access to guard against. `unsafe impl Sync` is sound for
/// exactly that reason (mirrors `heap.rs`'s allocator).
struct Locked(UnsafeCell<FrameAllocator>);
// SAFETY: single-core, no preemption while a &mut is held — no data race is possible.
unsafe impl Sync for Locked {}

static KFRAMES: Locked = Locked(UnsafeCell::new(FrameAllocator::empty()));

/// Largest number of frames this target can ever manage: the whole DRAM window. The real pool is
/// smaller (it starts above the kernel image), so this is a safe upper bound and costs 32 KiB of
/// `.bss` — one byte per 4 KiB frame.
pub const MAX_FRAMES: usize = (RAM_END - RAM_BASE) / FRAME_SIZE;

/// Backing store for the global pool's ownership table. Static, because the kernel has no heap at
/// the point `init` runs.
static mut OWNER_STATE: [u8; MAX_FRAMES] = [0; MAX_FRAMES];

/// Access the global allocator. SAFETY: single-core / no preemption (see `Locked`).
#[allow(clippy::mut_from_ref)]
fn kframes() -> &'static mut FrameAllocator {
    unsafe { &mut *KFRAMES.0.get() }
}

/// Initialize the global frame allocator over the RAM above the kernel's static region.
/// Called once, early in `kmain`, before any frame is allocated.
/// Initialize the global frame allocator over the RAM above the kernel's static region, and attach
/// its ownership table. Returns `false` if the table could not cover the pool — the caller must
/// treat that as fatal rather than run a pool with an unprotected tail.
pub fn init() -> bool {
    let base = unsafe { &__heap_end as *const u8 as usize };
    // SAFETY: [align_up(base), RAM_END) is RAM strictly above the image+stack+heap the linker
    // reserved, so it is not otherwise live; it is identity-accessible pre-MMU.
    unsafe { kframes().init(base, RAM_END) };
    // SAFETY: `OWNER_STATE` is a private static touched only here, once, before any allocation;
    // the resulting `&'static mut` is the only reference to it in the kernel.
    let state: &'static mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(OWNER_STATE) as *mut u8, MAX_FRAMES)
    };
    kframes().attach_owners(state)
}

/// Allocate one physical frame from the global pool, owned by `owner`.
pub fn alloc_as(owner: Owner) -> Option<PhysFrame> {
    kframes().alloc_as(owner)
}

/// Allocate one physical frame from the global pool (kernel-owned).
pub fn alloc() -> Option<PhysFrame> {
    kframes().alloc()
}

/// Allocate one zeroed physical frame for `owner` (page-table / cleared-page shape).
pub fn alloc_zeroed_as(owner: Owner) -> Option<PhysFrame> {
    kframes().alloc_zeroed_as(owner)
}

/// Allocate one zeroed physical frame (page-table / cleared-page shape), kernel-owned.
pub fn alloc_zeroed() -> Option<PhysFrame> {
    kframes().alloc_zeroed()
}

/// Return a frame held by `owner` to the global pool.
pub fn free_as(frame: PhysFrame, owner: Owner) -> bool {
    kframes().free_as(frame, owner)
}

/// Return a kernel-owned frame to the global pool.
pub fn free(frame: PhysFrame) -> bool {
    kframes().free(frame)
}

/// Return the frame at physical address `pa`, held by `owner`, to the global pool. The
/// address-taking form page-table reclamation needs: it walks live tables and knows their physical
/// addresses, not `PhysFrame` handles. Every ownership rule still applies (REQ-MM-002).
pub fn free_addr_as(pa: usize, owner: Owner) -> bool {
    kframes().free_as(PhysFrame::from_addr(pa), owner)
}

/// Current holder of the frame at `pa` in the global pool.
pub fn owner_of(pa: usize) -> Option<Owner> {
    kframes().owner_of(pa)
}

/// Free frames currently available in the global pool.
pub fn free_count() -> usize {
    kframes().free_count()
}

/// Total frames the global pool manages.
pub fn total_count() -> usize {
    kframes().total_count()
}

/// First physical address the global pool owns. `vm.rs` builds its `AddrPlan` window from this and
/// `total_count()`, so the mapping check can never drift from the pool it protects.
pub fn base() -> usize {
    kframes().base()
}

// ---------------------------------------------------------------------------
// Selftest — physical-memory invariants, aarch64-only (NOT in the shared `selftest.rs`, which
// compiles for all three kernels). Same shape as the spine suite: first failure sets the code.
// ---------------------------------------------------------------------------

/// A small, deterministic scratch pool used to prove exhaustion + reuse without draining the
/// (large) real RAM pool. 4 KiB-aligned so its frames are legal frame addresses.
#[repr(align(4096))]
#[allow(dead_code)] // the bytes exist to reserve real, aligned address space; only its address is read
struct Scratch([u8; FRAME_SIZE * 4]);
static mut SCRATCH: Scratch = Scratch([0; FRAME_SIZE * 4]);

/// Prove the physical-memory invariants against the real allocator (plus a scratch pool for the
/// exhaustion edge). `Ok(n)` = all n passed; `Err((idx,name))` = check idx failed.
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

    // 1 — the pool manages real RAM above the kernel region.
    let total = total_count();
    let free0 = free_count();
    check!(
        total > 0 && free0 == total,
        "frames: pool initialized with free RAM above kernel"
    );

    // 2 — alloc yields distinct, aligned, in-range frames.
    let f1 = alloc();
    let f2 = alloc();
    let (a, b) = match (f1, f2) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err((n + 1, "frames: alloc returned None with free RAM")),
    };
    check!(
        a != b
            && a.addr() % FRAME_SIZE == 0
            && b.addr() % FRAME_SIZE == 0
            && a.addr() >= kframes().base()
            && b.addr() < RAM_END,
        "frames: alloc gives distinct aligned in-range frames"
    );

    // 3 — an allocated frame is real, writable RAM (write a pattern, read it back).
    {
        let p = a.addr() as *mut u64;
        // SAFETY: we hold frame `a`; identity-accessible pre-MMU.
        unsafe {
            core::ptr::write_volatile(p, 0xA1E7_2026_DEAD_BEEF);
            check!(
                core::ptr::read_volatile(p) == 0xA1E7_2026_DEAD_BEEF,
                "frames: allocated frame is real read/write memory"
            );
        }
    }

    // 4 — freeing returns capacity; a rejected (misaligned) free does not corrupt the pool.
    free(a);
    free(b);
    check!(
        free_count() == free0,
        "frames: free returns capacity to the pool"
    );
    check!(
        !free(PhysFrame::from_addr(a.addr() + 1)),
        "frames: misaligned free rejected (fail-closed)"
    );

    // 5 — exhaustion is fail-closed, and freeing revives allocation (deterministic scratch pool).
    {
        let mut scratch = FrameAllocator::empty();
        let sbase = core::ptr::addr_of!(SCRATCH) as usize;
        // SAFETY: SCRATCH is a private, 4 KiB-aligned 4-frame static owned solely by this test.
        unsafe { scratch.init(sbase, sbase + FRAME_SIZE * 4) };
        let cap = scratch.total_count();
        let mut held = [PhysFrame::from_addr(0); 4];
        for slot in held.iter_mut().take(cap) {
            *slot = scratch.alloc().expect("scratch frame within capacity");
        }
        check!(
            scratch.alloc().is_none(),
            "frames: exhausted pool denies allocation (fail-closed)"
        );
        scratch.free(held[0]);
        check!(
            scratch.alloc().is_some(),
            "frames: freeing an exhausted pool revives allocation"
        );
    }

    // 6 — OWNERSHIP (REQ-MM-002, ADR-030, ALET-P1-003). The free list alone accepts any aligned
    //     in-window address, so a double free pushes one frame onto the list twice and two later
    //     allocations hand the SAME page to two owners. These checks run against the live global
    //     pool: each refusal below is that corruption being stopped at the boundary.
    {
        let f = match alloc_as(Owner::USER) {
            Some(f) => f,
            None => return Err((n + 1, "frames: no frame for the ownership checks")),
        };
        check!(
            owner_of(f.addr()) == Some(Owner::USER),
            "frames: an allocated frame reports the owner that claimed it"
        );
        let before = free_count();
        check!(
            !free_as(f, Owner::KERNEL),
            "frames: freeing another owner's frame is refused (fail-closed)"
        );
        check!(
            free_count() == before && owner_of(f.addr()) == Some(Owner::USER),
            "frames: the refused cross-owner free left the pool and the owner untouched"
        );
        check!(
            free_as(f, Owner::USER),
            "frames: the real owner can free its frame"
        );
        check!(
            owner_of(f.addr()).is_none(),
            "frames: a freed frame has no owner"
        );
        check!(
            !free_as(f, Owner::USER),
            "frames: double free is refused (would hand one page to two owners)"
        );
        check!(
            free_count() == before + 1,
            "frames: the refused double free did not push the frame twice"
        );
        // A frame that was never allocated cannot be freed either — same defense, other direction.
        let never = match alloc_as(Owner::KERNEL) {
            Some(f) => f,
            None => return Err((n + 1, "frames: no frame for the never-allocated check")),
        };
        let stranger = PhysFrame::from_addr(never.addr() + FRAME_SIZE);
        check!(
            owner_of(stranger.addr()).is_none() && !free_as(stranger, Owner::KERNEL),
            "frames: freeing a never-allocated frame is refused"
        );
        check!(
            free_as(never, Owner::KERNEL),
            "frames: legal free still succeeds after the refusals"
        );
        // 7 — the two views of the pool must agree, or one of them is lying about what is free.
        check!(
            kframes().owned_free_count() == Some(free_count()),
            "frames: ownership table and free list agree on the free count"
        );
    }

    // 8 — ERASE ON FREE (REQ-MM-005, ADR-033, ALET-P2-026). Ownership stops two owners holding one
    //     frame at the same TIME; it says nothing about what the next owner can READ. Write a
    //     recognizable pattern across a frame, free it, take it back, and require zeros — the only
    //     honest proof, since asserting `alloc_zeroed` returns zeros would prove nothing about what
    //     a plain `alloc` hands the next task.
    {
        let secret = match alloc_as(Owner::USER) {
            Some(f) => f,
            None => return Err((n + 1, "frames: no frame for the erase-on-free check")),
        };
        const SECRET: u64 = 0x5EC7_0000_5EC7_0000;
        let addr = secret.addr();
        // SAFETY: we hold this frame; it is identity-accessible.
        unsafe {
            let p = addr as *mut u64;
            for i in 0..(FRAME_SIZE / 8) {
                core::ptr::write_volatile(p.add(i), SECRET);
            }
            check!(
                core::ptr::read_volatile(p) == SECRET,
                "frames: a live frame holds the owner's data"
            );
        }
        check!(
            free_as(secret, Owner::USER),
            "frames: the owner frees the frame carrying its data"
        );
        // The pool is LIFO, so the very next allocation is that same frame — the exact case a
        // leak would expose.
        let reused = match alloc_as(Owner::KERNEL) {
            Some(f) => f,
            None => return Err((n + 1, "frames: the freed frame did not come back")),
        };
        check!(
            reused.addr() == addr,
            "frames: the next allocation reuses the just-freed frame (LIFO)"
        );
        let mut leaked = false;
        // SAFETY: we now hold the frame; reading its own bytes is sound.
        unsafe {
            let p = addr as *const u64;
            // Skip the first word: the allocator's own free-list link lives there and is
            // overwritten by the next push, not by the previous owner.
            for i in 1..(FRAME_SIZE / 8) {
                if core::ptr::read_volatile(p.add(i)) != 0 {
                    leaked = true;
                    break;
                }
            }
        }
        check!(
            !leaked,
            "frames: a reused frame carries NO bytes of its previous owner (erased on free)"
        );
        free(reused);
    }

    Ok(n)
}
