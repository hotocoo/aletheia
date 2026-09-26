//! `memset`, `memcpy`, `memmove` and `memcmp` for x86-64 programs (ADR-207). The prebuilt `core`
//! for `x86_64-unknown-none` calls them but its `compiler_builtins` does not provide them, so a
//! zeroed local array was a call through a null pointer. Written with volatile accesses so LLVM
//! cannot recognise the loop and turn it back into a call to itself.
#![cfg(target_arch = "x86_64")]

use core::ffi::c_void;

/// # Safety
/// `dst` is valid for `n` bytes of writes.
#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    let d = dst as *mut u8;
    for i in 0..n {
        // SAFETY: the caller guarantees `dst..dst + n` is writable.
        unsafe { core::ptr::write_volatile(d.add(i), c as u8) };
    }
    dst
}

/// # Safety
/// `src` is valid for `n` bytes of reads, `dst` for `n` bytes of writes, and they do not overlap.
#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst as *mut u8, src as *const u8);
    for i in 0..n {
        // SAFETY: as the caller guarantees.
        unsafe { core::ptr::write_volatile(d.add(i), core::ptr::read_volatile(s.add(i))) };
    }
    dst
}

/// # Safety
/// `src` is valid for `n` bytes of reads and `dst` for `n` bytes of writes; they may overlap.
#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if (dst as usize) <= (src as usize) {
        // SAFETY: copying forward never reads a byte already overwritten.
        unsafe { memcpy(dst, src, n) }
    } else {
        let (d, s) = (dst as *mut u8, src as *const u8);
        for i in (0..n).rev() {
            // SAFETY: copying backward never reads a byte already overwritten.
            unsafe { core::ptr::write_volatile(d.add(i), core::ptr::read_volatile(s.add(i))) };
        }
        dst
    }
}

/// # Safety
/// `a` and `b` are valid for `n` bytes of reads.
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (a as *const u8, b as *const u8);
    for i in 0..n {
        // SAFETY: as the caller guarantees.
        let (x, y) = unsafe {
            (
                core::ptr::read_volatile(a.add(i)),
                core::ptr::read_volatile(b.add(i)),
            )
        };
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

/// # Safety
/// As [`memcmp`].
#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // SAFETY: as the caller guarantees.
    unsafe { memcmp(a, b, n) }
}
