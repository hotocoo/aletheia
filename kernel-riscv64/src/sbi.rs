//! Supervisor Binary Interface (SBI) — the S-mode -> M-mode firmware call path on RISC-V.
//!
//! The kernel runs in S-mode; OpenSBI runs in M-mode below it. An `ecall` from S-mode traps into
//! the firmware, which services the request and returns. This is the RISC-V analogue of the
//! privilege boundary the aarch64 kernel crosses with `svc` and the x86-64 kernel with interrupts —
//! and it is genuinely exercised here: `probe()` calls the SBI Base extension (always present) to
//! read the spec version + implementation id, proving the boundary works before we rely on nothing
//! but it. Machine exit itself does NOT use SBI SRST (that can only signal success) — see `exit.rs`.
use core::arch::asm;

/// One SBI call. EID in a7, FID in a6, args in a0..a2; returns (error, value) in (a0, a1).
#[inline]
fn ecall(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> (isize, isize) {
    let (err, val): (isize, isize);
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") a0 as isize => err,
            inlateout("a1") a1 as isize => val,
            in("a2") a2,
            in("a6") fid,
            in("a7") eid,
            options(nostack),
        );
    }
    (err, val)
}

// SBI Base extension (EID 0x10) — mandatory in every SBI >= 0.2 implementation.
const EXT_BASE: usize = 0x10;
const BASE_GET_SPEC_VERSION: usize = 0;
const BASE_GET_IMPL_ID: usize = 1;

// SBI TIME extension (EID "TIME" = 0x5449_4D45) — the standard S-mode timer arming path. FID 0
// `set_timer(stime_value)` programs the next timer interrupt at absolute `time`-CSR value `stime`
// (and clears any currently-pending timer interrupt). Firmware-managed, so it works regardless of
// whether the Sstc extension is present — the robust choice for the preemptive scheduler.
const EXT_TIME: usize = 0x5449_4D45;
const TIME_SET_TIMER: usize = 0;

/// Arm the S-mode timer to fire when the `time` CSR reaches `abs` (absolute, in timebase ticks).
/// Also the way to *clear* a pending timer interrupt: call again with a future (or max) value.
pub fn set_timer(abs: u64) {
    // rv64: the 64-bit stime value fits in a single register argument (a0).
    let _ = ecall(EXT_TIME, TIME_SET_TIMER, abs as usize, 0, 0);
}

/// Prove the S->M SBI boundary works: read and print the spec version + implementation id. Returns
/// true if the firmware answered (error == 0), which it must for a conformant SBI.
pub fn probe() -> bool {
    let (err_v, version) = ecall(EXT_BASE, BASE_GET_SPEC_VERSION, 0, 0, 0);
    let (err_i, impl_id) = ecall(EXT_BASE, BASE_GET_IMPL_ID, 0, 0, 0);
    if err_v == 0 && err_i == 0 {
        let major = (version >> 24) & 0x7f;
        let minor = version & 0xff_ffff;
        let impl_name = match impl_id {
            0 => "Berkeley BBL",
            1 => "OpenSBI",
            2 => "Xvisor",
            3 => "KVM",
            4 => "RustSBI",
            5 => "Diosix",
            _ => "unknown",
        };
        kprintln!(
            "[sbi] S->M boundary OK: spec v{}.{}, impl={} (id {})",
            major,
            minor,
            impl_name,
            impl_id
        );
        true
    } else {
        kprintln!(
            "[sbi] base extension returned error (v={}, i={})",
            err_v,
            err_i
        );
        false
    }
}
