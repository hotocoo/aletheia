//! The Aletheia syscall ABI, as a program sees it (ADR-205). Numbers from `kernel_core::syscall`.
//! Every program includes this module and uses part of it.
#![allow(dead_code)]

pub const SYS_EXIT: u64 = 3;
pub const SYS_FS_READ: u64 = 8;
pub const SYS_WRITE_CONSOLE: u64 = 12;
pub const SYS_PRESENT: u64 = 13;
pub const SYS_POLL_INPUT: u64 = 14;
pub const SYS_CLOCK: u64 = 15;

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

#[cfg(target_arch = "aarch64")]
unsafe fn syscall4(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("svc #0", in("x8") num, inlateout("x0") a0 => ret, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    ret
}

#[cfg(target_arch = "riscv64")]
unsafe fn syscall4(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("ecall", in("a7") num, inlateout("a0") a0 => ret, in("a1") a1, in("a2") a2, in("a3") a3, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall4(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    core::arch::asm!("int 0x80", inlateout("rax") num => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, options(nostack));
    ret
}

/// The raw `SYS_FS_READ` (ADR-207): for programs that probe the kernel's refusals on purpose.
pub fn read_raw(name: u64, name_len: u64, buf: u64, buf_len: u64) -> u64 {
    // SAFETY: the kernel validates all four arguments against this program's own pages.
    unsafe { syscall4(SYS_FS_READ, name, name_len, buf, buf_len) }
}

/// Read object `name` from the namespace that started this program into `buf`. Returns the
/// object's FULL length: more than `buf.len()` means only `buf.len()` bytes were copied.
pub fn read(name: &[u8], buf: &mut [u8]) -> Result<usize, ()> {
    let r = read_raw(
        name.as_ptr() as u64,
        name.len() as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
    );
    if r == u64::MAX {
        Err(())
    } else {
        Ok(r as usize)
    }
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

/// Show `bits` (a packed one-bit bitmap, LSB first, row-major) as a `width` x `height` frame in
/// this program's desktop window (ADR-215). `bits` must lie in the program's writable data.
pub fn present(bits: &[u8], width: u32, height: u32) -> Result<(), ()> {
    // SAFETY: the kernel validates the range and the size against this program's own pages.
    let r = unsafe {
        syscall4(
            SYS_PRESENT,
            bits.as_ptr() as u64,
            width as u64,
            height as u64,
            0,
        )
    };
    if r == u64::MAX {
        Err(())
    } else {
        Ok(())
    }
}

/// Show `pixels` (one RGB332 byte per pixel, `RRRGGGBB`, row-major) as a `width` x `height`
/// colour frame in this program's desktop window (ADR-218).
pub fn present_rgb332(pixels: &[u8], width: u32, height: u32) -> Result<(), ()> {
    // SAFETY: the kernel validates the range and the size against this program's own pages.
    let r = unsafe {
        syscall4(
            SYS_PRESENT,
            pixels.as_ptr() as u64,
            (1 << 32) | width as u64,
            height as u64,
            0,
        )
    };
    if r == u64::MAX {
        Err(())
    } else {
        Ok(())
    }
}

/// One input event at this program's window (ADR-216, ADR-217).
pub enum Input {
    /// A key, in the console's decoded alphabet.
    Key(u8),
    /// The pointer over the window, in its own coordinates: `held` while the left button is
    /// down, `click` when this event is that button changing.
    Pointer { x: u32, y: u32, held: bool, click: bool },
    /// The window lost the keyboard.
    FocusLost,
}

/// The next input event the operator gave this program's window: `Ok(None)` when none is
/// waiting, `Err(())` when the program holds no open window.
pub fn poll_input() -> Result<Option<Input>, ()> {
    // SAFETY: the call takes no pointers; the kernel answers from the program's own window.
    let r = unsafe { syscall2(SYS_POLL_INPUT, 0, 0) };
    Ok(Some(match r {
        u64::MAX => return Err(()),
        0 => return Ok(None),
        0x100..=0x1FF => Input::Key(r as u8),
        _ if r & (1 << 40) != 0 => Input::Pointer {
            x: (r & 0xFFFF) as u32,
            y: ((r >> 16) & 0xFFFF) as u32,
            held: r & (1 << 32) != 0,
            click: r & (1 << 33) != 0,
        },
        _ => Input::FocusLost,
    }))
}

/// Nanoseconds since the machine booted (ADR-222), monotonic.
pub fn clock_ns() -> u64 {
    // SAFETY: the call takes no pointers.
    unsafe { syscall2(SYS_CLOCK, 0, 0) }
}

/// End this program with `status`.
pub fn exit(status: u64) -> ! {
    // SAFETY: SYS_EXIT never returns to this task.
    unsafe { syscall2(SYS_EXIT, status, 0) };
    loop {
        core::hint::spin_loop();
    }
}
