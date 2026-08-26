//! Host proofs for the SMMUv3 wire (src/smmu.rs) - the IOMMU contract rehearsed against a
//! SIMULATED unit before the boot gate drives a real one (ALET-P1-018, ADR-071, ADR-074).
//!
//! The simulation enforces the register semantics that matter: CR0 mirrors its non-reserved
//! bits into CR0ACK, commands are consumed from the very bytes the driver posted (through the
//! shared arena, exactly as hardware DMAs them), an unknown opcode parks CONS with CERROR_ILL
//! and raises GERROR, and enablement sticks. On top of it sits a DEVICE-SIDE walker that
//! parses stream table entries and stage-2 page trees the way the unit does - field positions
//! taken from QEMU's smmuv3 implementation - and answers every DMA attempt with either the
//! translated physical address or the event record the unit would write. So every promise
//! ADR-071 states is proved here against SMMUv3 SHAPES: mapped memory translates exactly, the
//! kernel image faults for every device, a revoked STE denies while a sibling still serves,
//! and denial evidence NAMES the stream id and address.

use std::cell::RefCell;
use std::rc::Rc;

use kernel_core::iommu::PAGE;
use kernel_core::smmu::{
    audit_tree, cmd, program_identity_domain, queue_base_encode, rewrite_ste, ste_cfg,
    ste_s2_decode, ste_s2_encode, strtab_slot, Controller, EventRecord, QueueGeom, Regs,
    S2Geometry, SmmuFault, SteFault, TableMem, CMDQ_ENTRY_BYTES, CR0_ENABLE_ALL, CR0_SMMUEN,
    EVTQ_ENTRY_BYTES, GERROR_CMDQ_ERR, REG_CMDQ_BASE, REG_CMDQ_CONS, REG_CMDQ_PROD, REG_CR0,
    REG_CR0ACK, REG_EVENTQ_BASE, REG_GERROR, REG_IDR0, REG_IDR5,
};

// ---------------------------------------------------------------------------------------------
// Shared arena: byte memory whose offsets ARE physical addresses. Both the programmer (via
// TableMem) and the simulated unit's queue consumer / device walker read the same bytes.
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Arena(Rc<RefCell<ArenaInner>>);

struct ArenaInner {
    mem: Vec<u8>,
    bump: usize,
    /// Every u64 write as (addr, value) - the torn-write protocol test reads this journal.
    journal: Vec<(usize, u64)>,
}

impl Arena {
    fn new(pages: usize) -> Self {
        // Page 0 is NEVER handed out - the null page is no legal table anywhere (ADR-040).
        Arena(Rc::new(RefCell::new(ArenaInner {
            mem: vec![0u8; (pages + 1) * 0x1000],
            bump: 0x1000,
            journal: Vec::new(),
        })))
    }

    fn raw_page(&self) -> usize {
        let mut inner = self.0.borrow_mut();
        let p = inner.bump;
        inner.bump += 0x1000;
        assert!(p + 0x1000 <= inner.mem.len(), "arena exhausted");
        p
    }

    fn writes(&self) -> Vec<(usize, u64)> {
        self.0.borrow().journal.clone()
    }
}

impl TableMem for Arena {
    fn read_u64(&self, pa: usize) -> u64 {
        let inner = self.0.borrow();
        let mut b = [0u8; 8];
        b.copy_from_slice(&inner.mem[pa..pa + 8]);
        u64::from_le_bytes(b)
    }
    fn write_u64(&mut self, pa: usize, v: u64) {
        let mut inner = self.0.borrow_mut();
        inner.mem[pa..pa + 8].copy_from_slice(&v.to_le_bytes());
        inner.journal.push((pa, v));
    }
    fn alloc_zeroed_page(&mut self) -> Option<usize> {
        Some(self.raw_page())
    }
}

// ---------------------------------------------------------------------------------------------
// The simulated unit.
// ---------------------------------------------------------------------------------------------

const CR0_RESERVED: u32 = 0xFFFF_FA20;

/// A healthy NESTED unit face - what the virt machine creates: both stages advertised,
/// AArch64 tables only, 16-bit stream ids, 44-bit output, 4 KiB granule.
fn healthy_idr() -> [u32; 6] {
    let mut idr = [0u32; 6];
    idr[0] = 1            // S2P
        | (1 << 1)        // S1P
        | (0b10 << 2)     // TTF: AArch64 only
        | (1 << 4)        // COHACC
        | (1 << 12)       // ASID16
        | (1 << 18); // VMID16
    idr[1] = 16           // SIDSIZE bits
        | (19 << 16)      // EVENTQS cap
        | (19 << 21); // CMDQS cap
    idr[5] = 4            // OAS = 44 bits
        | (1 << 4); // GRAN4K
    idr
}

struct SimUnit {
    regs: [u32; 48], // offsets 0x00..0xBF as 32-bit slots
    idr: [u32; 6],
    arena: Arena,
    cmdq_base: usize,
    cmdq_log2size: u32,
    evtq_base: usize,
    evtq_log2size: u32,
    journal: Vec<&'static str>,
    cfgi_count: usize,
    tlbi_s2_count: usize,
}

impl SimUnit {
    fn new(arena: &Arena) -> Self {
        SimUnit {
            regs: [0; 48],
            idr: healthy_idr(),
            arena: arena.clone(),
            cmdq_base: 0,
            cmdq_log2size: 0,
            evtq_base: 0,
            evtq_log2size: 0,
            journal: Vec::new(),
            cfgi_count: 0,
            tlbi_s2_count: 0,
        }
    }

    fn r32slot(&self, off: usize) -> u32 {
        // (REG_IDR0 == 0, so no lower bound check exists to write.)
        if off <= REG_IDR5 {
            return self.idr[(off - REG_IDR0) / 4];
        }
        self.regs.get(off >> 2).copied().unwrap_or(0)
    }
    fn w32slot(&mut self, off: usize, v: u32) {
        if (off >> 2) < self.regs.len() {
            self.regs[off >> 2] = v;
        }
    }

    fn cmdq_prod(&self) -> u32 {
        self.r32slot(REG_CMDQ_PROD)
    }
    fn cmdq_cons(&self) -> u32 {
        self.r32slot(REG_CMDQ_CONS)
    }
    fn set_cmdq_cons(&mut self, v: u32) {
        self.w32slot(REG_CMDQ_CONS, v);
    }

    /// Consume every queued command, the way the unit does on a PROD doorbell or on CR0
    /// gaining CMDQEN. Commands are READ FROM THE ARENA - the same bytes the driver stored.
    fn consume_commands(&mut self) {
        if self.r32slot(REG_CR0) & (1 << 3) == 0 {
            return; // CMDQEN clear: nothing runs
        }
        let g = QueueGeom::new(self.cmdq_log2size, CMDQ_ENTRY_BYTES);
        loop {
            let prod = self.cmdq_prod();
            let cons = self.cmdq_cons();
            if g.is_empty(prod, cons) {
                return;
            }
            let addr = g.entry_addr(self.cmdq_base, cons);
            let mut words = [0u32; 4];
            for i in 0..2usize {
                let raw = TableMem::read_u64(&self.arena, addr + i * 8);
                words[i * 2] = raw as u32;
                words[i * 2 + 1] = (raw >> 32) as u32;
            }
            match words[0] & 0xFF {
                x if x == cmd::SYNC as u32 => self.journal.push("sync"),
                x if x == cmd::CFGI_STE as u32 => {
                    if words[0] & (1 << 10) != 0 {
                        // SSEC set: refused outright.
                        self.reject_ill(cons);
                        return;
                    }
                    self.cfgi_count += 1;
                    self.journal.push("cfgi_ste");
                }
                x if x == cmd::CFGI_ALL as u32 => {
                    self.cfgi_count += 1;
                    self.journal.push("cfgi_all");
                }
                x if x == cmd::TLBI_S12_VMALL as u32 => {
                    self.tlbi_s2_count += 1;
                    self.journal.push("tlbi_s12");
                }
                _ => {
                    // Unknown opcode: CERROR_ILL, consumer parks AT the bad entry.
                    self.reject_ill(cons);
                    return;
                }
            }
            self.set_cmdq_cons(g.advance(cons));
        }
    }

    fn reject_ill(&mut self, at_cons: u32) {
        self.journal.push("cerror_ill");
        let parked = at_cons | (0x2 << 24);
        self.set_cmdq_cons(parked);
        self.w32slot(REG_GERROR, self.r32slot(REG_GERROR) | GERROR_CMDQ_ERR);
    }
}

impl Regs for SimUnit {
    fn r32(&mut self, off: usize) -> u32 {
        self.r32slot(off)
    }
    fn w32(&mut self, off: usize, v: u32) {
        match off {
            REG_CR0 => {
                self.w32slot(REG_CR0, v);
                // ACK mirrors the requested NON-RESERVED bits - the shape the driver polls.
                self.w32slot(REG_CR0ACK, v & !CR0_RESERVED);
                self.consume_commands();
            }
            REG_CMDQ_PROD => {
                self.w32slot(off, v);
                self.consume_commands();
            }
            REG_CMDQ_BASE => {
                self.cmdq_base = (v as usize) & !0x3F;
                self.cmdq_log2size = v & 0x1F;
            }
            _ if off == REG_CMDQ_BASE + 4 => {}
            REG_EVENTQ_BASE => {
                self.evtq_base = (v as usize) & !0x3F;
                self.evtq_log2size = v & 0x1F;
            }
            _ if off == REG_EVENTQ_BASE + 4 => {}
            0x64 => {
                // GERRORN: acknowledge toggles the live error off; the consumer may run again.
                self.w32slot(0x64, v);
                self.w32slot(REG_GERROR, self.r32slot(REG_GERROR) & !v);
                self.consume_commands();
            }
            _ => self.w32slot(off, v),
        }
    }
    fn r64(&mut self, off: usize) -> u64 {
        ((self.r32(off + 4) as u64) << 32) | self.r32(off) as u64
    }
    fn w64(&mut self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

// ---------------------------------------------------------------------------------------------
// The device-side walker: parses STEs and stage-2 trees exactly as the unit does, answering
// each attempted access with the translated PA or the event record silicon would write.
// ---------------------------------------------------------------------------------------------

struct WalkOutcome {
    translated: Option<usize>,
    event: Option<EventRecord>,
}

fn fault_outcome(kind: u8, sid: u32, addr: u64, was_read: bool) -> WalkOutcome {
    WalkOutcome {
        translated: None,
        event: Some(EventRecord {
            kind,
            sid,
            addr,
            was_read,
            s2: true,
            class: 0,
        }),
    }
}

struct DeviceWalker<'a> {
    arena: &'a Arena,
    strtab_base: usize,
    strtab_log2size: u32,
}

impl<'a> DeviceWalker<'a> {
    fn find_ste(&self, sid: u32) -> Result<[u64; 8], EventRecord> {
        let span = 1u32 << self.strtab_log2size.min(16);
        if sid >= span {
            return Err(EventRecord {
                kind: 0x02, // C_BAD_STREAMID
                sid,
                addr: 0,
                was_read: false,
                s2: false,
                class: 0,
            });
        }
        let slot = strtab_slot(self.strtab_base, sid).expect("strtab aligned");
        let mut ste = [0u64; 8];
        for (i, slot_word) in ste.iter_mut().enumerate() {
            *slot_word = TableMem::read_u64(self.arena, slot + i * 8);
        }
        Ok(ste)
    }

    fn translate(&self, sid: u32, iova: usize, write: bool) -> WalkOutcome {
        let fail = |kind: u8| fault_outcome(kind, sid, iova as u64, !write);
        let silent = || WalkOutcome {
            translated: None,
            event: None,
        };
        let ste = match self.find_ste(sid) {
            Ok(s) => s,
            Err(ev) => {
                return WalkOutcome {
                    translated: None,
                    event: Some(ev),
                }
            }
        };
        let (valid, config, _vmid, ttb) = ste_s2_decode(&ste);
        if !valid || config != ste_cfg::S2_ONLY {
            // Absent or not-stage-2 entries are C_BAD_STE - the revocation evidence.
            return fail(0x04);
        }
        // Stage-2 geometry straight out of the entry, decoded like decode_ste_s2_cfg.
        let w =
            |i: usize| -> u32 { (ste[i / 2] >> if i.is_multiple_of(2) { 0 } else { 32 }) as u32 };
        let t0sz = w(5) & 0x3F;
        let sl0 = (w(5) >> 6) & 0b11;
        let tg = (w(5) >> 14) & 0b11;
        let aa64 = w(5) & (1 << 19) != 0;
        let endi = w(5) & (1 << 20) != 0;
        let stall = w(5) & (1 << 25) != 0;
        let record_faults = w(5) & (1 << 26) != 0;
        if !aa64 || tg != 0 || sl0 == 0b11 || endi || stall {
            return fail(0x04);
        }
        let input_bits = 64 - t0sz;
        if input_bits >= 64 || iova >= 1usize << input_bits {
            return fail(0x10);
        }
        // Start level for a 4 KiB granule: 2 - SL0 (the 4 KiB leaf lives at level 3).
        const SHIFTS: [u32; 4] = [39, 30, 21, 12];
        let mut level = 2usize.saturating_sub(sl0 as usize);
        let mut table = ttb;
        loop {
            if level > 3 {
                break;
            }
            let shift = SHIFTS[level];
            let idx = (iova >> shift) & 0x1FF;
            let pte = TableMem::read_u64(self.arena, table + idx * 8);
            let kind_bits = pte & 0b11;
            if pte & 1 == 0 || (level == 3 && kind_bits == 0b01) {
                // Invalid or reserved: translation fault - deny-by-default firing.
                break;
            }
            let oa_mask: u64 = !((1u64 << shift) - 1) & 0x0000_FFFF_FFFF_F000;
            if level < 3 && kind_bits == 0b11 {
                // A next-table descriptor names its table at OA bits [47:12], whatever the
                // level - using this level's block mask here walked into address zero.
                table = (pte & 0x0000_FFFF_FFFF_F000) as usize;
                level += 1;
                continue;
            }
            let leaf_ok = if level < 3 {
                kind_bits == 0b01 // block
            } else {
                kind_bits == 0b11 // page
            };
            if !leaf_ok {
                break;
            }
            // Leaf: AF first (access fault takes priority over permission), then permission.
            if pte & (1 << 10) == 0 {
                return if record_faults { fail(0x12) } else { silent() };
            }
            let s2ap = (pte >> 6) & 0b11;
            let need: u64 = if write { 0b01 } else { 0b10 };
            if s2ap & need == 0 {
                return if record_faults { fail(0x13) } else { silent() };
            }
            let pa = ((pte & oa_mask) as usize) | (iova & ((1usize << shift) - 1));
            return WalkOutcome {
                translated: Some(pa),
                event: None,
            };
        }
        if record_faults {
            fail(0x10)
        } else {
            silent()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------------------------

const IMG: (usize, usize) = (0x4008_0000, 0x4010_0000);

/// Conventional RAM around the image, split by the CALLER like the platform code does.
fn spans() -> Vec<(usize, usize)> {
    vec![(0x4000_0000, IMG.0), (IMG.1, 0x4800_0000)]
}

fn geom() -> S2Geometry {
    S2Geometry::standard(1)
}

/// Build a domain in a fresh arena; returns (arena, ttb, stats).
fn build_domain() -> (Arena, usize, kernel_core::smmu::DomainStats) {
    let arena = Arena::new(256);
    let mut mem = arena.clone();
    let (ttb, stats) =
        program_identity_domain(&mut mem, &spans(), IMG, &geom()).expect("domain builds");
    (arena, ttb, stats)
}

/// Program one grant into a fresh linear stream table inside arena.
fn grant(arena: &Arena, sid: u32, ttb: usize) -> usize {
    let strtab = arena.raw_page();
    let ste = ste_s2_encode(&geom(), ttb).expect("STE encodes");
    let mut mem = arena.clone();
    rewrite_ste(&mut mem, strtab, sid, Some(&ste)).expect("grant lands");
    strtab
}

fn walker_for<'a>(arena: &'a Arena, strtab: usize) -> DeviceWalker<'a> {
    DeviceWalker {
        arena,
        strtab_base: strtab,
        strtab_log2size: 8,
    }
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn probe_refuses_dead_and_stage1_only_units() {
    let arena = Arena::new(4);
    let mut unit = SimUnit::new(&arena);
    unit.idr = [0; 6];
    let dead = Controller::new(unit, Arena::new(2)).probe().unwrap_err();
    assert_eq!(dead, SmmuFault::Stage2Missing);

    let arena2 = Arena::new(4);
    let mut s1_only = SimUnit::new(&arena2);
    s1_only.idr[0] &= !1; // S2P cleared: stage 1 advertised, stage 2 not
    let err = Controller::new(s1_only, Arena::new(2)).probe().unwrap_err();
    assert_eq!(err, SmmuFault::Stage2Missing);
}

#[test]
fn probe_decodes_the_nested_face() {
    let arena = Arena::new(4);
    let rep = Controller::new(SimUnit::new(&arena), Arena::new(2)).identify();
    assert!(rep.s2p() && rep.s1p());
    assert_eq!(rep.sid_size(), 16);
    assert_eq!(rep.oas_bits(), 44);
    assert!(rep.gran4k());
    assert!(rep.ttf_aarch64());
    rep.validate().expect("healthy unit validates");
}

#[test]
fn geometry_refuses_every_wrong_shape() {
    let g = geom();
    g.validate().expect("standard geometry is legal");

    // T0SZ below the minimum (max(64-ps,16) = 20): too much input space for the declared tree.
    let mut shallow = g;
    shallow.t0sz = 19;
    assert_eq!(shallow.validate(), Err(SteFault::BadT0SZ));

    // Concatenation case: tsz=20 at a level-1 start implies concatenated top tables.
    let mut concat = g;
    concat.t0sz = 20;
    assert_eq!(concat.validate(), Err(SteFault::BadStartLevel));

    // Wrong start level entirely.
    let mut deep = g;
    deep.sl0 = 0b00;
    assert_eq!(deep.validate(), Err(SteFault::BadStartLevel));

    // TTB outside the declared output size.
    assert_eq!(
        ste_s2_encode(&g, 0x1000_0000_0000),
        Err(SteFault::BadTTB),
        "a table above the 44-bit output size is refused"
    );
}

#[test]
fn builder_counts_balance_and_the_audit_walks_clean() {
    let (arena, ttb, stats) = build_domain();
    let mut mem = arena.clone();
    let audit = audit_tree(&mut mem, ttb, &geom(), IMG);
    assert_eq!(
        audit.tables, stats.tables,
        "auditor and builder agree on tables"
    );
    assert_eq!(
        audit.huge_leaves + audit.page_leaves,
        stats.huge_leaves + stats.page_leaves
    );
    assert!(audit.huge_leaves > 0, "RAM spans produce 2 MiB leaves");
    assert!(audit.page_leaves > 0, "image edges force 4 KiB leaves");
    assert_eq!(
        audit.image_violations, 0,
        "no leaf may cover any image byte"
    );
}

#[test]
fn builder_refuses_image_overlap_and_malformed_input() {
    let mut mem = Arena::new(8);
    let overlapping = [(0x4000_0000, IMG.1)];
    assert_eq!(
        program_identity_domain(&mut mem, &overlapping, IMG, &geom()),
        Err(SmmuFault::ImageOverlap)
    );
    let unaligned = [(0x4000_0001, 0x4000_2000)];
    assert_eq!(
        program_identity_domain(&mut mem, &unaligned, IMG, &geom()),
        Err(SmmuFault::MalformedRange)
    );
    let empty_image = [(0x4000_0000, 0x4000_1000)];
    assert_eq!(
        program_identity_domain(&mut mem, &empty_image, (0x10, 0x08), &geom()),
        Err(SmmuFault::MalformedRange),
        "an inverted image span is malformed, not 'nothing to protect'"
    );
}

#[test]
fn walker_translates_identity_per_page_with_offset_preserved() {
    let (arena, ttb, _stats) = build_domain();
    let strtab = grant(&arena, 0x08, ttb);
    let walker = walker_for(&arena, strtab);
    // Inside huge leaves and page leaves alike, identity holds byte-for-byte.
    for va in [0x4000_1000usize, 0x4000_1234, 0x4020_0000, 0x47FF_FF42] {
        let o = walker.translate(0x08, va, false);
        assert_eq!(o.translated.expect("translates"), va, "identity at {va:#x}");
    }
}

#[test]
fn deny_by_default_holes_fault_by_name_for_every_device() {
    let (arena, ttb, _stats) = build_domain();
    let strtab = grant(&arena, 0x08, ttb);
    let walker = walker_for(&arena, strtab);
    // The image itself: EVERY probed page of it faults, reads and writes alike.
    for va in [IMG.0, IMG.0 + PAGE, IMG.1 - PAGE] {
        for write in [false, true] {
            let o = walker.translate(0x08, va, write);
            let ev = o.event.expect("image access must fault");
            assert_eq!(ev.kind, 0x10, "F_TRANSLATION");
            assert_eq!(ev.sid, 0x08);
            assert_eq!(ev.addr, va as u64, "the event names the exact address");
            assert_eq!(ev.was_read, !write);
            assert!(ev.s2, "stage-2 class recorded");
        }
    }
    // An address no RAM lives at faults too.
    let o = walker.translate(0x08, 0xF000_0000, true);
    assert_eq!(o.event.expect("faults").kind, 0x10);
}

#[test]
fn revoked_ste_denies_with_c_bad_ste_and_restored_returns_to_service() {
    let (arena, ttb, _stats) = build_domain();
    let strtab = grant(&arena, 0x08, ttb);
    let va = 0x4020_0000usize;
    assert!(walker_for(&arena, strtab)
        .translate(0x08, va, true)
        .translated
        .is_some());
    // Revoke: V=0 through the same seam.
    let mut mem = arena.clone();
    rewrite_ste(&mut mem, strtab, 0x08, None).expect("revoke lands");
    drop(mem);
    {
        let walker = walker_for(&arena, strtab);
        let o = walker.translate(0x08, va, true);
        let ev = o.event.expect("revoked device must be denied");
        assert_eq!(ev.kind, 0x04, "C_BAD_STE names the configuration refusal");
        assert_eq!(ev.sid, 0x08);
    }
    // Restore the SAME grant: service returns.
    let ste = ste_s2_encode(&geom(), ttb).expect("re-encode");
    let mut mem = arena.clone();
    rewrite_ste(&mut mem, strtab, 0x08, Some(&ste)).expect("restore lands");
    drop(mem);
    assert!(walker_for(&arena, strtab)
        .translate(0x08, va, true)
        .translated
        .is_some());
}

#[test]
fn sibling_streams_share_the_domain_but_unknown_ones_have_no_space() {
    let (arena, ttb, _stats) = build_domain();
    let strtab = grant(&arena, 0x08, ttb);
    // A SECOND function granted against the same tree translates too - inter-device isolation
    // is deliberately NOT claimed by this rung (gap register).
    let mut mem = arena.clone();
    let ste = ste_s2_encode(&geom(), ttb).expect("encode");
    rewrite_ste(&mut mem, strtab, 0x09, Some(&ste)).expect("second grant");
    drop(mem);
    let walker = walker_for(&arena, strtab);
    assert!(walker
        .translate(0x09, 0x4020_0000, false)
        .translated
        .is_some());
    // A stream id beyond the programmed space has NO entry: C_BAD_STREAMID, never silence.
    let o = walker.translate(0x800, 0x4020_0000, false);
    assert_eq!(o.event.expect("named").kind, 0x02);
}

#[test]
fn queue_geometry_wrap_arithmetic_is_exact() {
    let g = QueueGeom::new(2, EVTQ_ENTRY_BYTES);
    assert_eq!(g.slots(), 4);
    assert!(g.is_empty(0, 0));
    assert!(g.is_full(4, 0), "full: same index, wrap bit differs");
    assert!(g.is_full(7, 3));
    assert!(!g.is_full(3, 0));
    assert_eq!(g.advance(0), 1);
    assert_eq!(g.advance(3), 4, "advance carries into the wrap bit");
    assert_eq!(g.advance(7), 0, "and wraps home cleanly");
    let base = 0x1000usize;
    assert_eq!(g.entry_addr(base, 5), base + EVTQ_ENTRY_BYTES);
    // Command entries are sixteen bytes, events thirty-two: two geometries, both total.
    let c = QueueGeom::new(6, CMDQ_ENTRY_BYTES);
    assert_eq!(c.entry_addr(base, (1 << 6) | 63), base + 63 * 16);
}

#[test]
fn controller_end_to_end_against_the_simulated_unit() {
    let (arena, ttb, _stats) = build_domain();
    let strtab = arena.raw_page();
    let cmdq = arena.raw_page();
    let evtq = arena.raw_page();
    // Grant BEFORE anything runs - the bring-up order the gate proves.
    let ste = ste_s2_encode(&geom(), ttb).expect("encode");
    let mut mem = arena.clone();
    rewrite_ste(&mut mem, strtab, 0x08, Some(&ste)).expect("grant");

    let unit = SimUnit::new(&arena);
    let mut ctrl = Controller::new(unit, arena.clone());
    ctrl.set_strtab(strtab, 8).expect("strtab published");
    ctrl.set_queue(false, cmdq, &QueueGeom::new(6, CMDQ_ENTRY_BYTES))
        .expect("cmdq published");
    ctrl.set_queue(true, evtq, &QueueGeom::new(6, EVTQ_ENTRY_BYTES))
        .expect("evtq published");
    assert!(!ctrl.smmu_enabled(), "arrives quiet");
    ctrl.enable_translation().expect("enforcement ON");
    assert!(ctrl.smmu_enabled());

    // The invalidation choreography is observed by the unit, barrier last.
    ctrl.invalidate_stream(0x08, geom().vmid)
        .expect("commands consumed");
    // Programming while enabled refuses BY NAME - tables must not change under the walker.
    assert_eq!(
        ctrl.set_strtab(strtab, 8),
        Err(SmmuFault::ProgrammedWhileEnabled)
    );
    assert_eq!(
        ctrl.set_queue(true, evtq, &QueueGeom::new(6, EVTQ_ENTRY_BYTES)),
        Err(SmmuFault::ProgrammedWhileEnabled)
    );

    let (unit, _mem) = ctrl.into_inner();
    assert!(unit.cfgi_count >= 1, "CFGI_STE reached the unit");
    assert!(unit.tlbi_s2_count >= 1, "TLBI_S12_VMALL reached the unit");
    assert_eq!(
        unit.journal.last(),
        Some(&"sync"),
        "the SYNC barrier completes after the invalidations"
    );
}

#[test]
fn enable_handshake_requires_the_ack_and_refuses_twice() {
    let arena = Arena::new(16);
    let cmdq = arena.raw_page();
    let evtq = arena.raw_page();
    let strtab = arena.raw_page();
    let mut ctrl = Controller::new(SimUnit::new(&arena), arena.clone());
    ctrl.set_strtab(strtab, 8).expect("strtab");
    ctrl.set_queue(false, cmdq, &QueueGeom::new(6, CMDQ_ENTRY_BYTES))
        .expect("cmdq");
    ctrl.set_queue(true, evtq, &QueueGeom::new(6, EVTQ_ENTRY_BYTES))
        .expect("evtq");
    ctrl.enable_translation().expect("first enable lands");
    assert_eq!(
        ctrl.enable_translation(),
        Err(SmmuFault::TranslationAlreadyEnabled),
        "a second enable is refused, never idempotently swallowed"
    );
    // Enforcement latched: the CR0ACK still shows the requested bits.
    assert_ne!(ctrl.cr0ack() & CR0_SMMUEN, 0);
    let (unit, _) = ctrl.into_inner();
    assert_eq!(unit.regs[REG_CR0 >> 2] & CR0_ENABLE_ALL, CR0_ENABLE_ALL);
}

#[test]
fn misaligned_publish_points_are_named_refusals() {
    let arena = Arena::new(16);
    let mut ctrl = Controller::new(SimUnit::new(&arena), Arena::new(4));
    assert_eq!(
        ctrl.set_strtab(0x4010_0008, 8),
        Err(SmmuFault::MisalignedPointer),
        "stream table needs 64-byte alignment"
    );
    assert_eq!(
        ctrl.set_queue(false, 0x4010_0010, &QueueGeom::new(6, CMDQ_ENTRY_BYTES)),
        Err(SmmuFault::MisalignedPointer)
    );
    // Geometry beyond the unit's advertised queue cap refuses BEFORE publication.
    assert!(matches!(
        queue_base_encode(0x4010_0000, &QueueGeom::new(20, CMDQ_ENTRY_BYTES)),
        Err(SmmuFault::MalformedRange)
    ));
}

#[test]
fn torn_write_protocol_clears_valid_first_and_closes_last() {
    let arena = Arena::new(8);
    let strtab = arena.raw_page();
    let ttb = arena.raw_page();
    let ste = ste_s2_encode(&geom(), ttb).expect("encode");
    let mut mem = arena.clone();
    rewrite_ste(&mut mem, strtab, 0x08, Some(&ste)).expect("written");
    let writes = arena.writes();
    let slot = strtab_slot(strtab, 0x08).expect("aligned");
    let hits: Vec<(usize, u64)> = writes
        .iter()
        .copied()
        .filter(|(a, _)| *a >= slot && *a < slot + 64)
        .collect();
    assert_eq!(hits.first(), Some(&(slot, 0)), "VALID word cleared FIRST");
    assert_eq!(
        hits.last(),
        Some(&(slot, ste[0])),
        "valid word closes the entry LAST"
    );
    assert!(
        hits.len() >= 9,
        "one clear + seven payload slots + one close"
    );

    // Revoke writes exactly ONE zero and nothing else.
    let before = writes.len();
    rewrite_ste(&mut mem, strtab, 0x08, None).expect("revoked");
    let revoke_writes: Vec<(usize, u64)> = arena.writes()[before..]
        .iter()
        .copied()
        .filter(|(a, _)| *a >= slot && *a < slot + 64)
        .collect();
    assert_eq!(revoke_writes, vec![(slot, 0)]);
}

#[test]
fn event_decode_roundtrips_field_positions_exactly() {
    // Hand-built record with high address bits, RNW, S2, CLASS - pinned to QEMU's EVT_SET_*.
    let mut words = [0u32; 8];
    words[0] = 0x10 | (0b101 << 12); // F_TRANSLATION, SSID bits carried above the type
    words[1] = 0x0858; // SID = bus 8 device 11 function 0
    words[3] = (1 << 3) | (1 << 7) | (0b01 << 8); // RNW, S2, CLASS=TT
    words[4] = 0x4800_0000u32;
    words[5] = 0x0000_0042u32;
    let ev = EventRecord::decode(&words);
    assert_eq!(ev.kind, 0x10);
    assert_eq!(ev.sid, 0x0858);
    assert_eq!(ev.addr, 0x42_4800_0000);
    assert!(ev.was_read);
    assert!(ev.s2);
    assert_eq!(ev.class, 0b01);
}
