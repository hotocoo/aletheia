//! The kernel heap over the linker-reserved region (`__heap_start..__heap_end`). Since ADR-198 it
//! FREES (`kernel_core::kheap`); before that it was a bump allocator that never did. SMP-safe as of REQ-SMP-002:
//! the bump pointer advances by compare-and-swap, so concurrent allocations on different cores
//! each carve a disjoint region (the old load-then-store pair could hand two cores the same
//! bytes). Enables `alloc` (Vec/String/BTreeMap) so the in-kernel spine can mirror the hosted
//! System Core's data structures without a full page allocator (that lands in a later phase).
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "heaptrace")]
use core::sync::atomic::Ordering;

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

/// The global allocator's handle; all state is in [`HEAP`].
pub struct BumpAlloc;

/// Set once the interactive console is up (feature `heaptrace` only).
#[cfg(feature = "heaptrace")]
pub static TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Print `[heaptrace] <size> <ra0> <ra1> ...` by walking the AArch64 frame-record chain (x29).
/// Reads only frame records the running code built; stops at a null or misaligned record.
#[cfg(feature = "heaptrace")]
fn trace(size: usize) {
    use core::fmt::Write;
    struct U;
    impl core::fmt::Write for U {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            crate::uart::puts(s);
            Ok(())
        }
    }
    let mut fp: usize;
    // SAFETY: reading the frame-pointer register has no side effect.
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
    let _ = write!(U, "[heaptrace] {}", size);
    for _ in 0..8 {
        if fp == 0 || !fp.is_multiple_of(16) {
            break;
        }
        // SAFETY: a frame record is two words at x29: the caller's x29, then the return address.
        let (next, ra) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        let _ = write!(U, " {:x}", ra);
        if next <= fp {
            break;
        }
        fp = next;
    }
    crate::uart::puts("\n");
}

/// The kernel heap (ADR-198): `kernel_core::kheap` — size classes, coalescing page runs, bump for
/// fresh memory — behind one spin lock taken with IRQs masked on this CPU, because the desktop's
/// pump allocates from the timer interrupt and must never spin on a lock its own CPU holds.
static HEAP: kernel_core::sync::SpinLock<kernel_core::kheap::Heap> =
    kernel_core::sync::SpinLock::new(kernel_core::kheap::Heap::empty());

/// Mask IRQs on this CPU; returns whether they were enabled.
#[inline]
fn irq_save() -> bool {
    let daif: u64;
    // SAFETY: reading DAIF and setting the I bit affect only this CPU's interrupt mask.
    unsafe {
        core::arch::asm!("mrs {d}, daif", d = out(reg) daif, options(nomem, nostack));
        core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
    daif & (1 << 7) == 0
}

#[inline]
fn irq_restore(were_enabled: bool) {
    if were_enabled {
        // SAFETY: clearing the I bit only changes this CPU's interrupt mask.
        unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
    }
}

fn with_heap<R>(f: impl FnOnce(&mut kernel_core::kheap::Heap) -> R) -> R {
    let saved = irq_save();
    let r = {
        let mut h = HEAP.lock();
        // SAFETY: the linker reserves `__heap_start..__heap_end` for this heap alone.
        let (start, end) = unsafe {
            (
                &__heap_start as *const u8 as usize,
                &__heap_end as *const u8 as usize,
            )
        };
        h.init(start, end);
        f(&mut h)
    };
    irq_restore(saved);
    r
}

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "heaptrace")]
        if TRACE.load(Ordering::Relaxed) {
            trace(layout.size());
        }
        // SAFETY: Layout guarantees a power-of-two alignment; the region is the heap's own.
        with_heap(|h| unsafe { h.alloc(layout.size(), layout.align()) }) as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc's contract: `ptr` came from `alloc` with this layout.
        with_heap(|h| unsafe { h.dealloc(ptr as usize, layout.size(), layout.align()) });
    }
}

#[global_allocator]
static ALLOCATOR: BumpAlloc = BumpAlloc;

/// Bytes ever handed out — never decreases, the meaning every "allocates nothing" storm reads
/// (ADR-063's watermark, kept by ADR-198).
pub fn used_bytes() -> usize {
    with_heap(|h| h.gross_bytes() as usize)
}

/// Bytes held by live allocations right now.
pub fn live_bytes() -> usize {
    with_heap(|h| h.live_bytes())
}

/// Bytes available to the next allocation: never-touched region plus everything freed.
pub fn free_bytes() -> usize {
    with_heap(|h| h.free_bytes())
}
