//! Kernel heap over a fixed 16 MiB static region (12 until ADR-154); since ADR-198 it FREES
//! (`kernel_core::kheap`) — it was a bump allocator before.
//!
//! Deliberately a STATIC array, not a region carved from the UEFI memory map: the `.efi` image
//! (including this BSS array) is loaded into conventional RAM and identity-mapped by firmware, so
//! the region stays valid across `ExitBootServices` — whereas the UEFI pool allocator dies at exit.
//! This lets the shared, alloc-heavy spine (`Vec`/`BTreeMap`/`String`) run in kernel space with zero
//! page-table work (firmware identity paging is kept). A real page-frame allocator lands in P5.
//!
//! This is ALSO the crate's `#[global_allocator]`; the `uefi` crate's own `global_allocator` feature
//! is intentionally OFF so there is exactly one global allocator, valid before and after exit.

use crate::cell::Racy;
use core::alloc::{GlobalAlloc, Layout};

/// 8 -> 12 MiB with ADR-084: this heap NEVER frees, so every suite's surfaces and every
/// resident window's pixels stay resident for the life of the boot. The window-manager suite
/// mints its own desktops and the live desktop now holds two windows and their render
/// buffers; at 8 MiB the vt-d gate's page tables were the allocation that found the ceiling.
const HEAP_SIZE: usize = 16 * 1024 * 1024;

static HEAP_AREA: Racy<[u8; HEAP_SIZE]> = Racy::new([0u8; HEAP_SIZE]);

/// The kernel heap (ADR-198): `kernel_core::kheap` behind one spin lock taken with interrupts
/// off, because IRQ0 (the desktop's pump) allocates and must never spin on a lock its CPU holds.
static HEAP: kernel_core::sync::SpinLock<kernel_core::kheap::Heap> =
    kernel_core::sync::SpinLock::new(kernel_core::kheap::Heap::empty());

fn with_heap<R>(f: impl FnOnce(&mut kernel_core::kheap::Heap) -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut h = HEAP.lock();
        // SAFETY: reads a static's address; no exclusive borrow of HEAP_AREA is ever taken.
        let base = unsafe { HEAP_AREA.get().as_ptr() as usize };
        h.init(base, base + HEAP_SIZE);
        f(&mut h)
    })
}

struct Kernel;

unsafe impl GlobalAlloc for Kernel {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Layout's alignment is a power of two; the region is the heap's own.
        with_heap(|h| unsafe { h.alloc(layout.size(), layout.align()) }) as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc's contract: `ptr` came from `alloc` with this layout.
        with_heap(|h| unsafe { h.dealloc(ptr as usize, layout.size(), layout.align()) });
    }
}

#[global_allocator]
static ALLOCATOR: Kernel = Kernel;

/// Bytes ever handed out; never decreases (the storms' watermark, ADR-198).
pub fn used_bytes() -> usize {
    with_heap(|h| h.gross_bytes() as usize)
}

/// Bytes held by live allocations right now.
pub fn live_bytes() -> usize {
    with_heap(|h| h.live_bytes())
}

/// Bytes available to the next allocation.
pub fn free_bytes() -> usize {
    with_heap(|h| h.free_bytes())
}
