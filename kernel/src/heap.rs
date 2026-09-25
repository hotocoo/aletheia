//! Minimal bump allocator over the linker-reserved heap region (`__heap_start..__heap_end`).
//! Never frees — sufficient for a boot-run-exit reference kernel. SMP-safe as of REQ-SMP-002:
//! the bump pointer advances by compare-and-swap, so concurrent allocations on different cores
//! each carve a disjoint region (the old load-then-store pair could hand two cores the same
//! bytes). Enables `alloc` (Vec/String/BTreeMap) so the in-kernel spine can mirror the hosted
//! System Core's data structures without a full page allocator (that lands in a later phase).
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

// SAFETY: the bump pointer is advanced by CAS, so concurrent `alloc` calls (multiple cores,
// REQ-SMP-002) each win a disjoint region or retry. Sync is required for a
// #[global_allocator] static.
unsafe impl Sync for BumpAlloc {}

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

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "heaptrace")]
        if TRACE.load(Ordering::Relaxed) {
            trace(layout.size());
        }
        let heap_start = &__heap_start as *const u8 as usize;
        let heap_end = &__heap_end as *const u8 as usize;

        loop {
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
            // CAS: if another core advanced `next` since our load, retry with the fresh value.
            if self
                .next
                .compare_exchange_weak(cur, new_next, Ordering::Relaxed, Ordering::Relaxed)
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

/// Bytes still available - the margin every later allocation lives in (ADR-154).
pub fn free_bytes() -> usize {
    let heap_start = unsafe { &__heap_start as *const u8 as usize };
    let heap_end = unsafe { &__heap_end as *const u8 as usize };
    (heap_end - heap_start).saturating_sub(used_bytes())
}
