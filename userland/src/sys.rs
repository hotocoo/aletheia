//! The Aletheia syscall ABI, as a program sees it (ADR-205). Numbers from `kernel_core::syscall`.

pub const SYS_EXIT: u64 = 3;
pub const SYS_WRITE_CONSOLE: u64 = 12;

#[cfg(target_arch = "aarch64")]
unsafe fn syscall2(num: u64, a0: u64, a1: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("svc #0", in("x8") num, inlateout("x0") a0 => ret, in("x1") a1, options(nostack));
    ret
}

#[cfg(target_arch = "riscv64")]
unsafe fn syscall2(num: u64, a0: u64, a1: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("ecall", in("a7") num, inlateout("a0") a0 => ret, in("a1") a1, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall2(num: u64, a0: u64, a1: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("int 0x80", inlateout("rax") num => ret, in("rdi") a0, in("rsi") a1, options(nostack));
    ret
}

/// Write `bytes` to the console that started this program. Returns the bytes the console kept.
pub fn write(bytes: &[u8]) -> Result<usize, ()> {
    // SAFETY: the kernel validates the range against this program's own pages before reading it.
    let r = unsafe { syscall2(SYS_WRITE_CONSOLE, bytes.as_ptr() as u64, bytes.len() as u64) };
    if r == u64::MAX {
        Err(())
    } else {
        Ok(r as usize)
    }
}

/// End this program with `status`.
pub fn exit(status: u64) -> ! {
    // SAFETY: SYS_EXIT never returns to this task.
    unsafe { syscall2(SYS_EXIT, status, 0) };
    loop {
        core::hint::spin_loop();
    }
}
