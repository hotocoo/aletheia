//! Hosted proof of the shared frame allocator (ADR-271): the code every CPU target now runs.

use kernel_core::framealloc::{FrameAllocator, PhysFrame, FRAME_SIZE};
use kernel_core::frameown::Owner;

/// A page-aligned region the test owns, leaked for 'static (the pool links frames inside it).
fn region(frames: usize) -> (usize, usize) {
    let layout = std::alloc::Layout::from_size_align(frames * FRAME_SIZE, FRAME_SIZE).unwrap();
    // SAFETY: non-zero layout; leaked on purpose.
    let p = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
    (p, p + frames * FRAME_SIZE)
}

fn pool(frames: usize) -> FrameAllocator {
    let (base, end) = region(frames);
    let mut a = FrameAllocator::empty();
    // SAFETY: the region is ours, writable at its address, and nothing else uses it.
    unsafe { a.init(base, end) };
    let state: &'static mut [u8] = Box::leak(vec![0u8; frames].into_boxed_slice());
    assert!(a.attach_owners(state));
    a
}

#[test]
fn every_frame_is_handed_out_once_then_the_pool_refuses() {
    let mut a = pool(8);
    assert_eq!((a.total_count(), a.free_count()), (8, 8));
    let mut seen = std::collections::BTreeSet::new();
    while let Some(f) = a.alloc() {
        assert!(f.addr().is_multiple_of(FRAME_SIZE) && f.addr() >= a.base() && f.addr() < a.end());
        assert!(seen.insert(f.addr()), "a frame was handed out twice");
    }
    assert_eq!(
        (seen.len(), a.free_count(), a.owned_free_count()),
        (8, 0, Some(0))
    );
}

#[test]
fn a_double_free_a_foreign_free_and_a_stray_address_are_refused() {
    let mut a = pool(4);
    let f = a.alloc_as(Owner::PAGETABLE).unwrap();
    assert!(!a.free_as(f, Owner::KERNEL), "someone else's frame");
    assert!(!a.free(PhysFrame::from_addr(f.addr() + 1)), "misaligned");
    assert!(!a.free(PhysFrame::from_addr(a.end())), "outside the pool");
    assert!(a.free_as(f, Owner::PAGETABLE));
    assert!(!a.free_as(f, Owner::PAGETABLE), "double free");
    assert_eq!(a.free_count(), 4);
    assert_eq!(a.owned_free_count(), Some(4));
}

#[test]
fn a_freed_frame_is_erased_and_comes_back_zeroed() {
    let mut a = pool(2);
    let f = a.alloc().unwrap();
    // SAFETY: we hold `f`.
    unsafe { core::slice::from_raw_parts_mut(f.addr() as *mut u8, FRAME_SIZE).fill(0xAB) };
    assert!(a.free(f));
    // SAFETY: free frames are readable at their address; only the first word is the link.
    let bytes = unsafe { core::slice::from_raw_parts(f.addr() as *const u8, FRAME_SIZE) };
    assert!(bytes[8..].iter().all(|&b| b == 0), "erase on free");
    let g = a.alloc_zeroed().unwrap();
    // SAFETY: we hold `g`.
    let g_bytes = unsafe { core::slice::from_raw_parts(g.addr() as *const u8, FRAME_SIZE) };
    assert!(g_bytes.iter().all(|&b| b == 0));
    assert_eq!(a.owner_of(g.addr()), Some(Owner::KERNEL));
}

#[test]
fn an_owner_table_too_small_for_the_pool_is_refused() {
    let (base, end) = region(4);
    let mut a = FrameAllocator::empty();
    // SAFETY: as above.
    unsafe { a.init(base, end) };
    let state: &'static mut [u8] = Box::leak(vec![0u8; 3].into_boxed_slice());
    assert!(
        !a.attach_owners(state),
        "a pool with an unprotected tail is refused"
    );
}
