//! AHCI (SATA) driver (REQ-DRV-011, ADR-225): the disk controller class almost every PC and every
//! mainstream hypervisor ships. One command slot per port, one PRD entry, polled, DMA-gated.
//!
//! Register offsets and structures are from the Serial ATA AHCI 1.3.1 specification (section
//! named per constant) and were checked against QEMU's `hw/ide/ahci-internal.h`; ATA commands
//! and IDENTIFY words are from ATA/ATAPI-8 ACS.
//!
//! Safety rule for a machine whose boot disk sits on the same controller: this driver WRITES
//! only to a disk whose IDENTIFY serial number is exactly [`SCRATCH_SERIAL`]. Every other disk is
//! read, never written: [`AhciDisk::write_block`] refuses on a disk not opened as scratch.

use core::cell::Cell;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use crate::devirq::{judge_run, IrqProbe};
use crate::dma::DmaRegistry;
use crate::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE};
use crate::virtioblk::{CompletionWait, VirtioHal};

// HBA registers (§3.1).
pub const HBA_CAP: usize = 0x00;
pub const HBA_GHC: usize = 0x04;
pub const HBA_IS: usize = 0x08;
pub const HBA_PI: usize = 0x0C;
pub const HBA_VS: usize = 0x10;
const GHC_AE: u32 = 1 << 31;
/// GHC.IE (§3.1.2): the HBA may raise its interrupt at all.
const GHC_IE: u32 = 1 << 1;
// Port registers (§3.3), at 0x100 + port * 0x80.
pub const PORT_BASE: usize = 0x100;
pub const PORT_STRIDE: usize = 0x80;
pub const PX_CLB: usize = 0x00;
pub const PX_CLBU: usize = 0x04;
pub const PX_FB: usize = 0x08;
pub const PX_FBU: usize = 0x0C;
pub const PX_IS: usize = 0x10;
pub const PX_IE: usize = 0x14;
pub const PX_CMD: usize = 0x18;
pub const PX_TFD: usize = 0x20;
pub const PX_SIG: usize = 0x24;
pub const PX_SSTS: usize = 0x28;
pub const PX_SERR: usize = 0x30;
pub const PX_CI: usize = 0x38;
const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
const IS_TFES: u32 = 1 << 30;
/// PxIS/PxIE bit 0 (§3.3.5): a D2H Register FIS arrived, how a non-NCQ command completes.
const IS_DHRS: u32 = 1 << 0;
/// The fatal port conditions (§3.3.5): host bus fatal, host bus data, interface fatal, task file
/// error. They end a command, so they interrupt too and are never swallowed.
const IS_FATAL: u32 = IS_TFES | (1 << 29) | (1 << 28) | (1 << 27);
/// What this driver asks a port to interrupt for: a command's completion or a fatal condition.
const IE_COMPLETION: u32 = IS_DHRS | IS_FATAL;
const SSTS_DET_PRESENT: u32 = 3; // device present, phy communication established
/// PxSIG of an ATA disk (§3.3.9; QEMU `SATA_SIGNATURE_DISK`).
pub const SIG_ATA: u32 = 0x0000_0101;

// FIS and commands.
const FIS_H2D: u8 = 0x27;
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xEA;

/// The serial number that marks a disk this driver may write. Nothing else is ever written.
pub const SCRATCH_SERIAL: &[u8] = b"ALETHEIA-SCRATCH";
const BUDGET_NS: u64 = 10_000_000_000;
/// Zero-budget reads the wedge invariant may try before one is left with the HBA (ADR-266).
const WEDGE_TRIES: usize = 64;

pub trait Regs {
    fn r32(&self, off: usize) -> u32;
    fn w32(&self, off: usize, v: u32);
}

impl<T: Regs + ?Sized> Regs for &T {
    fn r32(&self, off: usize) -> u32 {
        (**self).r32(off)
    }
    fn w32(&self, off: usize, v: u32) {
        (**self).w32(off, v)
    }
}

/// ABAR (BAR5) through an identity-mapped device window.
#[derive(Clone, Copy)]
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
    /// # Safety
    /// `base` must be the mapped ABAR of an AHCI controller, covering every implemented port.
    pub unsafe fn new(base: usize) -> Self {
        MmioRegs { base }
    }
}

impl Regs for MmioRegs {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `new` requires a mapped ABAR; offsets stay below 0x1100.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

/// Enable AHCI mode and list the implemented ports with an established link. Whether the device
/// is an ATA disk is only known after [`AhciDisk::open`] starts FIS receive: PxSIG holds the first
/// D2H FIS's signature (§3.3.9) and reads all-ones before one arrived.
pub fn disk_ports<R: Regs>(regs: &R) -> alloc::vec::Vec<u8> {
    regs.w32(HBA_GHC, regs.r32(HBA_GHC) | GHC_AE);
    let pi = regs.r32(HBA_PI);
    (0..32u8)
        .filter(|&p| pi & (1 << p) != 0)
        .filter(|&p| {
            let b = PORT_BASE + p as usize * PORT_STRIDE;
            regs.r32(b + PX_SSTS) & 0xF == SSTS_DET_PRESENT
        })
        .collect()
}

/// [`AhciDisk::open`]'s refusal for a port whose device is not an ATA disk (ATAPI, port
/// multiplier): callers skip such a port rather than fail the controller.
pub const NOT_ATA: &str = "ahci: the port's device is not an ATA disk";

fn wait<H: VirtioHal>(mut done: impl FnMut() -> bool) -> bool {
    let started = H::now_ns();
    while !done() {
        if H::now_ns().wrapping_sub(started) > BUDGET_NS {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// One ATA disk behind one AHCI port.
pub struct AhciDisk<H: VirtioHal, R: Regs> {
    regs: R,
    port: usize,
    list: usize,
    table: usize,
    data: usize,
    sector: usize,
    sectors: u64,
    scratch: bool,
    serial: [u8; 20],
    model: [u8; 40],
    dma: DmaRegistry,
    issued: Cell<u64>,
    /// PxIE and GHC.IE are programmed for this port (ADR-264): completions may interrupt.
    irq_armed: Cell<bool>,
    /// How a command waits for its completion: `None` spins, `Some(wait)` hands each look to the
    /// platform, which sleeps until the HBA's interrupt (ADR-264).
    wait: Option<CompletionWait>,
    /// Commands completed through the platform's wait, and by spinning.
    slept: Cell<u64>,
    polled: Cell<u64>,
    /// Diagnostic for the lost-wakeup proof (see `Nvme::race_read_block`).
    stale_look: Cell<bool>,
    /// A command was issued and never properly finished: the HBA may still own slot 0, its table
    /// and the data frame, which the next command would rewrite under it (ADR-266).
    wedged: Cell<bool>,
    /// How long one command may take, [`BUDGET_NS`] unless a proof tightens it.
    budget_ns: Cell<u64>,
    _hal: PhantomData<H>,
}

/// ATA strings are byte-swapped within each 16-bit word (ACS-2 §7.12.7.1).
fn ata_string(src: &[u8], out: &mut [u8]) {
    for (i, pair) in src.chunks(2).enumerate() {
        out[2 * i] = pair[1];
        out[2 * i + 1] = pair[0];
    }
}

impl<H: VirtioHal, R: Regs> AhciDisk<H, R> {
    /// Stop the port, publish its command list, FIS area and table, start it, IDENTIFY the disk.
    ///
    /// # Safety
    /// `regs` must reach the controller's ABAR, `port` must be a port [`disk_ports`] listed, and
    /// `H::alloc_frame` must return zeroed identity-mapped frames this kernel owns exclusively.
    pub unsafe fn open(regs: R, port: u8) -> Result<Self, &'static str> {
        let pb = PORT_BASE + port as usize * PORT_STRIDE;
        // §10.1.2: the port must be idle before CLB/FB change.
        let cmd = regs.r32(pb + PX_CMD);
        if cmd & CMD_ST != 0 {
            regs.w32(pb + PX_CMD, cmd & !CMD_ST);
        }
        if !wait::<H>(|| regs.r32(pb + PX_CMD) & CMD_CR == 0) {
            return Err("ahci: the port's command engine did not stop");
        }
        let cmd = regs.r32(pb + PX_CMD);
        if cmd & CMD_FRE != 0 {
            regs.w32(pb + PX_CMD, cmd & !CMD_FRE);
        }
        if !wait::<H>(|| regs.r32(pb + PX_CMD) & CMD_FR == 0) {
            return Err("ahci: the port's FIS receive engine did not stop");
        }

        let mut dma = DmaRegistry::new();
        let mut frame = |owner| -> Result<usize, &'static str> {
            let f = H::alloc_frame().ok_or("ahci: frame allocator exhausted")?;
            dma.register(f, crate::dma::PAGE, owner)
                .map_err(|_| "ahci: a frame was refused as a DMA region")?;
            Ok(f)
        };
        let list = frame("ahci.cmd-list+fis")?; // command list at +0 (1 KiB), received FIS at +0x400
        let table = frame("ahci.cmd-table")?;
        let data = frame("ahci.data")?;

        regs.w32(pb + PX_CLB, list as u32);
        regs.w32(pb + PX_CLBU, (list as u64 >> 32) as u32);
        regs.w32(pb + PX_FB, (list + 0x400) as u32);
        regs.w32(pb + PX_FBU, ((list + 0x400) as u64 >> 32) as u32);
        regs.w32(pb + PX_SERR, u32::MAX);
        regs.w32(pb + PX_IE, 0);
        regs.w32(pb + PX_IS, u32::MAX);
        regs.w32(pb + PX_CMD, regs.r32(pb + PX_CMD) | CMD_FRE);
        if !wait::<H>(|| regs.r32(pb + PX_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
            return Err("ahci: the disk stayed busy");
        }
        // The signature arrives with the device's first D2H FIS, now that FRE is on.
        // ponytail: the port's three frames stay allocated on this refusal (a few per boot, one per
        // non-disk port); return them to the allocator if ports ever come and go at run time.
        if !wait::<H>(|| regs.r32(pb + PX_SIG) != u32::MAX) || regs.r32(pb + PX_SIG) != SIG_ATA {
            regs.w32(pb + PX_CMD, regs.r32(pb + PX_CMD) & !CMD_FRE);
            return Err(NOT_ATA);
        }
        regs.w32(pb + PX_CMD, regs.r32(pb + PX_CMD) | CMD_ST);

        let mut d = AhciDisk {
            regs,
            port: pb,
            list,
            table,
            data,
            sector: 512,
            sectors: 0,
            scratch: false,
            serial: [0; 20],
            model: [0; 40],
            dma,
            issued: Cell::new(0),
            irq_armed: Cell::new(false),
            wait: None,
            slept: Cell::new(0),
            polled: Cell::new(0),
            stale_look: Cell::new(false),
            wedged: Cell::new(false),
            budget_ns: Cell::new(BUDGET_NS),
            _hal: PhantomData,
        };
        d.command(ATA_IDENTIFY, 0, 0, 512, false)
            .map_err(|_| "ahci: IDENTIFY DEVICE failed")?;
        let id = core::slice::from_raw_parts(data as *const u8, 512);
        let word = |w: usize| u16::from_le_bytes([id[2 * w], id[2 * w + 1]]);
        if word(83) & (1 << 10) == 0 {
            return Err("ahci: disk does not support 48-bit LBA - fail closed");
        }
        let sectors = (0..4).fold(0u64, |a, i| a | (word(100 + i) as u64) << (16 * i));
        let w106 = word(106);
        let sector = if w106 & 0xC000 == 0x4000 && w106 & (1 << 12) != 0 {
            2 * (word(117) as usize | (word(118) as usize) << 16)
        } else {
            512
        };
        if sector != 512 && sector != 4096 {
            return Err("ahci: logical sector size is neither 512 B nor 4 KiB - fail closed");
        }
        if sectors == 0 {
            return Err("ahci: disk reports zero sectors");
        }
        ata_string(&id[20..40], &mut d.serial);
        ata_string(&id[54..94], &mut d.model);
        d.sector = sector;
        d.sectors = sectors;
        let s = &d.serial;
        let end = s
            .iter()
            .rposition(|&c| c != b' ' && c != 0)
            .map_or(0, |e| e + 1);
        let start = s.iter().position(|&c| c != b' ').unwrap_or(end);
        d.scratch = &s[start..end] == SCRATCH_SERIAL;
        Ok(d)
    }

    /// Issue one command in slot 0 with at most one PRD entry over the data frame; poll to completion.
    unsafe fn command(
        &self,
        op: u8,
        lba: u64,
        count: u16,
        bytes: usize,
        write: bool,
    ) -> Result<(), StorageError> {
        if self.wedged.get() {
            return Err(StorageError::Device);
        }
        if bytes > 0 && !self.dma.visible(self.data, bytes) {
            return Err(StorageError::Device);
        }
        let t = self.table;
        core::ptr::write_bytes(t as *mut u8, 0, 0x90);
        let fis = t as *mut u8;
        write_volatile(fis, FIS_H2D);
        write_volatile(fis.add(1), 0x80); // C: this FIS carries a command
        write_volatile(fis.add(2), op);
        for i in 0..3 {
            write_volatile(fis.add(4 + i), (lba >> (8 * i)) as u8);
            write_volatile(fis.add(8 + i), (lba >> (8 * (i + 3))) as u8);
        }
        write_volatile(fis.add(7), 0x40); // device: LBA mode
        write_volatile(fis.add(12), count as u8);
        write_volatile(fis.add(13), (count >> 8) as u8);
        let prdtl = if bytes > 0 {
            write_volatile((t + 0x80) as *mut u64, self.data as u64);
            write_volatile((t + 0x8C) as *mut u32, (bytes - 1) as u32);
            1u32
        } else {
            0
        };
        // Command header 0 (§4.2.2): CFL = 5 dwords, W, PRDTL; PRDBC cleared; CTBA.
        let h = self.list as *mut u32;
        write_volatile(h, 5 | (write as u32) << 6 | prdtl << 16);
        write_volatile(h.add(1), 0);
        write_volatile(h.add(2), t as u32);
        write_volatile(h.add(3), (t as u64 >> 32) as u32);
        H::barrier();
        let pb = self.port;
        let waited = self.wait.is_some();
        if waited {
            // Interrupt mode: completion status is the interrupt's own, cleared only by the
            // acknowledgement below, so a stale bit is a missed acknowledgement, not something to
            // wipe before the next command hides it: this command is refused, naming it. The
            // refusal then acknowledges what it saw (PxIS, then the HBA's IS bit, as below) so one
            // late or extra D2H FIS fails one command and does not wedge the disk for good.
            // Bits that cannot interrupt (PIO setup, PRD done...) are not ours to keep.
            let stale = self.regs.r32(pb + PX_IS);
            if stale & IE_COMPLETION != 0 {
                self.regs.w32(pb + PX_IS, stale);
                self.regs.w32(HBA_IS, 1 << self.port_index());
                return Err(StorageError::Device);
            }
            self.regs.w32(pb + PX_IS, stale);
        } else {
            self.regs.w32(pb + PX_IS, u32::MAX);
        }
        self.regs.w32(pb + PX_CI, 1);
        self.issued.set(self.issued.get() + 1);
        let mut failed = false;
        let budget = self.budget_ns.get();
        let started = H::now_ns();
        // Polled: the command slot clears (or the task file fails). Interrupt mode: the status the
        // interrupt reports (a completion or a fatal condition) is what the wait is woken for.
        let posted = || {
            let is = self.regs.r32(pb + PX_IS);
            if waited {
                is & IE_COMPLETION != 0
            } else {
                is & IS_TFES != 0 || self.regs.r32(pb + PX_CI) & 1 == 0
            }
        };
        // Whether this command really entered the platform's wait: that call, not the
        // configuration (`waited`), credits it as completed through the wait (`slept`). The wait is
        // entered once BEFORE the first look; its own check (interrupts off) finds a completion that
        // is already there and returns without sleeping, so an early completion is still proved to
        // have gone through the platform and a driver that spins instead cannot be credited.
        let mut entered = false;
        let done = loop {
            if let Some(wait) = self.wait {
                entered = true;
                wait(&|| {
                    if self.stale_look.replace(false) {
                        // The lost-wakeup window: the completion lands between the wait's check
                        // and its sleep; report the stale "not ready" the check would have seen.
                        while !posted() && H::now_ns().wrapping_sub(started) <= budget {
                            core::hint::spin_loop();
                        }
                        return false;
                    }
                    posted()
                });
            }
            if posted() {
                break true;
            }
            if H::now_ns().wrapping_sub(started) > budget {
                break false;
            }
            if self.wait.is_none() {
                core::hint::spin_loop();
            }
        };
        let status = self.regs.r32(pb + PX_IS);
        if waited {
            // The acknowledgement, in the order the specification gives (§10.1.2): PxIS first, then
            // the HBA's IS bit for this port. Both are write-1-to-clear. With IS left set, a
            // message-signalled HBA raises nothing for the next completion.
            self.regs.w32(pb + PX_IS, status);
            self.regs.w32(HBA_IS, 1 << self.port_index());
        }
        failed |= status & IS_FATAL != 0;
        let counter = if entered { &self.slept } else { &self.polled };
        counter.set(counter.get() + 1);
        H::barrier();
        let tfd = self.regs.r32(pb + PX_TFD);
        // A slot the HBA has not released is still its: the next command would rewrite the table
        // and the data frame under it, and could take this one's late completion for its own.
        if !done || self.regs.r32(pb + PX_CI) & 1 != 0 {
            self.wedged.set(true);
            return Err(StorageError::Device);
        }
        if failed || tfd & (TFD_ERR | TFD_BSY) != 0 {
            return Err(StorageError::Device);
        }
        // Bytes the HBA reports moving must be exactly what the PRD asked for.
        if bytes > 0 && read_volatile(h.add(1)) as usize != bytes {
            return Err(StorageError::Device);
        }
        Ok(())
    }

    pub fn is_scratch(&self) -> bool {
        self.scratch
    }
    pub fn serial(&self) -> &[u8; 20] {
        &self.serial
    }
    pub fn model(&self) -> &[u8; 40] {
        &self.model
    }
    pub fn sector_bytes(&self) -> usize {
        self.sector
    }
    pub fn sectors(&self) -> u64 {
        self.sectors
    }
    /// Commands sent to the disk so far.
    pub fn issued(&self) -> u64 {
        self.issued.get()
    }

    /// Bring a wedged (or healthy) port back (ADR-266; AHCI 1.3.1 §10.4.2): stop the command
    /// engine, which clears PxCI and ends what the slot held (§3.3.14), wait for PxCMD.CR = 0,
    /// clear the port's error and status bits (and the HBA's IS bit when the port interrupts), and
    /// start the engine again once the disk is idle. The frames stay where they are; nothing is
    /// allocated. A disk still busy after its engine stopped needs a COMRESET, which this driver
    /// does not issue: that is refused by name and the port stays wedged.
    pub fn reset(&self) -> Result<(), &'static str> {
        let pb = self.port;
        self.wedged.set(true);
        self.regs
            .w32(pb + PX_CMD, self.regs.r32(pb + PX_CMD) & !CMD_ST);
        if !wait::<H>(|| self.regs.r32(pb + PX_CMD) & CMD_CR == 0) {
            return Err("ahci: the port's command engine did not stop");
        }
        self.regs.w32(pb + PX_SERR, u32::MAX);
        self.regs.w32(pb + PX_IS, u32::MAX);
        if self.irq_armed.get() {
            self.regs.w32(HBA_IS, 1 << self.port_index());
        }
        if !wait::<H>(|| self.regs.r32(pb + PX_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
            return Err("ahci: the disk stayed busy after its port stopped (it needs a COMRESET)");
        }
        self.regs
            .w32(pb + PX_CMD, self.regs.r32(pb + PX_CMD) | CMD_ST);
        self.wedged.set(false);
        Ok(())
    }

    /// Whether every command is refused until [`Self::reset`].
    pub fn is_wedged(&self) -> bool {
        self.wedged.get()
    }

    /// Tighten (or restore, with `None`) the per-command completion budget.
    pub fn set_completion_budget_ns(&self, ns: Option<u64>) {
        self.budget_ns.set(ns.unwrap_or(BUDGET_NS));
    }

    /// This port's number on the HBA (the bit of its IS register).
    fn port_index(&self) -> usize {
        (self.port - PORT_BASE) / PORT_STRIDE
    }

    /// Let this port's completions interrupt (ADR-264): clear whatever status polling left (PxIS,
    /// then the HBA's IS bit, write-1-to-clear), unmask the completion and fatal conditions in
    /// PxIE, then set GHC.IE. Needs the HBA's message path (MSI) programmed by the caller before
    /// the first command waits on it. Idempotent.
    pub fn enable_interrupts(&self) {
        let pb = self.port;
        self.regs.w32(pb + PX_IS, u32::MAX);
        self.regs.w32(HBA_IS, 1 << self.port_index());
        self.regs.w32(pb + PX_IE, IE_COMPLETION);
        self.regs.w32(HBA_GHC, self.regs.r32(HBA_GHC) | GHC_IE);
        self.irq_armed.set(true);
    }

    /// Wait for completions through `wait` (sleeping until the HBA's interrupt) instead of
    /// spinning, or spin again with `None`. Refuses by name a port whose interrupts were never
    /// enabled: a wait there could only ever end on the clock.
    pub fn set_completion_wait(
        &mut self,
        wait: Option<CompletionWait>,
    ) -> Result<(), &'static str> {
        if wait.is_some() && !self.irq_armed.get() {
            return Err(
                "ahci: the port's interrupts are not enabled - a wait could never be woken",
            );
        }
        self.wait = wait;
        Ok(())
    }

    /// Commands completed through the platform's wait, and by spinning.
    pub fn completion_counts(&self) -> (u64, u64) {
        (self.slept.get(), self.polled.get())
    }

    /// Whether this port's completions go through a platform wait.
    pub fn waits_on_platform(&self) -> bool {
        self.wait.is_some()
    }

    /// `(HBA IS bit for this port, PxIS)` as the hardware holds them: both zero once a completion
    /// has been acknowledged.
    pub fn interrupt_status(&self) -> (u32, u32) {
        let is = self.regs.r32(HBA_IS) & (1 << self.port_index());
        (is, self.regs.r32(self.port + PX_IS))
    }

    /// Read sector 0 with a deliberately stale first look at the port's status: the wait's check
    /// finds the completion already landed (its interrupt latched) and still reports "not ready".
    pub fn race_read_first_sector(&self, out: &mut [u8; 512]) -> Result<(), StorageError> {
        self.stale_look.set(true);
        let r = self.read_first_sector(out);
        self.stale_look.set(false);
        r
    }

    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        !self.dma.visible(0x7fff_0000_0000, 64)
            && !self.dma.visible(self.data, crate::dma::PAGE * 2)
            && self.dma.live_regions() == 3
    }

    /// Read the disk's first 512 bytes (sector 0, or the head of the first 4 KiB sector).
    pub fn read_first_sector(&self, out: &mut [u8; 512]) -> Result<(), StorageError> {
        // SAFETY: data frame is ours; one command in flight.
        unsafe {
            self.command(ATA_READ_DMA_EXT, 0, 1, self.sector, false)?;
            out.copy_from_slice(core::slice::from_raw_parts(self.data as *const u8, 512));
        }
        Ok(())
    }

    fn per_block(&self) -> u64 {
        (BLOCK_SIZE / self.sector) as u64
    }

    fn check(&self, idx: usize, len: usize) -> Result<u64, StorageError> {
        if len != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        if idx >= self.num_blocks() {
            return Err(StorageError::OutOfRange);
        }
        Ok(idx as u64 * self.per_block())
    }
}

/// A dropped disk's port stops: command engine, then FIS receive, so no unsolicited FIS is DMA
/// nobody owns (ADR-228, the class of the e1000 fault in ADR-227).
impl<H: VirtioHal, R: Regs> Drop for AhciDisk<H, R> {
    fn drop(&mut self) {
        let pb = self.port;
        self.regs.w32(pb + PX_IE, 0); // no interrupt for a port nobody waits on
        self.regs
            .w32(pb + PX_CMD, self.regs.r32(pb + PX_CMD) & !CMD_ST);
        let _ = wait::<H>(|| self.regs.r32(pb + PX_CMD) & CMD_CR == 0);
        self.regs
            .w32(pb + PX_CMD, self.regs.r32(pb + PX_CMD) & !CMD_FRE);
    }
}

impl<H: VirtioHal, R: Regs> BlockDevice for AhciDisk<H, R> {
    fn num_blocks(&self) -> usize {
        (self.sectors / self.per_block()) as usize
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        let lba = self.check(idx, buf.len())?;
        // SAFETY: as above.
        unsafe {
            self.command(
                ATA_READ_DMA_EXT,
                lba,
                self.per_block() as u16,
                BLOCK_SIZE,
                false,
            )?;
            buf.copy_from_slice(core::slice::from_raw_parts(
                self.data as *const u8,
                BLOCK_SIZE,
            ));
        }
        Ok(())
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        if !self.scratch {
            return Err(StorageError::Device); // never write a disk not marked scratch
        }
        let lba = self.check(idx, buf.len())?;
        // SAFETY: as above.
        unsafe {
            core::slice::from_raw_parts_mut(self.data as *mut u8, BLOCK_SIZE).copy_from_slice(buf);
            self.command(
                ATA_WRITE_DMA_EXT,
                lba,
                self.per_block() as u16,
                BLOCK_SIZE,
                true,
            )
        }
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        if !self.scratch {
            return Err(StorageError::Device);
        }
        // SAFETY: no data transfer.
        unsafe { self.command(ATA_FLUSH_EXT, 0, 0, 0, false) }
    }
}

/// The controller contract: every attached disk identified and read; the boot disk's first
/// sector carries the 0x55AA boot signature; the scratch disk carries the full storage suite.
pub fn device_suite<H: VirtioHal, R: Regs, F: FnMut(usize, bool, &str)>(
    disks: &mut [AhciDisk<H, R>],
    expect_scratch_blocks: usize,
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
    // Read-only group: holds on any machine with any SATA disk (ADR-228).
    check!("ahci: at least one ATA disk identified", !disks.is_empty());
    let mut sigs = 0;
    let mut reads = true;
    for d in disks.iter() {
        let mut s = [0u8; 512];
        reads &= d.read_first_sector(&mut s).is_ok();
        sigs += (s[510] == 0x55 && s[511] == 0xAA) as usize;
    }
    check!("ahci: every disk reads its first sector", reads);
    check!(
        "ahci: every disk's DMA gate denies an unregistered address (list, table and data registered)",
        disks.iter().all(|d| d.dma_gate_refuses_unregistered())
    );
    let mut over = [0u8; BLOCK_SIZE];
    check!(
        "ahci: a block past the end is refused without a command",
        disks.iter().all(|d| {
            let before = d.issued();
            d.read_block(d.num_blocks(), &mut over) == Err(StorageError::OutOfRange)
                && d.issued() == before
        })
    );
    let mut refused = true;
    for d in disks.iter_mut().filter(|d| !d.is_scratch()) {
        let before = d.issued();
        refused &= d.write_block(0, &[0u8; BLOCK_SIZE]).is_err() && d.issued() == before;
    }
    check!(
        "ahci: a write to any disk not marked scratch is refused without a command",
        refused
    );
    // ADR-266: a command left with the HBA wedges the port until a reset. A zero budget makes the
    // first look miss (the HBA moves the data after PxCI is written, not during the write); that
    // command then finishes late, in the slot, table and frame the next command would rewrite.
    let d = &disks[0];
    let mut first = [0u8; 512];
    let mut left = d.read_first_sector(&mut first).is_ok();
    d.set_completion_budget_ns(Some(0));
    let mut timed_out = false;
    for _ in 0..WEDGE_TRIES {
        let mut b = [0u8; 512];
        if d.read_first_sector(&mut b).is_err() {
            timed_out = d.is_wedged();
            break;
        }
    }
    d.set_completion_budget_ns(None);
    left &= timed_out;
    let before = d.issued();
    let mut b = [0u8; 512];
    let refused = d.read_first_sector(&mut b).is_err() && d.issued() == before;
    let back = d.reset().is_ok()
        && !d.is_wedged()
        && d.read_first_sector(&mut b).is_ok()
        && b == first
        && d.dma.live_regions() == 3;
    check!(
        "ahci: a command left with the HBA wedges the port: the next is refused before PxCI, and a port reset brings it back",
        left && refused && back
    );
    // Gate group: only where a disk carries the scratch serial. Elsewhere the read-only group is
    // the whole suite, and each VM gate pins which count it expects.
    let Some(dev) = disks.iter_mut().find(|d| d.is_scratch()) else {
        return Ok(n);
    };
    check!(
        "ahci: a disk written by another tool reads back its 0x55AA boot signature",
        sigs >= 1
    );
    check!(
        "ahci: scratch capacity matches the attached image geometry",
        dev.num_blocks() == expect_scratch_blocks
    );
    let last = dev.num_blocks() - 1;
    let mut ok = true;
    for blk in [crate::storage::DATA_START + 5, last] {
        let p: [u8; BLOCK_SIZE] = core::array::from_fn(|j| (j as u8) ^ (blk as u8) ^ 0x3c);
        let mut b = [0u8; BLOCK_SIZE];
        ok &= dev.write_block(blk, &p).is_ok()
            && dev.flush().is_ok()
            && dev.read_block(blk, &mut b).is_ok()
            && b == p;
    }
    check!(
        "ahci: write -> flush -> read-back (inner and LAST block) returns the bytes",
        ok
    );
    let (h1, h2) = (
        crate::storage::DATA_START + 10,
        crate::storage::DATA_START + 11,
    );
    let (a, b) = ([0xA1u8; BLOCK_SIZE], [0xB2u8; BLOCK_SIZE]);
    let committed = Journal::new()
        .commit(&mut *dev, &[(h1, a), (h2, b)])
        .is_ok();
    let mut rec = Journal::new();
    let replayed = rec.recover(&mut *dev) == Ok(true);
    check!(
        "ahci: journal commit + fresh recover reproduce state over the scratch disk",
        committed && replayed && rec.read(&*dev, h1) == Ok(a) && rec.read(&*dev, h2) == Ok(b)
    );
    let base = n;
    match crate::fs::selftest_on(&mut *dev, |i, p, name| log(base + i, p, name)) {
        Ok(c) => n += c,
        Err((i, name)) => return Err((base + i, name)),
    }
    Ok(n)
}

/// Reads of sector 0 per disk in the measured interrupt run: enough that a missing acknowledgement
/// or a lost interrupt shows as a count mismatch, few enough to be invisible in the boot budget.
const RUN_READS_PER_DISK: u64 = 8;
/// The quiet window after the run, in nanoseconds of the platform clock (see `nvme`).
const QUIET_WINDOW_NS: u64 = 5_000_000;

/// The interrupt group (ADR-264): proves every disk's completions wake the CPU, reads only, so it
/// holds on any machine's disks. `first` is the number already proved; the return is how many this
/// group proved. `probe` reads the platform's interrupt counters (taking any pending interrupt
/// first). Each check fails if the path is reverted: spinning shows in the driver's polled count,
/// an unarmed HBA as no interrupt, an unacknowledged IS as the NEXT completion raising none, and a
/// lost wakeup as a sleep only the bounding timer ended.
pub fn interrupt_suite<H: VirtioHal, R: Regs, F: FnMut(usize, bool, &str)>(
    disks: &[AhciDisk<H, R>],
    first: usize,
    probe: IrqProbe,
    log: &mut F,
) -> Result<usize, (usize, &'static str)> {
    use crate::devirq::RunFault::{ExtraInterrupts, LostInterrupt, NoInterrupt, SilentlyPolled};
    let mut n = first;
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
    macro_rules! verdict {
        ($name:expr, $res:expr, $pass:expr) => {{
            n += 1;
            let res: Result<(), crate::devirq::RunFault> = $res;
            let ok = res.map_or_else(|f| $pass(f), |_| true);
            match res {
                Err(f) if !ok => {
                    log(n, false, f.name());
                    return Err((n, f.name()));
                }
                _ => log(n, true, $name),
            }
        }};
    }
    // Total commands completed through the wait / by spinning, across every disk.
    let counts = || {
        disks.iter().fold((0u64, 0u64), |(s, p), d| {
            let (ds, dp) = d.completion_counts();
            (s + ds, p + dp)
        })
    };
    check!(
        "ahci: every disk's port interrupts are enabled and its commands wait through the platform",
        !disks.is_empty() && disks.iter().all(|d| d.waits_on_platform())
    );
    let commands = RUN_READS_PER_DISK * disks.len() as u64;
    let (slept0, polled0) = counts();
    let before = probe();
    let mut same = true;
    for d in disks {
        let mut base = [0u8; 512];
        same &= d.read_first_sector(&mut base).is_ok();
        for _ in 1..RUN_READS_PER_DISK {
            let mut back = [0u8; 512];
            same &= d.read_first_sector(&mut back).is_ok() && back == base;
        }
    }
    let after = probe();
    let (slept1, polled1) = counts();
    let run = judge_run(commands, slept1 - slept0, polled1 - polled0, before, after);
    verdict!(
        "ahci: every command of the run raised exactly one interrupt and none was polled (counted interrupts == completed commands)",
        run,
        |f| !matches!(f, SilentlyPolled | NoInterrupt | LostInterrupt | ExtraInterrupts)
    );
    check!("ahci: the run's sector data is intact on every disk", same);
    verdict!(
        "ahci: the CPU slept until the HBA answered, and no sleep needed the bounding timer",
        run,
        |_f| false
    );
    // Acknowledged: PxIS then IS were written 1-to-clear after each completion, so both read zero
    // now, and the NEXT completion still raises its interrupt (a stale IS would swallow it).
    let clear = disks.iter().all(|d| d.interrupt_status() == (0, 0));
    let (slept0, polled0) = counts();
    let before = probe();
    let mut next_ok = true;
    for d in disks {
        let mut s = [0u8; 512];
        next_ok &= d.read_first_sector(&mut s).is_ok();
    }
    let after = probe();
    let (slept1, polled1) = counts();
    check!(
        "ahci: PxIS then IS are acknowledged write-1-to-clear (both read clear) and the next completion still interrupts",
        clear
            && next_ok
            && judge_run(
                disks.len() as u64,
                slept1 - slept0,
                polled1 - polled0,
                before,
                after
            )
            .is_ok()
    );
    // The lost-wakeup window on the first disk.
    let (slept0, polled0) = counts();
    let before = probe();
    let mut raced = [0u8; 512];
    let landed = disks[0].race_read_first_sector(&mut raced).is_ok();
    let after = probe();
    let (slept1, polled1) = counts();
    check!(
        "ahci: a completion that races the wait is not lost (its interrupt wakes the sleep at once, not the bounding timer)",
        landed && judge_run(1, slept1 - slept0, polled1 - polled0, before, after).is_ok()
    );
    let quiet_from = probe();
    let t0 = H::now_ns();
    while H::now_ns().wrapping_sub(t0) < QUIET_WINDOW_NS {
        core::hint::spin_loop();
    }
    check!(
        "ahci: nothing is left pending once the status is acknowledged (a quiet window takes no interrupt)",
        probe().interrupts == quiet_from.interrupts && disks.iter().all(|d| d.interrupt_status() == (0, 0))
    );
    Ok(n - first)
}

/// What stands in for the interrupt group when the platform could not arm the HBA's interrupt
/// (ADR-264): ONE invariant, and it passes only if the fallback is real, NAMED and counted - every
/// disk completed its commands by polling, none through a wait, and the reason is not empty. A
/// machine that falls back silently, or claims a reason while waiting on a wake that cannot come,
/// fails here. Returns the invariants proved (1).
pub fn fallback_suite<H: VirtioHal, R: Regs, F: FnMut(usize, bool, &str)>(
    disks: &[AhciDisk<H, R>],
    first: usize,
    reason: &str,
    log: &mut F,
) -> Result<usize, (usize, &'static str)> {
    const NAME: &str =
        "ahci: completions are polled for a named, counted reason (no interrupt path armed)";
    let mut s = [0u8; 512];
    let reads = disks.iter().all(|d| d.read_first_sector(&mut s).is_ok());
    let polled_only = disks.iter().all(|d| {
        let (slept, polled) = d.completion_counts();
        slept == 0 && polled > 0 && !d.waits_on_platform()
    });
    let ok = !reason.is_empty() && reads && polled_only;
    log(first + 1, ok, NAME);
    if ok {
        Ok(1)
    } else {
        Err((
            first + 1,
            "ahci: fallback to polling without a named reason or with a wait armed",
        ))
    }
}
