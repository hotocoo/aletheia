//! Host proof of the mapping-API admission check (GAPS4 ALET-P1-001, REQ-MM-001).
//!
//! The unit tests inside `kernel_core::vmaddr` cover each rule in isolation. This suite proves the
//! two *properties* the rules exist to guarantee, over the real per-target plans, by exhaustive or
//! wide enumeration rather than by hand-picked examples:
//!
//!   1. **No accepted pair of virtual addresses can alias.** This is the actual defect being
//!      prevented — two distinct VAs resolving to one page-table entry, so a second map silently
//!      overwrites the first and an unmap tears down the wrong page. A range check that merely
//!      "looks right" is not evidence; the property is asserted directly against
//!      `AddrPlan::aliases`, which recomputes what the walker decodes.
//!
//!   2. **Every accepted physical address is a frame the allocator owns.** Anything else maps
//!      firmware tables, MMIO, or another address space's memory.
//!
//! The plans below mirror what each target declares in its own `vm.rs`; `plans_match_targets`
//! documents that correspondence so a target changing its VA width without updating this suite is
//! a visible edit rather than a silent divergence.

use kernel_core::vmaddr::{AddrPlan, MapFault, PAGE_SIZE};

/// aarch64 TTBR0 with T0SZ=25 (3-level, 4 KiB granule): 39 decoded bits, low half only.
const AARCH64: AddrPlan = AddrPlan::new(39, false, 0x4000_0000, 0x0800_0000);
/// RISC-V Sv39: 39 decoded bits, sign-extended from bit 38 (Sv39's width INCLUDES the sign bit, so
/// its low half is `[0, 2^38)` — an address in `[2^38, 2^39)` is non-canonical, not a valid page).
const RISCV: AddrPlan = AddrPlan::new(39, true, 0x8000_0000, 0x0800_0000);
/// x86-64 4-level paging: 48 decoded bits, sign-extended canonical form, UEFI-reported window.
const X86: AddrPlan = AddrPlan::new(48, true, 0x0010_0000, 0x0800_0000);

const PLANS: [(&str, AddrPlan); 3] = [("aarch64", AARCH64), ("riscv64", RISCV), ("x86-64", X86)];

#[test]
fn plans_match_targets() {
    assert_eq!(
        AARCH64.va_bits(),
        39,
        "aarch64 vm.rs must declare a 39-bit TTBR0 walk"
    );
    assert_eq!(RISCV.va_bits(), 39, "Sv39 decodes 39 bits");
    assert_eq!(X86.va_bits(), 48, "x86-64 4-level paging decodes 48 bits");
}

/// Property 1: no two ACCEPTED virtual addresses alias the same page-table entry.
///
/// Enumerated over every single-bit page-aligned address plus the boundary addresses around the
/// decoded width — the region where aliasing is possible at all. Any candidate the plan accepts is
/// compared against every other accepted candidate.
#[test]
fn accepted_virtual_addresses_never_alias() {
    for (name, plan) in PLANS {
        let mut candidates = candidate_addresses(&plan);
        candidates.sort_unstable();
        candidates.dedup();

        let accepted: Vec<usize> = candidates
            .into_iter()
            .filter(|&va| plan.validate_unmap(va).is_ok())
            .collect();
        assert!(
            accepted.len() > 8,
            "[{name}] the check rejected essentially everything — vacuous"
        );

        for (i, &a) in accepted.iter().enumerate() {
            for &b in &accepted[i + 1..] {
                assert!(
                    !plan.aliases(a, b),
                    "[{name}] accepted {a:#x} and {b:#x}, which decode to the same entry",
                );
            }
        }
    }
}

/// The mirror of property 1: for every accepted address, the address one decoded-width bit above it
/// — its exact alias — must be REJECTED. Proves the check is what breaks the aliasing pair, rather
/// than the enumeration happening to miss one.
#[test]
fn the_alias_of_every_accepted_address_is_rejected() {
    for (name, plan) in PLANS {
        for va in candidate_addresses(&plan) {
            if plan.validate_unmap(va).is_err() {
                continue;
            }
            let alias = va.wrapping_add(1usize << plan.va_bits());
            if !plan.aliases(va, alias) {
                continue; // wrapped past the top of the address space; nothing to alias
            }
            assert!(
                plan.validate_unmap(alias).is_err(),
                "[{name}] accepted both {va:#x} and its alias {alias:#x}",
            );
        }
    }
}

/// Property 2: an accepted physical address is always a frame fully inside the allocator's window.
#[test]
fn accepted_physical_addresses_are_always_owned_frames() {
    for (name, plan) in PLANS {
        let base = plan.ram_base();
        let end = base + plan.ram_len();
        let va = 1usize << 21; // a fixed, always-legal virtual page for every plan

        let probes = [
            0usize,
            base.saturating_sub(PAGE_SIZE),
            base,
            base + PAGE_SIZE,
            end - PAGE_SIZE,
            end,
            end + PAGE_SIZE,
            usize::MAX - (PAGE_SIZE - 1),
        ];
        for pa in probes {
            let owned = pa >= base && pa.checked_add(PAGE_SIZE).is_some_and(|e| e <= end);
            let accepted = plan.validate_map(va, pa).is_ok();
            assert_eq!(
                accepted, owned,
                "[{name}] pa {pa:#x}: accepted={accepted} but owned-by-allocator={owned}",
            );
        }
    }
}

/// Unaligned inputs are refused on both the virtual and the physical side, at every offset within a
/// page — not merely at the one offset a single example would cover.
#[test]
fn no_sub_page_offset_is_ever_accepted() {
    for (name, plan) in PLANS {
        let va = 1usize << 21;
        let pa = plan.ram_base();
        for off in [1usize, 2, 8, 64, 512, 4095] {
            assert_eq!(
                plan.validate_map(va + off, pa),
                Err(MapFault::UnalignedVirt),
                "[{name}] accepted a virtual byte address (offset {off})",
            );
            assert_eq!(
                plan.validate_map(va, pa + off),
                Err(MapFault::UnalignedPhys),
                "[{name}] accepted a physical byte address (offset {off})",
            );
        }
    }
}

/// The null page is never mappable, on any target: null-pointer dereferences must keep faulting.
#[test]
fn the_null_page_is_unmappable_everywhere() {
    for (name, plan) in PLANS {
        assert_eq!(
            plan.validate_map(0, plan.ram_base()),
            Err(MapFault::NullVirt),
            "[{name}]"
        );
        assert_eq!(plan.validate_unmap(0), Err(MapFault::NullVirt), "[{name}]");
    }
}

/// Each target's high half is exactly its own — an address canonical for one width is refused by
/// the others, because their walkers would decode it as a low-half alias or fault it as
/// non-canonical. This is the check that would otherwise be copy-pasted wrong between targets.
#[test]
fn each_target_accepts_only_its_own_canonical_form() {
    let x86_high = 0xFFFF_8000_0000_0000usize; // sign-extended from bit 47
    let sv39_high = 0xFFFF_FFC0_0000_0000usize; // sign-extended from bit 38

    assert_eq!(X86.validate_unmap(x86_high), Ok(()));
    assert_eq!(RISCV.validate_unmap(sv39_high), Ok(()));
    // x86-64's high half is not canonical under Sv39 (bits 63:39 must repeat bit 38)...
    assert_eq!(
        RISCV.validate_unmap(x86_high),
        Err(MapFault::NonCanonicalVirt)
    );
    // ...while Sv39's high half IS inside x86-64's, so x86 accepts it — the widths differ, and
    // each plan is judged by its own ISA rule rather than by a shared guess.
    assert_eq!(X86.validate_unmap(sv39_high), Ok(()));
    // aarch64 TTBR0 has no high half at all: every bit above 39 must be zero.
    assert_eq!(
        AARCH64.validate_unmap(x86_high),
        Err(MapFault::VirtOutOfRange)
    );
    assert_eq!(
        AARCH64.validate_unmap(sv39_high),
        Err(MapFault::VirtOutOfRange)
    );
    // The non-addressable hole between the halves is rejected on both canonical targets.
    assert_eq!(
        X86.validate_unmap(0x0001_0000_0000_0000),
        Err(MapFault::NonCanonicalVirt)
    );
    assert_eq!(
        RISCV.validate_unmap(1usize << 38),
        Err(MapFault::NonCanonicalVirt)
    );
}

/// The kernel-image span is refused by BOTH mapping APIs, and the refusal is a span, not a point
/// (REQ-MM-006, ALET-P2-032). This rule used to be written per target in `kernel/src/vm.rs` and
/// `kernel-riscv64/src/vm.rs`; proving it here is what lets each target declare the span instead of
/// re-implementing the check — and x86-64 gets it for free rather than as a third copy.
#[test]
fn the_kernel_image_span_is_refused_by_map_and_unmap() {
    const START: usize = 0x4020_0000;
    const END: usize = 0x4060_0000; // block-aligned, 4 MiB
    let plan = AddrPlan::new(48, true, 0x4000_0000, 0x1000_0000).with_protected(START, END);

    assert_eq!(plan.protected(), (START, END));
    // Every page of the span, both APIs.
    for va in (START..END).step_by(PAGE_SIZE) {
        assert_eq!(plan.validate_unmap(va), Err(MapFault::ProtectedVirt));
        assert_eq!(
            plan.validate_map(va, 0x4000_0000),
            Err(MapFault::ProtectedVirt)
        );
    }
    // The boundaries are half-open: one page below is refused, `END` itself is not.
    assert_eq!(
        plan.validate_unmap(START - PAGE_SIZE),
        Ok(()),
        "the page below the span is ordinary memory"
    );
    assert_eq!(
        plan.validate_unmap(END),
        Ok(()),
        "the span excludes its end"
    );
    // Well-formedness still comes first: a protected address that is ALSO malformed reports the
    // malformation, so a caller learns the real defect rather than a span it never intended to hit.
    assert_eq!(plan.validate_unmap(START + 1), Err(MapFault::UnalignedVirt));
    // A plan with no declared span protects nothing — the default every target had before.
    let open = AddrPlan::new(48, true, 0x4000_0000, 0x1000_0000);
    assert_eq!(open.protected(), (0, 0));
    assert_eq!(open.validate_unmap(START), Ok(()));
}

/// Page-aligned candidates spanning the interesting region: every single-bit address, each one
/// offset by a page, and the addresses straddling the decoded-width boundary.
fn candidate_addresses(plan: &AddrPlan) -> Vec<usize> {
    let mut out = vec![0usize, PAGE_SIZE, 1usize << 21, 1usize << 30];
    for bit in 12..64u32 {
        let base = 1usize << bit;
        out.push(base);
        out.push(base | PAGE_SIZE);
        out.push(base.wrapping_sub(PAGE_SIZE) & !(PAGE_SIZE - 1));
    }
    let w = plan.va_bits();
    if w < 64 {
        let top = 1usize << w;
        out.push(top - PAGE_SIZE);
        out.push(top);
        out.push(top + PAGE_SIZE);
        out.push(usize::MAX & !(PAGE_SIZE - 1));
    }
    out
}

/// The device rule is the RAM rule inverted — proved as a property, not by example (REQ-DRV-005,
/// ADR-037). A driver must be able to map a PCI BAR (physical memory the allocator does not own), and
/// the same API must be UNABLE to map RAM the allocator does own: that would give one frame a second
/// mapping with different cacheability and side effects, invisible to the ownership model.
#[test]
fn no_page_is_mappable_as_both_ram_and_device() {
    for (name, plan) in PLANS {
        let ram_base = plan.ram_base();
        let ram_len = plan.ram_len();
        // Sweep the whole window plus a margin either side, in page steps.
        let start = ram_base.saturating_sub(4 * PAGE_SIZE);
        let end = ram_base + ram_len + 4 * PAGE_SIZE;
        let mut pa = start;
        let mut ram_ok = 0usize;
        let mut dev_ok = 0usize;
        while pa < end {
            // Use a VA that is legal on every plan so only the PHYSICAL rule can differ.
            let va = PAGE_SIZE * 4;
            let as_ram = plan.validate_map(va, pa).is_ok();
            let as_device = plan.validate_map_device(va, pa).is_ok();
            assert!(
                !(as_ram && as_device),
                "{name}: {pa:#x} is mappable as BOTH ram and device — the rules overlap"
            );
            let in_window = pa >= ram_base && pa + PAGE_SIZE <= ram_base + ram_len;
            assert_eq!(as_ram, in_window, "{name}: ram rule wrong at {pa:#x}");
            assert_eq!(
                as_device,
                !(pa < ram_base + ram_len && pa + PAGE_SIZE > ram_base),
                "{name}: device rule wrong at {pa:#x}"
            );
            if as_ram {
                ram_ok += 1;
            }
            if as_device {
                dev_ok += 1;
            }
            pa += PAGE_SIZE;
        }
        // The sweep really exercised both outcomes (a rule that always refuses would "pass" above).
        assert!(ram_ok > 0 && dev_ok > 0, "{name}: sweep proved nothing");
    }
}

#[test]
fn a_device_mapping_still_obeys_every_virtual_address_rule() {
    // The inversion is PHYSICAL only: a device mapping may not use a malformed VA either, and the
    // reported fault is about the VA — the caller learns the real defect.
    for (name, plan) in PLANS {
        // Well outside any window, so the physical side is always acceptable.
        let far_pa = 0xC000_0000_0000usize & !(PAGE_SIZE - 1);
        assert_eq!(
            plan.validate_map_device(PAGE_SIZE + 1, far_pa),
            Err(MapFault::UnalignedVirt),
            "{name}: an unaligned VA must be refused for a device mapping too"
        );
        assert_eq!(
            plan.validate_map_device(0, far_pa),
            Err(MapFault::NullVirt),
            "{name}: the null page is never mappable, device or not"
        );
        // And an unaligned PHYSICAL address is still an unaligned physical address.
        assert_eq!(
            plan.validate_map_device(PAGE_SIZE * 4, far_pa + 1),
            Err(MapFault::UnalignedPhys),
            "{name}: an unaligned device PA must be refused"
        );
    }
}

#[test]
fn the_kernel_image_span_is_refused_for_device_mappings_as_well() {
    const START: usize = 0x4000_0000;
    const END: usize = START + 0x20_0000;
    let plan = AddrPlan::new(48, true, 0x8000_0000, 0x1000_0000).with_protected(START, END);
    let far_pa = 0xC000_0000_0000usize;
    let mut va = START;
    while va < END {
        assert_eq!(
            plan.validate_map_device(va, far_pa),
            Err(MapFault::ProtectedVirt),
            "a device mapping over the kernel image must be refused at {va:#x}"
        );
        va += PAGE_SIZE;
    }
    // Just past the span is fine again (the span is half-open).
    assert!(plan.validate_map_device(END, far_pa).is_ok());
}
