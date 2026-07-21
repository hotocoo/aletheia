//! Physical page-frame allocator (RISC-V/RV64 first-class backend) — the first brick of real
//! memory management on this target (PRD P5), bringing RISC-V to parity with the aarch64 dev
//! backend and the x86-64 image. It manages the RAM that lies *above* the kernel image, stack, and
//! the static bump heap, handing out fixed 4 KiB physical frames one at a time.
//!
//! WHY THIS EXISTS: until now the RISC-V kernel ran in a single flat physical space (no MMU) with
//! one static bump heap that never freed. To isolate programs in their own address spaces (the
//! defining trait of an OS) the kernel must first own *physical* memory: allocate a frame, use it
//! as a page table or a program's page, and reclaim it. This module is that ownership — pure
//! software (no MMU yet), so it cannot break the boot path. `vm.rs` builds Sv39 page tables out of
//! these frames.
//!
//! DESIGN: identical in shape to the aarch64 backend (`kernel/src/frames.rs`) — an intrusive LIFO
//! free-list where each free frame stores, in its own first 8 bytes, the physical address of the
//! next free frame, so the allocator needs no side table. Works before the MMU is on
//! (physical == effective address) and afterwards because `vm.rs` identity-maps this RAM. The only
//! difference from aarch64 is the platform memory map: QEMU `virt` for RISC-V places DRAM at
//! 0x8000_0000 (OpenSBI reserves the first 2 MiB), not 0x4000_0000. Single-core, no preemption,
//! fail-closed on exhaustion.
use core::cell::UnsafeCell;

/// Page size for the RV64 Sv39 4 KiB granule.
pub const FRAME_SIZE: usize = 4096;

/// Base of DRAM on QEMU `virt` (RISC-V): 0x8000_0000. OpenSBI (M-mode) lives in the first 2 MiB;
/// our `-kernel` S-mode payload links at 0x8020_0000 (see `linker.ld`). The frame allocator only
/// ever manages RAM *above* `__heap_end`, so OpenSBI's reserved region is never handed out.
pub const RAM_BASE: usize = 0x8000_0000;

/// End of usable RAM on QEMU `virt` with `-m 128M`: DRAM base 0x8000_0000 + 128 MiB. The backend
/// hardcodes this (contract-honest: the run command fixes `-m 128M`, matching `.cargo/config.toml`
/// and `scripts/vm-e2e-riscv.sh`); a hardware port parses the DTB `/memory` node instead.
pub const RAM_END: usize = 0x8800_0000;

extern "C" {
    /// End of the linker-reserved region (image + stack + bump heap). Everything below this is
    /// in use by the running kernel and must never be handed out as a free frame.
    static __heap_end: u8;
}

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// A 4 KiB-aligned physical frame. Holding one is a claim on that physical memory; it is
/// returned to the allocator with [`FrameAllocator::free`]. The inner address is always
/// `FRAME_SIZE`-aligned and inside the allocator's managed region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PhysFrame(usize);

impl PhysFrame {
    /// Physical (== identity-virtual) base address of the frame.
    #[inline]
    pub fn addr(self) -> usize {
        self.0
    }
    /// Mutable byte view of the frame's contents. SAFETY: caller must hold this frame (it came
    /// from `alloc` and has not been freed), and the frame must be accessible at its identity
    /// address (true pre-MMU and under `vm.rs`'s identity map).
    #[inline]
    unsafe fn as_bytes_mut(self) -> &'static mut [u8] {
        core::slice::from_raw_parts_mut(self.0 as *mut u8, FRAME_SIZE)
    }
}

/// Intrusive free-list frame allocator over a half-open physical range `[base, end)`.
pub struct FrameAllocator {
    head: usize, // phys addr of first free frame, or 0 when empty
    free: usize,
    total: usize,
    base: usize,
    end: usize,
}

impl FrameAllocator {
    const fn empty() -> Self {
        FrameAllocator {
            head: 0,
            free: 0,
            total: 0,
            base: 0,
            end: 0,
        }
    }

    /// Populate the free-list from every aligned frame in `[base, end)`. `base`/`end` are
    /// rounded inward to `FRAME_SIZE`. Frames are pushed high-to-low so the lowest ends up at
    /// the list head (cosmetic — the order carries no meaning).
    ///
    /// SAFETY: `[base, end)` must be RAM the caller owns exclusively and that is not otherwise
    /// live (not the running image, stack, or heap). The allocator writes a link word into each
    /// frame, so the range must be readable/writable at its identity address.
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
        // Store the current head inside the freed frame, then point head at it.
        *(frame as *mut usize) = self.head;
        self.head = frame;
        self.free += 1;
    }

    /// Allocate one frame, or `None` when the pool is empty (fail-closed).
    pub fn alloc(&mut self) -> Option<PhysFrame> {
        if self.head == 0 {
            return None;
        }
        let f = self.head;
        // SAFETY: `f` is a frame we previously pushed; its first word is the next-free link.
        self.head = unsafe { *(f as *const usize) };
        self.free -= 1;
        Some(PhysFrame(f))
    }

    /// Allocate one frame and zero it — the required shape for a fresh page table (all entries
    /// invalid) or a cleared program page.
    pub fn alloc_zeroed(&mut self) -> Option<PhysFrame> {
        let frame = self.alloc()?;
        // SAFETY: we hold `frame` exclusively; it is accessible at its identity address.
        unsafe { frame.as_bytes_mut().fill(0) };
        Some(frame)
    }

    /// Return a frame to the pool. A frame outside the managed region or misaligned is rejected
    /// (returns `false`) rather than corrupting the list.
    pub fn free(&mut self, frame: PhysFrame) -> bool {
        let a = frame.addr();
        if a < self.base || a >= self.end || !a.is_multiple_of(FRAME_SIZE) {
            return false;
        }
        // SAFETY: `a` is a valid, aligned, in-range frame address; writing its link word is sound.
        unsafe { self.push_raw(a) };
        true
    }

    pub fn free_count(&self) -> usize {
        self.free
    }
    pub fn total_count(&self) -> usize {
        self.total
    }
    pub fn base(&self) -> usize {
        self.base
    }
}

/// Single-core interior-mutability wrapper: the kernel is uniprocessor with no preemption during
/// allocation, so there is no concurrent access to guard against. `unsafe impl Sync` is sound for
/// exactly that reason (mirrors `heap.rs`'s allocator).
struct Locked(UnsafeCell<FrameAllocator>);
// SAFETY: single-core, no preemption while a &mut is held — no data race is possible.
unsafe impl Sync for Locked {}

static KFRAMES: Locked = Locked(UnsafeCell::new(FrameAllocator::empty()));

/// Access the global allocator. SAFETY: single-core / no preemption (see `Locked`).
#[allow(clippy::mut_from_ref)]
fn kframes() -> &'static mut FrameAllocator {
    unsafe { &mut *KFRAMES.0.get() }
}

/// Initialize the global frame allocator over the RAM above the kernel's static region.
/// Called once, early in `kmain`, before any frame is allocated.
pub fn init() {
    let base = unsafe { &__heap_end as *const u8 as usize };
    // SAFETY: [align_up(base), RAM_END) is RAM strictly above the image+stack+heap the linker
    // reserved, so it is not otherwise live; it is identity-accessible pre-MMU.
    unsafe { kframes().init(base, RAM_END) };
}

/// Allocate one physical frame from the global pool.
pub fn alloc() -> Option<PhysFrame> {
    kframes().alloc()
}

/// Allocate one zeroed physical frame (page-table / cleared-page shape).
pub fn alloc_zeroed() -> Option<PhysFrame> {
    kframes().alloc_zeroed()
}

/// Return a frame to the global pool.
pub fn free(frame: PhysFrame) -> bool {
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

// ---------------------------------------------------------------------------
// Selftest — physical-memory invariants, riscv64-only (NOT in the shared `selftest.rs`, which
// compiles for all three kernels). Same shape + same 5 invariants as the aarch64 frame allocator:
// first failure sets the code.
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
        !free(PhysFrame(a.addr() + 1)),
        "frames: misaligned free rejected (fail-closed)"
    );

    // 5 — exhaustion is fail-closed, and freeing revives allocation (deterministic scratch pool).
    {
        let mut scratch = FrameAllocator::empty();
        let sbase = core::ptr::addr_of!(SCRATCH) as usize;
        // SAFETY: SCRATCH is a private, 4 KiB-aligned 4-frame static owned solely by this test.
        unsafe { scratch.init(sbase, sbase + FRAME_SIZE * 4) };
        let cap = scratch.total_count();
        let mut held = [PhysFrame(0); 4];
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

    Ok(n)
}
