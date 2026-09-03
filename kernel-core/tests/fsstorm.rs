//! Host proofs for the filesystem storm (REQ-QUAL-007 / REQ-FS-001, ADR-088).
//!
//! Like ADR-086's desktop storm, this counts the way the kernel's bump heap counts: GROSS bytes
//! handed out, frees ignored. A structure that grows and drops its old buffer costs nothing on a
//! host allocator and costs that buffer forever on the machine.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use kernel_core::fsstorm::storm_suite;

static GROSS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; only the counter is ours.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            GROSS.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            GROSS.fetch_add(new_size, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn the_boot_suite_passes_on_the_host_against_a_never_freeing_counter() {
    let mut used = || GROSS.load(Ordering::Relaxed);
    let mut seen = 0;
    let n = storm_suite(&mut used, |k, ok, name| {
        seen += 1;
        assert_eq!(k, seen);
        assert!(ok, "{name}");
    })
    .unwrap();
    assert_eq!(n, 5);
}
