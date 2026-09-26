//! A kernel heap that frees (ADR-197).
//!
//! Until this module every target's heap was a bump allocator: `dealloc` did nothing and memory
//! came back only at reboot (ADR-063). That kept allocation lock-free and made every "this path
//! allocates nothing" proof a simple watermark read, but it bounded everything that allocates and
//! frees — a display-mode switch, a long browsing session — by the heap's size.
//!
//! This allocator reclaims:
//!
//! * **Small blocks** (size and alignment up to [`MAX_SMALL`]) are rounded up to a power-of-two
//!   size class from 16 bytes; each class keeps an intrusive free list (the next pointer lives in the
//!   free block itself). A block of class `c` is carved `c`-aligned, so any alignment up to `c` holds.
//! * **Large blocks** are whole pages, page-aligned, kept on a first-fit free list that splits a
//!   larger block and coalesces a freed block with its neighbours.
//! * Fresh memory comes from a bump pointer over the region, as before.
//!
//! The proofs keep their meaning: [`Heap::gross_bytes`] counts every byte ever handed out and never
//! goes down — exactly what the old bump watermark measured — so a storm that asserts "allocates
//! nothing" still catches a path that allocates and frees per event. [`Heap::free_bytes`] is the
//! space actually available again.
//!
//! Locking is the caller's: the heap takes `&mut self`. Each target wraps it in a spin lock taken
//! with interrupts masked, because the desktop's pump allocates from the timer interrupt.

/// The smallest block handed out: room for the free-list link.
pub const MIN_BLOCK: usize = 16;
/// The largest small-class block; anything bigger (or more strictly aligned) is page-granular.
pub const MAX_SMALL: usize = 4096;
/// The page granule of large blocks.
pub const PAGE: usize = 4096;
const CLASSES: usize = 9; // 16, 32, ..., 4096

/// One free large run: `pages` pages starting at its own address. Kept sorted by address.
#[repr(C)]
struct Run {
    pages: usize,
    next: usize,
}

/// The heap over one region. All addresses are plain `usize`; nothing here dereferences memory
/// outside `[start, end)`.
pub struct Heap {
    start: usize,
    end: usize,
    next: usize,
    classes: [usize; CLASSES],
    runs: usize,
    gross: u64,
    live: usize,
    free_small: usize,
    free_large: usize,
    failures: u64,
    /// Bytes skipped to align a fresh block: not reclaimable, counted so nothing goes unnamed.
    padding: usize,
}

fn class_of(size: usize, align: usize) -> Option<usize> {
    let need = size.max(align).max(MIN_BLOCK);
    if need > MAX_SMALL {
        return None;
    }
    let c = need.next_power_of_two();
    Some(c.trailing_zeros() as usize - MIN_BLOCK.trailing_zeros() as usize)
}

impl Heap {
    /// An empty heap. [`Heap::init`] gives it its region.
    pub const fn empty() -> Self {
        Heap {
            start: 0,
            end: 0,
            next: 0,
            classes: [0; CLASSES],
            runs: 0,
            gross: 0,
            live: 0,
            free_small: 0,
            free_large: 0,
            failures: 0,
            padding: 0,
        }
    }

    /// Give the heap `[start, end)`. Idempotent: a heap that already has a region keeps it.
    pub fn init(&mut self, start: usize, end: usize) {
        if self.end == 0 && end > start {
            self.start = start;
            self.end = end;
            self.next = start;
        }
    }

    fn bump(&mut self, size: usize, align: usize) -> usize {
        let aligned = match self.next.checked_add(align - 1) {
            Some(v) => v & !(align - 1),
            None => return 0,
        };
        match aligned.checked_add(size) {
            Some(n) if n <= self.end => {
                self.padding += aligned - self.next;
                self.next = n;
                aligned
            }
            _ => 0,
        }
    }

    /// Allocate `size` bytes at `align`. Returns 0 when the region is exhausted (fail closed).
    ///
    /// # Safety
    /// `align` must be a power of two, and the region handed to `init` must be memory this heap
    /// exclusively owns.
    pub unsafe fn alloc(&mut self, size: usize, align: usize) -> usize {
        if self.end == 0 || size == 0 {
            return 0;
        }
        let p = match class_of(size, align) {
            Some(c) => {
                let bsize = MIN_BLOCK << c;
                let head = self.classes[c];
                if head != 0 {
                    self.classes[c] = *(head as *const usize);
                    self.free_small -= bsize;
                    head
                } else {
                    self.bump(bsize, bsize)
                }
                .then_count(bsize, self)
            }
            None => {
                let pages = size.div_ceil(PAGE);
                if align > PAGE {
                    self.bump(pages * PAGE, align)
                        .then_count(pages * PAGE, self)
                } else {
                    match self.take_run(pages) {
                        0 => self.bump(pages * PAGE, PAGE),
                        r => r,
                    }
                    .then_count(pages * PAGE, self)
                }
            }
        };
        if p == 0 {
            self.failures += 1;
        }
        p
    }

    /// First-fit a run of `pages` from the large free list, splitting a larger one.
    unsafe fn take_run(&mut self, pages: usize) -> usize {
        let mut prev: usize = 0;
        let mut cur = self.runs;
        while cur != 0 {
            let run = &mut *(cur as *mut Run);
            if run.pages >= pages {
                let next = run.next;
                let rest = run.pages - pages;
                let replacement = if rest > 0 {
                    let tail = cur + pages * PAGE;
                    let t = &mut *(tail as *mut Run);
                    t.pages = rest;
                    t.next = next;
                    tail
                } else {
                    next
                };
                if prev == 0 {
                    self.runs = replacement;
                } else {
                    (*(prev as *mut Run)).next = replacement;
                }
                self.free_large -= pages * PAGE;
                return cur;
            }
            prev = cur;
            cur = run.next;
        }
        0
    }

    /// Return a block. `size` and `align` must be the ones it was allocated with.
    ///
    /// # Safety
    /// `ptr` must have come from [`Heap::alloc`] on this heap with this `size` and `align`, and not
    /// have been freed since.
    pub unsafe fn dealloc(&mut self, ptr: usize, size: usize, align: usize) {
        if ptr == 0 || ptr < self.start || ptr >= self.end {
            return;
        }
        match class_of(size, align) {
            Some(c) => {
                let bsize = MIN_BLOCK << c;
                *(ptr as *mut usize) = self.classes[c];
                self.classes[c] = ptr;
                self.free_small += bsize;
                self.live -= bsize;
            }
            None if align > PAGE => {
                // Over-aligned large blocks are rare (none in this kernel today) and would need
                // their alignment remembered to be reused safely: they are not reclaimed.
                self.live -= size.div_ceil(PAGE) * PAGE;
            }
            None => {
                let pages = size.div_ceil(PAGE);
                self.live -= pages * PAGE;
                self.free_large += pages * PAGE;
                self.insert_run(ptr, pages);
            }
        }
    }

    /// Insert a run into the address-sorted list and coalesce it with touching neighbours.
    unsafe fn insert_run(&mut self, addr: usize, pages: usize) {
        let mut prev: usize = 0;
        let mut cur = self.runs;
        while cur != 0 && cur < addr {
            prev = cur;
            cur = (*(cur as *const Run)).next;
        }
        let node = &mut *(addr as *mut Run);
        node.pages = pages;
        node.next = cur;
        if cur != 0 && addr + pages * PAGE == cur {
            let c = &*(cur as *const Run);
            node.pages += c.pages;
            node.next = c.next;
        }
        if prev == 0 {
            self.runs = addr;
        } else {
            let p = &mut *(prev as *mut Run);
            if prev + p.pages * PAGE == addr {
                p.pages += node.pages;
                p.next = node.next;
            } else {
                p.next = addr;
            }
        }
    }

    /// Every byte ever handed out; never decreases (the old bump watermark's meaning).
    pub fn gross_bytes(&self) -> u64 {
        self.gross
    }
    /// Bytes held by live allocations right now.
    pub fn live_bytes(&self) -> usize {
        self.live
    }
    /// Bytes available again: untouched region plus every freed block.
    pub fn free_bytes(&self) -> usize {
        self.end.saturating_sub(self.next) + self.free_small + self.free_large
    }
    /// The region never yet touched (the bump pointer's headroom).
    pub fn fresh_bytes(&self) -> usize {
        self.end.saturating_sub(self.next)
    }
    /// Bytes lost to alignment padding (never reclaimable).
    pub fn padding_bytes(&self) -> usize {
        self.padding
    }
    /// Allocations refused for want of memory.
    pub fn failures(&self) -> u64 {
        self.failures
    }
}

trait CountExt {
    fn then_count(self, bytes: usize, h: &mut Heap) -> usize;
}
impl CountExt for usize {
    fn then_count(self, bytes: usize, h: &mut Heap) -> usize {
        if self != 0 {
            h.gross += bytes as u64;
            h.live += bytes;
        }
        self
    }
}

/// The heap's contract, proved at boot over a scratch region the caller lends (no global heap is
/// touched). `region` must be at least 64 KiB, page-aligned.
///
/// # Safety
/// `[region, region + len)` must be memory the caller owns and nothing else uses meanwhile.
pub unsafe fn kheap_suite(
    region: usize,
    len: usize,
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n = 0u32;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }
    let mut h = Heap::empty();
    h.init(region, region + len);

    // 1 - a freed small block is the next one handed out for its class; gross still grows.
    let a = h.alloc(40, 8);
    h.dealloc(a, 40, 8);
    let b = h.alloc(33, 8);
    check!(
        a != 0 && a == b && h.gross_bytes() == 128 && h.live_bytes() == 64,
        "kheap: a freed block is reused for its class, and gross bytes still count every hand-out"
    );
    h.dealloc(b, 33, 8);

    // 2 - alignment is honoured in every class and on the large path.
    let mut aligned = true;
    for (sz, al) in [(1, 1), (24, 16), (100, 64), (3000, 2048), (5000, 4096)] {
        let p = h.alloc(sz, al);
        aligned &= p != 0 && p.is_multiple_of(al);
        h.dealloc(p, sz, al);
    }
    check!(
        aligned,
        "kheap: every block is aligned as asked, small and large"
    );

    // 3 - large runs split and coalesce: three pages freed in any order become one run again.
    //     A fresh heap over the region's second half, so earlier runs cannot interleave.
    let mut h = {
        let mut g = Heap::empty();
        g.init(region + len / 2, region + len);
        g
    };
    let fresh = h.fresh_bytes();
    // PAGE + 1 bytes: two pages each, on the large path (a PAGE-sized block is the top small class).
    let x = h.alloc(PAGE + 1, 8);
    let y = h.alloc(PAGE + 1, 8);
    let z = h.alloc(PAGE + 1, 8);
    h.dealloc(y, PAGE + 1, 8);
    h.dealloc(x, PAGE + 1, 8);
    h.dealloc(z, PAGE + 1, 8);
    let big = h.alloc(6 * PAGE, 8);
    check!(
        big == x && h.fresh_bytes() == fresh - 6 * PAGE,
        "kheap: freed pages coalesce into one run that serves a larger request without new memory"
    );
    h.dealloc(big, 6 * PAGE, 8);

    // 4 - a steady alloc/free cycle takes no fresh memory after its first round.
    for _ in 0..4 {
        let v = h.alloc(2 * PAGE + 1, 8);
        let w = h.alloc(48, 8);
        h.dealloc(w, 48, 8);
        h.dealloc(v, 2 * PAGE + 1, 8);
    }
    let settled = h.fresh_bytes();
    for _ in 0..256 {
        let v = h.alloc(2 * PAGE + 1, 8);
        let w = h.alloc(48, 8);
        h.dealloc(w, 48, 8);
        h.dealloc(v, 2 * PAGE + 1, 8);
    }
    check!(
        h.fresh_bytes() == settled && h.live_bytes() == 0,
        "kheap: two hundred and fifty-six alloc/free cycles reuse memory and leave nothing live"
    );

    // 5 - exhaustion fails closed and is counted; freeing makes the memory available again.
    let mut held = [0usize; 64];
    let mut k = 0;
    while k < held.len() {
        let p = h.alloc(PAGE + 1, 8);
        if p == 0 {
            break;
        }
        held[k] = p;
        k += 1;
    }
    let mut exhausted = h.alloc(len, 8) == 0 && h.failures() >= 1;
    for p in &held[..k] {
        h.dealloc(*p, PAGE + 1, 8);
    }
    exhausted &= h.alloc(PAGE + 1, 8) != 0;
    check!(
        exhausted,
        "kheap: a request past the region fails closed and is counted; freed memory serves again"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn region(len: usize) -> (Vec<u8>, usize) {
        let v = std::vec![0u8; len + PAGE];
        let base = (v.as_ptr() as usize).div_ceil(PAGE) * PAGE;
        (v, base)
    }

    #[test]
    fn the_boot_suite_holds_on_the_host() {
        let (_keep, base) = region(1 << 20);
        let n = unsafe { kheap_suite(base, 1 << 20, |_, p, name| assert!(p, "{name}")) }.unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn a_seeded_random_workload_never_overlaps_and_accounts_exactly() {
        let (_keep, base) = region(4 << 20);
        let mut h = Heap::empty();
        h.init(base, base + (4 << 20));
        let mut live: Vec<(usize, usize, usize)> = Vec::new();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if live.len() < 400 && (!x.is_multiple_of(3) || live.is_empty()) {
                let size = match x % 10 {
                    0 => 4097 + (x >> 8) as usize % 30_000,
                    _ => 1 + (x >> 8) as usize % 3000,
                };
                let align = 1usize << ((x >> 40) % 7);
                let p = unsafe { h.alloc(size, align) };
                if p == 0 {
                    continue;
                }
                assert_eq!(p % align, 0);
                for &(q, qs, _) in &live {
                    assert!(p + size <= q || q + qs <= p, "overlap");
                }
                unsafe { core::ptr::write_bytes(p as *mut u8, 0xA5, size) };
                live.push((p, size, align));
            } else {
                let i = (x >> 20) as usize % live.len();
                let (p, s, a) = live.swap_remove(i);
                unsafe { h.dealloc(p, s, a) };
            }
        }
        for (p, s, a) in live.drain(..) {
            unsafe { h.dealloc(p, s, a) };
        }
        assert_eq!(h.live_bytes(), 0);
        assert_eq!(
            h.free_bytes() + h.padding_bytes(),
            4 << 20,
            "everything freed is available again; only alignment padding is not"
        );
        assert!(h.padding_bytes() < (4 << 20) / 10, "padding stays small");
    }
}
