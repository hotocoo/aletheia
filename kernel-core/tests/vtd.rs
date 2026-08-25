//! Host proofs for the VT-d wire (`src/vtd.rs`) - the IOMMU contract rehearsed against a
//! SIMULATED unit before the boot gate drives a real one (ALET-P1-018, ADR-071, ADR-073).
//!
//! The simulation enforces the register semantics that matter: a command issued without its
//! precondition sets command-field-error status, an adopted root pointer reads back, enablement
//! sticks, invalidation requests complete and are journaled. On top of it sits a DEVICE-SIDE
//! walker that consumes the very bytes the programmer wrote - root entry, context entry,
//! second-level tables - the way hardware walks them, and answers every DMA attempt with either
//! the translated physical address or the reason the unit would report. So every promise ADR-071
//! states is proved here against VT-d SHAPES: mapped memory translates exactly, the kernel image
//! faults for every device, a revoked context denies while its sibling still serves, permissions
//! bind at the leaf.

use kernel_core::vtd::{
    audit_tree, context_entry_decode, context_entry_encode, decode_fault_record, layout_of,
    program_identity_domain, rewrite_context_entry, root_entry_encode, Agaw, Controller,
    DomainStats, FaultRecord, RegLayout, Regs, TableMem, VtdFault, CCMD_ICC, FSTS_PPF, GCMD_SRTP,
    GCMD_TE, GSTS_CFR, GSTS_RTPS, GSTS_TES, IAM_GLOBAL, IOTLB_IVT, REG_CCMD, REG_FSTS, REG_GCMD,
    REG_GSTS, REG_RTADDR, REG_VER,
};

// ---------------------------------------------------------------------------------------------
// The simulated unit.
//
// Register file: u64 slots; 32-bit registers live in the documented half of their slot. Reads at
// offsets inside the fault-record bank (declared by the layout) address record (off-bank)/16.
// Everything the unit did is journaled so suites can assert requests were OBSERVED.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SimUnit {
    slots: [u64; 16], // register page 0x00..0x7F
    journal: Vec<&'static str>,
    records: Vec<(u64, u64)>,
    lay: Option<RegLayout>, // configured right after build()
}

impl SimUnit {
    fn r(&self, off: usize) -> u64 {
        // Offsets outside the low register page (the variable-position ones live higher) hold
        // nothing in this simulation - they read as zero, like unimplemented register bits.
        self.slots.get(off >> 3).copied().unwrap_or(0)
    }
    fn set_half(&mut self, off: usize, v: u32) {
        let s = &mut self.slots[off >> 3];
        if off & 4 != 0 {
            *s = (*s & 0xFFFF_FFFF) | ((v as u64) << 32);
        } else {
            *s = (*s & 0xFFFF_FFFF_0000_0000) | v as u64;
        }
    }
    fn get_half(&self, off: usize) -> u32 {
        let s = self.slots[off >> 3];
        if off & 4 != 0 {
            (s >> 32) as u32
        } else {
            s as u32
        }
    }
    fn in_frcd_bank(&self, off: usize) -> bool {
        self.lay
            .map(|l| off >= l.frcd_bank_off && off < l.frcd_bank_off + l.frcd_count * 16)
            .unwrap_or(false)
    }
}

impl Regs for SimUnit {
    fn r32(&mut self, off: usize) -> u32 {
        self.get_half(off)
    }
    fn w32(&mut self, off: usize, v: u32) {
        if off == REG_GCMD {
            // Commands are LEVEL observations of preconditions: a request whose basis is missing
            // raises command-field-error instead of taking effect. A LATER command that DOES have
            // its basis completes and clears the stale flag - the error described the earlier
            // request, not the unit permanent state.
            if v & GCMD_SRTP != 0 {
                let rt = self.r(REG_RTADDR);
                if rt == 0 || !rt.is_multiple_of(0x1000) {
                    self.set_half(REG_GSTS, self.get_half(REG_GSTS) | GSTS_CFR);
                    self.journal.push("cfr:srtp");
                    return;
                }
                self.set_half(REG_GSTS, self.get_half(REG_GSTS) & !GSTS_CFR | GSTS_RTPS);
                self.journal.push("srtp");
            }
            if v & GCMD_TE != 0 {
                if self.get_half(REG_GSTS) & GSTS_RTPS == 0 {
                    self.set_half(REG_GSTS, self.get_half(REG_GSTS) | GSTS_CFR);
                    self.journal.push("cfr:te");
                    return;
                }
                self.set_half(REG_GSTS, self.get_half(REG_GSTS) & !GSTS_CFR | GSTS_TES);
                self.journal.push("te");
            }
            return;
        }
        self.set_half(off, v);
    }
    fn r64(&mut self, off: usize) -> u64 {
        let lay = match self.lay {
            Some(l) => l,
            None => return self.r(off),
        };
        if lay.frcd_count > 0 && self.in_frcd_bank(off) {
            let rel = off - lay.frcd_bank_off;
            let rec = self.records.get(rel / 16).copied().unwrap_or((0, 0));
            return if rel % 16 == 8 { rec.1 } else { rec.0 };
        }
        // ICC and IVT are write-1-self-clearing: reads observe them already cleared.
        match off {
            REG_CCMD => self.r(off) & !CCMD_ICC,
            o if o == lay.iotlb_off => self.r(o) & !IOTLB_IVT,
            _ => self.r(off),
        }
    }
    fn w64(&mut self, off: usize, v: u64) {
        let lay = match self.lay {
            Some(l) => l,
            None => {
                self.slots[off >> 3] = v;
                return;
            }
        };
        if lay.frcd_count > 0 && self.in_frcd_bank(off) {
            return; // records are written BY THE UNIT, never by software
        }
        if off == REG_RTADDR {
            self.slots[off >> 3] = v;
            return;
        }
        if off == REG_CCMD {
            if v & CCMD_ICC != 0 {
                self.journal.push("ccmd-global");
            }
            return;
        }
        if off == lay.iva_off {
            if v & IAM_GLOBAL == IAM_GLOBAL {
                self.journal.push("iva-global");
            }
            return;
        }
        if off == lay.iotlb_off {
            if v & IOTLB_IVT != 0 {
                self.journal.push("iotlb-global");
            }
            return;
        }
        self.slots[off >> 3] = v;
    }
}

/// A healthy identification face: version 1.2 NIBBLE-encoded (bits 7:4 major, bits 3:0 minor -
/// the shape QEMU and real units report), SAGAW offering BOTH expressible depths, fault-record
/// bank declared at field 0x22 (= offset 0x220), ONE record (NFR field zero), no write-buffer
/// requirement, and ECAP.IRO naming an IOTLB register at offset 0xF0.
fn healthy_caps() -> (u32, u64, u64) {
    (0x12, 0b110 << 8 | (0x22 << 20), 0xF00)
}

fn build() -> SimUnit {
    let mut u = SimUnit::default();
    let (ver, cap, ecap) = healthy_caps();
    u.set_half(REG_VER, ver);
    u.slots[0x08 >> 3] = cap;
    u.slots[0x10 >> 3] = ecap;
    u.lay = Some(layout_of(cap, ecap));
    u
}

// ---------------------------------------------------------------------------------------------
// Table memory: an arena whose byte offsets ARE physical addresses.
// ---------------------------------------------------------------------------------------------

struct Arena {
    mem: Vec<u8>,
    bump: usize,
}

impl Arena {
    fn new(pages: usize) -> Self {
        // Page 0 is NEVER handed out - the null page is no legal table anywhere (ADR-040), and
        // the simulated unit refuses a zero root pointer outright. Same posture as frames.rs.
        Arena {
            mem: vec![0u8; (pages + 1) * 0x1000],
            bump: 0x1000,
        }
    }
    /// A fresh zeroed frame, bypassing the trait so tests can lay out root/context tables by hand.
    fn raw_page(&mut self) -> usize {
        let p = self.bump;
        self.bump += 0x1000;
        assert!(p + 0x1000 <= self.mem.len(), "arena exhausted");
        p
    }
}

impl TableMem for Arena {
    fn read_u64(&self, pa: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.mem[pa..pa + 8]);
        u64::from_le_bytes(b)
    }
    fn write_u64(&mut self, pa: usize, v: u64) {
        self.mem[pa..pa + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn alloc_zeroed_page(&mut self) -> Option<usize> {
        if self.bump + 0x1000 > self.mem.len() {
            return None;
        }
        Some(self.raw_page())
    }
}

// ---------------------------------------------------------------------------------------------
// The device-side walker: what the UNIT does with one DMA attempt.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Walk {
    Translated(usize),
    RootNotPresent,
    ContextNotPresent,
    TtUnsupported(u8),
    AwUnsupported(u8),
    NotMapped { iova: usize },
    PermDenied { iova: usize },
}

/// Walk ONLY the second-level tree - used where the suite programs a domain directly.
fn walk_slpt(arena: &Arena, slpt: usize, agaw: Agaw, iova: usize, want_write: bool) -> Walk {
    const MASK9: usize = 0x1FF;
    const PS: u64 = 1 << 7;
    const W: u64 = 1 << 1;
    let shifts = agaw.shifts();
    let mut table = slpt;
    for (i, &sh) in shifts.iter().enumerate() {
        let e = arena.read_u64(table + ((iova >> sh) & MASK9) * 8);
        if e & 1 == 0 {
            return Walk::NotMapped { iova };
        }
        if i + 2 == shifts.len() && e & PS != 0 {
            if want_write && e & W == 0 {
                return Walk::PermDenied { iova };
            }
            let base = (e as usize) & !0x1F_FFFF;
            return Walk::Translated(base | (iova & 0x1F_FFFF));
        }
        if i + 1 == shifts.len() {
            if want_write && e & W == 0 {
                return Walk::PermDenied { iova };
            }
            let base = (e as usize) & !0xFFF;
            return Walk::Translated(base | (iova & 0xFFF));
        }
        table = (e as usize) & !0xFFF;
    }
    unreachable!("shift table is never empty")
}

/// The FULL path: root entry for the bus, context entry for the function, then the tree.
fn hw_walk(
    arena: &Arena,
    root_pa: usize,
    sid: (u8, u8, u8),
    iova: usize,
    want_write: bool,
) -> Walk {
    let re = arena.read_u64(root_pa + (sid.0 as usize) * 8);
    if re & 1 == 0 {
        return Walk::RootNotPresent;
    }
    let ctx_table = (re as usize) & !0xFFF;
    // Entries are SIXTEEN bytes, indexed by devfn - same stride as the programmer's door.
    let slot = ctx_table + (((sid.1 as usize) << 3) | sid.2 as usize) * 16;
    let lo = arena.read_u64(slot);
    let hi = arena.read_u64(slot + 8);
    let (present, tt, _did, aw, slpt) = context_entry_decode(lo, hi);
    if !present {
        return Walk::ContextNotPresent;
    }
    if tt != 0 {
        return Walk::TtUnsupported(tt);
    }
    let agaw = match aw {
        0b001 => Agaw::Lev3,
        0b010 => Agaw::Lev4,
        other => return Walk::AwUnsupported(other),
    };
    walk_slpt(arena, slpt, agaw, iova, want_write)
}

// ---------------------------------------------------------------------------------------------
// Shared geometry: RAM spans pre-split around an image at 16..17 MiB.
// ---------------------------------------------------------------------------------------------

const RAM: (usize, usize) = (0x0020_0000, 0x0200_0000);
const IMAGE: (usize, usize) = (0x0100_0000, 0x0110_0000);
const DEV_A: (u8, u8, u8) = (0, 4, 0);
const DEV_B: (u8, u8, u8) = (0, 5, 0);

/// The RAM range PRE-SPLIT around the image. The builder REFUSES a range that touches the image
/// (`ImageOverlap`) rather than silently deciding memory policy - the caller declares exactly
/// which spans are translatable, and sloppiness about the image is a named error, never a
/// correction. The kernel-side suite does this same split from the UEFI map before programming.
const RAM_SPANS: [(usize, usize); 2] = [(RAM.0, IMAGE.0), (IMAGE.1, RAM.1)];

/// Program the shared domain AND the full two-level structure above it for DEV_A/DEV_B:
/// returns (root table pa, context table pa, domain top pa, stats).
fn program_with_devices(arena: &mut Arena) -> (usize, usize, usize, DomainStats) {
    let (tree, stats) =
        program_identity_domain(arena, &RAM_SPANS, IMAGE, Agaw::Lev4).expect("domain programs");
    let root = arena.raw_page();
    let ctx = arena.raw_page();
    let (rlo, rhi) = root_entry_encode(ctx).expect("root entry encodes");
    arena.write_u64(root, rlo);
    arena.write_u64(root + 8, rhi);
    for (i, dev) in [DEV_A, DEV_B].iter().enumerate() {
        let (lo, hi) =
            context_entry_encode(tree, 1 + i as u16, Agaw::Lev4).expect("context encodes");
        rewrite_context_entry(arena, ctx, dev.1, dev.2, Some((lo, hi))).expect("grant");
    }
    (root, ctx, tree, stats)
}

// ---------------------------------------------------------------------------------------------
// Proofs.
// ---------------------------------------------------------------------------------------------

#[test]
fn probe_refuses_garbage_identification_registers() {
    for bad_ver in [u32::MAX, 0] {
        let mut u = build();
        u.set_half(REG_VER, bad_ver);
        let mut c = Controller::new(u);
        assert_eq!(c.probe(), Err(VtdFault::UnsupportedVersion));
    }
    let mut u = build();
    u.slots[0x08 >> 3] = u64::MAX;
    let mut c = Controller::new(u);
    assert_eq!(c.probe(), Err(VtdFault::CapabilityGarbage));
}

#[test]
fn probe_refuses_a_unit_that_demands_write_buffer_flushing() {
    // CAP.RWBF (bit 4) set: this rung implements no flush protocol, so driving the unit would
    // mean pretending an unordered sequence is ordered. Refusal is the honest answer.
    let mut u = build();
    u.slots[0x08 >> 3] |= 1 << 4;
    let mut c = Controller::new(u);
    assert_eq!(c.probe(), Err(VtdFault::WriteBufferFlushRequired));
}

#[test]
fn probe_picks_the_deepest_expressible_agaw_and_derives_the_layout() {
    for (sagaw, want) in [
        (0b110u64, Some(Agaw::Lev4)),
        (0b010, Some(Agaw::Lev3)),
        (0b111, Some(Agaw::Lev4)),
        (0b001, None),
    ] {
        let mut u = build();
        u.slots[0x08 >> 3] = (sagaw << 8) | (0x22 << 20);
        let mut c = Controller::new(u);
        match want {
            Some(a) => assert_eq!(c.probe().expect("probes").agaw, Some(a), "sagaw {sagaw:#b}"),
            None => assert_eq!(
                c.probe(),
                Err(VtdFault::UnsupportedAgaw),
                "sagaw {sagaw:#b}"
            ),
        }
    }

    // The variable-register layout comes from the CAPABILITY registers, not hardcoded offsets:
    // healthy face declares IVA@0xF0/IOTLB@0xF8 via ECAP.IRO and the FRCD bank @0x220 via CAP.FRO.
    let u = build();
    let mut c = Controller::new(u);
    let rep = c.probe().expect("probes");
    assert_eq!(rep.layout.iva_off, 0xF0);
    assert_eq!(rep.layout.iotlb_off, 0xF8);
    assert_eq!(rep.layout.frcd_bank_off, 0x220);
    assert_eq!(rep.layout.frcd_count, 1);
}

#[test]
fn set_root_refuses_misaligned_pointers_without_touching_the_unit() {
    let u = build();
    let mut c = Controller::new(u);
    assert_eq!(c.set_root(0x1234_5001), Err(VtdFault::MisalignedPointer));
    let u = c.into_inner();
    assert_eq!(u.r(REG_RTADDR), 0);
}

#[test]
fn enablement_requires_an_adopted_root_and_sticks_once_on() {
    let mut arena = Arena::new(8);
    let (tree, _) =
        program_identity_domain(&mut arena, &RAM_SPANS, IMAGE, Agaw::Lev4).expect("programs");

    let u = build();
    let mut c = Controller::new(u);
    // TE without SRTP: the unit answers with command-field-error, named here.
    assert_eq!(c.enable_translation(), Err(VtdFault::CommandFieldError));
    assert!(!c.translation_enabled());

    assert_eq!(c.set_root(tree), Ok(()));
    assert_eq!(c.enable_translation(), Ok(()));
    assert!(c.translation_enabled());
    // Re-entry is refused, not silently idempotent: the sequence stays total-order.
    assert_eq!(
        c.enable_translation(),
        Err(VtdFault::TranslationAlreadyEnabled)
    );

    let u = c.into_inner();
    assert_eq!(u.r(REG_RTADDR) as usize, tree);
    assert!(u.get_half(REG_GSTS) & GSTS_TES != 0);
}

#[test]
fn invalidations_complete_at_the_declared_offsets_and_in_order() {
    let u = build();
    let lay = layout_of(u.slots[0x08 >> 3], u.slots[0x10 >> 3]);
    let mut c = Controller::new(u);
    assert_eq!(c.invalidate_context_cache_global(), Ok(()));
    assert_eq!(c.invalidate_iotlb_global(&lay), Ok(()));
    let u = c.into_inner();
    assert_eq!(
        &u.journal[..],
        &["ccmd-global", "iva-global", "iotlb-global"][..],
        "both invalidation requests reached the unit AT ITS DECLARED OFFSETS, in driver order"
    );
}

#[test]
fn domain_plan_refuses_image_overlap_and_malformed_ranges_by_name() {
    let mut arena = Arena::new(4);
    assert_eq!(
        program_identity_domain(&mut arena, &[(0x0020_0000, 0x0180_0000)], IMAGE, Agaw::Lev4),
        Err(VtdFault::ImageOverlap)
    );
    assert_eq!(
        program_identity_domain(&mut arena, &[(0x123, 0x2000)], IMAGE, Agaw::Lev4),
        Err(VtdFault::MalformedRange)
    );
    assert_eq!(
        program_identity_domain(
            &mut arena,
            &[(RAM_SPANS[0]), RAM_SPANS[1]],
            (0x10, 0x10),
            Agaw::Lev4
        ),
        Err(VtdFault::MalformedRange)
    );
}

#[test]
fn identity_domain_translates_mapped_memory_exactly() {
    let mut arena = Arena::new(16);
    let (tree, stats) =
        program_identity_domain(&mut arena, &RAM_SPANS, IMAGE, Agaw::Lev4).expect("programs");

    for &va in &[0x0020_1234usize, 0x00FF_F800, 0x0110_0ABC, 0x01FF_F000] {
        match walk_slpt(&arena, tree, Agaw::Lev4, va, true) {
            Walk::Translated(pa) => assert_eq!(pa, va, "identity holds at {va:#x}"),
            other => panic!("walk at {va:#x} returned {other:?}"),
        }
    }

    for &va in &[IMAGE.0 + 0x123, IMAGE.1 - 8, 0x1000, 0x4000_0000] {
        assert_eq!(
            walk_slpt(&arena, tree, Agaw::Lev4, va, true),
            Walk::NotMapped { iova: va },
            "unmapped at {va:#x}"
        );
    }

    let audit = audit_tree(&mut arena, tree, Agaw::Lev4, IMAGE);
    assert_eq!(audit.image_violations, 0);
    assert_eq!(audit.huge_leaves, stats.huge_leaves);
    assert_eq!(audit.page_leaves, stats.page_leaves);
    assert!(
        stats.huge_leaves >= 12,
        "2 MiB leaves carry the bulk: {stats:?}"
    );
    assert!(stats.tables <= 8, "a toy geometry stays tiny: {stats:?}");
}

#[test]
fn the_kernel_image_faults_for_every_device_attached() {
    let mut arena = Arena::new(16);
    let (root, _, _, _) = program_with_devices(&mut arena);

    for sid in [DEV_A, DEV_B] {
        for &va in &[IMAGE.0, IMAGE.0 + 0x40, IMAGE.1 - 4] {
            assert_eq!(
                hw_walk(&arena, root, sid, va, true),
                Walk::NotMapped { iova: va },
                "device {sid:?} must not touch image byte {va:#x}"
            );
        }
        assert!(matches!(
            hw_walk(&arena, root, sid, RAM.0, true),
            Walk::Translated(_)
        ));
    }
}

#[test]
fn revoked_context_denies_while_sibling_serves() {
    let mut arena = Arena::new(16);
    let (root, ctx, _, _) = program_with_devices(&mut arena);
    assert!(matches!(
        hw_walk(&arena, root, DEV_A, RAM.0, true),
        Walk::Translated(_)
    ));
    assert!(matches!(
        hw_walk(&arena, root, DEV_B, RAM.0, true),
        Walk::Translated(_)
    ));

    rewrite_context_entry(&mut arena, ctx, DEV_A.1, DEV_A.2, None).expect("revoke");
    assert_eq!(
        hw_walk(&arena, root, DEV_A, RAM.0, true),
        Walk::ContextNotPresent,
        "the revoked function must find its context ABSENT"
    );
    assert!(matches!(
        hw_walk(&arena, root, DEV_B, RAM.0, true),
        Walk::Translated(_)
    ));
}

#[test]
fn the_write_permission_bit_binds_at_the_leaf() {
    let mut arena = Arena::new(16);
    let (tree, _) =
        program_identity_domain(&mut arena, &RAM_SPANS, IMAGE, Agaw::Lev4).expect("programs");

    // Hand-clear W on the leaf covering one page: the next write faults, reads continue.
    let mut table = tree;
    for sh in [39u32, 30, 21] {
        let e = arena.read_u64(table + ((0x0110_1000usize >> sh) & 0x1FF) * 8);
        table = (e as usize) & !0xFFF;
    }
    let slot = table + ((0x0110_1000usize >> 12) & 0x1FF) * 8;
    let e = arena.read_u64(slot);
    arena.write_u64(slot, e & !(1 << 1));

    assert_eq!(
        walk_slpt(&arena, tree, Agaw::Lev4, 0x0110_1000, true),
        Walk::PermDenied { iova: 0x0110_1000 },
        "a read-only leaf denies stores"
    );
    assert!(matches!(
        walk_slpt(&arena, tree, Agaw::Lev4, 0x0110_1000, false),
        Walk::Translated(_)
    ));
}

#[test]
fn fault_evidence_flows_from_the_unit_and_decodes_to_named_fields() {
    let mut u = build();
    let lay = layout_of(u.slots[0x08 >> 3], u.slots[0x10 >> 3]);
    // The unit recorded a fault: pending bit, oldest-record index 0, and the record itself
    // carrying source-id 04:00, reason CONTEXT_ENTRY_P, a fault address, and the READ marker.
    u.set_half(REG_FSTS, FSTS_PPF);
    let hi = (1u64 << 63)          // F: record active
        | (1u64 << 62)             // T: the blocked access was a READ
        | ((kernel_core::vtd::fr::CONTEXT_ENTRY_P as u64) << 32)
        | 0x0400; // SID bus 04 dev/fn 00
    u.records.push((0x0000_DEAD_0000, hi));

    let mut c = Controller::new(u);
    assert!(
        c.fsts() & FSTS_PPF != 0,
        "pending-fault evidence is visible"
    );
    let fri = (c.fsts() >> 8) & 0xFF;
    let (lo, hi) = c.fault_record(&lay, fri as usize);
    let rec: FaultRecord = decode_fault_record(lo, hi);
    assert!(rec.present);
    assert_eq!(
        rec.source_id, 0x0400,
        "the record names the device that attempted the access"
    );
    assert_eq!(rec.reason, kernel_core::vtd::fr::CONTEXT_ENTRY_P);
    assert_eq!(rec.address, 0x0000_DEAD_0000);
    assert!(rec.was_read);
}
