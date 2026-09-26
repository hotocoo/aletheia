//! The kernel heap over the linker-reserved region (`__heap_start..__heap_end`). Since ADR-198 it
//! FREES (`kernel_core::kheap`); before that it was a bump allocator that never did. SMP-safe as of REQ-SMP-002:
//! the bump pointer advances by compare-and-swap, so concurrent allocations on different cores
//! each carve a disjoint region (the old load-then-store pair could hand two cores the same
//! bytes). Enables `alloc` (Vec/String/BTreeMap) so the in-kernel spine can mirror the hosted
//! System Core's data structures without a full page allocator (that lands in a later phase).
use core::alloc::{GlobalAlloc, Layout};

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

/// The global allocator's handle; all state is in [`HEAP`].
pub struct BumpAlloc;

/// The kernel heap (ADR-198): `kernel_core::kheap` — size classes, coalescing page runs, bump for
/// fresh memory — behind one spin lock taken with IRQs masked on this CPU, because the desktop's
/// pump allocates from the timer interrupt and must never spin on a lock its own CPU holds.
static HEAP: kernel_core::sync::SpinLock<kernel_core::kheap::Heap> =
    kernel_core::sync::SpinLock::new(kernel_core::kheap::Heap::empty());

/// Mask supervisor interrupts on this hart; returns whether they were enabled.
#[inline]
pub(crate) fn irq_save() -> bool {
    let prev: usize;
    // SAFETY: clearing sstatus.SIE only changes this hart's interrupt enable.
    unsafe {
        core::arch::asm!("csrrci {p}, sstatus, 2", p = out(reg) prev, options(nomem, nostack))
    };
    prev & 2 != 0
}

#[inline]
pub(crate) fn irq_restore(were_enabled: bool) {
    if were_enabled {
        // SAFETY: setting sstatus.SIE only changes this hart's interrupt enable.
        unsafe { core::arch::asm!("csrsi sstatus, 2", options(nomem, nostack)) };
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
