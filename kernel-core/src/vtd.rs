//! The VT-d wire: the IOMMU contract programmed into a REAL DMA-remapping unit (ALET-P1-018,
//! ADR-071 contract; ADR-073 first hardware rung).
//!
//! ADR-071 defined the enforcement semantics once (`crate::iommu`) and proved them against a
//! software model, stating plainly that a hardware implementation must satisfy the same contract.
//! This module is that statement made concrete for Intel VT-d: the legacy register map, the
//! root/context wire formats, the second-level page-table shape, and a controller whose every
//! operation either completes or returns a NAMED refusal - never a guess about why the unit
//! stayed silent.
//!
//! Every wire constant below was PINNED against the emulated unit's actual behavior (QEMU's
//! intel-iommu implementation) - several first drafts carried plausible-but-wrong positions that
//! real traffic corrected: SRTP is bit 30 (bit 24 is SIRTP; writing our draft's bit 24 latched an
//! interrupt-remapping table while the poll for RTPS read IRTPS - two mirrored mistakes cancelling
//! into a green), the context-entry domain id lives at high-qword bits [23:8], and the IOTLB
//! registers sit where ECAP.IRO says, not where the oldest spec tables put them.
//!
//! # Shape of the first rung
//!
//! One identity domain shared by every present PCI function: conventional RAM minus the kernel
//! image, translated 1:1 (PA == IOVA), built entirely from frames the ownership model claims.
//! Everything else - the image, MMIO holes, addresses no RAM lives at - has NO leaf, so a device
//! inventing such an address faults against real silicon instead of reading kernel text. That is
//! ADR-071's sharpest promise (the image is not a DMA target) delivered on hardware, and it is
//! why the drivers needed no change: they already hand devices physical addresses of owned RAM
//! frames, which is exactly what the domain translates.
//!
//! What this rung deliberately does NOT claim (kept open in the gap register): per-device WINDOWS
//! (device-A buffers stay translatable for device B too - inter-device isolation is still the
//! software registry job), per-frame granularity, interrupt remapping, queued invalidation, and
//! pass-through translation types.

use crate::iommu::PAGE;

// --- the legacy register map (offsets the unit itself declares; see ProbeReport) ----------------

pub const REG_VER: usize = 0x00; // ro, 32-bit
pub const REG_CAP: usize = 0x08; // ro, 64-bit
pub const REG_ECAP: usize = 0x10; // ro, 64-bit
pub const REG_GCMD: usize = 0x18; // wo, 32-bit
pub const REG_GSTS: usize = 0x1C; // ro, 32-bit
pub const REG_RTADDR: usize = 0x20; // rw, 64-bit
pub const REG_CCMD: usize = 0x28; // rw, 64-bit (ICC write-1, self-clearing)
pub const REG_FSTS: usize = 0x30; // ro, 32-bit

/// Global Command: SET ROOT TABLE POINTER - BIT 30. The first draft used bit 24, which is SIRTP
/// (interrupt-remapping table); the unit happily latched nothing and set IRTPS, which the draft's
/// poll then read as success. Bit positions are pinned against the emulated unit now.
pub const GCMD_SRTP: u32 = 1 << 30;
/// Global Command: TRANSLATION ENABLE.
pub const GCMD_TE: u32 = 1 << 31;
/// Global Status: root-table pointer in effect (bit 30, mirroring SRTP).
pub const GSTS_RTPS: u32 = 1 << 30;
/// Global Status: translation ENABLED (the enforcement bit).
pub const GSTS_TES: u32 = 1 << 31;
/// Global Status: COMMAND FIELD ERROR - the unit rejected a command whose precondition was missing.
pub const GSTS_CFR: u32 = 1;

/// Context-command: INVALIDATE CONTEXT CACHE (write-1, self-clearing on completion).
pub const CCMD_ICC: u64 = 1 << 63;
/// Context-command request granularity (bits 62:61): 00b RESERVED. Global = 01b. A bare ICC
/// with CIRG=00 is refused by the unit outright ("invalid context").
pub const CCMD_CIRG_GLOBAL: u64 = 0b01 << 61;
pub const CCMD_CIRG_DOMAIN: u64 = 0b10 << 61;
pub const CCMD_CIRG_DEVICE: u64 = 0b11 << 61;
/// Context-command fields, for the SCOPED forms (next rung): DID at [15:0], SID at [31:16].
pub const CCMD_DID_SHIFT: u32 = 0;
pub const CCMD_SID_SHIFT: u32 = 16;

/// IVA address-mask field (bits 5:0): GLOBAL granularity hint for a whole-IOTLB flush.
pub const IAM_GLOBAL: u64 = 0b11;
/// IOTLB: INVALIDATE (write-1, self-clearing on completion).
pub const IOTLB_IVT: u64 = 1 << 63;
/// IOTLB request granularity (bits 61:60): GLOBAL = 01b (00b reserved - same trap as CCMD).
pub const IOTLB_IIRG_GLOBAL: u64 = 0b01 << 60;
/// IOTLB domain-id field (bits 47:32), for the DOMAIN-scoped form (next rung).
pub const IOTLB_DID_SHIFT: u32 = 32;

/// Fault status: overflow / pending-fault / oldest-record index (bits 15:8).
pub const FSTS_PFO: u32 = 1 << 0;
pub const FSTS_PPF: u32 = 1 << 1;
pub const FSTS_FRI_SHIFT: u32 = 8;

/// Why a programming step refused. Every variant names the way the caller or the machine was
/// wrong; none is a bare integer, because a log that says "failed" says nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtdFault {
    /// VER did not read back as a sane major.minor (wrong offsets, absent unit, lying firmware).
    UnsupportedVersion,
    /// CAP read back all-ones or zero - no unit answers at this base.
    CapabilityGarbage,
    /// CAP.RWBF (bit 4) demands write-buffer flushing around programming; this rung implements
    /// no flush protocol, so it refuses rather than running unsynchronized (fail-closed posture).
    WriteBufferFlushRequired,
    /// No AGAW this builder can express (needs 3- or 4-level support; neither offered).
    UnsupportedAgaw,
    /// A pointer handed to the unit was not page-aligned.
    MisalignedPointer,
    /// A command completed with GSTS.CFR - the unit rejected its precondition.
    CommandFieldError,
    /// A handshake poll exhausted its bound without the unit observing the request.
    HandshakeTimeout,
    /// Translation asked to turn ON while already ON.
    TranslationAlreadyEnabled,
    /// Programming attempted while translation was ENABLED - tables must not change under the walker.
    ProgrammedWhileEnabled,
    /// A RAM range overlapped the kernel image. The one overlap this subsystem never permits.
    ImageOverlap,
    /// An empty range, or alignment/field bounds violated where they are structural.
    MalformedRange,
    /// The frame supplier could not provide another table frame.
    NoSpace,
}

/// Which second-level depth the unit supports and the domain was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agaw {
    /// 3-level walk, 39-bit coverage (context AW field = 001).
    Lev3,
    /// 4-level walk, 48-bit coverage (context AW field = 010).
    Lev4,
}

impl Agaw {
    pub fn aw_field(self) -> u64 {
        match self {
            Agaw::Lev3 => 0b001,
            Agaw::Lev4 => 0b010,
        }
    }

    /// Page-walk shifts from the TOP table down to the LEAF level; the last entry is the leaf.
    pub fn shifts(self) -> &'static [u32] {
        match self {
            Agaw::Lev3 => &[30, 21, 12],
            Agaw::Lev4 => &[39, 30, 21, 12],
        }
    }

    /// Read CAP.SAGAW (bits 12:8) and pick the deepest supported expressible depth:
    /// bit1 = 3-level (39-bit), bit2 = 4-level (48-bit).
    pub fn pick(cap: u64) -> Option<Agaw> {
        let sagaw = (cap >> 8) & 0x1F;
        if sagaw & 0b100 != 0 {
            Some(Agaw::Lev4)
        } else if sagaw & 0b010 != 0 {
            Some(Agaw::Lev3)
        } else {
            None
        }
    }
}

/// Where the variable-position registers live on THIS unit. All three are DECLARED by the
/// unit in its capability registers - enumerated, never poked:
/// * IOTLB/IVA base: ECAP.IRO (bits 19:8), units of 16 bytes.
/// * Fault-record bank: CAP.FRO (bits 27:20), units of 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegLayout {
    pub iva_off: usize,
    pub iotlb_off: usize,
    pub frcd_bank_off: usize,
    /// Fault-record COUNT (CAP.NFR field + 1). Bounds the record index a reader may touch.
    pub frcd_count: usize,
}

/// What probe learned. Logged at boot so a failure names what the unit actually said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub ver_raw: u32,
    pub cap: u64,
    pub ecap: u64,
    pub agaw: Option<Agaw>,
    pub layout: RegLayout,
}

impl ProbeReport {
    /// Sanity the controller insists on before anything is programmed.
    pub fn validate(&self) -> Result<(), VtdFault> {
        // VER is NIBGLE-encoded BCD: bits 7:4 major, bits 3:0 minor (the shape Linux's
        // DMAR_VER_MAJOR/MINOR macros decode). A byte-shifted reading calls a healthy unit
        // unsupported - which is exactly how this rung found its own first draft wrong.
        let major = (self.ver_raw >> 4) & 0xF;
        if self.ver_raw == 0 || self.ver_raw == u32::MAX || self.ver_raw >> 8 != 0 || major == 0 {
            return Err(VtdFault::UnsupportedVersion);
        }
        if self.cap == 0 || self.cap == u64::MAX || self.cap >> 56 == 0xFF {
            return Err(VtdFault::CapabilityGarbage);
        }
        // CAP.RWBF is BIT 4 (the position Linux's cap_rwbf decodes). The first draft checked
        // bit 54 - one of CAP's drain-capability bits - and refused a healthy unit; the gate
        // caught it because the refusal NAMES what it refused rather than failing silently.
        if self.cap & (1 << 4) != 0 {
            return Err(VtdFault::WriteBufferFlushRequired);
        }
        if self.agaw.is_none() {
            return Err(VtdFault::UnsupportedAgaw);
        }
        Ok(())
    }
}

/// Derive the variable-register layout from the capability registers - PUBLIC because the host
/// simulation configures itself through the SAME function, so both sides decode identically.
pub fn layout_of(cap: u64, ecap: u64) -> RegLayout {
    let iva_off = (((ecap >> 8) & 0xFFF) as usize) << 4;
    RegLayout {
        iva_off,
        iotlb_off: iva_off + 8,
        frcd_bank_off: (((cap >> 20) & 0xFF) as usize) << 4,
        frcd_count: (((cap >> 40) & 0xF) as usize) + 1,
    }
}

// --- the two seams -------------------------------------------------------------------------------

/// The register file. 32-bit registers are read/written as 32, 64-bit ones as 64 - matching the
/// widths the spec defines rather than hoping byte lanes merge.
pub trait Regs {
    fn r32(&mut self, off: usize) -> u32;
    fn w32(&mut self, off: usize, v: u32);
    fn r64(&mut self, off: usize) -> u64;
    fn w64(&mut self, off: usize, v: u64);
}

/// Table memory: read/write one u64 at a PHYSICAL address inside a page-walk frame, and hand out
/// fresh ZEROED page frames. In-kernel this is volatile access over identity-mapped owned frames;
/// on the host it is a byte arena whose offsets ARE the physical addresses, which lets the test
/// device-side walker consume the very bytes the programmer wrote.
pub trait TableMem {
    fn read_u64(&self, pa: usize) -> u64;
    fn write_u64(&mut self, pa: usize, v: u64);
    fn alloc_zeroed_page(&mut self) -> Option<usize>;
}

// --- wire formats ----------------------------------------------------------------------------------

const ENTRY_PRESENT: u64 = 1;
const ENTRY_WRITE: u64 = 1 << 1;
const ENTRY_PS: u64 = 1 << 7; // 2 MiB leaf at the PD level
const ADDR_MASK_4K: u64 = 0x000F_FFFF_FFFF_F000;

/// Encode a legacy ROOT ENTRY: present + context-table pointer (bits 11:1 reserved zero).
pub fn root_entry_encode(ctx_table_pa: usize) -> Result<(u64, u64), VtdFault> {
    if !ctx_table_pa.is_multiple_of(PAGE) {
        return Err(VtdFault::MisalignedPointer);
    }
    Ok((ENTRY_PRESENT | (ctx_table_pa as u64 & ADDR_MASK_4K), 0))
}

/// Decode a root entry the way the unit reads it: (present, context-table pointer).
pub fn root_entry_decode(lo: u64) -> (bool, usize) {
    ((lo & ENTRY_PRESENT) != 0, (lo & ADDR_MASK_4K) as usize)
}

/// Encode a legacy CONTEXT ENTRY. Layout pinned against the emulated unit's decoder:
/// low qword carries P, FPD, TT(3:2) and the second-level pointer bits [63:12]; the high qword
/// carries AW[2:0] and DID at bits [23:8] - NOT at [31:16], which is where a plausible first
/// draft put it and where the unit does not look. The pointer spans only the low qword in
/// legacy mode (the unit marks high bits reserved), which bounds translatable PA below 2^52.
pub fn context_entry_encode(slpt_pa: usize, did: u16, agaw: Agaw) -> Result<(u64, u64), VtdFault> {
    if !slpt_pa.is_multiple_of(PAGE) {
        return Err(VtdFault::MisalignedPointer);
    }
    let p = slpt_pa as u64;
    let lo = ENTRY_PRESENT | (p & ADDR_MASK_4K);
    let hi = ((did as u64) << 8) | agaw.aw_field();
    Ok((lo, hi))
}

/// Decode a context entry: (present, translation type, did, aw field, second-level pointer).
pub fn context_entry_decode(lo: u64, hi: u64) -> (bool, u8, u16, u8, usize) {
    let present = lo & ENTRY_PRESENT != 0;
    let tt = ((lo >> 2) & 0b11) as u8;
    let did = ((hi >> 8) & 0xFFFF) as u16;
    let aw = (hi & 0b111) as u8;
    let p = (lo & ADDR_MASK_4K) as usize;
    (present, tt, did, aw, p)
}

/// One decoded fault record. Layout pinned against the emulated unit's recorder:
/// low qword = fault ADDRESS (bits 63:12); high qword = F(bit63), T(bit62: set when the
/// blocked access was a READ), FR reason bits [39:32], source-id bits [15:0].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultRecord {
    pub present: bool,
    pub source_id: u16,
    pub reason: u8,
    pub address: u64,
    pub was_read: bool,
}

/// Decode one raw (low, high) pair.
pub fn decode_fault_record(lo: u64, hi: u64) -> FaultRecord {
    FaultRecord {
        present: hi & (1 << 63) != 0,
        source_id: (hi & 0xFFFF) as u16,
        reason: ((hi >> 32) & 0xFF) as u8,
        address: lo & ADDR_MASK_4K,
        was_read: hi & (1 << 62) != 0,
    }
}

/// The fault reasons this rung can be named-by. Values are the unit's own encoding.
pub mod fr {
    /// Root entry present bit clear.
    pub const ROOT_ENTRY_P: u8 = 1;
    /// Context entry present bit clear - what revoking a function produces.
    pub const CONTEXT_ENTRY_P: u8 = 2;
    /// No write permission at a mapped leaf.
    pub const WRITE: u8 = 4;
    /// No read permission at a mapped leaf.
    pub const READ: u8 = 5;
}

// --- the domain builder ---------------------------------------------------------------------------

/// Stats the builder reports, logged at boot and asserted by both suites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainStats {
    pub tables: usize,
    pub huge_leaves: usize,
    pub page_leaves: usize,
}

struct Builder<'m, M: TableMem> {
    mem: &'m mut M,
    top: usize,
    shifts: &'static [u32],
    stats: DomainStats,
}

impl<'m, M: TableMem> Builder<'m, M> {
    /// Physical address of the LEAF SLOT covering `va` at level `level_shift`, creating interior
    /// tables along the way. Identity mapping means PA == VA throughout.
    fn leaf_slot(&mut self, va: usize, level_shift: u32) -> Result<usize, VtdFault> {
        const MASK: usize = 0x1FF;
        let pos = self
            .shifts
            .iter()
            .position(|&s| s == level_shift)
            .ok_or(VtdFault::MalformedRange)?;
        let mut table = self.top;
        for &sh in &self.shifts[..pos] {
            let idx = (va >> sh) & MASK;
            let e = self.mem.read_u64(table + idx * 8);
            let next = if e & ENTRY_PRESENT != 0 {
                (e & ADDR_MASK_4K) as usize
            } else {
                let np = self.mem.alloc_zeroed_page().ok_or(VtdFault::NoSpace)?;
                self.stats.tables += 1;
                self.mem
                    .write_u64(table + idx * 8, ENTRY_PRESENT | (np as u64));
                np
            };
            table = next;
        }
        let idx = (va >> level_shift) & MASK;
        Ok(table + idx * 8)
    }
}

/// Program ONE identity domain covering `ranges` minus `image`, at depth `agaw`. Conventional RAM
/// ranges arrive page-aligned (the UEFI map guarantees it); the image span is SUBTRACTED by the
/// CALLER (this builder REFUSES image-touching input rather than deciding memory policy silently),
/// and every leaf is identity. Returns the top-table physical address plus what was built.
pub fn program_identity_domain<M: TableMem>(
    mem: &mut M,
    ranges: &[(usize, usize)],
    image: (usize, usize),
    agaw: Agaw,
) -> Result<(usize, DomainStats), VtdFault> {
    if image.1 <= image.0 || !image.0.is_multiple_of(PAGE) || !image.1.is_multiple_of(PAGE) {
        return Err(VtdFault::MalformedRange);
    }
    for &(s, e) in ranges {
        if s >= e || s % PAGE != 0 || e % PAGE != 0 {
            return Err(VtdFault::MalformedRange);
        }
        if s < image.1 && image.0 < e {
            return Err(VtdFault::ImageOverlap);
        }
    }

    let top = mem.alloc_zeroed_page().ok_or(VtdFault::NoSpace)?;
    let mut b = Builder {
        mem,
        top,
        shifts: agaw.shifts(),
        stats: DomainStats {
            tables: 1,
            huge_leaves: 0,
            page_leaves: 0,
        },
    };

    const HUGE: usize = 512 * PAGE;
    for &(s, e) in ranges {
        let mut va = s;
        while va < e {
            if va % HUGE == 0 && e - va >= HUGE {
                let slot = b.leaf_slot(va, 21)?;
                b.mem
                    .write_u64(slot, ENTRY_PRESENT | ENTRY_WRITE | ENTRY_PS | (va as u64));
                b.stats.huge_leaves += 1;
                va += HUGE;
            } else {
                let slot = b.leaf_slot(va, 12)?;
                b.mem.write_u64(
                    slot,
                    ENTRY_PRESENT | ENTRY_WRITE | (va as u64 & ADDR_MASK_4K),
                );
                b.stats.page_leaves += 1;
                va += PAGE;
            }
        }
    }
    Ok((top, b.stats))
}

// --- the auditor --------------------------------------------------------------------------------------

/// What a walk of the LIVE tree found. `image_violations` must be zero for the boot to continue -
/// the hardware-shaped twin of the registry's image rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeAudit {
    pub huge_leaves: usize,
    pub page_leaves: usize,
    pub tables: usize,
    pub image_violations: usize,
}

/// Walk EVERY present entry of a programmed domain, counting leaves and flagging any leaf whose
/// translated span intersects the image. Reads only - it is the proof, not the programmer.
pub fn audit_tree<M: TableMem>(
    mem: &mut M,
    top: usize,
    agaw: Agaw,
    image: (usize, usize),
) -> TreeAudit {
    let mut audit = TreeAudit {
        huge_leaves: 0,
        page_leaves: 0,
        tables: 0,
        image_violations: 0,
    };
    let shifts = agaw.shifts();
    let mut stack: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    stack.push((top, 0));
    while let Some((table, level)) = stack.pop() {
        audit.tables += 1;
        for idx in 0..512usize {
            let e = mem.read_u64(table + idx * 8);
            if e & ENTRY_PRESENT == 0 {
                continue;
            }
            if level + 1 == shifts.len() {
                count_leaf(&mut audit, e, false, image, shifts[level]);
            } else if level + 2 == shifts.len() && e & ENTRY_PS != 0 {
                count_leaf(&mut audit, e, true, image, shifts[level + 1]);
            } else {
                stack.push(((e & ADDR_MASK_4K) as usize, level + 1));
            }
        }
    }
    audit
}

fn count_leaf(audit: &mut TreeAudit, e: u64, huge: bool, image: (usize, usize), shift: u32) {
    if huge {
        audit.huge_leaves += 1;
    } else {
        audit.page_leaves += 1;
    }
    let span = 1usize << shift;
    let base = (e & ADDR_MASK_4K) as usize;
    if base < image.1 && image.0 < base.saturating_add(span) {
        audit.image_violations += 1;
    }
}

// --- the controller ------------------------------------------------------------------------------------

const HANDSHAKE_BOUND: u32 = 1_000_000;

/// The driver half of the conversation with the unit. Every method is total over `VtdFault`: it
/// either completed the step or names why it did not.
pub struct Controller<R: Regs> {
    regs: R,
}

impl<R: Regs> Controller<R> {
    pub fn new(regs: R) -> Self {
        Controller { regs }
    }

    /// Hand the register seam back - how the host suite inspects what the unit retained after a
    /// sequence (latched status, published pointers). The kernel never needs it.
    pub fn into_inner(self) -> R {
        self.regs
    }

    /// Raw identification reads WITHOUT validation - what the boot log prints so a refusal names
    /// what the unit actually said rather than only the verdict.
    pub fn identify(&mut self) -> ProbeReport {
        let ver_raw = self.regs.r32(REG_VER);
        let cap = self.regs.r64(REG_CAP);
        let ecap = self.regs.r64(REG_ECAP);
        ProbeReport {
            ver_raw,
            cap,
            ecap,
            agaw: Agaw::pick(cap),
            layout: layout_of(cap, ecap),
        }
    }

    /// Read the identification registers and sanity-check them. Nothing is written.
    pub fn probe(&mut self) -> Result<ProbeReport, VtdFault> {
        let rep = self.identify();
        rep.validate()?;
        Ok(rep)
    }

    pub fn translation_enabled(&mut self) -> bool {
        self.regs.r32(REG_GSTS) & GSTS_TES != 0
    }

    /// The root-table pointer as the unit currently holds it - a diagnostic for suites that need
    /// to see what the unit adopted rather than what was written.
    pub fn rtaddr(&mut self) -> usize {
        self.regs.r64(REG_RTADDR) as usize
    }

    /// Publish the root table and make the unit adopt it (SRTP handshake).
    pub fn set_root(&mut self, rt_pa: usize) -> Result<(), VtdFault> {
        if !rt_pa.is_multiple_of(PAGE) {
            return Err(VtdFault::MisalignedPointer);
        }
        if self.translation_enabled() {
            return Err(VtdFault::ProgrammedWhileEnabled);
        }
        self.regs.w64(REG_RTADDR, rt_pa as u64);
        self.regs.w32(REG_GCMD, GCMD_SRTP);
        self.poll_gsts(GSTS_RTPS)
    }

    /// Turn ENFORCEMENT on. From here, every PCI DMA cycle walks the tables.
    pub fn enable_translation(&mut self) -> Result<(), VtdFault> {
        if self.translation_enabled() {
            return Err(VtdFault::TranslationAlreadyEnabled);
        }
        self.regs.w32(REG_GCMD, GCMD_TE);
        self.poll_gsts(GSTS_TES)
    }

    /// Invalidate the whole context cache. Used after ANY context entry changed - the coarse but
    /// unconditional form, chosen because correctness cannot depend on getting a scoped field right.
    pub fn invalidate_context_cache_global(&mut self) -> Result<(), VtdFault> {
        self.regs.w64(REG_CCMD, CCMD_ICC | CCMD_CIRG_GLOBAL);
        for _ in 0..HANDSHAKE_BOUND {
            if self.regs.r64(REG_CCMD) & CCMD_ICC == 0 {
                return Ok(());
            }
        }
        Err(VtdFault::HandshakeTimeout)
    }

    /// Global IOTLB invalidation at the offsets the unit DECLARES (ECAP.IRO). IVA carries the
    /// granularity hint, IOTLB carries the go with both drain hints so in-flight requests retire
    /// before the flush completes rather than racing it.
    pub fn invalidate_iotlb_global(&mut self, lay: &RegLayout) -> Result<(), VtdFault> {
        self.regs.w64(lay.iva_off, IAM_GLOBAL);
        self.regs.w64(lay.iotlb_off, IOTLB_IVT | IOTLB_IIRG_GLOBAL);
        for _ in 0..HANDSHAKE_BOUND {
            if self.regs.r64(lay.iotlb_off) & IOTLB_IVT == 0 {
                return Ok(());
            }
        }
        Err(VtdFault::HandshakeTimeout)
    }

    /// Raw fault status. DIAGNOSTIC ONLY on this seam: one emulator implements FSTS at a
    /// non-spec offset, so enforcement EVIDENCE is taken from the fault-record bank (see
    /// `fault_record`/`clear_fault_record`) and this register is read only to log it.
    pub fn fsts(&mut self) -> u32 {
        self.regs.r32(REG_FSTS)
    }

    /// Read one fault record (both qwords), raw, at the bank offset the unit declared.
    pub fn fault_record(&mut self, lay: &RegLayout, idx: usize) -> (u64, u64) {
        let base = lay.frcd_bank_off + idx * 16;
        (self.regs.r64(base), self.regs.r64(base + 8))
    }

    /// Clear one fault record's F bit. The bank is WRITE-ONE-TO-CLEAR: the high qword accepts no
    /// ordinary writes at all - only a 1 written at the F position retires the record (the unit
    /// then re-computes FSTS.PPF from the bank). Used between live probes so later silence is
    /// EVIDENCE of a clean walk rather than residue of an earlier record.
    pub fn clear_fault_record(&mut self, lay: &RegLayout, idx: usize) -> Result<(), VtdFault> {
        if idx >= lay.frcd_count {
            return Err(VtdFault::MalformedRange);
        }
        let base = lay.frcd_bank_off + idx * 16 + 8;
        self.regs.w64(base, 1u64 << 63);
        Ok(())
    }

    fn poll_gsts(&mut self, want: u32) -> Result<(), VtdFault> {
        for _ in 0..HANDSHAKE_BOUND {
            let gsts = self.regs.r32(REG_GSTS);
            if gsts & GSTS_CFR != 0 {
                return Err(VtdFault::CommandFieldError);
            }
            if gsts & want != 0 {
                return Ok(());
            }
        }
        Err(VtdFault::HandshakeTimeout)
    }
}

/// Rewrite one function context entry IN PLACE - grant (`Some`) or revoke (`None`) - through the
/// same seam the builder used, so live deny/grant choreography cannot diverge from the format.
pub fn rewrite_context_entry<M: TableMem>(
    mem: &mut M,
    ctx_table_pa: usize,
    dev: u8,
    fun: u8,
    entry: Option<(u64, u64)>,
) -> Result<(), VtdFault> {
    if dev > 31 || fun > 7 {
        return Err(VtdFault::MalformedRange);
    }
    // Context entries are SIXTEEN bytes each, indexed straight by devfn - 256 x 16B fills the
    // 4 KiB context table exactly. An eight-byte stride here writes half-way between where the
    // unit reads, and every granted function reads back absent; the emulated unit's faults
    // named it (sid 0x18, reason CONTEXT_ENTRY_P) on the first enforced boot.
    let slot = ctx_table_pa + (((dev as usize) << 3) | fun as usize) * 16;
    match entry {
        Some((lo, hi)) => {
            // Never publish a torn entry: absent first, then the pair.
            mem.write_u64(slot + 8, 0);
            mem.write_u64(slot, lo);
            mem.write_u64(slot + 8, hi);
        }
        None => {
            mem.write_u64(slot, 0);
            mem.write_u64(slot + 8, 0);
        }
    }
    Ok(())
}
