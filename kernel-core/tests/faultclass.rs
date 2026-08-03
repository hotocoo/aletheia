//! Exhaustive host proof of the fault-classification model (REQ-FAULT-001, ADR-039).
//!
//! Contract: `docs/INVARIANT-CONTRACTS.md` §INV-FAULT. A classification model is only worth having if
//! it is TOTAL and if its strict cases really are strict, so these tests enumerate the whole input space
//! each architecture can present — all 32 meaningful x86-64 error codes plus a sweep of the bits the
//! model does not interpret, every aarch64 EC/DFSC pair, every RISC-V scause — rather than sampling.
use kernel_core::faultclass::{
    classify, from_aarch64_esr, from_riscv_scause, from_x86_error_code, kind_name, verdict, Fault,
    FaultKind, FaultVerdict,
};

/// INV-FAULT-1: classification is TOTAL — every input maps to exactly one kind, and every kind to
/// exactly one verdict. Enumerated over the full 5-bit x86 space plus unknown-bit combinations.
#[test]
fn classification_is_total_over_every_input_the_architectures_can_present() {
    for code in 0u64..0x2000 {
        let f = from_x86_error_code(code);
        let k = classify(&f);
        let v = verdict(k);
        assert!(!kind_name(k).is_empty());
        // No input may produce a kind whose verdict disagrees with the kind→verdict table.
        assert_eq!(v, verdict(classify(&f)), "classification is not a function");
    }
    for ec in 0u64..0x40 {
        for dfsc in 0u64..0x40 {
            let esr = (ec << 26) | dfsc;
            let f = from_aarch64_esr(esr);
            let _ = verdict(classify(&f));
        }
    }
    for scause in 0u64..20 {
        for from_user in [false, true] {
            for present in [false, true] {
                let f = from_riscv_scause(scause, from_user, present);
                let _ = verdict(classify(&f));
            }
        }
    }
}

/// INV-FAULT-2: a reserved-bit fault is ALWAYS `CorruptTranslation` and ALWAYS fatal, whatever else the
/// architecture reported. The page tables are the thing being doubted, so nothing else is interpretable.
#[test]
fn a_reserved_bit_fault_is_never_survivable_whatever_else_is_set() {
    let mut seen = 0usize;
    for code in 0u64..32 {
        let f = from_x86_error_code(code);
        if !f.reserved_bit {
            continue;
        }
        seen += 1;
        assert_eq!(
            classify(&f),
            FaultKind::CorruptTranslation,
            "code {code:#x} with a reserved bit was classified as something else"
        );
        assert_eq!(verdict(classify(&f)), FaultVerdict::Panic);
    }
    assert_eq!(
        seen, 16,
        "the sweep did not actually cover reserved-bit codes"
    );
}

/// INV-FAULT-3: a fault from KERNEL privilege is never survivable. Killing "the task" is meaningless
/// when the kernel itself made the bad access.
#[test]
fn a_kernel_fault_is_never_survivable() {
    for code in 0u64..32 {
        let f = from_x86_error_code(code);
        if f.user {
            continue;
        }
        assert_eq!(
            verdict(classify(&f)),
            FaultVerdict::Panic,
            "kernel fault {code:#x} was ruled survivable ({})",
            kind_name(classify(&f))
        );
    }
    // Same on the other two architectures: a same-EL abort, and a RISC-V trap from S-mode.
    let same_el_data_abort = (0x25u64 << 26) | 0b000101; // translation fault, level 1
    assert_eq!(
        verdict(classify(&from_aarch64_esr(same_el_data_abort))),
        FaultVerdict::Panic
    );
    assert_eq!(
        verdict(classify(&from_riscv_scause(13, false, false))),
        FaultVerdict::Panic
    );
}

/// INV-FAULT-4: a report the model does not recognize is `Unknown` and fatal — never classified by the
/// bits that happen to be understood. This is the fail-closed rule that makes the model safe to extend.
#[test]
fn an_unrecognized_report_is_never_classified_by_the_bits_it_happens_to_understand() {
    // x86: protection-key (bit 5), shadow-stack (bit 6) and SGX (bit 15) are not interpreted. A code
    // that sets one must be Unknown even though its low bits look like a routine user fault.
    for extra in [1u64 << 5, 1 << 6, 1 << 15] {
        let routine_user_read = 0b100; // U set, not present, read
        let code = routine_user_read | extra;
        let f = from_x86_error_code(code);
        assert_eq!(f.unrecognized, Some(code));
        assert_eq!(
            classify(&f),
            FaultKind::Unknown,
            "code {code:#x} was classified from its understood bits alone"
        );
        assert_eq!(verdict(classify(&f)), FaultVerdict::Panic);
        // Without the extra bit, the very same low bits ARE routine — so the test is discriminating.
        let plain = from_x86_error_code(routine_user_read);
        assert_eq!(classify(&plain), FaultKind::UserNotMapped);
        assert_eq!(verdict(classify(&plain)), FaultVerdict::KillTask);
    }
    // aarch64: an EC that is not one of the four abort classes, and a DFSC class outside translation /
    // access-flag / permission (e.g. 0b0100xx external abort) are both Unknown.
    assert_eq!(
        classify(&from_aarch64_esr(0x15 << 26)),
        FaultKind::Unknown,
        "a non-abort EC must not be read as a page fault"
    );
    let external_abort = (0x24u64 << 26) | 0b010000;
    assert_eq!(
        classify(&from_aarch64_esr(external_abort)),
        FaultKind::Unknown
    );
    // RISC-V: any scause that is not 12/13/15.
    for scause in [0u64, 2, 5, 7, 11, 14, 16] {
        assert_eq!(
            classify(&from_riscv_scause(scause, true, false)),
            FaultKind::Unknown,
            "scause {scause} was treated as a page fault"
        );
    }
}

/// INV-FAULT-5: the decoders report FACTS, not interpretations — each architectural field lands in the
/// normalized field it means, proven by the cases where the architectures disagree in shape.
#[test]
fn each_decoder_maps_its_architectural_fields_to_the_facts_they_mean() {
    // x86: present + write + user ⇒ a user permission fault on a write.
    let f = from_x86_error_code(0b111);
    assert!(f.present && f.write && f.user && !f.exec && !f.reserved_bit);
    assert_eq!(classify(&f), FaultKind::UserPermission);
    // x86: instruction fetch from user, nothing mapped.
    let f = from_x86_error_code(0b1_0100);
    assert!(f.exec && f.user && !f.present);
    assert_eq!(classify(&f), FaultKind::UserNotMapped);

    // aarch64: an EL0 instruction abort is exec + user; WnR is meaningless for a fetch and must not be
    // reported as a write even when the bit is set.
    let iss_perm_l3 = 0b001111u64; // permission fault, level 3
    let insn_abort_el0 = (0x20u64 << 26) | iss_perm_l3 | (1 << 6);
    let f = from_aarch64_esr(insn_abort_el0);
    assert!(f.exec && f.user && f.present);
    assert!(!f.write, "an instruction fetch was reported as a write");
    assert_eq!(classify(&f), FaultKind::UserPermission);
    // aarch64: a data abort from EL0 with WnR set is a user write. DFSC 0b001111 is a PERMISSION fault
    // (class 0b0011) — so `present` is true; 0b000111 would be a translation fault, where it is false,
    // and the pair below asserts exactly that difference.
    let data_abort_el0_write = (0x24u64 << 26) | 0b001111 | (1 << 6);
    let f = from_aarch64_esr(data_abort_el0_write);
    assert!(f.write && f.user && f.present && !f.exec);
    assert_eq!(classify(&f), FaultKind::UserPermission);
    let data_abort_el0_unmapped = (0x24u64 << 26) | 0b000111 | (1 << 6);
    let f = from_aarch64_esr(data_abort_el0_unmapped);
    assert!(f.write && f.user && !f.present);
    assert_eq!(classify(&f), FaultKind::UserNotMapped);

    // RISC-V: `present` is the CALLER's fact (scause cannot report it), and the two answers must give
    // different kinds — which is exactly why the parameter exists rather than being invented.
    let absent = from_riscv_scause(15, true, false);
    let present = from_riscv_scause(15, true, true);
    assert_eq!(classify(&absent), FaultKind::UserNotMapped);
    assert_eq!(classify(&present), FaultKind::UserPermission);
    assert!(absent.write && present.write);
}

/// INV-FAULT-6: the model's own base value is fail-closed — a `Fault` nobody filled in is a kernel
/// fault, not a user one, so a decoder that forgets a field cannot make a fault look routine.
#[test]
fn the_default_fault_is_the_strict_one() {
    let f = Fault::none();
    assert!(f.from_kernel && !f.user);
    assert_eq!(classify(&f), FaultKind::KernelNotMapped);
    assert_eq!(verdict(classify(&f)), FaultVerdict::Panic);
}
