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
use core::cell::UnsafeCell;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use x86_64::structures::paging::{FrameAllocator as X86FrameAllocator, PhysFrame as X86PhysFrame, Size4KiB};
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
}

impl FrameAllocator {
    const fn empty() -> Self {
        FrameAllocator { head: 0, free: 0, total: 0, base: 0, end: 0 }
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

    fn alloc(&mut self) -> Option<Frame> {
        if self.head == 0 {
            return None;
        }
        let f = self.head;
        // SAFETY: `f` is a frame we previously pushed; its first word is the next-free link.
        self.head = unsafe { *(f as *const usize) };
        self.free -= 1;
        Some(Frame(f))
    }

    fn alloc_zeroed(&mut self) -> Option<Frame> {
        let frame = self.alloc()?;
        // SAFETY: we hold `frame` exclusively; identity-accessible.
        unsafe { frame.as_bytes_mut().fill(0) };
        Some(frame)
    }

    fn free(&mut self, frame: Frame) -> bool {
        let a = frame.addr();
        if a < self.base || a >= self.end || !a.is_multiple_of(FRAME_SIZE) {
            return false;
        }
        // SAFETY: `a` is a valid, aligned, in-range frame address.
        unsafe { self.push_raw(a) };
        true
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
    let end = (best_base + best_len) as usize;
    // SAFETY: the largest CONVENTIONAL region is RAM the UEFI spec frees to the OS at
    // ExitBootServices; it never overlaps our loaded image/stack/firmware tables, and OVMF
    // identity-maps it (writable at its identity address).
    unsafe { kframes().init(base, end) };
    (kframes().base(), kframes().total_count())
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

/// Zero-size adapter so the global allocator satisfies `x86_64`'s `FrameAllocator<Size4KiB>` —
/// the trait `Mapper::map_to` needs to pull intermediate page-table frames. `vm.rs` hands a
/// `&mut GlobalFrames` to the mapper.
pub struct GlobalFrames;

// SAFETY: `allocate_frame` returns frames from our exclusive pool; each is real, unused,
// 4 KiB-aligned RAM — exactly the contract `x86_64::FrameAllocator` requires.
unsafe impl X86FrameAllocator<Size4KiB> for GlobalFrames {
    fn allocate_frame(&mut self) -> Option<X86PhysFrame<Size4KiB>> {
        alloc().map(|f| X86PhysFrame::containing_address(PhysAddr::new(f.addr() as u64)))
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
    check!(total > 0 && free0 == total, "frames: pool seeded from UEFI conventional RAM");

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
    check!(free_count() == free0, "frames: free returns capacity to the pool");
    check!(!free(Frame(a.addr() + 1)), "frames: misaligned free rejected (fail-closed)");

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
        check!(scratch.alloc().is_none(), "frames: exhausted pool denies allocation (fail-closed)");
        scratch.free(held[0]);
        check!(scratch.alloc().is_some(), "frames: freeing an exhausted pool revives allocation");
    }

    Ok(n)
}
