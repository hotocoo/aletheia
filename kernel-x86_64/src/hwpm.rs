//! x86 hardware power/performance control.
//!
//! This is the hardware half of ADR-076. The generic PM engine remains the policy and authority
//! boundary; this module is only the privileged actuator.
//!
//! Intel HWP is the first backend because it has an architectural capability MSR. A request may
//! reach the CPU's advertised highest performance point, but never a value above that point.
//! This is a hardware boost request, not an invented unlocked-ratio overclock.

use core::arch::asm;

const HWP_CAPABILITY_BIT: u32 = 1 << 7;
const IA32_PM_ENABLE: u32 = 0x770;
const IA32_HWP_CAPABILITIES: u32 = 0x771;
const IA32_HWP_REQUEST: u32 = 0x774;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwPmStatus {
    Unsupported,
    Hwp {
        lowest: u8,
        highest: u8,
        guaranteed: u8,
        most_efficient: u8,
    },
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nostack, preserves_flags)
        );
    }
}

/// Discover the architectural HWP envelope without changing CPU state.
pub fn probe() -> HwPmStatus {
    let max_leaf = core::arch::x86_64::__cpuid(0).eax;
    if max_leaf < 0x06 {
        return HwPmStatus::Unsupported;
    }
    let r = core::arch::x86_64::__cpuid_count(0x06, 0);
    if r.eax & HWP_CAPABILITY_BIT == 0 {
        return HwPmStatus::Unsupported;
    }
    let caps = unsafe { rdmsr(IA32_HWP_CAPABILITIES) };
    HwPmStatus::Hwp {
        lowest: (caps & 0xff) as u8,
        highest: ((caps >> 8) & 0xff) as u8,
        guaranteed: ((caps >> 16) & 0xff) as u8,
        most_efficient: ((caps >> 24) & 0xff) as u8,
    }
}

/// Enable architectural HWP. Idempotent; does not select a frequency.
pub fn enable() -> bool {
    if !matches!(probe(), HwPmStatus::Hwp { .. }) {
        return false;
    }
    unsafe {
        let current = rdmsr(IA32_PM_ENABLE);
        wrmsr(IA32_PM_ENABLE, current | 1);
    }
    true
}

/// Put HWP into an explicit latency/performance posture while remaining entirely inside the
/// processor's architectural envelope. `guaranteed` is the lowest requested performance point,
/// `highest` is both the maximum and desired point, and EPP=0 asks the hardware governor to prefer
/// performance over energy efficiency. This is deliberately different from unlocked-ratio
/// overclocking: no value outside `IA32_HWP_CAPABILITIES` is ever written.
pub fn request_performance_mode() -> Result<HwPmStatus, HwPmStatus> {
    let status = probe();
    let HwPmStatus::Hwp {
        guaranteed,
        highest,
        ..
    } = status
    else {
        return Err(status);
    };
    enable();
    unsafe {
        let request = rdmsr(IA32_HWP_REQUEST);
        let updated = (request & !0xffff_ffffu64)
            | (guaranteed as u64)
            | ((highest as u64) << 8)
            | ((highest as u64) << 16);
        wrmsr(IA32_HWP_REQUEST, updated);
    }
    Ok(status)
}

/// Read back the four architecturally visible HWP request fields used by the performance mode.
pub fn performance_request() -> Option<(u8, u8, u8, u8)> {
    if !matches!(probe(), HwPmStatus::Hwp { .. }) {
        return None;
    }
    let request = unsafe { rdmsr(IA32_HWP_REQUEST) };
    Some((
        (request & 0xff) as u8,
        ((request >> 8) & 0xff) as u8,
        ((request >> 16) & 0xff) as u8,
        ((request >> 24) & 0xff) as u8,
    ))
}
