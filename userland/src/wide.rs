//! `wide` (ADR-214): a program whose writable memory is more than one page. It fills a three-page
//! `.bss` buffer with a pattern, reads the object its argument names into the middle of the second
//! page, and exits with a checksum over all three pages, so a run proves every data page is mapped,
//! zeroed, writable, distinct, and reachable by `SYS_FS_READ`.
#![no_std]
#![no_main]

mod mem;
mod sys;

const PAGES: usize = 3;
const PAGE: usize = 4096;

/// `.bss`, three pages: the kernel must hand all of them over zeroed.
static mut WIDE: [u8; PAGES * PAGE] = [0; PAGES * PAGE];

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: single-threaded program; nothing else touches this global.
    let wide = unsafe { &mut *core::ptr::addr_of_mut!(WIDE) };
    if wide.iter().any(|&b| b != 0) {
        let _ = sys::write(b"wide: .bss was not zero\n");
        sys::exit(u64::MAX);
    }
    // Each page gets its own byte, so a page mapped twice or not at all changes the sum.
    for (i, b) in wide.iter_mut().enumerate() {
        *b = (i / PAGE) as u8 + 1;
    }
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    let name = unsafe { core::slice::from_raw_parts(args, len) };
    if name == b"grow" {
        grow()
    }
    if !name.is_empty() {
        let at = PAGE + PAGE / 2;
        match sys::read(name, &mut wide[at..at + 256]) {
            Ok(n) => {
                let _ = sys::write(b"wide read: ");
                let _ = sys::write(&wide[at..at + n.min(256)]);
                let _ = sys::write(b"\n");
            }
            Err(()) => {
                let _ = sys::write(b"wide: the namespace refused that name\n");
                sys::exit(u64::MAX);
            }
        }
    }
    sys::exit(wide.iter().map(|&b| b as u64).sum())
}

/// `wide grow` (ADR-230): grow writable memory by eight pages at run time. A top past the ceiling
/// is refused, the new pages arrive zeroed and distinct, a read lands in the last of them, and
/// asking for less never shrinks. Exits with the checksum of the grown pages (442,368), or 1..6
/// naming the step that failed.
fn grow() -> ! {
    const GROWN: usize = 8;
    let Ok(top) = sys::brk(0) else { sys::exit(1) };
    if sys::brk(top + (1 << 30)).is_ok() {
        sys::exit(2);
    }
    let want = top + (GROWN * PAGE) as u64;
    if sys::brk(want) != Ok(want) {
        sys::exit(3);
    }
    // SAFETY: the kernel just mapped these pages, writable, for this program alone.
    let heap = unsafe { core::slice::from_raw_parts_mut(top as *mut u8, GROWN * PAGE) };
    if heap.iter().any(|&b| b != 0) {
        sys::exit(4);
    }
    for (i, b) in heap.iter_mut().enumerate() {
        *b = (i / PAGE) as u8 + 10;
    }
    let sum = heap.iter().map(|&b| b as u64).sum();
    let at = (GROWN - 1) * PAGE;
    match sys::read(b"note", &mut heap[at..at + 256]) {
        Ok(n) => {
            let _ = sys::write(b"wide grow: ");
            let _ = sys::write(&heap[at..at + n.min(256)]);
            let _ = sys::write(b"\n");
        }
        Err(()) => sys::exit(5),
    }
    if sys::brk(top) != Ok(want) {
        sys::exit(6);
    }
    sys::exit(sum)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
