//! NVMe block driver (REQ-DRV-009, ADR-223): the first storage driver for a device class that real
//! machines ship, not a paravirtual one. One namespace, one admin queue pair, one I/O queue pair,
//! one 4 KiB block per command (PRP1 only), completions POLLED against a time budget.
//!
//! Every register offset, bit position and structure offset below is taken from the NVM Express
//! Base Specification (rev. 1.4/2.0 numbering, section named per constant) and cross-checked
//! against QEMU's `include/block/nvme.h`, the model the VM gates drive. Nothing is typed from
//! memory alone: a wrong offset here is a write to the wrong register of a real controller.
//!
//! The driver is generic over [`NvmeRegs`] (how the controller's BAR0 is reached) and
//! [`VirtioHal`] (frames, barrier, clock - the same seam virtio-blk uses), so the hosted tests
//! drive the IDENTICAL code against a simulated controller that can be told to misbehave.
//!
//! Fail-closed rules: a controller that never reports ready, reports fatal status, needs a page
//! size other than 4 KiB, has no NVM command set, or formats namespace 1 with metadata or an LBA
//! size that does not divide 4 KiB is refused at init. A completion whose command id, queue id or
//! status is wrong is an error, never data.

use core::cell::Cell;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use crate::dma::DmaRegistry;
use crate::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE};
use crate::virtioblk::VirtioHal;

// --- Controller registers (Base spec §3.1 "Controller Properties") -----------------------------
/// Controller Capabilities, 64-bit (§3.1.1).
pub const REG_CAP: usize = 0x00;
/// Version (§3.1.2).
pub const REG_VS: usize = 0x08;
/// Interrupt Mask Set (§3.1.3).
pub const REG_INTMS: usize = 0x0C;
/// Controller Configuration (§3.1.5).
pub const REG_CC: usize = 0x14;
/// Controller Status (§3.1.6).
pub const REG_CSTS: usize = 0x1C;
/// Admin Queue Attributes (§3.1.8).
pub const REG_AQA: usize = 0x24;
/// Admin Submission Queue base, 64-bit (§3.1.9).
pub const REG_ASQ: usize = 0x28;
/// Admin Completion Queue base, 64-bit (§3.1.10).
pub const REG_ACQ: usize = 0x30;
/// First doorbell (§3.1.24): SQ y tail at `0x1000 + 2y*(4<<DSTRD)`, CQ y head one stride later.
pub const REG_DOORBELL_BASE: usize = 0x1000;

// CAP fields (§3.1.1).
const CAP_MQES_MASK: u64 = 0xFFFF; // bits 15:0, zero-based max queue entries
const CAP_TO_SHIFT: u64 = 24; // bits 31:24, ready timeout in 500 ms units
const CAP_DSTRD_SHIFT: u64 = 32; // bits 35:32, doorbell stride 2^(2+DSTRD)
const CAP_CSS_NVM: u64 = 1 << 37; // CSS bit 0 (bits 44:37): NVM command set
const CAP_MPSMIN_SHIFT: u64 = 48; // bits 51:48, min page size 2^(12+MPSMIN)

// CC fields (§3.1.5).
const CC_EN: u32 = 1 << 0;
const CC_IOSQES: u32 = 6 << 16; // 2^6 = 64-byte submission entries
const CC_IOCQES: u32 = 4 << 20; // 2^4 = 16-byte completion entries
                                // CSS (6:4) = 000 NVM, MPS (10:7) = 0 -> 4 KiB, AMS (13:11) = 0 round robin: all zero.

// CSTS fields (§3.1.6).
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

// --- Commands (§4.2 SQE, §4.6 CQE) --------------------------------------------------------------
const SQE_BYTES: usize = 64;
const CQE_BYTES: usize = 16;

// Admin opcodes (§5, Figure "Opcodes for Admin Commands").
const ADM_CREATE_SQ: u8 = 0x01;
const ADM_CREATE_CQ: u8 = 0x05;
const ADM_IDENTIFY: u8 = 0x06;
// NVM opcodes (NVM Command Set spec §3, Figure "Opcodes for NVM Commands").
const NVM_FLUSH: u8 = 0x00;
const NVM_WRITE: u8 = 0x01;
const NVM_READ: u8 = 0x02;

// Identify CNS values (§5.15.1).
const CNS_NAMESPACE: u32 = 0x00;
const CNS_CONTROLLER: u32 = 0x01;

// Identify Controller offsets (§5.15.2.1).
const IDC_MN: usize = 24; // model number, 40 ASCII bytes
const IDC_MDTS: usize = 77;
const IDC_NN: usize = 516; // number of namespaces, u32
const IDC_VWC: usize = 525; // bit 0: volatile write cache present
                            // Identify Namespace offsets (NVM Command Set spec, Identify Namespace data structure).
const IDN_NSZE: usize = 0; // namespace size in LBAs, u64
const IDN_NLBAF: usize = 25; // zero-based number of LBA formats
const IDN_FLBAS: usize = 26; // bits 3:0 select the format in use
const IDN_LBAF: usize = 128; // LBA format table, 4 bytes each: MS u16, LBADS u8, RP u8

/// The one namespace this driver speaks to.
pub const NSID: u32 = 1;
/// Entries per queue this driver asks for (capped by CAP.MQES + 1).
pub const QUEUE_DEPTH_WANT: u16 = 16;
/// Time budget for one command's completion.
pub const COMMAND_BUDGET_NS: u64 = 20_000_000_000;
/// Upper bound on any ready-wait even if CAP.TO claims longer (TO max is 127.5 s).
const READY_CAP_NS: u64 = 60_000_000_000;

/// How the controller's register window (BAR0) is reached. MMIO on hardware; a simulation in tests.
pub trait NvmeRegs {
    fn r32(&self, off: usize) -> u32;
    fn w32(&self, off: usize, v: u32);
    fn r64(&self, off: usize) -> u64 {
        self.r32(off) as u64 | (self.r32(off + 4) as u64) << 32
    }
    fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

/// BAR0 reached through an identity-mapped device-memory window.
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
    /// # Safety
    /// `base` must be the mapped BAR0 of an NVMe controller, at least 0x2000 bytes long.
    pub unsafe fn new(base: usize) -> Self {
        MmioRegs { base }
    }
}

impl NvmeRegs for MmioRegs {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `new` requires a mapped BAR0; offsets used are inside the first two pages.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

/// Why a command did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NvmeError {
    /// No completion within the budget.
    Timeout,
    /// A completion named a command id this driver did not issue.
    WrongCommandId,
    /// A completion named a submission queue other than the one used.
    WrongQueue,
    /// Status Code Type / Status Code non-zero: (SCT << 8) | SC.
    Status(u16),
    /// A buffer about to be handed to the controller is not a registered DMA region.
    Unregistered,
}

/// What init observed, for the caller's log line.
#[derive(Clone, Copy, Debug)]
pub struct NvmeReport {
    pub version: u32,
    pub queue_depth: u16,
    pub doorbell_stride: usize,
    pub namespaces: u32,
    pub lba_bytes: usize,
    pub namespace_lbas: u64,
    pub volatile_cache: bool,
    pub mdts: u8,
    /// Model number as reported (ASCII, space padded).
    pub model: [u8; 40],
}

struct Queue {
    sq: usize,
    cq: usize,
    qid: u16,
    tail: Cell<u16>,
    head: Cell<u16>,
    phase: Cell<bool>,
}

/// A live NVMe controller with namespace 1 attached.
pub struct Nvme<H: VirtioHal, R: NvmeRegs> {
    regs: R,
    admin: Queue,
    io: Queue,
    data: usize,
    depth: u16,
    stride: usize,
    next_cid: Cell<u16>,
    lbas_per_block: u64,
    namespace_lbas: u64,
    volatile_cache: bool,
    /// Identify Controller's serial is exactly [`SCRATCH_SERIAL`]: the only controller written.
    scratch: bool,
    dma: DmaRegistry,
    budget_ns: u64,
    _hal: PhantomData<H>,
}

/// The serial number that marks a controller this driver may write (ADR-228). A real machine's
/// SSD holds someone's data; nothing without this mark is ever written.
pub const SCRATCH_SERIAL: &[u8] = b"ALETHEIA-SCRATCH";
const IDC_SN: usize = 4; // serial number, 20 ASCII bytes, space padded

/// A disabled controller does no DMA: a driver nobody holds stops it (ADR-228).
impl<H: VirtioHal, R: NvmeRegs> Drop for Nvme<H, R> {
    fn drop(&mut self) {
        self.regs.w32(REG_CC, 0);
    }
}

fn wait_status<H: VirtioHal, R: NvmeRegs>(
    regs: &R,
    want_ready: bool,
    budget_ns: u64,
) -> Result<(), &'static str> {
    let started = H::now_ns();
    loop {
        let csts = regs.r32(REG_CSTS);
        if csts == 0xFFFF_FFFF {
            return Err("nvme: controller status reads all-ones (device gone) - fail closed");
        }
        if want_ready && csts & CSTS_CFS != 0 {
            return Err("nvme: controller reports fatal status while enabling");
        }
        if (csts & CSTS_RDY != 0) == want_ready {
            return Ok(());
        }
        if H::now_ns().wrapping_sub(started) > budget_ns {
            return Err("nvme: controller ready bit did not change within CAP.TO");
        }
        core::hint::spin_loop();
    }
}

unsafe fn rd8(a: usize) -> u8 {
    unsafe { read_volatile(a as *const u8) }
}
unsafe fn rd32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
unsafe fn rd64(a: usize) -> u64 {
    unsafe { read_volatile(a as *const u64) }
}

impl<H: VirtioHal, R: NvmeRegs> Nvme<H, R> {
    /// Reset, configure the admin queue, enable, identify, create one I/O queue pair.
    ///
    /// # Safety
    /// `regs` must reach a live NVMe controller's BAR0 and `H::alloc_frame` must return zeroed,
    /// identity-mapped frames this kernel owns exclusively (they become the controller's DMA targets).
    pub unsafe fn init(regs: R) -> Result<(Self, NvmeReport), &'static str> {
        let cap = regs.r64(REG_CAP);
        if cap == u64::MAX {
            return Err("nvme: CAP reads all-ones (no controller behind BAR0)");
        }
        if cap & CAP_CSS_NVM == 0 {
            return Err("nvme: controller does not support the NVM command set - fail closed");
        }
        if (cap >> CAP_MPSMIN_SHIFT) & 0xF != 0 {
            return Err("nvme: controller minimum page size is above 4 KiB - fail closed");
        }
        let mqes = (cap & CAP_MQES_MASK) as u32 + 1;
        if mqes < 2 {
            return Err("nvme: CAP.MQES allows fewer than two queue entries");
        }
        let depth = core::cmp::min(QUEUE_DEPTH_WANT as u32, mqes) as u16;
        let stride = 4usize << ((cap >> CAP_DSTRD_SHIFT) & 0xF);
        let ready_ns = core::cmp::min(
            ((cap >> CAP_TO_SHIFT) & 0xFF).max(1) * 500_000_000,
            READY_CAP_NS,
        );
        let version = regs.r32(REG_VS);

        // Disable first (§3.5.1 "Controller Initialization"): admin queue registers may only be
        // written while CC.EN = 0 and CSTS.RDY = 0.
        if regs.r32(REG_CC) & CC_EN != 0 {
            regs.w32(REG_CC, 0);
        }
        wait_status::<H, R>(&regs, false, ready_ns)?;

        let mut frames = [0usize; 5];
        for f in frames.iter_mut() {
            *f = H::alloc_frame().ok_or("nvme: frame allocator exhausted")?;
        }
        let [asq, acq, iosq, iocq, data] = frames;
        let mut dma = DmaRegistry::new();
        for (addr, owner) in [
            (asq, "nvme.admin-sq"),
            (acq, "nvme.admin-cq"),
            (iosq, "nvme.io-sq"),
            (iocq, "nvme.io-cq"),
            (data, "nvme.data"),
        ] {
            dma.register(addr, crate::dma::PAGE, owner)
                .map_err(|_| "nvme: a queue or data frame was refused as a DMA region")?;
        }

        let q = (depth - 1) as u32;
        regs.w32(REG_AQA, q << 16 | q);
        regs.w64(REG_ASQ, asq as u64);
        regs.w64(REG_ACQ, acq as u64);
        H::barrier();
        regs.w32(REG_CC, CC_EN | CC_IOSQES | CC_IOCQES);
        wait_status::<H, R>(&regs, true, ready_ns)?;
        // Polled driver: mask the pin-based/MSI vector 0 (§3.1.3). MSI-X is never enabled here.
        regs.w32(REG_INTMS, 1);

        let mut dev = Nvme {
            regs,
            admin: Queue {
                sq: asq,
                cq: acq,
                qid: 0,
                tail: Cell::new(0),
                head: Cell::new(0),
                phase: Cell::new(true),
            },
            io: Queue {
                sq: iosq,
                cq: iocq,
                qid: 1,
                tail: Cell::new(0),
                head: Cell::new(0),
                phase: Cell::new(true),
            },
            data,
            depth,
            stride,
            next_cid: Cell::new(1),
            lbas_per_block: 0,
            namespace_lbas: 0,
            volatile_cache: false,
            scratch: false,
            dma,
            budget_ns: COMMAND_BUDGET_NS,
            _hal: PhantomData,
        };

        // Identify Controller.
        dev.admin_cmd(ADM_IDENTIFY, 0, data, CNS_CONTROLLER, 0)
            .map_err(|_| "nvme: Identify Controller failed")?;
        let nn = rd32(data + IDC_NN);
        let vwc = rd8(data + IDC_VWC) & 1 != 0;
        let mdts = rd8(data + IDC_MDTS);
        let mut model = [0u8; 40];
        for (i, b) in model.iter_mut().enumerate() {
            *b = rd8(data + IDC_MN + i);
        }
        let mut sn = [0u8; 20];
        for (i, b) in sn.iter_mut().enumerate() {
            *b = rd8(data + IDC_SN + i);
        }
        let sn_end = sn
            .iter()
            .rposition(|&c| c != b' ' && c != 0)
            .map_or(0, |e| e + 1);
        dev.scratch = &sn[..sn_end] == SCRATCH_SERIAL;
        if nn < NSID {
            return Err("nvme: controller reports no namespace 1");
        }

        // Identify Namespace 1.
        dev.admin_cmd(ADM_IDENTIFY, NSID, data, CNS_NAMESPACE, 0)
            .map_err(|_| "nvme: Identify Namespace 1 failed")?;
        let nsze = rd64(data + IDN_NSZE);
        let nlbaf = rd8(data + IDN_NLBAF);
        let fmt = rd8(data + IDN_FLBAS) & 0xF;
        if fmt > nlbaf {
            return Err("nvme: FLBAS selects an LBA format beyond NLBAF - fail closed");
        }
        let lbaf = rd32(data + IDN_LBAF + 4 * fmt as usize);
        let ms = lbaf & 0xFFFF;
        let lbads = (lbaf >> 16) & 0xFF;
        if ms != 0 {
            return Err("nvme: namespace 1 carries per-LBA metadata - unsupported, fail closed");
        }
        if !(9..=12).contains(&lbads) {
            return Err("nvme: namespace 1 LBA size is not 512 B..4 KiB - fail closed");
        }
        let lba_bytes = 1usize << lbads;
        if nsze == 0 {
            return Err("nvme: namespace 1 is empty");
        }
        dev.lbas_per_block = (BLOCK_SIZE / lba_bytes) as u64;
        dev.namespace_lbas = nsze;
        dev.volatile_cache = vwc;

        // Create I/O Completion Queue 1 (§5.4): PC=1, IEN=0 (polled). Then SQ 1 bound to CQ 1 (§5.5).
        let qdw10 = q << 16 | 1;
        dev.admin_cmd(ADM_CREATE_CQ, 0, iocq, qdw10, 1)
            .map_err(|_| "nvme: Create I/O Completion Queue failed")?;
        dev.admin_cmd(ADM_CREATE_SQ, 0, iosq, qdw10, 1 << 16 | 1)
            .map_err(|_| "nvme: Create I/O Submission Queue failed")?;

        let report = NvmeReport {
            version,
            queue_depth: depth,
            doorbell_stride: stride,
            namespaces: nn,
            lba_bytes,
            namespace_lbas: nsze,
            volatile_cache: vwc,
            mdts,
            model,
        };
        Ok((dev, report))
    }

    /// Tighten (or restore) the per-command completion budget.
    pub fn set_completion_budget_ns(&mut self, ns: u64) {
        self.budget_ns = ns;
    }

    /// The grants this driver holds for its controller (queues + data), for the IOMMU window.
    pub fn dma_grants(&self) -> alloc::vec::Vec<crate::dma::Grant> {
        self.dma.grants()
    }

    /// Does the DMA gate refuse an address this driver never registered, with all five frames live?
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        !self.dma.visible(0x7fff_0000_0000, 64)
            && !self.dma.visible(self.data, crate::dma::PAGE * 2)
            && self.dma.live_regions() == 5
    }

    /// Does this controller carry the scratch serial (the only kind this driver writes)?
    pub fn is_scratch(&self) -> bool {
        self.scratch
    }

    /// Bytes per logical block of namespace 1.
    pub fn lba_bytes(&self) -> usize {
        BLOCK_SIZE / self.lbas_per_block as usize
    }

    fn admin_cmd(
        &self,
        opcode: u8,
        nsid: u32,
        prp1: usize,
        cdw10: u32,
        cdw11: u32,
    ) -> Result<u32, NvmeError> {
        self.submit(&self.admin, opcode, nsid, prp1, [cdw10, cdw11, 0])
    }

    fn sq_doorbell(&self, qid: u16) -> usize {
        REG_DOORBELL_BASE + (2 * qid as usize) * self.stride
    }

    fn cq_doorbell(&self, qid: u16) -> usize {
        REG_DOORBELL_BASE + (2 * qid as usize + 1) * self.stride
    }

    /// Write one SQE, ring the tail doorbell, poll the CQ slot at head for the phase flip, validate it.
    fn submit(
        &self,
        q: &Queue,
        opcode: u8,
        nsid: u32,
        prp1: usize,
        cdw: [u32; 3],
    ) -> Result<u32, NvmeError> {
        // THE GATE (REQ-DRV-006): the only buffer address a command carries must be registered.
        if prp1 != 0 && !self.dma.visible(prp1, crate::dma::PAGE) {
            return Err(NvmeError::Unregistered);
        }
        let cid = self.next_cid.get();
        // CID 0 is skipped so a zeroed completion slot can never match a live command.
        self.next_cid.set(if cid == u16::MAX { 1 } else { cid + 1 });
        let tail = q.tail.get();
        let sqe = q.sq + tail as usize * SQE_BYTES;
        // SAFETY: `sqe` lies inside the queue's registered, identity-mapped frame (depth*64 <= 4096).
        unsafe {
            let w = sqe as *mut u32;
            for i in 0..16 {
                write_volatile(w.add(i), 0);
            }
            // CDW0: opcode 7:0, FUSE 9:8 = 0, PSDT 15:14 = 0 (PRPs), CID 31:16.
            write_volatile(w, opcode as u32 | (cid as u32) << 16);
            write_volatile(w.add(1), nsid);
            write_volatile((sqe + 24) as *mut u64, prp1 as u64); // PRP1 = DW6..7
            write_volatile(w.add(10), cdw[0]);
            write_volatile(w.add(11), cdw[1]);
            write_volatile(w.add(12), cdw[2]);
        }
        let new_tail = (tail + 1) % self.depth;
        q.tail.set(new_tail);
        H::barrier();
        self.regs.w32(self.sq_doorbell(q.qid), new_tail as u32);

        let head = q.head.get();
        let cqe = q.cq + head as usize * CQE_BYTES;
        let started = H::now_ns();
        // SAFETY: `cqe` lies inside the completion queue's registered frame (depth*16 <= 4096).
        let dw3 = loop {
            let dw3 = unsafe { read_volatile((cqe + 12) as *const u32) };
            if ((dw3 >> 16) & 1 != 0) == q.phase.get() {
                break dw3;
            }
            if H::now_ns().wrapping_sub(started) > self.budget_ns {
                return Err(NvmeError::Timeout);
            }
            core::hint::spin_loop();
        };
        H::barrier();
        let (dw0, dw2) = unsafe {
            (
                read_volatile(cqe as *const u32),
                read_volatile((cqe + 8) as *const u32),
            )
        };
        // Consume the slot whatever it says, so the queue stays in step with the controller.
        let new_head = (head + 1) % self.depth;
        if new_head == 0 {
            q.phase.set(!q.phase.get());
        }
        q.head.set(new_head);
        self.regs.w32(self.cq_doorbell(q.qid), new_head as u32);

        if dw3 & 0xFFFF != cid as u32 {
            return Err(NvmeError::WrongCommandId);
        }
        if dw2 >> 16 != q.qid as u32 {
            return Err(NvmeError::WrongQueue);
        }
        // Status field 31:17: SC 24:17, SCT 27:25 (§4.6.1).
        let sc = (dw3 >> 17) & 0xFF;
        let sct = (dw3 >> 25) & 0x7;
        if sc != 0 || sct != 0 {
            return Err(NvmeError::Status((sct << 8 | sc) as u16));
        }
        Ok(dw0)
    }

    fn io(&self, opcode: u8, idx: usize) -> Result<(), StorageError> {
        let slba = idx as u64 * self.lbas_per_block;
        let nlb = (self.lbas_per_block - 1) as u32; // zero-based
        self.submit(
            &self.io,
            opcode,
            NSID,
            self.data,
            [slba as u32, (slba >> 32) as u32, nlb],
        )
        .map(|_| ())
        .map_err(|_| StorageError::Device)
    }

    fn check(&self, idx: usize, len: usize) -> Result<(), StorageError> {
        if len != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        if idx >= self.num_blocks() {
            return Err(StorageError::OutOfRange);
        }
        Ok(())
    }
}

impl<H: VirtioHal, R: NvmeRegs> BlockDevice for Nvme<H, R> {
    fn num_blocks(&self) -> usize {
        (self.namespace_lbas / self.lbas_per_block) as usize
    }

    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.check(idx, buf.len())?;
        // Sentinel so a controller that completes without moving data is caught by the caller's
        // content checks rather than handing back the previous block.
        // SAFETY: the data frame is ours, identity-mapped, one command in flight.
        unsafe { core::ptr::write_bytes(self.data as *mut u8, 0xA5, BLOCK_SIZE) };
        self.io(NVM_READ, idx)?;
        let src = unsafe { core::slice::from_raw_parts(self.data as *const u8, BLOCK_SIZE) };
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        if !self.scratch {
            return Err(StorageError::Device); // never write a controller not marked scratch
        }
        self.check(idx, buf.len())?;
        // SAFETY: as above.
        let dst = unsafe { core::slice::from_raw_parts_mut(self.data as *mut u8, BLOCK_SIZE) };
        dst.copy_from_slice(buf);
        self.io(NVM_WRITE, idx)
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        if !self.scratch {
            return Err(StorageError::Device);
        }
        if !self.volatile_cache {
            // No volatile write cache: a completed write is already non-volatile (§5.15.2.1 VWC).
            return Ok(());
        }
        self.submit(&self.io, NVM_FLUSH, NSID, 0, [0; 3])
            .map(|_| ())
            .map_err(|_| StorageError::Device)
    }
}

/// The invariant suite for a real NVMe namespace. `expect_blocks` is the geometry the VM gate
/// attached. Its write group (which reformats) runs ONLY on a scratch controller. Returns invariants proved.
pub fn device_suite<H: VirtioHal, R: NvmeRegs, F: FnMut(usize, bool, &str)>(
    dev: &mut Nvme<H, R>,
    expect_blocks: usize,
    log: &mut F,
) -> Result<usize, (usize, &'static str)> {
    let mut n = 0usize;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let ok = $cond;
            log(n, ok, $name);
            if !ok {
                return Err((n, $name));
            }
        }};
    }
    check!(
        "nvme: controller enabled, namespace 1 identified, I/O queue pair created",
        dev.num_blocks() > 0
    );
    check!(
        "nvme: the DMA gate denies an unregistered buffer address (queues and data registered)",
        dev.dma_gate_refuses_unregistered()
    );
    let refused = dev.submit(&dev.io, NVM_READ, NSID, 0x7fff_0000_0000, [0; 3])
        == Err(NvmeError::Unregistered);
    check!(
        "nvme: a command carrying an unregistered PRP is refused before the doorbell",
        refused
    );
    let last = dev.num_blocks() - 1;
    let mut over = [0u8; BLOCK_SIZE];
    check!(
        "nvme: a block past the namespace end is refused without touching the device",
        dev.read_block(last + 1, &mut over) == Err(StorageError::OutOfRange)
    );
    // More commands than queue entries, READS only so any disk can carry it: the tail, head and
    // phase must wrap in step and every read return the same bytes.
    let mut first = [0u8; BLOCK_SIZE];
    let mut wrapped = dev.read_block(0, &mut first).is_ok();
    for _ in 0..(3 * dev.depth as usize) {
        let mut back = [0u8; BLOCK_SIZE];
        wrapped &= dev.read_block(0, &mut back).is_ok() && back == first;
    }
    check!(
        "nvme: queue tail, head and phase wrap in step over three full queue lengths",
        wrapped
    );
    // Everything below WRITES. Only a controller marked scratch carries it (ADR-228); on any other
    // machine the read-only group above is the whole suite, and each VM gate pins which count.
    if !dev.is_scratch() {
        return Ok(n);
    }
    check!(
        "nvme: namespace capacity matches the attached image geometry",
        dev.num_blocks() == expect_blocks
    );
    let mut ok = true;
    for (i, blk) in [crate::storage::DATA_START + 5, last]
        .into_iter()
        .enumerate()
    {
        let pattern: [u8; BLOCK_SIZE] =
            core::array::from_fn(|j| (j as u8) ^ (0x5a + i as u8) ^ (blk as u8));
        let mut back = [0u8; BLOCK_SIZE];
        ok &= dev.write_block(blk, &pattern).is_ok()
            && dev.flush().is_ok()
            && dev.read_block(blk, &mut back).is_ok()
            && back == pattern;
    }
    check!(
        "nvme: write -> flush -> read-back round-trip (inner and LAST block) returns the bytes",
        ok
    );
    let h1 = crate::storage::DATA_START + 10;
    let h2 = crate::storage::DATA_START + 11;
    let (a, b) = ([0xA1u8; BLOCK_SIZE], [0xB2u8; BLOCK_SIZE]);
    let committed = Journal::new()
        .commit(&mut *dev, &[(h1, a), (h2, b)])
        .is_ok();
    let mut recovered = Journal::new();
    let replayed = recovered.recover(&mut *dev) == Ok(true);
    check!(
        "nvme: journal commit + fresh recover reproduce state over the namespace",
        committed
            && replayed
            && recovered.read(&*dev, h1) == Ok(a)
            && recovered.read(&*dev, h2) == Ok(b)
    );
    let fs_base = n;
    match crate::fs::selftest_on(&mut *dev, |i, passed, name| log(fs_base + i, passed, name)) {
        Ok(count) => n += count,
        Err((i, name)) => return Err((fs_base + i, name)),
    }
    Ok(n)
}

/// Format the init report as the one boot log line every target prints.
pub fn log_report(r: &NvmeReport, bus: u8, dev: u8, func: u8, out: &mut dyn FnMut(&str)) {
    use core::fmt::Write;
    let model = core::str::from_utf8(&r.model).unwrap_or("?").trim_end();
    let mut s = alloc::string::String::new();
    let _ = write!(
        s,
        "[nvme] controller @ PCI {:02x}:{:02x}.{} v{}.{} \"{}\" depth {} stride {} ns {} lba {} B x {} vwc {}",
        bus,
        dev,
        func,
        r.version >> 16,
        (r.version >> 8) & 0xFF,
        model,
        r.queue_depth,
        r.doorbell_stride,
        r.namespaces,
        r.lba_bytes,
        r.namespace_lbas,
        r.volatile_cache
    );
    out(&s);
}
