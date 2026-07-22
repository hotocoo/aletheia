//! Kernel heap: a bump allocator over a fixed 8 MiB static region.
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
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_SIZE: usize = 8 * 1024 * 1024;

static HEAP_AREA: Racy<[u8; HEAP_SIZE]> = Racy::new([0u8; HEAP_SIZE]);

struct Bump {
    next: AtomicUsize, // 0 = uninitialized; lazily anchored to the heap base on first alloc.
}

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = HEAP_AREA.get().as_ptr() as usize;
        let end = base + HEAP_SIZE;
        loop {
            let cur = self.next.load(Ordering::Relaxed);
            let start = if cur == 0 { base } else { cur };
            let aligned = (start + layout.align() - 1) & !(layout.align() - 1);
            let new_next = match aligned.checked_add(layout.size()) {
                Some(n) => n,
                None => return core::ptr::null_mut(),
            };
            if new_next > end {
                return core::ptr::null_mut(); // out of heap => fail closed
            }
            if self
                .next
                .compare_exchange(cur, new_next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return aligned as *mut u8;
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator: memory is reclaimed only at reboot.
    }
}

#[global_allocator]
static ALLOCATOR: Bump = Bump {
    next: AtomicUsize::new(0),
};

pub fn used_bytes() -> usize {
    // SAFETY: reads a static; no exclusive borrow of HEAP_AREA is ever taken.
    let base = unsafe { HEAP_AREA.get().as_ptr() as usize };
    let cur = ALLOCATOR.next.load(Ordering::Relaxed);
    if cur == 0 {
        0
    } else {
        cur - base
    }
}
