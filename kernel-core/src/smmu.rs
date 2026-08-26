//! The SMMUv3 wire: the IOMMU contract programmed into ARM's system MMU (ALET-P1-018,
//! ADR-071 contract; ADR-073 first rung on VT-d; this module the second rung on SMMUv3).
//!
//! ADR-071 defined the enforcement semantics once ([crate::iommu]) and ADR-073 delivered them
//! on Intel VT-d under q35. This module is the same statement made concrete for the ARM
//! SMMUv3 QEMU emulates on the virt machines: the fixed register map, the linear stream
//! table, stage-2-only stream table entries, the command queue that invalidates them, and a
//! controller whose every operation either completes or returns a NAMED refusal.
//!
//! # Discovery, not poking
//!
//! The unit answers at an address the platform DECLARES in the device tree (compatible
//! "arm,smmu-v3"), and every PCI Requester it translates is declared beside it: the host
//! bridge's iommu-map property binds RID -> StreamID. Both facts are parsed by the target's DT
//! walker; nothing here hardcodes either.
//!
//! # Shape of this rung
//!
//! One identity domain shared by every present PCI function - conventional RAM minus the
//! kernel image, translated 1:1 through a STAGE-2-ONLY STE (CONFIG = 0b010), because stage 2
//! is exactly VT-d's second-level translation: input address == output address, no context
//! descriptors, one tree per domain. Every leaf comes from frames the ownership model claims.
//! Everything else on the wire - the image, MMIO holes, addresses no RAM lives at - has NO
//! leaf, so a device inventing such an address faults against real silicon instead of reading
//! kernel text.
//!
//! Evidence comes from the unit's own EVENT QUEUE, not from request completions - the same
//! posture as the VT-d rung, for the same reason: QEMU's TCG loses virtio completions across
//! a mid-run enablement while the unit's translation verdicts stay exact (ADR-073 carries the
//! evidence trail). Stage-2 walk-fault recording is gated by STE.S2R, so the domain sets it;
//! a domain that forgets would translate silently and the suite would hang rather than lie.
//!
//! # Wire facts pinned against the emulated unit (QEMU hw/arm/smmuv3)
//!
//! Several first drafts carried plausible-but-wrong constants; each correction is recorded
//! where it lives:
//! * CR0.SMMUEN is bit 0, EVENTQEN bit 2, CMDQEN bit 3 - bits [15:5] are RESERVED
//!   (the emulator's SMMU_CR0_RESERVED = 0xFFFFFA20), so the enable word is exactly 0b1101.
//! * Queue base registers carry the ADDRESS at [51:6] and LOG2SIZE at [4:0] of the SAME
//!   register - a spec-shaped draft that split them across two registers published garbage
//!   geometry the unit silently honored until its first doorbell.
//! * VT-d's fault bank does not exist here: denial EVIDENCE is an event record, and stage-2
//!   walk faults reach the event queue only when STE.S2R is set. An STE without S2R denies
//!   silently - correct enforcement, zero evidence - which is why the encoder refuses to emit
//!   one.
//! * The top table's alignment follows from the IPA space, not from PAGE alone: with SL0=0b01
//!   and S2T0SZ=25 the walk consumes exactly 39 input bits, so the level-1 table needs natural
//!   4 KiB alignment and represents ONE table, not a concatenated set. A wider IPA space at
//!   the same start level makes the unit align the base DOWN by concatenation strides - a
//!   different tree than the programmer built.
//!
//! What this rung deliberately does NOT claim (kept open in the gap register): inter-device
//! isolation (every present function shares the one identity domain, exactly as on the VT-d
//! rung), interrupt remapping, ATS/PRI, stage-1 translation, per-device windows, and MSI
//! signaling of events - completion is polled everywhere.

use crate::iommu::PAGE;

// --- the register map (fixed offsets; IHI 0070 section 6) --------------------------------------------

pub const REG_IDR0: usize = 0x00;
pub const REG_IDR1: usize = 0x04;
pub const REG_IDR2: usize = 0x08;
pub const REG_IDR3: usize = 0x0C;
pub const REG_IDR4: usize = 0x10;
pub const REG_IDR5: usize = 0x14;
pub const REG_IIDR: usize = 0x18;
pub const REG_AIDR: usize = 0x1C;
pub const REG_CR0: usize = 0x20;
pub const REG_CR0ACK: usize = 0x24;
pub const REG_GBPA: usize = 0x44;
pub const REG_IRQ_CTRL: usize = 0x50;
pub const REG_GERROR: usize = 0x60;
pub const REG_GERRORN: usize = 0x64;
/// 64-bit registers: STRTAB_BASE, CMDQ_BASE, EVENTQ_BASE go through the 64-bit accessor.
pub const REG_STRTAB_BASE: usize = 0x80;
pub const REG_STRTAB_BASE_CFG: usize = 0x88;
pub const REG_CMDQ_BASE: usize = 0x90;
pub const REG_CMDQ_PROD: usize = 0x98;
pub const REG_CMDQ_CONS: usize = 0x9C;
pub const REG_EVENTQ_BASE: usize = 0xA0;
pub const REG_EVENTQ_PROD: usize = 0xA8;
pub const REG_EVENTQ_CONS: usize = 0xAC;

/// CR0: the three bits this rung turns on. Bits [15:5] are RESERVED; writing any of them is
/// UNPREDICTABLE per spec and the unit mirrors only non-reserved bits into CR0ACK - so the
/// enable word is exact, never "just set bit 0".
pub const CR0_SMMUEN: u32 = 1 << 0;
pub const CR0_EVENTQEN: u32 = 1 << 2;
pub const CR0_CMDQEN: u32 = 1 << 3;
/// The full legal enable word for this driver.
pub const CR0_ENABLE_ALL: u32 = CR0_SMMUEN | CR0_EVENTQEN | CR0_CMDQEN;

/// GERROR: command-queue error pending. Acked by writing GERRORN against the live set, after
/// which the consumer may run again.
pub const GERROR_CMDQ_ERR: u32 = 1 << 0;

/// CMDQ_CONS bits [30:24]: the error field a rejected command leaves behind.
pub const CMDQ_CONS_ERR_SHIFT: u32 = 24;
/// CERROR_ILL: the unit refused a command outright (bad opcode or bad field).
pub const CMD_CONS_ERR_ILL: u32 = 0x2;

// --- identification -----------------------------------------------------------------------------------

/// What probe learned, logged at boot so a refusal names what the unit actually said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub idr: [u32; 6],
    pub iidr: u32,
    pub aidr: u32,
}

impl ProbeReport {
    /// IDR0.S2P: stage-2 translation supported. This rung programs stage-2-only STEs, so a
    /// unit without it is refused before anything is written.
    pub fn s2p(&self) -> bool {
        self.idr[0] & 1 != 0
    }
    /// IDR0.S1P: stage-1 advertised (the virt machine creates the unit with stage=nested).
    pub fn s1p(&self) -> bool {
        self.idr[0] & (1 << 1) != 0
    }
    /// IDR0.TTF == 0b10: AArch64 page-table format only.
    pub fn ttf_aarch64(&self) -> bool {
        (self.idr[0] >> 2) & 0b11 == 0b10
    }
    /// IDR1.SIDSIZE: stream-id bits implemented. Bounds every SID we may program.
    pub fn sid_size(&self) -> u32 {
        self.idr[1] & 0x3F
    }
    /// IDR5.OAS: physical output size code -> bits (0..=5 -> 32/36/40/42/44/48).
    pub fn oas_bits(&self) -> u32 {
        match self.idr[5] & 0b111 {
            0 => 32,
            1 => 36,
            2 => 40,
            3 => 42,
            4 => 44,
            _ => 48,
        }
    }
    /// IDR5.GRAN4K: the 4 KiB granule this tree is built from.
    pub fn gran4k(&self) -> bool {
        self.idr[5] & (1 << 4) != 0
    }

    /// Sanity the controller insists on before anything is programmed.
    pub fn validate(&self) -> Result<(), SmmuFault> {
        let dead = self.idr.iter().all(|&v| v == 0) || self.idr.iter().all(|&v| v == 0xFFFF_FFFF);
        if dead || !self.s2p() {
            return Err(SmmuFault::Stage2Missing);
        }
        if !self.ttf_aarch64() || !self.gran4k() {
            return Err(SmmuFault::UnsupportedGranule);
        }
        if self.sid_size() < 8 {
            // A PCI requester id needs eight bits (device<<3|function) even on bus 0.
            return Err(SmmuFault::SidSpaceTooSmall);
        }
        Ok(())
    }
}

// --- the two seams (identical shapes to kernel_core::vtd) ---------------------------------------------

/// The register file.
pub trait Regs {
    fn r32(&mut self, off: usize) -> u32;
    fn w32(&mut self, off: usize, v: u32);
    fn r64(&mut self, off: usize) -> u64;
    fn w64(&mut self, off: usize, v: u64);
}

/// Table memory: read/write one u64 at a PHYSICAL address inside a walk frame, hand out fresh
/// ZEROED page frames. In-kernel this is volatile access over identity-mapped owned frames; on
/// the host it is a byte arena whose offsets ARE physical addresses, so the simulated device-side
/// walker consumes the very bytes the programmer wrote.
pub trait TableMem {
    fn read_u64(&self, pa: usize) -> u64;
    fn write_u64(&mut self, pa: usize, v: u64);
    fn alloc_zeroed_page(&mut self) -> Option<usize>;
}

// --- queues ---------------------------------------------------------------------------------------------

/// A command is four little-endian words (16 bytes); an event is eight (32 bytes). Pinned
/// against the emulator's Cmd/Evt structs.
pub const CMDQ_ENTRY_BYTES: usize = 16;
pub const EVTQ_ENTRY_BYTES: usize = 32;

/// Queue ring arithmetic. PROD/CONS registers hold (wrap << log2size) | index; the wrap bit
/// sits at bit log2size, immediately above the index field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueGeom {
    pub log2size: u32,
    pub entry_bytes: usize,
}

impl QueueGeom {
    pub const fn new(log2size: u32, entry_bytes: usize) -> Self {
        QueueGeom {
            log2size,
            entry_bytes,
        }
    }

    pub fn slots(&self) -> usize {
        1usize << self.log2size
    }
    pub fn index_mask(&self) -> u32 {
        self.slots() as u32 - 1
    }
    pub fn wrap_bit(&self) -> u32 {
        self.slots() as u32
    }
    pub fn wrap_index_mask(&self) -> u32 {
        self.wrap_bit() | self.index_mask()
    }
    fn idx(reg: u32, g: &QueueGeom) -> u32 {
        reg & g.index_mask()
    }

    /// Empty: indices equal INCLUDING wrap phase.
    pub fn is_empty(&self, prod: u32, cons: u32) -> bool {
        prod & self.wrap_index_mask() == cons & self.wrap_index_mask()
    }
    /// Full: indices differ ONLY by the wrap phase.
    pub fn is_full(&self, prod: u32, cons: u32) -> bool {
        (prod ^ cons) & self.wrap_index_mask() == self.wrap_bit()
    }
    /// Byte address of the entry a register value names.
    pub fn entry_addr(&self, base_pa: usize, reg: u32) -> usize {
        base_pa + Self::idx(reg, self) as usize * self.entry_bytes
    }
    /// Advance one slot, carrying into the wrap bit past the last index.
    pub fn advance(&self, reg: u32) -> u32 {
        (reg + 1) & self.wrap_index_mask()
    }
}

/// Encode a Q_BASE-style register: address at [51:6], log2size at [4:0]. The first draft split
/// these across two registers; the unit reads BOTH from this one, so the draft's doorbell then
/// walked nowhere - recorded where it was fixed.
pub fn queue_base_encode(pa: usize, g: &QueueGeom) -> Result<u64, SmmuFault> {
    if !pa.is_multiple_of(64) {
        return Err(SmmuFault::MisalignedPointer);
    }
    if g.log2size > 19 {
        // IDR1 names the queue-size caps (EVENTQS/CMDQS); 19 is what this unit advertises.
        return Err(SmmuFault::MalformedRange);
    }
    Ok((pa as u64 & 0x000F_FFFF_FFFF_FFC0) | g.log2size as u64)
}

/// STRTAB_BASE_CFG for a LINEAR stream table: FMT=00b, LOG2SIZE in bits [5:0]. Linear means
/// the STE for sid lives at base + sid*64 - one table, no second-level descriptors, sized to
/// cover exactly the requester space bus 0 can produce.
pub fn strtab_cfg_linear(log2size: u32) -> Result<u32, SmmuFault> {
    if log2size > 0x3F {
        return Err(SmmuFault::MalformedRange);
    }
    Ok(log2size)
}

// --- commands -------------------------------------------------------------------------------------------

pub mod cmd {
    /// CFGI_STE: invalidate cached config for ONE stream id.
    pub const CFGI_STE: u8 = 0x03;
    /// TLBI_S12_VMALL: invalidate all stage-2 TLB entries of one VMID.
    pub const TLBI_S12_VMALL: u8 = 0x28;
    /// SYNC: completion barrier - everything before it is observed before it completes.
    pub const CFGI_ALL: u8 = 0x07;
    pub const SYNC: u8 = 0x46;
}

/// CMD_CFGI_STE.SSEC must be zero (secure streams are not this rung's business).
const CMD_SSEC_BIT: u32 = 1 << 10;

/// One command, ready to store into the queue (four LE words).
pub type Cmd = [u32; 4];

/// Invalidate the cached configuration of one stream id.
pub fn cmd_cfgi_ste(sid: u32) -> Cmd {
    debug_assert_eq!([cmd::CFGI_STE as u32, sid, 0, 0][0] & CMD_SSEC_BIT, 0);
    [cmd::CFGI_STE as u32, sid, 0, 0]
}

/// Invalidate every stage-2 TLB entry of vmid.
pub fn cmd_tlbi_s12_vmall(vmid: u16) -> Cmd {
    [cmd::TLBI_S12_VMALL as u32, vmid as u32, 0, 0]
}

/// The completion barrier.
pub fn cmd_cfgi_all() -> Cmd {
    [cmd::CFGI_ALL as u32, 0, 0, 0]
}

pub fn cmd_sync() -> Cmd {
    [cmd::SYNC as u32, 0, 0, 0]
}

// --- events ----------------------------------------------------------------------------------------------

/// Event kinds this rung is named-by. Values are the unit's own encoding.
pub mod evt {
    /// Configuration: the stream table entry was absent or invalid.
    pub const C_BAD_STE: u8 = 0x04;
    /// Translation walk found no leaf - deny-by-default firing.
    pub const F_TRANSLATION: u8 = 0x10;
    /// The leaf exists but its access flag was clear.
    pub const F_ACCESS: u8 = 0x12;
    /// The leaf forbids the attempted access.
    pub const F_PERMISSION: u8 = 0x13;
}

/// One decoded event record (eight LE words, 32 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRecord {
    pub kind: u8,
    pub sid: u32,
    pub addr: u64,
    /// RNW: true when the denied access was a READ.
    pub was_read: bool,
    /// S2: the faulting class walked stage 2 (always true for this domain).
    pub s2: bool,
    /// CLASS field: 0b00 names the input address.
    pub class: u8,
}

impl EventRecord {
    /// Decode from the eight words the queue holds. Field positions pinned against the
    /// emulator's EVT_SET_* macros: type word0[7:0], SSID word0[31:12], SID word1,
    /// RNW word3 bit3, S2 word3 bit7, CLASS word3[9:8], ADDR words4/5.
    pub fn decode(words: &[u32; 8]) -> EventRecord {
        EventRecord {
            kind: (words[0] & 0xFF) as u8,
            sid: words[1],
            addr: ((words[5] as u64) << 32) | words[4] as u64,
            was_read: words[3] & (1 << 3) != 0,
            s2: words[3] & (1 << 7) != 0,
            class: ((words[3] >> 8) & 0b11) as u8,
        }
    }
}

// --- stream table entries --------------------------------------------------------------------------------

/// STE bytes as eight u64 slots (the TableMem grain). Slot N covers u32 words 2N and 2N+1.
pub type Ste = [u64; 8];

/// CONFIG field values (STE word0 bits [3:1]).
pub mod ste_cfg {
    pub const ABORT: u32 = 0b000;
    pub const S1_ONLY: u32 = 0b001;
    pub const S2_ONLY: u32 = 0b010;
    pub const S1_S2: u32 = 0b011;
    pub const BYPASS: u32 = 0b100;
}

/// Why an STE or its geometry was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteFault {
    MisalignedTable,
    /// S2T0SZ outside the architectural window (>= max(64-eff_ps,16), <= 39).
    BadT0SZ,
    /// SL0/S2T0SZ combination implies concatenated top tables - a different tree than built.
    BadStartLevel,
    /// S2TTB outside the effective physical output size.
    BadTTB,
    /// A field this rung refuses to set (S2ENDI, stall) or forgot to set (S2AA64, S2R).
    BadField,
}

/// Stage-2 geometry: how the unit will walk. SL0=0b01 starts at level 1, and S2T0SZ=25 makes
/// the walk consume exactly 39 input bits - one level-1 table, no concatenation, natural page
/// alignment, and the same 30/21/12 shifts the target's own TTBR0 walk uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S2Geometry {
    pub vmid: u16,
    pub sl0: u8,
    pub t0sz: u8,
    /// Physical output size BITS declared in S2PS (we declare the unit's OAS).
    pub ps_bits: u32,
}

impl S2Geometry {
    /// The one geometry this rung builds: VMID nonzero, level-1 start, 39-bit IPA space,
    /// 44-bit output. Everything else is refused by validate.
    pub const fn standard(vmid: u16) -> Self {
        S2Geometry {
            vmid,
            sl0: 0b01,
            t0sz: 25,
            ps_bits: 44,
        }
    }

    /// Input-address bits the walk consumes (64 - T0SZ).
    pub fn input_bits(&self) -> u32 {
        64 - self.t0sz as u32
    }

    /// Page-walk shifts from the TOP table down to the LEAF level; the last entry is the leaf.
    /// Start level 1 with 4 KiB granule: L1 blocks 1 GiB, L2 blocks 2 MiB, L3 pages 4 KiB.
    pub fn shifts(&self) -> &'static [u32] {
        &[30, 21, 12]
    }

    /// Mirror the unit's own acceptance rules (decode_ste_s2_cfg + s2t0sz_valid +
    /// s2_pgtable_config_valid): refusing HERE means the suite fails with a name instead of
    /// discovering a C_BAD_STE event at kick time.
    pub fn validate(&self) -> Result<(), SteFault> {
        if self.t0sz > 39 {
            return Err(SteFault::BadT0SZ);
        }
        // Minimum valid value is MAX(64 - eff_ps, 16) for a 4 KiB granule.
        let min_tsz = 64usize.saturating_sub(self.ps_bits as usize).max(16);
        if (self.t0sz as usize) < min_tsz {
            return Err(SteFault::BadT0SZ);
        }
        // Single top table: the input space must fit INSIDE one level-1 span (2^39), otherwise
        // the unit aligns the base by concatenation strides and walks a different tree.
        if self.sl0 != 0b01 || self.input_bits() != 39 {
            return Err(SteFault::BadStartLevel);
        }
        Ok(())
    }
}

/// Encode a stage-2-only STE. Refuses to emit one WITHOUT S2R: a domain that denies silently
/// enforces nothing provable, and silent enforcement is exactly what this subsystem exists to
/// prevent.
pub fn ste_s2_encode(g: &S2Geometry, ttb_pa: usize) -> Result<Ste, SteFault> {
    g.validate()?;
    if !ttb_pa.is_multiple_of(PAGE) {
        return Err(SteFault::MisalignedTable);
    }
    if ttb_pa >= 1usize << g.ps_bits.min(63) {
        return Err(SteFault::BadTTB);
    }
    let mut w32 = [0u32; 16];
    // word0: V=1, CONFIG=S2_ONLY.
    w32[0] = 1 | (ste_cfg::S2_ONLY << 1);
    // word4: S2VMID.
    w32[4] = g.vmid as u32;
    // word5: S2T0SZ[5:0] | S2SL0[7:6] | S2TG[15:14]=00 (4K) | S2PS[18:16] |
    //        S2AA64(19)=1 | S2ENDI(20)=0 | S2AFFD(21)=0 | S2S(25)=0 | S2R(26)=1.
    let ps_code: u32 = match g.ps_bits {
        44 => 4,
        48 => 5,
        _ => return Err(SteFault::BadField),
    };
    w32[5] = ((g.t0sz as u32) & 0x3F)
        | ((g.sl0 as u32 & 0b11) << 6)
        | (ps_code << 16)
        | (1 << 19)
        | (1 << 26);
    // word6/7: S2TTB occupies word BITS [51:4] directly - the low four bits of word6 are
    // reserved-zero, the rest IS the table address. A draft that shifted right by four encoded
    // a different table than it built; the walker faulted every access and named it.
    w32[6] = (ttb_pa as u32) & 0xFFFF_FFF0;
    w32[7] = ((ttb_pa >> 32) as u32) & 0xF_FFFF;
    let mut ste: Ste = [0; 8];
    for (i, slot) in ste.iter_mut().enumerate() {
        *slot = (w32[i * 2] as u64) | ((w32[i * 2 + 1] as u64) << 32);
    }
    Ok(ste)
}

/// Decode the fields the live choreography cares about: (valid, config, vmid, ttb).
pub fn ste_s2_decode(ste: &Ste) -> (bool, u32, u16, usize) {
    let w = |i: usize| -> u32 { (ste[i / 2] >> if i.is_multiple_of(2) { 0 } else { 32 }) as u32 };
    let valid = w(0) & 1 != 0;
    let config = (w(0) >> 1) & 0b111;
    let vmid = (w(4) & 0xFFFF) as u16;
    let ttb = (((w(7) as usize) & 0xF_FFFF) << 32) | ((w(6) as usize) & 0xFFFF_FFF0);
    (valid, config, vmid, ttb)
}

/// Byte address of one linear-stream-table entry.
pub fn strtab_slot(strtab_pa: usize, sid: u32) -> Result<usize, SmmuFault> {
    if !strtab_pa.is_multiple_of(64) {
        return Err(SmmuFault::MisalignedPointer);
    }
    Ok(strtab_pa + sid as usize * 64)
}

/// Rewrite one function's STE IN PLACE - grant (Some) or revoke (None) - through the same seam
/// the builder used. Never publishes a torn VALID bit: the valid slot is cleared FIRST, then
/// the rest of the entry lands, then the new valid slot closes it.
pub fn rewrite_ste<M: TableMem>(
    mem: &mut M,
    strtab_pa: usize,
    sid: u32,
    entry: Option<&Ste>,
) -> Result<(), SmmuFault> {
    let slot = strtab_slot(strtab_pa, sid)?;
    match entry {
        Some(ste) => {
            mem.write_u64(slot, 0);
            for (i, w) in ste.iter().enumerate().skip(1) {
                mem.write_u64(slot + i * 8, *w);
            }
            mem.write_u64(slot, ste[0]);
        }
        None => {
            mem.write_u64(slot, 0);
        }
    }
    Ok(())
}

// --- the stage-2 page-tree formats ------------------------------------------------------------------------

const DESC_VALID: u64 = 1;
/// Level < 3: block descriptor (output = a whole aligned region).
const DESC_BLOCK: u64 = 0b01;
/// Level < 3: next-table descriptor; level == 3: page descriptor.
const DESC_TABLE_OR_PAGE: u64 = 0b11;
/// AF (bit 10): the unit faults EVERY access to an AF-clear leaf unless the STE sets S2AFFD.
/// We set AF per leaf and leave AFFD clear, so "mapped" always means "access-flagged".
const DESC_AF: u64 = 1 << 10;
/// S2AP bits [7:6] = 0b11: read AND write permitted (the unit reads the pair as an
/// IOMMU-permission bitmask, so 0b01 would be read-only and 0b10 write-only).
const DESC_S2AP_RW: u64 = 0b11 << 6;
/// MemAttr bits [5:2] = 0b1111: normal memory. The emulator ignores attributes; real silicon
/// does not, so the honest value travels in the descriptor instead of zeros.
const DESC_MEMATTR_NORMAL: u64 = 0b1111 << 2;
const OA_MASK_4K: u64 = 0x0000_FFFF_FFFF_F000;

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
    /// Physical address of the LEAF SLOT covering va at level level_shift, creating interior
    /// tables along the way. Identity mapping means PA == VA throughout.
    fn leaf_slot(&mut self, va: usize, level_shift: u32) -> Result<usize, SmmuFault> {
        const MASK: usize = 0x1FF;
        let pos = self
            .shifts
            .iter()
            .position(|&s| s == level_shift)
            .ok_or(SmmuFault::MalformedRange)?;
        let mut table = self.top;
        for &sh in &self.shifts[..pos] {
            let idx = (va >> sh) & MASK;
            let e = self.mem.read_u64(table + idx * 8);
            let next = if e & DESC_VALID != 0 && e & 0b11 == DESC_TABLE_OR_PAGE {
                (e & OA_MASK_4K) as usize
            } else {
                let np = self.mem.alloc_zeroed_page().ok_or(SmmuFault::NoSpace)?;
                self.stats.tables += 1;
                self.mem.write_u64(
                    table + idx * 8,
                    DESC_VALID | DESC_TABLE_OR_PAGE | (np as u64),
                );
                np
            };
            table = next;
        }
        let idx = (va >> level_shift) & MASK;
        Ok(table + idx * 8)
    }
}

/// Program ONE identity domain covering ranges minus image, at the geometry's depth.
/// Conventional RAM spans arrive page-aligned; the image span is SUBTRACTED by the CALLER (this
/// builder REFUSES image-touching input rather than deciding memory policy silently), and every
/// leaf is identity. Returns the top-table physical address plus what was built.
pub fn program_identity_domain<M: TableMem>(
    mem: &mut M,
    ranges: &[(usize, usize)],
    image: (usize, usize),
    g: &S2Geometry,
) -> Result<(usize, DomainStats), SmmuFault> {
    g.validate()?;
    if image.1 <= image.0 || !image.0.is_multiple_of(PAGE) || !image.1.is_multiple_of(PAGE) {
        return Err(SmmuFault::MalformedRange);
    }
    for &(s, e) in ranges {
        if s >= e || s % PAGE != 0 || e % PAGE != 0 {
            return Err(SmmuFault::MalformedRange);
        }
        if s < image.1 && image.0 < e {
            return Err(SmmuFault::ImageOverlap);
        }
        // The walk must be able to EXPRESS every mapped byte: an address at or above the input
        // space faults by construction, and handing the builder such a range is a caller bug.
        if e > 1usize << g.input_bits().min(63) {
            return Err(SmmuFault::MalformedRange);
        }
    }

    let top = mem.alloc_zeroed_page().ok_or(SmmuFault::NoSpace)?;
    let mut b = Builder {
        mem,
        top,
        shifts: g.shifts(),
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
                b.mem.write_u64(
                    slot,
                    DESC_VALID
                        | DESC_BLOCK
                        | DESC_AF
                        | DESC_S2AP_RW
                        | DESC_MEMATTR_NORMAL
                        | (va as u64 & OA_MASK_4K),
                );
                b.stats.huge_leaves += 1;
                va += HUGE;
            } else {
                let slot = b.leaf_slot(va, 12)?;
                b.mem.write_u64(
                    slot,
                    DESC_VALID
                        | DESC_TABLE_OR_PAGE
                        | DESC_AF
                        | DESC_S2AP_RW
                        | DESC_MEMATTR_NORMAL
                        | (va as u64 & OA_MASK_4K),
                );
                b.stats.page_leaves += 1;
                va += PAGE;
            }
        }
    }
    Ok((top, b.stats))
}

// --- the auditor -------------------------------------------------------------------------------------------

/// What a walk of the LIVE tree found. image_violations must be zero for the boot to continue -
/// the hardware-shaped twin of the registry's image rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeAudit {
    pub tables: usize,
    pub huge_leaves: usize,
    pub page_leaves: usize,
    pub image_violations: usize,
}

/// Walk EVERY present entry of a programmed domain, counting leaves and flagging any leaf whose
/// translated span intersects the image. Reads only - it is the proof, not the programmer.
pub fn audit_tree<M: TableMem>(
    mem: &mut M,
    top: usize,
    g: &S2Geometry,
    image: (usize, usize),
) -> TreeAudit {
    let shifts = g.shifts();
    let mut audit = TreeAudit {
        tables: 0,
        huge_leaves: 0,
        page_leaves: 0,
        image_violations: 0,
    };
    let mut stack: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    stack.push((top, 0));
    while let Some((table, level)) = stack.pop() {
        audit.tables += 1;
        for idx in 0..512usize {
            let e = mem.read_u64(table + idx * 8);
            if e & DESC_VALID == 0 {
                continue;
            }
            if level + 1 == shifts.len() {
                count_leaf(&mut audit, e, false, image, shifts[level]);
            } else if e & 0b11 == DESC_BLOCK {
                count_leaf(&mut audit, e, true, image, shifts[level]);
            } else {
                stack.push(((e & OA_MASK_4K) as usize, level + 1));
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
    let base = (e & OA_MASK_4K) as usize;
    if base < image.1 && image.0 < base.saturating_add(span) {
        audit.image_violations += 1;
    }
}

// --- the controller ------------------------------------------------------------------------------------------

const HANDSHAKE_BOUND: u32 = 1_000_000;

/// The driver half of the conversation with the unit. Every method is total over SmmuFault:
/// it either completed the step or names why it did not.
pub struct Controller<R: Regs, M: TableMem> {
    regs: R,
    mem: M,
    cmdq: Option<(usize, QueueGeom)>,
    evtq: Option<(usize, QueueGeom)>,
}

impl<R: Regs, M: TableMem> Controller<R, M> {
    pub fn new(regs: R, mem: M) -> Self {
        Controller {
            regs,
            mem,
            cmdq: None,
            evtq: None,
        }
    }

    /// Hand both seams back - how a host suite inspects what the unit retained.
    pub fn into_inner(self) -> (R, M) {
        (self.regs, self.mem)
    }

    pub fn table_mem(&mut self) -> &mut M {
        &mut self.mem
    }

    /// Raw identification reads WITHOUT validation - what the boot log prints so a refusal
    /// names what the unit actually said rather than only the verdict.
    pub fn identify(&mut self) -> ProbeReport {
        let mut idr = [0u32; 6];
        for (i, slot) in idr.iter_mut().enumerate() {
            *slot = self.regs.r32(REG_IDR0 + i * 4);
        }
        ProbeReport {
            idr,
            iidr: self.regs.r32(REG_IIDR),
            aidr: self.regs.r32(REG_AIDR),
        }
    }

    /// Read the identification registers and sanity-check them. Nothing is written.
    pub fn probe(&mut self) -> Result<ProbeReport, SmmuFault> {
        let rep = self.identify();
        rep.validate()?;
        Ok(rep)
    }

    pub fn cr0ack(&mut self) -> u32 {
        self.regs.r32(REG_CR0ACK)
    }

    pub fn smmu_enabled(&mut self) -> bool {
        self.cr0ack() & CR0_SMMUEN != 0
    }

    pub fn gerror(&mut self) -> u32 {
        self.regs.r32(REG_GERROR)
    }

    /// Publish the LINEAR stream table. Only legal while enforcement is off - tables must not
    /// change under the walker.
    pub fn set_strtab(&mut self, base_pa: usize, log2size: u32) -> Result<(), SmmuFault> {
        if self.smmu_enabled() {
            return Err(SmmuFault::ProgrammedWhileEnabled);
        }
        if !base_pa.is_multiple_of(64) {
            return Err(SmmuFault::MisalignedPointer);
        }
        self.regs
            .w64(REG_STRTAB_BASE, base_pa as u64 & 0x000F_FFFF_FFFF_FFC0);
        self.regs
            .w32(REG_STRTAB_BASE_CFG, strtab_cfg_linear(log2size)?);
        Ok(())
    }

    /// Publish one queue. Same off-while-enabled rule.
    pub fn set_queue(
        &mut self,
        eventq: bool,
        base_pa: usize,
        g: &QueueGeom,
    ) -> Result<(), SmmuFault> {
        if self.smmu_enabled() {
            return Err(SmmuFault::ProgrammedWhileEnabled);
        }
        let (base_reg, prod_reg, cons_reg) = if eventq {
            (REG_EVENTQ_BASE, REG_EVENTQ_PROD, REG_EVENTQ_CONS)
        } else {
            (REG_CMDQ_BASE, REG_CMDQ_PROD, REG_CMDQ_CONS)
        };
        let enc = queue_base_encode(base_pa, g)?;
        self.regs.w64(base_reg, enc);
        self.regs.w32(prod_reg, 0);
        self.regs.w32(cons_reg, 0);
        if eventq {
            self.evtq = Some((base_pa, *g));
        } else {
            self.cmdq = Some((base_pa, *g));
        }
        Ok(())
    }

    /// Turn ENFORCEMENT on. From here, every translated DMA cycle walks the tables.
    pub fn enable_translation(&mut self) -> Result<(), SmmuFault> {
        if self.smmu_enabled() {
            return Err(SmmuFault::TranslationAlreadyEnabled);
        }
        self.regs.w32(REG_CR0, CR0_ENABLE_ALL);
        // Poll the ACK: the unit confirms the REQUESTED (non-reserved) bits, so a mirrored
        // mistake elsewhere would show up here as a mismatch instead of a green.
        for _ in 0..HANDSHAKE_BOUND {
            let ack = self.regs.r32(REG_CR0ACK);
            if ack & CR0_ENABLE_ALL == CR0_ENABLE_ALL {
                if self.gerror() & GERROR_CMDQ_ERR != 0 {
                    return Err(SmmuFault::GError(GERROR_CMDQ_ERR));
                }
                return Ok(());
            }
        }
        Err(SmmuFault::HandshakeTimeout)
    }

    /// Post ONE command and wait for the unit to consume it.
    fn post_raw(&mut self, c: &Cmd) -> Result<(), SmmuFault> {
        let (base, g) = self.cmdq.ok_or(SmmuFault::NoQueue)?;
        let prod = self.regs.r32(REG_CMDQ_PROD);
        let cons = self.regs.r32(REG_CMDQ_CONS);
        if g.is_full(prod, cons) {
            return Err(SmmuFault::QueueFull);
        }
        let addr = g.entry_addr(base, prod);
        for (i, w) in c.chunks(2).enumerate() {
            let lo = w[0] as u64;
            let hi = *w.get(1).unwrap_or(&0) as u64;
            self.mem.write_u64(addr + i * 8, lo | (hi << 32));
        }
        // Doorbell: the unit consumes synchronously under emulation, asynchronously on
        // silicon; either way the bounded poll below is the contract. A REJECTED command parks
        // CONS at the offending entry with ERR set and GERROR.CMDQ_ERR raised instead of
        // advancing - the poll watches both outcomes, because waiting only for progress would
        // turn a named refusal into a timeout.
        let next = g.advance(prod);
        self.regs.w32(REG_CMDQ_PROD, next);
        for _ in 0..HANDSHAKE_BOUND {
            let cons_now = self.regs.r32(REG_CMDQ_CONS);
            let err = (cons_now >> CMDQ_CONS_ERR_SHIFT) & 0x7F;
            if err != 0 || self.gerror() & GERROR_CMDQ_ERR != 0 {
                return Err(SmmuFault::CommandRefused(if err != 0 {
                    err
                } else {
                    CMD_CONS_ERR_ILL
                }));
            }
            if cons_now & g.wrap_index_mask() == next {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(SmmuFault::HandshakeTimeout)
    }

    /// Invalidate the cached configuration of one stream, then the whole stage-2 TLB of our
    /// VMID, then order both with a SYNC. Coarse but unconditional - correctness cannot depend
    /// on getting a scoped granularity right (same posture as the VT-d global flushes).
    pub fn invalidate_stream(&mut self, sid: u32, vmid: u16) -> Result<(), SmmuFault> {
        self.post_raw(&cmd_cfgi_ste(sid))?;
        self.post_raw(&cmd_tlbi_s12_vmall(vmid))?;
        self.post_raw(&cmd_sync())
    }

    /// Drain every pending event oldest-first into out, retiring each slot as it is read, so a
    /// later quiet check is EVIDENCE of a clean walk rather than residue of older records.
    pub fn drain_events(
        &mut self,
        out: &mut alloc::vec::Vec<EventRecord>,
    ) -> Result<(), SmmuFault> {
        let (base, g) = match self.evtq {
            Some(q) => q,
            None => return Err(SmmuFault::NoQueue),
        };
        loop {
            let prod = self.regs.r32(REG_EVENTQ_PROD);
            let cons = self.regs.r32(REG_EVENTQ_CONS);
            if g.is_empty(prod, cons) {
                return Ok(());
            }
            let addr = g.entry_addr(base, cons);
            // Four u64 slots carry the eight LE words an event record is made of.
            let mut words = [0u32; 8];
            for i in 0..4usize {
                let raw = self.mem.read_u64(addr + i * 8);
                words[i * 2] = raw as u32;
                words[i * 2 + 1] = (raw >> 32) as u32;
            }
            out.push(EventRecord::decode(&words));
            self.regs.w32(REG_EVENTQ_CONS, g.advance(cons));
        }
    }

    /// True when no event is pending.
    pub fn events_quiet(&mut self) -> bool {
        match self.evtq {
            Some((_base, g)) => {
                let prod = self.regs.r32(REG_EVENTQ_PROD);
                let cons = self.regs.r32(REG_EVENTQ_CONS);
                g.is_empty(prod, cons)
            }
            None => false,
        }
    }

    /// Diagnostics for suites that need to see what the unit retained.
    pub fn strtab_base(&mut self) -> u64 {
        self.regs.r64(REG_STRTAB_BASE)
    }
    pub fn evtq_prod(&mut self) -> u32 {
        self.regs.r32(REG_EVENTQ_PROD)
    }
}

/// Why a programming step refused. Every variant names the way the caller or the machine was
/// wrong; none is a bare integer, because a log that says "failed" says nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmmuFault {
    /// IDRs read back dead (absent unit, wrong offsets, lying firmware) or lack stage 2.
    Stage2Missing,
    /// No AArch64 4 KiB granule support advertised.
    UnsupportedGranule,
    /// SIDSIZE too small to name a bus-0 requester.
    SidSpaceTooSmall,
    /// A pointer handed to the unit was not aligned as its format requires.
    MisalignedPointer,
    /// A field/geometry combination is architecturally illegal.
    MalformedRange,
    /// A RAM range overlapped the kernel image. The one overlap this subsystem never permits.
    ImageOverlap,
    /// A handshake poll exhausted its bound without the unit observing the request.
    HandshakeTimeout,
    /// Enforcement asked to turn ON while already ON.
    TranslationAlreadyEnabled,
    /// Programming attempted while enforcement was ENABLED.
    ProgrammedWhileEnabled,
    /// The unit refused a command (CONS.ERR carried the reason).
    CommandRefused(u32),
    /// GERROR reported a queue error.
    GError(u32),
    /// The frame supplier could not provide another table frame.
    NoSpace,
    /// The command queue has no room.
    QueueFull,
    /// Queues were never published.
    NoQueue,
    /// An STE field/geometry refusal (see SteFault).
    Ste(SteFault),
}

impl From<SteFault> for SmmuFault {
    fn from(e: SteFault) -> Self {
        SmmuFault::Ste(e)
    }
}
