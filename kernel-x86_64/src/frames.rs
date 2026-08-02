//! Physical page-frame allocator (x86-64 first-class target) — the P5 memory-management brick,
//! the AMD64 twin of `kernel/src/frames.rs`. It owns the physical RAM the firmware handed us and
//! hands out fixed 4 KiB frames one at a time; `vm.rs` builds page tables out of those frames.
//!
//! WHY THIS EXISTS: until now the x86-64 kernel re-proved the capability spine but owned no
//! physical memory of its own — it ran on the flat identity map OVMF left behind. To manage
//! address spaces (the defining trait of an OS) the kernel must first own physical frames:
//! allocate one, use it as a page table or a program page, reclaim it.
//!
//! SEEDING (differs from the aarch64 backend, honestly): aarch64 hardcodes `[__heap_end, RAM_END)`
//! because its QEMU `-m` fixes the DRAM size. x86-64 has something better — the **UEFI memory
//! map** captured at `ExitBootServices`. Per the UEFI spec, `CONVENTIONAL` memory is free for the
//! OS to claim once boot services exit; it never overlaps our loaded image (`LOADER_*`), stack,
//! or firmware tables. We seed the allocator from the single largest `CONVENTIONAL` region, which
//! gives us one contiguous `[base, end)` range with the same simple bounds semantics the aarch64
//! allocator has. Post-ExitBootServices, OVMF still identity-maps this RAM (phys == virt), so the
//! intrusive free-list's link words are writable at each frame's own address.
//!
//! DESIGN: an intrusive LIFO free-list — each free frame stores the next free frame's physical
//! address in its own first 8 bytes, so no side table is needed. Single-core, no preemption,
//! fail-closed on exhaustion. Identical strategy to the aarch64 backend.
//!
//! OWNERSHIP (REQ-MM-002, ADR-030, GAPS4 ALET-P1-003): the free-list alone cannot tell a frame it
//! already holds from one a caller legitimately owns, so `free` used to accept any aligned
//! in-window address — including one already on the list. That pushes the same frame twice, and two
//! later `alloc` calls hand ONE page to two owners. Every alloc/free now goes through the shared
//! arch-independent [`kernel_core::frameown::FrameOwnerTable`], identically to the aarch64 and
//! RISC-V backends. One x86-64-specific wrinkle: the window comes from the UEFI map at runtime, so
//! it is not a compile-time constant like the QEMU `virt` targets' DRAM. The state array therefore
//! covers a fixed [`MAX_FRAMES`] and the seeded window is CLAMPED to it — a machine with more
//! conventional RAM manages less of it rather than running a tail with no ownership state, and
//! `init_from_uefi` reports the clamp so it is never silent.
use core::cell::UnsafeCell;
use kernel_core::frameown::{FrameOwnerTable, Owner};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use x86_64::structures::paging::{
    FrameAllocator as X86FrameAllocator, PhysFrame as X86PhysFrame, Size4KiB,
};
use x86_64::PhysAddr;

/// 4 KiB frame — the x86-64 base page size.
pub const FRAME_SIZE: usize = 4096;

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// A 4 KiB-aligned physical frame; holding one is a claim on that physical memory. The inner
/// address is always `FRAME_SIZE`-aligned and inside the allocator's managed region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame(usize);

impl Frame {
    /// Physical (== identity-virtual, under OVMF's map) base address of the frame.
    #[inline]
    pub fn addr(self) -> usize {
        self.0
    }
    /// Mutable byte view of the frame. SAFETY: caller holds this frame (from `alloc`, not freed)
    /// and it is identity-accessible (true under OVMF's post-exit identity map).
    #[inline]
    unsafe fn as_bytes_mut(self) -> &'static mut [u8] {
        core::slice::from_raw_parts_mut(self.0 as *mut u8, FRAME_SIZE)
    }
}

/// Intrusive free-list allocator over a half-open physical range `[base, end)`.
struct FrameAllocator {
    head: usize, // phys addr of first free frame, or 0 when empty
    free: usize,
    total: usize,
    base: usize,
    end: usize,
    /// `None` until `attach_owners` runs; the global pool always attaches one during init.
    owners: Option<FrameOwnerTable<'static>>,
}

impl FrameAllocator {
    const fn empty() -> Self {
        FrameAllocator {
            head: 0,
            free: 0,
            total: 0,
            base: 0,
            end: 0,
            owners: None,
        }
    }

    /// Populate the free-list from every aligned frame in `[base, end)` (rounded inward).
    ///
    /// SAFETY: `[base, end)` must be RAM the caller owns exclusively and that is not otherwise
    /// live (not the running image, stack, or firmware structures). The allocator writes a link
    /// word into each frame, so the range must be read/write-accessible at its identity address.
    unsafe fn init(&mut self, base: usize, end: usize) {
        let base = align_up(base, FRAME_SIZE);
        let end = end & !(FRAME_SIZE - 1);
        self.base = base;
        self.end = end;
        self.head = 0;
        self.free = 0;
        self.total = 0;
        if end <= base {
            return;
        }
        let mut f = end - FRAME_SIZE;
        loop {
            self.push_raw(f);
            self.total += 1;
            if f == base {
                break;
            }
            f -= FRAME_SIZE;
        }
    }

    #[inline]
    unsafe fn push_raw(&mut self, frame: usize) {
        *(frame as *mut usize) = self.head;
        self.head = frame;
        self.free += 1;
    }

    /// Give this pool an ownership table covering exactly the frames it manages, built from the
    /// pool's OWN base and frame count so the two can never describe different windows.
    fn attach_owners(&mut self, state: &'static mut [u8]) -> bool {
        match FrameOwnerTable::new(self.base, self.total, state) {
            Ok(t) => {
                self.owners = Some(t);
                true
            }
            Err(_) => false, // fail-closed: an unprotected tail is worse than no pool
        }
    }

    /// Allocate one frame for `owner`. Ownership is claimed BEFORE the frame leaves the list, so a
    /// frame the table already considers owned stays put and the caller gets `None`.
    fn alloc_as(&mut self, owner: Owner) -> Option<Frame> {
        if self.head == 0 {
            return None;
        }
        let f = self.head;
        if let Some(t) = self.owners.as_mut() {
            t.claim(f, owner).ok()?;
        }
        // SAFETY: `f` is a frame we previously pushed; its first word is the next-free link.
        self.head = unsafe { *(f as *const usize) };
        self.free -= 1;
        Some(Frame(f))
    }

    fn alloc(&mut self) -> Option<Frame> {
        self.alloc_as(Owner::KERNEL)
    }

    fn alloc_zeroed_as(&mut self, owner: Owner) -> Option<Frame> {
        let frame = self.alloc_as(owner)?;
        // SAFETY: we hold `frame` exclusively; identity-accessible.
        unsafe { frame.as_bytes_mut().fill(0) };
        Some(frame)
    }

    fn alloc_zeroed(&mut self) -> Option<Frame> {
        self.alloc_zeroed_as(Owner::KERNEL)
    }

    /// Return a frame held by `owner`. Rejected (list untouched) when misaligned, out of region,
    /// already free (double free), or held by a different owner.
    fn free_as(&mut self, frame: Frame, owner: Owner) -> bool {
        let a = frame.addr();
        if a < self.base || a >= self.end || !a.is_multiple_of(FRAME_SIZE) {
            return false;
        }
        if let Some(t) = self.owners.as_mut() {
            if t.release(a, owner).is_err() {
                return false;
            }
        }
        // SAFETY: `a` is a valid, aligned, in-range frame address.
        unsafe { self.push_raw(a) };
        true
    }

    fn free(&mut self, frame: Frame) -> bool {
        self.free_as(frame, Owner::KERNEL)
    }

    fn owner_of(&self, pa: usize) -> Option<Owner> {
        self.owners.as_ref()?.owner_of(pa).ok().flatten()
    }

    fn owned_free_count(&self) -> Option<usize> {
        Some(self.owners.as_ref()?.free_count())
    }

    fn free_count(&self) -> usize {
        self.free
    }
    fn total_count(&self) -> usize {
        self.total
    }
    fn base(&self) -> usize {
        self.base
    }
    fn end(&self) -> usize {
        self.end
    }
}

/// Single-core interior-mutability wrapper (mirrors `heap.rs` / the aarch64 backend): uniprocessor
/// with no preemption during allocation, so `unsafe impl Sync` is sound.
struct Locked(UnsafeCell<FrameAllocator>);
// SAFETY: single-core, no preemption while a &mut is held — no data race is possible.
unsafe impl Sync for Locked {}

static KFRAMES: Locked = Locked(UnsafeCell::new(FrameAllocator::empty()));

/// Frames the ownership state array covers — 1 GiB at one byte per 4 KiB frame (256 KiB of
/// `.bss`). Unlike the QEMU `virt` targets, x86-64 learns its window from the UEFI map at runtime,
/// so this is a ceiling rather than the exact pool size; `init_from_uefi` clamps and reports.
pub const MAX_FRAMES: usize = 262_144;

/// Backing store for the global pool's ownership table (no heap exists this early).
static mut OWNER_STATE: [u8; MAX_FRAMES] = [0; MAX_FRAMES];

#[allow(clippy::mut_from_ref)]
fn kframes() -> &'static mut FrameAllocator {
    // SAFETY: single-core / no preemption (see `Locked`).
    unsafe { &mut *KFRAMES.0.get() }
}

/// Seed the global allocator from the UEFI memory map: the single largest `CONVENTIONAL` region.
/// Called once, early in `kmain`, before any frame is allocated. Returns `(base, frames)` of the
/// region claimed, or `(0, 0)` if the map exposed no conventional RAM (fail-closed — later allocs
/// return `None`). Frames below 1 MiB are excluded defensively (legacy low memory).
pub fn init_from_uefi(map: &MemoryMapOwned) -> (usize, usize) {
    const LOW_FLOOR: u64 = 0x10_0000; // 1 MiB
    let mut best_base: u64 = 0;
    let mut best_len: u64 = 0;
    for d in map.entries() {
        if d.ty != MemoryType::CONVENTIONAL {
            continue;
        }
        let start = d.phys_start.max(LOW_FLOOR);
        let end = d.phys_start + d.page_count * FRAME_SIZE as u64;
        if end <= start {
            continue;
        }
        let len = end - start;
        if len > best_len {
            best_len = len;
            best_base = start;
        }
    }
    if best_len == 0 {
        return (0, 0);
    }
    let base = best_base as usize;
    // Clamp to what the ownership state array covers: managing RAM with no ownership state would
    // reintroduce exactly the double-free window this model closes. The caller reports the clamp.
    let end = ((best_base + best_len) as usize).min(base + MAX_FRAMES * FRAME_SIZE);
    // SAFETY: the largest CONVENTIONAL region is RAM the UEFI spec frees to the OS at
    // ExitBootServices; it never overlaps our loaded image/stack/firmware tables, and OVMF
    // identity-maps it (writable at its identity address).
    unsafe { kframes().init(base, end) };
    // SAFETY: `OWNER_STATE` is a private static touched only here, once, before any allocation.
    let state: &'static mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(OWNER_STATE) as *mut u8, MAX_FRAMES)
    };
    if !kframes().attach_owners(state) {
        return (0, 0); // fail-closed: no ownership table, no pool
    }
    (kframes().base(), kframes().total_count())
}

/// Allocate one physical frame owned by `owner`.
pub fn alloc_as(owner: Owner) -> Option<Frame> {
    kframes().alloc_as(owner)
}

/// Allocate one zeroed physical frame owned by `owner`.
pub fn alloc_zeroed_as(owner: Owner) -> Option<Frame> {
    kframes().alloc_zeroed_as(owner)
}

/// Return a frame held by `owner` to the global pool.
pub fn free_as(frame: Frame, owner: Owner) -> bool {
    kframes().free_as(frame, owner)
}

/// Current holder of the frame at `pa`.
pub fn owner_of(pa: usize) -> Option<Owner> {
    kframes().owner_of(pa)
}

/// Allocate one physical frame from the global pool.
pub fn alloc() -> Option<Frame> {
    kframes().alloc()
}

/// Allocate one zeroed physical frame (page-table / cleared-page shape).
pub fn alloc_zeroed() -> Option<Frame> {
    kframes().alloc_zeroed()
}

/// Return a frame to the global pool.
pub fn free(frame: Frame) -> bool {
    kframes().free(frame)
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

/// Zero-size adapter so the global allocator satisfies `x86_64`'s `FrameAllocator<Size4KiB>` —
/// the trait `Mapper::map_to` needs to pull intermediate page-table frames. `vm.rs` hands a
/// `&mut GlobalFrames` to the mapper.
pub struct GlobalFrames;

// SAFETY: `allocate_frame` returns frames from our exclusive pool; each is real, unused,
// 4 KiB-aligned RAM — exactly the contract `x86_64::FrameAllocator` requires.
unsafe impl X86FrameAllocator<Size4KiB> for GlobalFrames {
    fn allocate_frame(&mut self) -> Option<X86PhysFrame<Size4KiB>> {
        // The mapper pulls these for intermediate page tables, so they are PAGETABLE-owned.
        alloc_as(Owner::PAGETABLE)
            .map(|f| X86PhysFrame::containing_address(PhysAddr::new(f.addr() as u64)))
    }
}

// ---------------------------------------------------------------------------
// Selftest — physical-memory invariants, x86-64-specific (NOT in the shared `selftest.rs`, which
// compiles for all three kernels). Same shape + invariant set as the aarch64 backend.
// ---------------------------------------------------------------------------

/// Deterministic scratch pool to prove exhaustion + reuse without draining the real RAM pool.
#[repr(align(4096))]
#[allow(dead_code)] // bytes exist to reserve real aligned address space; only its address is read
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

    // 1 — the pool manages real RAM from the UEFI map.
    let total = total_count();
    let free0 = free_count();
    check!(
        total > 0 && free0 == total,
        "frames: pool seeded from UEFI conventional RAM"
    );

    // 2 — alloc yields distinct, aligned, in-range frames.
    let (a, b) = match (alloc(), alloc()) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err((n + 1, "frames: alloc returned None with free RAM")),
    };
    check!(
        a != b
            && a.addr() % FRAME_SIZE == 0
            && b.addr() % FRAME_SIZE == 0
            && a.addr() >= kframes().base()
            && b.addr() < kframes().end(),
        "frames: alloc gives distinct aligned in-range frames"
    );

    // 3 — an allocated frame is real, writable RAM (write a pattern, read it back).
    {
        let p = a.addr() as *mut u64;
        // SAFETY: we hold frame `a`; identity-accessible under OVMF's map.
        unsafe {
            core::ptr::write_volatile(p, 0xA1E7_2026_DEAD_BEEF);
            check!(
                core::ptr::read_volatile(p) == 0xA1E7_2026_DEAD_BEEF,
                "frames: allocated frame is real read/write memory"
            );
        }
    }

    // 4 — freeing returns capacity; a misaligned free is rejected without corrupting the pool.
    free(a);
    free(b);
    check!(
        free_count() == free0,
        "frames: free returns capacity to the pool"
    );
    check!(
        !free(Frame(a.addr() + 1)),
        "frames: misaligned free rejected (fail-closed)"
    );

    // 5 — exhaustion is fail-closed, and freeing revives allocation (deterministic scratch pool).
    {
        let mut scratch = FrameAllocator::empty();
        let sbase = core::ptr::addr_of!(SCRATCH) as usize;
        // SAFETY: SCRATCH is a private, 4 KiB-aligned 4-frame static owned solely by this test.
        unsafe { scratch.init(sbase, sbase + FRAME_SIZE * 4) };
        let cap = scratch.total_count();
        let mut held = [Frame(0); 4];
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

    // 6 — OWNERSHIP (REQ-MM-002, ADR-030, ALET-P1-003). Identical checks and identical wording to
    //     the aarch64 and RISC-V backends: `scripts/conformance.sh` requires all three targets to
    //     refuse the same things in the same words, because a security boundary that varies by CPU
    //     is not a boundary.
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
        let never = match alloc_as(Owner::KERNEL) {
            Some(f) => f,
            None => return Err((n + 1, "frames: no frame for the never-allocated check")),
        };
        let stranger = Frame(never.addr() + FRAME_SIZE);
        check!(
            owner_of(stranger.addr()).is_none() && !free_as(stranger, Owner::KERNEL),
            "frames: freeing a never-allocated frame is refused"
        );
        check!(
            free_as(never, Owner::KERNEL),
            "frames: legal free still succeeds after the refusals"
        );
        check!(
            kframes().owned_free_count() == Some(free_count()),
            "frames: ownership table and free list agree on the free count"
        );
    }

    Ok(n)
}
