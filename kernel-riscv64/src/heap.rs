//! Minimal bump allocator over the linker-reserved heap region (`__heap_start..__heap_end`).
//! Single-core, no preemption, never frees — sufficient for a boot-run-exit reference kernel.
//! Enables `alloc` (Vec/String/BTreeMap) so the in-kernel spine can mirror the hosted System Core's
//! data structures without a full page allocator (that lands in a later phase). Identical in shape
//! to the aarch64 backend's allocator — both bump over a linker-defined region on a `-kernel` ELF.
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

pub struct BumpAlloc {
    next: AtomicUsize,
}

impl BumpAlloc {
    const fn new() -> Self {
        BumpAlloc {
            next: AtomicUsize::new(0),
        }
    }
}

// SAFETY: the kernel is single-core with no preemption while allocating, so Relaxed load/store on
// `next` cannot race. Sync is required for a #[global_allocator] static.
unsafe impl Sync for BumpAlloc {}

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let heap_start = &__heap_start as *const u8 as usize;
        let heap_end = &__heap_end as *const u8 as usize;

        let cur = self.next.load(Ordering::Relaxed);
        let base = if cur == 0 { heap_start } else { cur };
        let aligned = (base + layout.align() - 1) & !(layout.align() - 1);
        let new_next = match aligned.checked_add(layout.size()) {
            Some(n) => n,
            None => return core::ptr::null_mut(),
        };
        if new_next > heap_end {
            return core::ptr::null_mut(); // out of heap -> allocation fails (fail closed)
        }
        self.next.store(new_next, Ordering::Relaxed);
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator: memory is reclaimed only at reboot.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAlloc = BumpAlloc::new();

/// Bytes used so far — reported by the observability line at boot.
pub fn used_bytes() -> usize {
    let heap_start = unsafe { &__heap_start as *const u8 as usize };
    let cur = ALLOCATOR.next.load(Ordering::Relaxed);
    if cur == 0 {
        0
    } else {
        cur - heap_start
    }
}
