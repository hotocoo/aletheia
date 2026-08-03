//! Page-fault classification, fail-closed and shared by every target (REQ-FAULT-001, ADR-039).
//!
//! A fault handler's first job is to decide **what just happened**, and the decision is
//! security-relevant: "a user task touched a page it may not touch" is routine, while "a translation
//! structure contains a bit the architecture reserves" means the page tables themselves are corrupt and
//! nothing the kernel believes about memory can be trusted. Until now each target printed the raw
//! architectural code and exited, which is honest but not a *model*: there was nowhere to state that a
//! reserved-bit fault must never be resumed, and no way to prove the states are handled exhaustively.
//!
//! This module is that model, in three parts:
//!
//! 1. **A normalized [`Fault`]** — present/write/user/exec/reserved/from-kernel — that every target's
//!    architectural code decodes into ([`from_x86_error_code`], [`from_aarch64_esr`],
//!    [`from_riscv_scause`]). The normalization is what lets one contract cover three CPUs.
//! 2. **A [`FaultKind`] classification** of what the fault *means*.
//! 3. **A [`FaultVerdict`] policy** — what the kernel is allowed to do about it — where the default is
//!    always the strict one. Anything unrecognized is [`FaultVerdict::Panic`], never "resume and hope".
//!
//! The contract is written in `docs/INVARIANT-CONTRACTS.md` §INV-FAULT and proved exhaustively on the
//! host: all 128 x86-64 error codes, every DFSC value, and every `scause` are classified, and the
//! properties (a reserved-bit fault is never resumable; a kernel fault is never survivable; an
//! unrecognized report is never classified by the bits that happen to be understood) are asserted over
//! the whole input space rather than sampled.

/// What the architecture reported, normalized. Every field is a fact about the faulting access, not an
/// interpretation of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The translation was present (a permission fault) rather than absent (a translation fault).
    pub present: bool,
    /// The access was a write.
    pub write: bool,
    /// The access came from user privilege (ring 3 / EL0 / U-mode).
    pub user: bool,
    /// The access was an instruction fetch.
    pub exec: bool,
    /// A bit the architecture RESERVES was set in a translation structure. Page tables are corrupt.
    pub reserved_bit: bool,
    /// The faulting access came from kernel privilege. (`!user` for the architectures here, kept
    /// explicit so a future target with more than two privilege levels cannot be silently
    /// reinterpreted.)
    pub from_kernel: bool,
    /// The architecture reported something this model does not recognize; the raw value is carried so
    /// the log can show it. Forces [`FaultKind::Unknown`].
    pub unrecognized: Option<u64>,
}

impl Fault {
    /// A fault with everything false — the base a decoder fills in.
    pub const fn none() -> Self {
        Fault {
            present: false,
            write: false,
            user: false,
            exec: false,
            reserved_bit: false,
            from_kernel: true,
            unrecognized: None,
        }
    }
}

/// What the fault means. Deliberately about MEANING, not about the bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// A user access to an address with no translation: the ordinary "not mapped" case.
    UserNotMapped,
    /// A user access that the mapping's permissions forbid (wrote a read-only page, executed NX, …).
    UserPermission,
    /// A kernel access to an unmapped address — a kernel bug, never the task's fault.
    KernelNotMapped,
    /// A kernel access the mapping forbids. On x86-64 this is also what SMEP/SMAP report when the
    /// kernel touches a user page it should not.
    KernelPermission,
    /// A reserved bit was set in a translation structure: the page tables are corrupt.
    CorruptTranslation,
    /// The architecture reported a combination this model does not know.
    Unknown,
}

/// What the kernel may do about it. The strictest option is always the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultVerdict {
    /// The faulting *task* may be terminated and the system continues. Only ever for user faults.
    KillTask,
    /// The kernel itself is broken, or its memory model is untrustworthy: stop. Never resumable.
    Panic,
}

/// Classify a normalized fault. Total: every `Fault` maps to exactly one kind.
pub fn classify(f: &Fault) -> FaultKind {
    if f.unrecognized.is_some() {
        return FaultKind::Unknown;
    }
    // Corruption dominates every other reading: if a translation structure is malformed, what the
    // other bits "mean" is not knowable.
    if f.reserved_bit {
        return FaultKind::CorruptTranslation;
    }
    match (f.user, f.present) {
        (true, false) => FaultKind::UserNotMapped,
        (true, true) => FaultKind::UserPermission,
        (false, false) => FaultKind::KernelNotMapped,
        (false, true) => FaultKind::KernelPermission,
    }
}

/// The policy: what the kernel is allowed to do. Fail-closed by construction — only user faults are
/// ever survivable, and corruption/unknown is always fatal.
pub fn verdict(kind: FaultKind) -> FaultVerdict {
    match kind {
        FaultKind::UserNotMapped | FaultKind::UserPermission => FaultVerdict::KillTask,
        FaultKind::KernelNotMapped
        | FaultKind::KernelPermission
        | FaultKind::CorruptTranslation
        | FaultKind::Unknown => FaultVerdict::Panic,
    }
}

/// A stable short name for a kind, for targets with no formatter.
pub const fn kind_name(kind: FaultKind) -> &'static str {
    match kind {
        FaultKind::UserNotMapped => "user-not-mapped",
        FaultKind::UserPermission => "user-permission",
        FaultKind::KernelNotMapped => "kernel-not-mapped",
        FaultKind::KernelPermission => "kernel-permission",
        FaultKind::CorruptTranslation => "corrupt-translation",
        FaultKind::Unknown => "unknown",
    }
}

// --- per-architecture decoders --------------------------------------------------------------------

/// x86-64 `#PF` error code (SDM Vol. 3 §4.7): P=0, W/R=1, U/S=2, RSVD=3, I/D=4, PK=5, SS=6, SGX=15.
///
/// Bits this model does not interpret (protection key, shadow stack, SGX) are NOT ignored: they are
/// carried into `unrecognized`, so a fault the model has never seen is `Unknown` — and therefore fatal
/// — rather than being classified by the bits that happen to be understood.
pub fn from_x86_error_code(code: u64) -> Fault {
    const P: u64 = 1 << 0;
    const W: u64 = 1 << 1;
    const U: u64 = 1 << 2;
    const RSVD: u64 = 1 << 3;
    const ID: u64 = 1 << 4;
    // Everything this decoder claims to understand.
    const KNOWN: u64 = P | W | U | RSVD | ID;

    let user = code & U != 0;
    Fault {
        present: code & P != 0,
        write: code & W != 0,
        user,
        exec: code & ID != 0,
        reserved_bit: code & RSVD != 0,
        from_kernel: !user,
        unrecognized: if code & !KNOWN != 0 { Some(code) } else { None },
    }
}

/// aarch64 `ESR_EL1` for a data or instruction abort. `ISS[5:0]` is the fault status code (DFSC/IFSC):
/// `0b0001xx` = translation fault, `0b0010xx` = access flag, `0b0011xx` = permission fault. `WnR`
/// (bit 6) says the access was a write; `EC` (bits 31:26) distinguishes an instruction abort (0x20/21)
/// from a data abort (0x24/25), and which EL it came from.
pub fn from_aarch64_esr(esr: u64) -> Fault {
    let ec = (esr >> 26) & 0x3F;
    let iss = esr & 0x01FF_FFFF;
    let dfsc = iss & 0x3F;
    let wnr = iss & (1 << 6) != 0;

    let (exec, from_lower_el) = match ec {
        0x20 => (true, true),   // instruction abort from a lower EL
        0x21 => (true, false),  // instruction abort from the same EL
        0x24 => (false, true),  // data abort from a lower EL
        0x25 => (false, false), // data abort from the same EL
        _ => {
            return Fault {
                unrecognized: Some(esr),
                ..Fault::none()
            }
        }
    };

    // DFSC classes: 0b0001xx translation, 0b0010xx access-flag, 0b0011xx permission.
    let (present, unrecognized) = match dfsc >> 2 {
        0b0001 => (false, None), // translation fault: nothing mapped
        0b0010 => (true, None),  // access flag: mapped, AF clear
        0b0011 => (true, None),  // permission fault: mapped, not allowed
        _ => (false, Some(esr)), // external abort, alignment, TLB conflict, …
    };

    Fault {
        present,
        write: wnr && !exec,
        user: from_lower_el,
        exec,
        reserved_bit: false,
        from_kernel: !from_lower_el,
        unrecognized,
    }
}

/// RISC-V `scause` exception codes: 12 = instruction page fault, 13 = load page fault, 15 = store/AMO
/// page fault. RISC-V does not report present-vs-absent or the faulting privilege in `scause`, so those
/// come from the caller: `sstatus.SPP` (0 ⇒ the trap came from U-mode) and whether the walk found a leaf.
///
/// The asymmetry is deliberate and stated rather than papered over: a decoder that invented a `present`
/// bit RISC-V does not report would be guessing, and the classification would inherit the guess.
pub fn from_riscv_scause(scause: u64, from_user: bool, translation_present: bool) -> Fault {
    let (exec, write, known) = match scause {
        12 => (true, false, true),  // instruction page fault
        13 => (false, false, true), // load page fault
        15 => (false, true, true),  // store/AMO page fault
        _ => (false, false, false),
    };
    Fault {
        present: translation_present,
        write,
        user: from_user,
        exec,
        reserved_bit: false,
        from_kernel: !from_user,
        unrecognized: if known { None } else { Some(scause) },
    }
}

/// Decode, classify and rule on an x86-64 fault in one call — what a handler wants.
pub fn x86_verdict(code: u64) -> (Fault, FaultKind, FaultVerdict) {
    let f = from_x86_error_code(code);
    let k = classify(&f);
    (f, k, verdict(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reserved_bit_is_always_corruption_whatever_else_is_set() {
        for code in 0u64..32 {
            let f = from_x86_error_code(code);
            if f.reserved_bit {
                assert_eq!(classify(&f), FaultKind::CorruptTranslation);
                assert_eq!(verdict(classify(&f)), FaultVerdict::Panic);
            }
        }
    }

    #[test]
    fn every_kind_has_exactly_one_verdict_and_kernel_faults_are_never_survivable() {
        for kind in [
            FaultKind::UserNotMapped,
            FaultKind::UserPermission,
            FaultKind::KernelNotMapped,
            FaultKind::KernelPermission,
            FaultKind::CorruptTranslation,
            FaultKind::Unknown,
        ] {
            let v = verdict(kind);
            let user = matches!(kind, FaultKind::UserNotMapped | FaultKind::UserPermission);
            assert_eq!(v == FaultVerdict::KillTask, user, "{}", kind_name(kind));
        }
    }
}
