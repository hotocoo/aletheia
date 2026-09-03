//! Host proofs for the desktop storm (ALET-P2-021 / REQ-QUAL-007, ADR-086).
//!
//! The boot suite measures the KERNEL's own bump heap, which NEVER FREES (ADR-063). So this test
//! counts the way that heap counts: GROSS bytes handed out, with frees ignored. A `Vec` that
//! doubles and drops its old buffer costs nothing on a host allocator and costs the old buffer
//! forever on the machine — measuring net bytes here would let exactly that through.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use kernel_core::wmstorm::storm_suite;

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; only the counters are ours.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Deliberately NOT subtracted: the kernel heap cannot give bytes back, so neither does
        // this counter. What we measure is what that machine would keep.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn the_boot_suite_passes_on_the_host_against_a_real_allocator() {
    let mut used = || LIVE.load(Ordering::Relaxed);
    let mut seen = 0;
    let n = storm_suite(&mut used, |k, ok, name| {
        seen += 1;
        assert_eq!(k, seen);
        assert!(ok, "{name}");
    })
    .unwrap();
    assert_eq!(n, 6);
}
