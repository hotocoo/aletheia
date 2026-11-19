//! The physical page-frame allocator every CPU target runs (ADR-271): an intrusive LIFO free list
//! over `[base, end)` with the ownership model ([`FrameOwnerTable`]) layered on top and erase on
//! free (REQ-MM-005). Until ADR-271 each kernel crate carried its own copy, identical but for the
//! handle's name; the targets now keep only their memory map, their state array and the global.
//!
//! Each free frame stores, in its own first 8 bytes, the physical address of the next free frame,
//! so the allocator needs no side table. That works before the MMU is on (physical == effective)
//! and afterwards because every target identity-maps its RAM, which is also why this module may
//! touch memory by physical address: its callers promise the range is theirs and identity-mapped.

use crate::frameown::{FrameOwnerTable, Owner};

/// The 4 KiB granule every target uses.
pub const FRAME_SIZE: usize = 4096;

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
    /// A handle naming the frame at `pa` without claiming it: what an address-taking free and a
    /// selftest's deliberately bad frame need. Every pool operation still validates it.
    #[inline]
    pub const fn from_addr(pa: usize) -> Self {
        PhysFrame(pa)
    }
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

/// Intrusive free-list frame allocator over a half-open physical range `[base, end)`, with the
/// arch-independent ownership model layered on top: the list says which frames are *available*,
/// the table says who holds each frame that is not.
pub struct FrameAllocator {
    head: usize, // phys addr of first free frame, or 0 when empty
    free: usize,
    total: usize,
    base: usize,
    end: usize,
    /// `None` until [`FrameAllocator::attach_owners`] runs. A pool with no table still bounds-checks
    /// but cannot detect a double free, so the global pool always attaches one during `init`.
    owners: Option<FrameOwnerTable<'static>>,
}

impl FrameAllocator {
    pub const fn empty() -> Self {
        FrameAllocator {
            head: 0,
            free: 0,
            total: 0,
            base: 0,
            end: 0,
            owners: None,
        }
    }

    /// Populate the free-list from every aligned frame in `[base, end)`. `base`/`end` are
    /// rounded inward to `FRAME_SIZE`. Frames are pushed high-to-low so the lowest ends up at
    /// the list head (cosmetic — the order carries no meaning).
    ///
    /// # Safety
    /// `[base, end)` must be RAM the caller owns exclusively and that is not otherwise live (not
    /// the running image, stack, or heap). The allocator writes a link word into each frame, so
    /// the range must be readable/writable at its identity address.
    pub unsafe fn init(&mut self, base: usize, end: usize) {
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

    /// Give this pool an ownership table covering exactly the frames it manages. Called once,
    /// right after [`FrameAllocator::init`]; the table is built from the pool's OWN base and frame
    /// count so the two can never describe different windows.
    pub fn attach_owners(&mut self, state: &'static mut [u8]) -> bool {
        match FrameOwnerTable::new(self.base, self.total, state) {
            Ok(t) => {
                self.owners = Some(t);
                true
            }
            // Fail-closed: a pool whose tail has no ownership state is worse than no pool, so the
            // caller learns instead of running unprotected.
            Err(_) => false,
        }
    }

    /// Allocate one frame for `owner`, or `None` when the pool is empty (fail-closed).
    ///
    /// Ownership is claimed BEFORE the frame leaves the list: if the table refuses (the frame is
    /// somehow already owned — the free-list corruption this whole model exists to catch) the frame
    /// stays on the list and the caller gets `None` rather than a page someone else holds.
    pub fn alloc_as(&mut self, owner: Owner) -> Option<PhysFrame> {
        if self.head == 0 {
            return None;
        }
        let f = self.head;
        if let Some(t) = self.owners.as_mut() {
            t.claim(f, owner).ok()?;
        }
        // SAFETY: `f` is a frame we previously pushed; its first word is the next-free link.
        self.head = unsafe { *(f as *const usize) };
        self.free -= 1;
        Some(PhysFrame(f))
    }

    /// Allocate one frame owned by the kernel itself.
    pub fn alloc(&mut self) -> Option<PhysFrame> {
        self.alloc_as(Owner::KERNEL)
    }

    /// Allocate one frame for `owner` and zero it — the required shape for a fresh page table (all
    /// entries invalid) or a cleared program page.
    pub fn alloc_zeroed_as(&mut self, owner: Owner) -> Option<PhysFrame> {
        let frame = self.alloc_as(owner)?;
        // SAFETY: we hold `frame` exclusively; it is accessible at its identity address.
        unsafe { frame.as_bytes_mut().fill(0) };
        Some(frame)
    }

    /// Allocate one zeroed frame owned by the kernel itself.
    pub fn alloc_zeroed(&mut self) -> Option<PhysFrame> {
        self.alloc_zeroed_as(Owner::KERNEL)
    }

    /// Return a frame held by `owner` to the pool. Rejected (returns `false`, list untouched) when
    /// the address is misaligned or out of region, and — with an ownership table attached — when
    /// the frame is already free (double free) or held by a different owner.
    pub fn free_as(&mut self, frame: PhysFrame, owner: Owner) -> bool {
        let a = frame.addr();
        if a < self.base || a >= self.end || !a.is_multiple_of(FRAME_SIZE) {
            return false;
        }
        if let Some(t) = self.owners.as_mut() {
            if t.release(a, owner).is_err() {
                return false;
            }
        }
        // ERASE ON FREE (REQ-MM-005, ADR-033, GAPS4 ALET-P2-026). Ownership stops two owners from
        // holding one frame at the same TIME; it says nothing about what the next owner can READ.
        // A page released by a task carrying keys, message bodies, or decrypted content kept those
        // bytes verbatim until someone overwrote them, so the next `alloc` — in any address space,
        // for any task — could read them. Erasing at RELEASE (not at allocation) makes the
        // guarantee unconditional: it holds for every path that returns a frame, including page-table
        // reclamation and address-space teardown, and it cannot be skipped by a caller who used
        // plain `alloc`. The link word `push_raw` writes below is the only thing left in the frame.
        // SAFETY: the release above proves this frame is ours and now unowned; it is identity-
        // accessible, so writing its whole 4 KiB is sound and observed by nobody else.
        unsafe { frame_bytes_mut(a).fill(0) };
        // SAFETY: `a` is a valid, aligned, in-range frame address; writing its link word is sound.
        unsafe { self.push_raw(a) };
        true
    }

    /// Return a kernel-owned frame to the pool.
    pub fn free(&mut self, frame: PhysFrame) -> bool {
        self.free_as(frame, Owner::KERNEL)
    }

    /// Current holder of `pa`, or `None` when it is free / not a frame of this pool.
    pub fn owner_of(&self, pa: usize) -> Option<Owner> {
        self.owners.as_ref()?.owner_of(pa).ok().flatten()
    }

    /// Frames the ownership table says are unowned. Must always equal [`Self::free_count`]: a
    /// divergence means the free list and the ownership model disagree.
    pub fn owned_free_count(&self) -> Option<usize> {
        Some(self.owners.as_ref()?.free_count())
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
    pub fn end(&self) -> usize {
        self.end
    }
}

/// Mutable byte view of the frame at physical address `a`, for erase-on-free.
///
/// SAFETY: `a` must be a frame this allocator owns, aligned and in range, with no other live
/// reference to its contents.
#[inline]
unsafe fn frame_bytes_mut(a: usize) -> &'static mut [u8] {
    core::slice::from_raw_parts_mut(a as *mut u8, FRAME_SIZE)
}
