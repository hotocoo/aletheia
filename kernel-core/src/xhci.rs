//! xHCI USB host controller driver (REQ-DRV-012, ADR-253): the first USB driver, so a machine whose
//! only keyboard is on USB has one. First rung: bring the controller up, reset a connected root
//! port, enable a slot, address the device, read its descriptors, and configure the interrupt-IN
//! endpoint of a HID keyboard in boot protocol. Everything is POLLED against a time budget, as the
//! NVMe and AHCI drivers are (ADR-223, ADR-225).
//!
//! Register offsets, bit positions, TRB layouts and context layouts follow the eXtensible Host
//! Controller Interface specification, revision 1.2 (section named per constant); USB requests and
//! descriptors follow USB 2.0 chapter 9 and the HID 1.11 boot protocol (appendix B). The VM gates
//! drive QEMU's `qemu-xhci` with a `usb-kbd` attached.
//!
//! The driver is generic over [`NvmeRegs`] (32/64-bit register access to a BAR; the same seam the
//! NVMe driver uses) and [`VirtioHal`] (frames, barrier, clock), so the hosted tests drive the
//! IDENTICAL code against a simulated controller that can be told to misbehave.
//!
//! Fail-closed rules: a controller whose registers fall outside the mapped window, that never
//! leaves reset, does not run 4 KiB pages, asks for more scratchpad pages than this driver
//! provides, or reports a host-system or controller error is refused at init. Every command
//! completion must name the command TRB it answers and carry Success; every transfer event must
//! name a TRB of the transfer in flight and the slot it belongs to; a descriptor that does not
//! parse is an error, never a guess. Every address handed to the controller is a registered DMA
//! region (REQ-DRV-006).

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_bytes, write_volatile};

use crate::dma::DmaRegistry;
use crate::keymap::Keys;
use crate::nvme::NvmeRegs;
use crate::vinput::{KeyDecoder, RawEvent, EV_KEY};
use crate::virtioblk::VirtioHal;

/// PCI class code of an xHCI controller: serial bus (0Ch), USB (03h), xHCI programming interface
/// (30h).
pub const CLASS_XHCI: u32 = 0x0C_03_30;
/// Bytes of BAR0 the caller maps. Every register the driver touches must lie inside it; QEMU's
/// controller is exactly this size, and real controllers place the operational, runtime and
/// doorbell registers in the first 16 KiB.
pub const MAP_LEN: usize = 0x4000;

// --- Capability registers (§5.3) ----------------------------------------------------------------
/// CAPLENGTH (bits 7:0) and HCIVERSION (bits 31:16).
pub const CAP_CAPLENGTH: usize = 0x00;
/// HCSPARAMS1: MaxSlots 7:0, MaxIntrs 18:8, MaxPorts 31:24.
pub const CAP_HCSPARAMS1: usize = 0x04;
/// HCSPARAMS2: Max Scratchpad Buffers Hi 25:21, Lo 31:27.
pub const CAP_HCSPARAMS2: usize = 0x08;
/// HCCPARAMS1: AC64 bit 0, CSZ bit 2.
pub const CAP_HCCPARAMS1: usize = 0x10;
/// Doorbell array offset (bits 31:2).
pub const CAP_DBOFF: usize = 0x14;
/// Runtime register space offset (bits 31:5).
pub const CAP_RTSOFF: usize = 0x18;

// --- Operational registers (§5.4), from CAPLENGTH -----------------------------------------------
pub const OP_USBCMD: usize = 0x00;
pub const OP_USBSTS: usize = 0x04;
pub const OP_PAGESIZE: usize = 0x08;
pub const OP_CRCR: usize = 0x18;
pub const OP_DCBAAP: usize = 0x30;
pub const OP_CONFIG: usize = 0x38;
/// PORTSC of port 1; port `n` at `OP_PORTSC + 0x10 * (n - 1)` (§5.4.8).
pub const OP_PORTSC: usize = 0x400;

const USBCMD_RUN: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_HCH: u32 = 1 << 0;
const USBSTS_HSE: u32 = 1 << 2;
const USBSTS_CNR: u32 = 1 << 11;
const USBSTS_HCE: u32 = 1 << 12;

pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;
pub const PORTSC_PR: u32 = 1 << 4;
pub const PORTSC_PP: u32 = 1 << 9;
pub const PORTSC_PRC: u32 = 1 << 21;
/// The RW1C change bits CSC..CEC (17..23): written back as ones to clear them.
pub const PORTSC_CHANGES: u32 = 0x7F << 17;
const PORTSC_SPEED_SHIFT: u32 = 10;

// --- Runtime registers (§5.5), from RTSOFF: interrupter 0 -----------------------------------------
pub const RT_IR0: usize = 0x20;
pub const IR_IMAN: usize = 0x00;
pub const IR_ERSTSZ: usize = 0x08;
pub const IR_ERSTBA: usize = 0x10;
pub const IR_ERDP: usize = 0x18;
const ERDP_EHB: u64 = 1 << 3;

// --- TRBs (§6.4) ---------------------------------------------------------------------------------
const TRB_BYTES: usize = 16;
/// TRBs in a command or transfer ring frame; the last slot is reserved for the Link TRB.
const RING_TRBS: usize = crate::dma::PAGE / TRB_BYTES;
/// TRBs in the event ring segment (the ERST entry sits in the same frame after it).
pub const EVENT_TRBS: usize = 128;
const ERST_OFF: usize = EVENT_TRBS * TRB_BYTES;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TC: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;

pub const TRB_NORMAL: u32 = 1;
pub const TRB_SETUP: u32 = 2;
pub const TRB_DATA: u32 = 3;
pub const TRB_STATUS: u32 = 4;
pub const TRB_LINK: u32 = 6;
pub const TRB_ENABLE_SLOT: u32 = 9;
pub const TRB_ADDRESS_DEVICE: u32 = 11;
pub const TRB_CONFIGURE_EP: u32 = 12;
pub const TRB_EVALUATE_CTX: u32 = 13;
pub const TRB_TRANSFER_EVENT: u32 = 32;
pub const TRB_COMMAND_COMPLETION: u32 = 33;
pub const TRB_PORT_STATUS_CHANGE: u32 = 34;

/// Completion codes (§6.4.5).
pub const CC_SUCCESS: u8 = 1;
pub const CC_STALL: u8 = 6;
pub const CC_SHORT_PACKET: u8 = 13;

// --- Contexts (§6.2) -----------------------------------------------------------------------------
const SLOT_STATE_ADDRESSED: u32 = 2;
const EP_STATE_RUNNING: u32 = 1;
const EP_TYPE_CONTROL: u32 = 4;
const EP_TYPE_INTERRUPT_IN: u32 = 7;

/// Scratchpad pages this driver will provide. A controller asking for more is refused by name.
pub const MAX_SCRATCHPADS: usize = 64;
/// Device slots this driver enables (CONFIG.MaxSlotsEn); a first rung binds one device.
const SLOTS_WANT: u32 = 8;
/// Bytes of the per-device buffer that carry control-transfer data; the report follows.
const CTRL_BUF: usize = 2048;
const REPORT_OFF: usize = CTRL_BUF;
/// A HID boot keyboard report: modifiers, reserved, six usages.
pub const REPORT_BYTES: usize = 8;

/// Time budget for one command or control transfer, and for each reset/run wait.
pub const BUDGET_NS: u64 = 2_000_000_000;

/// Why the controller or a device was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XhciError {
    /// No completion, or the controller did not change state, within the budget.
    Timeout,
    /// Capability registers that cannot describe a working controller.
    Capabilities,
    /// A register the capabilities place lies outside the mapped window.
    Window,
    /// The controller does not run 4 KiB pages.
    PageSize,
    /// More scratchpad pages requested than [`MAX_SCRATCHPADS`].
    Scratchpads,
    /// Host System Error or Host Controller Error after run.
    ControllerError,
    /// No frame, or a frame the DMA gate refused.
    Memory,
    /// A structure above 4 GiB on a controller without 64-bit addressing.
    Addressing,
    /// The port has nothing connected, or did not come out of reset enabled.
    Port,
    /// A command completed with this completion code.
    Command(u8),
    /// A transfer completed with this completion code.
    Transfer(u8),
    /// An event named a TRB, slot or type other than the one being waited for.
    Unexpected,
    /// A descriptor that does not parse, or is not what was asked for.
    Descriptor,
    /// The device has no HID boot keyboard interface with an interrupt-IN endpoint.
    NotKeyboard,
    /// The output context does not show the state the command promised.
    State,
}

/// What init observed, for the caller's log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XhciReport {
    pub version: u16,
    pub max_slots: u8,
    pub max_ports: u8,
    pub context_bytes: usize,
    pub scratchpads: usize,
}

/// One raw TRB as read from the event ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3F
    }
    pub fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }
    pub fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
}

/// A producer ring (command or transfer): one frame, the last TRB its Link back to the start.
#[derive(Clone, Copy, Debug)]
struct Ring {
    base: usize,
    idx: usize,
    cycle: bool,
}

impl Ring {
    fn new(base: usize) -> Self {
        Ring {
            base,
            idx: 0,
            cycle: true,
        }
    }

    /// Write one TRB at the enqueue point and return its address. The cycle bit is written last,
    /// after a barrier, so the controller never sees a half-written TRB as its own.
    unsafe fn push<H: VirtioHal>(&mut self, param: u64, status: u32, control: u32) -> u64 {
        if self.idx == RING_TRBS - 1 {
            let link = self.base + self.idx * TRB_BYTES;
            write_volatile(link as *mut u64, self.base as u64);
            write_volatile((link + 8) as *mut u32, 0);
            H::barrier();
            write_volatile(
                (link + 12) as *mut u32,
                TRB_LINK << 10 | TRB_TC | self.cycle as u32,
            );
            self.cycle = !self.cycle;
            self.idx = 0;
        }
        let t = self.base + self.idx * TRB_BYTES;
        write_volatile(t as *mut u64, param);
        write_volatile((t + 8) as *mut u32, status);
        H::barrier();
        write_volatile((t + 12) as *mut u32, control | self.cycle as u32);
        H::barrier();
        self.idx += 1;
        t as u64
    }

    /// The dequeue pointer to give the controller for this ring: base plus the cycle state.
    fn dequeue(&self) -> u64 {
        self.base as u64 | self.cycle as u64
    }
}

/// One addressed USB device: its slot, where it hangs, and the frames that belong to it.
#[derive(Debug)]
pub struct UsbDevice {
    pub slot: u8,
    pub port: u8,
    pub speed: u8,
    pub max_packet0: u16,
    ictx: usize,
    octx: usize,
    ep0: Ring,
    intr: Ring,
    buf: usize,
}

/// What a configuration descriptor says about its HID boot keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKeyboardDesc {
    pub config_value: u8,
    pub interface: u8,
    pub endpoint: u8,
    pub max_packet: u16,
    pub interval: u8,
}

/// A configured boot keyboard: the device, its endpoint's context index, and the TRB in flight.
#[derive(Debug)]
pub struct Keyboard {
    pub dev: UsbDevice,
    pub desc: BootKeyboardDesc,
    pub dci: u8,
    pending: u64,
}

/// A live xHCI controller.
pub struct Xhci<H: VirtioHal, R: NvmeRegs> {
    regs: R,
    op: usize,
    rt: usize,
    db: usize,
    csz: usize,
    ac64: bool,
    max_ports: u8,
    slots_en: u8,
    dcbaa: usize,
    cmd: Ring,
    evt: usize,
    evt_idx: usize,
    evt_cycle: bool,
    dma: DmaRegistry,
    budget_ns: u64,
    port_events: u64,
    _hal: PhantomData<H>,
}

/// One zeroed frame, registered with the gate under `owner`.
fn frame<H: VirtioHal>(dma: &mut DmaRegistry, owner: &'static str) -> Result<usize, XhciError> {
    let f = H::alloc_frame().ok_or(XhciError::Memory)?;
    // SAFETY: a frame the HAL handed us exclusively, identity-mapped.
    unsafe { write_bytes(f as *mut u8, 0, crate::dma::PAGE) };
    dma.register(f, crate::dma::PAGE, owner)
        .map_err(|_| XhciError::Memory)?;
    Ok(f)
}

impl<H: VirtioHal, R: NvmeRegs> Xhci<H, R> {
    /// Bring the controller from whatever state firmware left it in to running, with a command
    /// ring, one event ring segment, the device context base array and any scratchpad pages.
    /// `window` is how many bytes of BAR0 the caller mapped.
    ///
    /// # Safety
    /// `regs` must reach an xHCI controller's BAR0, mapped for `window` bytes, that this kernel
    /// owns; `H::alloc_frame` must return identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(regs: R, window: usize) -> Result<(Self, XhciReport), XhciError> {
        let cap0 = regs.r32(CAP_CAPLENGTH);
        let caplen = (cap0 & 0xFF) as usize;
        let version = (cap0 >> 16) as u16;
        let hcs1 = regs.r32(CAP_HCSPARAMS1);
        let hcs2 = regs.r32(CAP_HCSPARAMS2);
        let hcc1 = regs.r32(CAP_HCCPARAMS1);
        let db = (regs.r32(CAP_DBOFF) & !0x3) as usize;
        let rt = (regs.r32(CAP_RTSOFF) & !0x1F) as usize;
        let max_slots = (hcs1 & 0xFF) as u8;
        let max_intrs = (hcs1 >> 8) & 0x7FF;
        let max_ports = (hcs1 >> 24) as u8;
        if caplen < 0x20 || max_slots == 0 || max_intrs == 0 || max_ports == 0 {
            return Err(XhciError::Capabilities);
        }
        let op = caplen;
        if op + OP_PORTSC + 0x10 * max_ports as usize > window
            || rt + RT_IR0 + 0x20 > window
            || db + 4 * (max_slots as usize + 1) > window
        {
            return Err(XhciError::Window);
        }
        let scratchpads = ((((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F)) as usize;
        if scratchpads > MAX_SCRATCHPADS {
            return Err(XhciError::Scratchpads);
        }
        let csz = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        let ac64 = hcc1 & 1 != 0;
        let slots_en = core::cmp::min(max_slots as u32, SLOTS_WANT) as u8;

        let mut x = Xhci {
            regs,
            op,
            rt,
            db,
            csz,
            ac64,
            max_ports,
            slots_en,
            dcbaa: 0,
            cmd: Ring::new(0),
            evt: 0,
            evt_idx: 0,
            evt_cycle: true,
            dma: DmaRegistry::new(),
            budget_ns: BUDGET_NS,
            port_events: 0,
            _hal: PhantomData,
        };

        // Stop, then reset (§4.2): a controller firmware left running must halt before HCRST.
        x.opw(OP_USBCMD, x.opr(OP_USBCMD) & !USBCMD_RUN);
        x.wait_op(OP_USBSTS, USBSTS_HCH, USBSTS_HCH)?;
        x.opw(OP_USBCMD, USBCMD_HCRST);
        x.wait_op(OP_USBCMD, USBCMD_HCRST, 0)?;
        x.wait_op(OP_USBSTS, USBSTS_CNR, 0)?;
        if x.opr(OP_PAGESIZE) & 1 == 0 {
            return Err(XhciError::PageSize);
        }

        // The DCBAA and the scratchpad array share one frame: 256 slots of 8 bytes, then up to
        // 256 scratchpad pointers.
        x.dcbaa = frame::<H>(&mut x.dma, "xhci.dcbaa")?;
        if scratchpads > 0 {
            let array = x.dcbaa + 2048;
            for i in 0..scratchpads {
                let page = frame::<H>(&mut x.dma, "xhci.scratchpad")?;
                x.check_addr(page)?;
                write_volatile((array + 8 * i) as *mut u64, page as u64);
            }
            write_volatile(x.dcbaa as *mut u64, array as u64);
        }
        x.cmd = Ring::new(frame::<H>(&mut x.dma, "xhci.cmd")?);
        x.evt = frame::<H>(&mut x.dma, "xhci.event")?;
        for a in [x.dcbaa, x.cmd.base, x.evt] {
            x.check_addr(a)?;
        }

        let config = x.opr(OP_CONFIG) & !0xFF;
        x.opw(OP_CONFIG, config | slots_en as u32);
        x.regs.w64(x.op + OP_DCBAAP, x.dcbaa as u64);
        x.regs.w64(x.op + OP_CRCR, x.cmd.dequeue());

        // Event ring: one segment, its ERST entry after it in the same frame (§4.9.4, §6.5).
        let erst = x.evt + ERST_OFF;
        write_volatile(erst as *mut u64, x.evt as u64);
        write_volatile((erst + 8) as *mut u32, EVENT_TRBS as u32);
        H::barrier();
        let ir = x.rt + RT_IR0;
        x.regs.w32(ir + IR_ERSTSZ, 1);
        x.regs.w64(ir + IR_ERDP, x.evt as u64);
        x.regs.w64(ir + IR_ERSTBA, erst as u64);
        // Polled: clear any pending interrupt and leave the interrupter disabled.
        x.regs.w32(ir + IR_IMAN, 1);

        x.opw(OP_USBCMD, USBCMD_RUN);
        x.wait_op(OP_USBSTS, USBSTS_HCH, 0)?;
        if x.opr(OP_USBSTS) & (USBSTS_HSE | USBSTS_HCE) != 0 {
            return Err(XhciError::ControllerError);
        }
        let report = XhciReport {
            version,
            max_slots,
            max_ports,
            context_bytes: csz,
            scratchpads,
        };
        Ok((x, report))
    }

    fn opr(&self, off: usize) -> u32 {
        self.regs.r32(self.op + off)
    }
    fn opw(&self, off: usize, v: u32) {
        self.regs.w32(self.op + off, v)
    }

    /// Wait until `reg & mask == want`, bounded on the clock.
    fn wait_op(&self, reg: usize, mask: u32, want: u32) -> Result<(), XhciError> {
        let started = H::now_ns();
        while self.opr(reg) & mask != want {
            if H::now_ns().wrapping_sub(started) > self.budget_ns {
                return Err(XhciError::Timeout);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn check_addr(&self, a: usize) -> Result<(), XhciError> {
        if !self.ac64 && (a as u64) >> 32 != 0 {
            return Err(XhciError::Addressing);
        }
        Ok(())
    }

    /// Tighten (or restore) the completion budget for subsequent waits, in nanoseconds.
    pub fn set_budget_ns(&mut self, ns: u64) {
        self.budget_ns = ns;
    }

    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }

    /// PORTSC of root port `port` (1-based).
    pub fn portsc(&self, port: u8) -> u32 {
        self.opr(OP_PORTSC + 0x10 * (port as usize - 1))
    }

    /// Port Status Change events seen (consumed while waiting for something else).
    pub fn port_events(&self) -> u64 {
        self.port_events
    }

    /// Whether an address nobody registered is refused, so a suite can prove the gate is live.
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        !self.dma.visible(0x7fff_0000_0000, 64) && !self.dma.visible(self.dcbaa, 2 * 4096)
    }

    /// DMA regions this controller holds.
    pub fn dma_regions(&self) -> usize {
        self.dma.live_regions()
    }

    /// The live grants, for an IOMMU suite to program (ADR-075).
    pub fn dma_grants(&self) -> Vec<crate::dma::Grant> {
        self.dma.grants()
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        self.regs.w32(self.db + 4 * slot as usize, target);
    }

    /// The next event, if the controller has posted one. Advances the dequeue pointer and tells
    /// the controller (ERDP, clearing Event Handler Busy).
    pub fn poll_event(&mut self) -> Option<Trb> {
        let t = self.evt + self.evt_idx * TRB_BYTES;
        // SAFETY: `t` lies in the event ring frame this controller owns.
        unsafe {
            let control = read_volatile((t + 12) as *const u32);
            if (control & TRB_CYCLE != 0) != self.evt_cycle {
                return None;
            }
            H::barrier();
            let ev = Trb {
                param: read_volatile(t as *const u64),
                status: read_volatile((t + 8) as *const u32),
                control,
            };
            self.evt_idx += 1;
            if self.evt_idx == EVENT_TRBS {
                self.evt_idx = 0;
                self.evt_cycle = !self.evt_cycle;
            }
            let erdp = (self.evt + self.evt_idx * TRB_BYTES) as u64;
            self.regs.w64(self.rt + RT_IR0 + IR_ERDP, erdp | ERDP_EHB);
            Some(ev)
        }
    }

    /// The next event of `kind`, consuming Port Status Change events on the way. Any other event
    /// is refused: nothing else should be in flight.
    fn wait_event(&mut self, kind: u32) -> Result<Trb, XhciError> {
        let started = H::now_ns();
        loop {
            match self.poll_event() {
                Some(ev) if ev.kind() == TRB_PORT_STATUS_CHANGE => self.port_events += 1,
                Some(ev) if ev.kind() == kind => return Ok(ev),
                Some(_) => return Err(XhciError::Unexpected),
                None => {
                    if H::now_ns().wrapping_sub(started) > self.budget_ns {
                        return Err(XhciError::Timeout);
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Issue one command and wait for the completion that names it.
    unsafe fn command(&mut self, param: u64, control: u32) -> Result<Trb, XhciError> {
        let at = self.cmd.push::<H>(param, 0, control);
        self.ring_doorbell(0, 0);
        let ev = self.wait_event(TRB_COMMAND_COMPLETION)?;
        if ev.param != at {
            return Err(XhciError::Unexpected);
        }
        if ev.code() != CC_SUCCESS {
            return Err(XhciError::Command(ev.code()));
        }
        Ok(ev)
    }

    /// Context `i` of a context frame (input contexts count the Input Control Context as 0).
    fn ctx(&self, base: usize, i: usize) -> *mut u32 {
        (base + i * self.csz) as *mut u32
    }

    /// Reset root port `port`, enable a slot and address the device on it (§4.3).
    ///
    /// # Safety
    /// The controller must be running (after [`Self::init`]).
    pub unsafe fn address(&mut self, port: u8) -> Result<UsbDevice, XhciError> {
        if port == 0 || port > self.max_ports {
            return Err(XhciError::Port);
        }
        let reg = OP_PORTSC + 0x10 * (port as usize - 1);
        if self.opr(reg) & PORTSC_CCS == 0 {
            return Err(XhciError::Port);
        }
        // Port reset (§4.3.1). PED is RW1C and must never be written as one, so the write carries
        // only Port Power and Port Reset.
        self.opw(reg, PORTSC_PP | PORTSC_PR);
        self.wait_op(reg, PORTSC_PRC, PORTSC_PRC)?;
        let sc = self.opr(reg);
        self.opw(reg, PORTSC_PP | (sc & PORTSC_CHANGES));
        if sc & PORTSC_PED == 0 {
            return Err(XhciError::Port);
        }
        let speed = ((sc >> PORTSC_SPEED_SHIFT) & 0xF) as u8;
        let max_packet0 = match speed {
            1 | 2 => 8,
            3 => 64,
            4 | 5 => 512,
            _ => return Err(XhciError::Port),
        };

        let ev = self.command(0, TRB_ENABLE_SLOT << 10)?;
        let slot = ev.slot();
        if slot == 0 || slot > self.slots_en {
            return Err(XhciError::Unexpected);
        }
        let mut dev = UsbDevice {
            slot,
            port,
            speed,
            max_packet0,
            ictx: frame::<H>(&mut self.dma, "xhci.input")?,
            octx: frame::<H>(&mut self.dma, "xhci.device")?,
            ep0: Ring::new(frame::<H>(&mut self.dma, "xhci.ep0")?),
            intr: Ring::new(frame::<H>(&mut self.dma, "xhci.intr")?),
            buf: frame::<H>(&mut self.dma, "xhci.buf")?,
        };
        for a in [dev.ictx, dev.octx, dev.ep0.base, dev.intr.base, dev.buf] {
            self.check_addr(a)?;
        }
        write_volatile(
            (self.dcbaa + 8 * slot as usize) as *mut u64,
            dev.octx as u64,
        );

        // Input context: add the slot (A0) and EP0 (A1) contexts (§4.3.3).
        let icc = self.ctx(dev.ictx, 0);
        write_volatile(icc, 0);
        write_volatile(icc.add(1), 0b11);
        let sc0 = self.ctx(dev.ictx, 1);
        write_volatile(sc0, (speed as u32) << 20 | 1 << 27);
        write_volatile(sc0.add(1), (port as u32) << 16);
        self.write_ep0(&dev);
        H::barrier();
        self.command(
            dev.ictx as u64,
            TRB_ADDRESS_DEVICE << 10 | (slot as u32) << 24,
        )?;
        let out = read_volatile(self.ctx(dev.octx, 0).add(3));
        if out >> 27 != SLOT_STATE_ADDRESSED || out & 0xFF == 0 {
            return Err(XhciError::State);
        }

        // A full/low-speed device's real EP0 packet size is in byte 7 of its device descriptor;
        // tell the controller if it differs from the 8 assumed (§4.6.7, Evaluate Context).
        let got = self.control(&mut dev, 0x80, 6, 0x0100, 0, 8)?;
        if got != 8 {
            return Err(XhciError::Descriptor);
        }
        let mps = read_volatile((dev.buf + 7) as *const u8) as u16;
        if speed <= 2 && mps != dev.max_packet0 {
            if !matches!(mps, 8 | 16 | 32 | 64) {
                return Err(XhciError::Descriptor);
            }
            dev.max_packet0 = mps;
            write_volatile(icc, 0);
            write_volatile(icc.add(1), 0b10);
            self.write_ep0(&dev);
            H::barrier();
            self.command(
                dev.ictx as u64,
                TRB_EVALUATE_CTX << 10 | (slot as u32) << 24,
            )?;
        }
        Ok(dev)
    }

    /// EP0's context in the input context: control, three retries, its packet size, its ring.
    unsafe fn write_ep0(&self, dev: &UsbDevice) {
        let ep = self.ctx(dev.ictx, 2);
        write_volatile(ep, 0);
        write_volatile(
            ep.add(1),
            3 << 1 | EP_TYPE_CONTROL << 3 | (dev.max_packet0 as u32) << 16,
        );
        write_volatile(ep.add(2) as *mut u64, dev.ep0.dequeue());
        write_volatile(ep.add(4), 8);
    }

    /// The next transfer event for `slot`; any event for another slot is refused.
    fn transfer_event(&mut self, slot: u8) -> Result<Trb, XhciError> {
        let ev = self.wait_event(TRB_TRANSFER_EVENT)?;
        if ev.slot() != slot {
            return Err(XhciError::Unexpected);
        }
        Ok(ev)
    }

    /// One control transfer on EP0 (USB 2.0 §9.3; TRBs §6.4.1.2). `len` bytes of data move
    /// through the device buffer, IN or OUT as `req_type` says. Returns the bytes the data stage
    /// moved; a short IN is reported, never padded.
    ///
    /// # Safety
    /// `dev` must be a device this controller addressed.
    pub unsafe fn control(
        &mut self,
        dev: &mut UsbDevice,
        req_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: u16,
    ) -> Result<usize, XhciError> {
        if len as usize > CTRL_BUF || !self.dma.visible(dev.buf, CTRL_BUF) {
            return Err(XhciError::Memory);
        }
        let dir_in = req_type & 0x80 != 0;
        let setup = req_type as u64
            | (request as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (len as u64) << 48;
        let trt = match (len, dir_in) {
            (0, _) => 0,
            (_, true) => 3,
            (_, false) => 2,
        };
        let s = dev
            .ep0
            .push::<H>(setup, 8, TRB_SETUP << 10 | TRB_IDT | trt << 16);
        let d = (len > 0).then(|| {
            let dir = if dir_in { TRB_DIR_IN } else { 0 };
            dev.ep0.push::<H>(
                dev.buf as u64,
                len as u32,
                TRB_DATA << 10 | dir | TRB_IOC | TRB_ISP,
            )
        });
        let st_dir = if len == 0 || !dir_in { TRB_DIR_IN } else { 0 };
        let st = dev.ep0.push::<H>(0, 0, TRB_STATUS << 10 | st_dir | TRB_IOC);
        self.ring_doorbell(dev.slot, 1);

        let ours = |p: u64| p == s || Some(p) == d || p == st;
        let mut moved = 0usize;
        if let Some(d) = d {
            let ev = self.transfer_event(dev.slot)?;
            if !ours(ev.param) {
                return Err(XhciError::Unexpected);
            }
            let residual = (ev.status & 0xFF_FFFF) as usize;
            match ev.code() {
                CC_SUCCESS | CC_SHORT_PACKET if ev.param == d && residual <= len as usize => {
                    moved = len as usize - residual
                }
                CC_SUCCESS | CC_SHORT_PACKET => return Err(XhciError::Unexpected),
                c => return Err(XhciError::Transfer(c)),
            }
        }
        let ev = self.transfer_event(dev.slot)?;
        if !ours(ev.param) {
            return Err(XhciError::Unexpected);
        }
        match ev.code() {
            CC_SUCCESS if ev.param == st => Ok(moved),
            CC_SUCCESS => Err(XhciError::Unexpected),
            c => Err(XhciError::Transfer(c)),
        }
    }

    /// GET_DESCRIPTOR into the device buffer; returns the bytes read.
    unsafe fn descriptor(
        &mut self,
        dev: &mut UsbDevice,
        kind: u8,
        len: u16,
    ) -> Result<&[u8], XhciError> {
        let got = self.control(dev, 0x80, 6, (kind as u16) << 8, 0, len)?;
        Ok(core::slice::from_raw_parts(dev.buf as *const u8, got))
    }

    /// Read the device and configuration descriptors and find a HID boot keyboard.
    ///
    /// # Safety
    /// `dev` must be a device this controller addressed.
    pub unsafe fn find_boot_keyboard(
        &mut self,
        dev: &mut UsbDevice,
    ) -> Result<BootKeyboardDesc, XhciError> {
        let d = self.descriptor(dev, 1, 18)?;
        if d.len() != 18 || d[0] != 18 || d[1] != 1 {
            return Err(XhciError::Descriptor);
        }
        let head = self.descriptor(dev, 2, 9)?;
        if head.len() != 9 || head[1] != 2 {
            return Err(XhciError::Descriptor);
        }
        let total = u16::from_le_bytes([head[2], head[3]]);
        if (total as usize) < 9 || total as usize > CTRL_BUF {
            return Err(XhciError::Descriptor);
        }
        let all = self.descriptor(dev, 2, total)?;
        if all.len() != total as usize {
            return Err(XhciError::Descriptor);
        }
        parse_boot_keyboard(all)
    }

    /// SET_CONFIGURATION, SET_PROTOCOL(boot), then Configure Endpoint for the keyboard's
    /// interrupt-IN endpoint and post the first report transfer (§4.6.6, HID 1.11 §7.2.6).
    ///
    /// # Safety
    /// `dev` must be a device this controller addressed and `desc` what it described.
    pub unsafe fn configure_keyboard(
        &mut self,
        mut dev: UsbDevice,
        desc: BootKeyboardDesc,
    ) -> Result<Keyboard, XhciError> {
        self.control(&mut dev, 0x00, 9, desc.config_value as u16, 0, 0)?;
        self.control(&mut dev, 0x21, 0x0B, 0, desc.interface as u16, 0)?;

        let dci = (desc.endpoint & 0x0F) * 2 + 1;
        let icc = self.ctx(dev.ictx, 0);
        write_volatile(icc, 0);
        write_volatile(icc.add(1), 1 | 1 << dci);
        let sc0 = self.ctx(dev.ictx, 1);
        write_volatile(sc0, (dev.speed as u32) << 20 | (dci as u32) << 27);
        write_volatile(sc0.add(1), (dev.port as u32) << 16);
        let ep = self.ctx(dev.ictx, dci as usize + 1);
        write_volatile(ep, (xhci_interval(dev.speed, desc.interval) as u32) << 16);
        write_volatile(
            ep.add(1),
            3 << 1 | EP_TYPE_INTERRUPT_IN << 3 | (desc.max_packet as u32) << 16,
        );
        write_volatile(ep.add(2) as *mut u64, dev.intr.dequeue());
        write_volatile(ep.add(4), (desc.max_packet as u32) << 16 | 8);
        H::barrier();
        self.command(
            dev.ictx as u64,
            TRB_CONFIGURE_EP << 10 | (dev.slot as u32) << 24,
        )?;
        if read_volatile(self.ctx(dev.octx, dci as usize)) & 0x7 != EP_STATE_RUNNING {
            return Err(XhciError::State);
        }
        let mut kb = Keyboard {
            dev,
            desc,
            dci,
            pending: 0,
        };
        self.post_report(&mut kb);
        Ok(kb)
    }

    unsafe fn post_report(&mut self, kb: &mut Keyboard) {
        let buf = kb.dev.buf + REPORT_OFF;
        write_bytes(buf as *mut u8, 0, REPORT_BYTES);
        kb.pending = kb.dev.intr.push::<H>(
            buf as u64,
            REPORT_BYTES as u32,
            TRB_NORMAL << 10 | TRB_IOC | TRB_ISP,
        );
        self.ring_doorbell(kb.dev.slot, kb.dci as u32);
    }

    /// A report, if the keyboard sent one since the last call; never waits. A report that is not
    /// exactly eight bytes, or an event that is not the pending transfer's, is an error.
    pub fn poll_report(&mut self, kb: &mut Keyboard) -> Result<Option<[u8; 8]>, XhciError> {
        let ev = loop {
            match self.poll_event() {
                None => return Ok(None),
                Some(ev) if ev.kind() == TRB_PORT_STATUS_CHANGE => self.port_events += 1,
                Some(ev) => break ev,
            }
        };
        if ev.kind() != TRB_TRANSFER_EVENT || ev.slot() != kb.dev.slot || ev.param != kb.pending {
            return Err(XhciError::Unexpected);
        }
        match ev.code() {
            CC_SUCCESS | CC_SHORT_PACKET => {}
            c => return Err(XhciError::Transfer(c)),
        }
        if ev.status & 0xFF_FFFF != 0 {
            return Err(XhciError::Descriptor);
        }
        let mut r = [0u8; REPORT_BYTES];
        // SAFETY: the report area of the device buffer, which the controller just finished.
        unsafe {
            for (i, b) in r.iter_mut().enumerate() {
                *b = read_volatile((kb.dev.buf + REPORT_OFF + i) as *const u8);
            }
            self.post_report(kb);
        }
        Ok(Some(r))
    }

    /// Stop the controller and reset it, so nothing it was told about is touched after this
    /// (ADR-228: a driver leaves a machine quiet). Returns whether it halted within the budget.
    pub fn halt(&mut self) -> bool {
        self.opw(OP_USBCMD, self.opr(OP_USBCMD) & !USBCMD_RUN);
        let halted = self.wait_op(OP_USBSTS, USBSTS_HCH, USBSTS_HCH).is_ok();
        if halted {
            self.opw(OP_USBCMD, USBCMD_HCRST);
            let _ = self.wait_op(OP_USBCMD, USBCMD_HCRST, 0);
        }
        halted
    }

    /// Whether USBSTS says halted.
    pub fn is_halted(&self) -> bool {
        self.opr(OP_USBSTS) & USBSTS_HCH != 0
    }
}

impl<H: VirtioHal, R: NvmeRegs> Drop for Xhci<H, R> {
    fn drop(&mut self) {
        if !self.is_halted() {
            self.halt();
        }
    }
}

/// The xHCI Interval field for an interrupt endpoint (§6.2.3.6): periods of 2^Interval x 125 us.
/// Full and low speed give bInterval in frames (1 ms); high and super speed give 2^(bInterval-1)
/// microframes.
pub fn xhci_interval(speed: u8, b_interval: u8) -> u8 {
    if speed >= 3 {
        b_interval.clamp(1, 16) - 1
    } else {
        let micro = (b_interval.max(1) as u32) * 8;
        ((31 - micro.leading_zeros()) as u8).clamp(3, 10)
    }
}

/// Walk a whole configuration descriptor and find the first interface that is a HID boot keyboard
/// (class 3, subclass 1, protocol 1) with an interrupt-IN endpoint. A descriptor whose lengths do
/// not tile the buffer is refused rather than read past.
pub fn parse_boot_keyboard(cfg: &[u8]) -> Result<BootKeyboardDesc, XhciError> {
    if cfg.len() < 9 || cfg[1] != 2 {
        return Err(XhciError::Descriptor);
    }
    let config_value = cfg[5];
    let mut at = 0usize;
    let mut kbd_iface: Option<u8> = None;
    while at < cfg.len() {
        let len = cfg[at] as usize;
        if len < 2 || at + len > cfg.len() {
            return Err(XhciError::Descriptor);
        }
        let d = &cfg[at..at + len];
        match d[1] {
            4 if len >= 9 => {
                kbd_iface = (d[5] == 3 && d[6] == 1 && d[7] == 1).then_some(d[2]);
            }
            5 if len >= 7 => {
                if let Some(interface) = kbd_iface {
                    if d[2] & 0x80 != 0 && d[3] & 0x3 == 3 {
                        let max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7FF;
                        if max_packet == 0 {
                            return Err(XhciError::Descriptor);
                        }
                        return Ok(BootKeyboardDesc {
                            config_value,
                            interface,
                            endpoint: d[2],
                            max_packet,
                            interval: d[6],
                        });
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    Err(XhciError::NotKeyboard)
}

/// HID keyboard usage (HID Usage Tables §10) to the evdev key code the console's decoder speaks
/// (the same table Linux's `hid-input` uses). Zero means no key the console knows.
const HID_TO_EVDEV: [u8; 0x53] = [
    0, 0, 0, 0, // 0x00..0x03: none, ErrorRollOver, POSTFail, ErrorUndefined
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44, // a..z
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, // 1..9, 0
    28, 1, 14, 15, 57, 12, 13, 26, 27, 43, 43, 39, 40, 41, 51, 52, 53, 58, // Enter..CapsLock
    59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, // F1..F12
    99, 70, 119, 110, 102, 104, 111, 107, 109, 106, 105, 108, 103, // SysRq..Up
];
/// Modifier bits 0..7 of a boot report: LCtrl, LShift, LAlt, LGUI, RCtrl, RShift, RAlt, RGUI.
const MODIFIER_EVDEV: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

/// Usage to evdev key code, or `None` for a usage the console has no key for.
pub fn usage_to_evdev(usage: u8) -> Option<u16> {
    match usage {
        0x58 => Some(96), // Keypad Enter
        u => HID_TO_EVDEV
            .get(u as usize)
            .copied()
            .filter(|&c| c != 0)
            .map(u16::from),
    }
}

/// Boot reports in, console bytes out: each report is compared with the last, and every key or
/// modifier that went down or up becomes one key event for the same decoder the virtio keyboard
/// uses (ADR-080), so a USB keyboard types exactly what any other keyboard types.
#[derive(Clone, Debug, Default)]
pub struct BootKeyboard {
    prev: [u8; REPORT_BYTES],
    dec: KeyDecoder,
    rollover: u64,
}

impl BootKeyboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports refused as phantom states (ErrorRollOver and its siblings in every key slot).
    pub fn rollover_refusals(&self) -> u64 {
        self.rollover
    }

    fn key(&mut self, code: u16, down: bool, out: &mut Vec<u8>) {
        let k: Keys = self.dec.feed(RawEvent {
            ty: EV_KEY,
            code,
            value: down as u32,
        });
        out.extend_from_slice(k.as_slice());
    }

    /// Decode one report, appending the bytes it types to `out`. Order within a report: modifier
    /// presses, key releases, modifier releases, key presses, so a shift that went down with a
    /// letter shifts it and one that came up with a letter does not.
    pub fn feed(&mut self, report: &[u8; REPORT_BYTES], out: &mut Vec<u8>) {
        if report[2..].iter().any(|&u| (1..=3).contains(&u)) {
            self.rollover += 1;
            return;
        }
        let prev = self.prev;
        let (was, now) = (prev[0], report[0]);
        for (bit, &code) in MODIFIER_EVDEV.iter().enumerate() {
            if now & !was & (1 << bit) != 0 {
                self.key(code, true, out);
            }
        }
        for &u in prev[2..].iter().filter(|&&u| u >= 4) {
            if !report[2..].contains(&u) {
                if let Some(code) = usage_to_evdev(u) {
                    self.key(code, false, out);
                }
            }
        }
        for (bit, &code) in MODIFIER_EVDEV.iter().enumerate() {
            if was & !now & (1 << bit) != 0 {
                self.key(code, false, out);
            }
        }
        for &u in report[2..].iter().filter(|&&u| u >= 4) {
            if !prev[2..].contains(&u) {
                if let Some(code) = usage_to_evdev(u) {
                    self.key(code, true, out);
                }
            }
        }
        self.prev = *report;
    }
}

/// The boot invariant suite over a real controller (REQ-DRV-012): bring-up, the DMA gate, one
/// keyboard enumerated and configured, an idle keyboard types nothing, and the controller halts
/// clean. `Ok(0)` when no USB keyboard is connected: a controller with nothing on it is a named
/// skip after bring-up, not a failure (ADR-228). Returns the number of invariants proved, or
/// `(index, name)` of the first failure.
///
/// # Safety
/// As [`Xhci::init`].
pub unsafe fn device_suite<H: VirtioHal, R: NvmeRegs, F: FnMut(usize, bool, &str)>(
    regs: R,
    window: usize,
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
    let init = Xhci::<H, R>::init(regs, window);
    check!(
        "xhci: controller halted, reset and running (4 KiB pages, registers inside the window)",
        init.is_ok()
    );
    let Ok((mut x, _report)) = init else {
        unreachable!()
    };
    check!(
        "xhci: DMA gate refuses an address no structure registered",
        x.dma_gate_refuses_unregistered()
    );
    // Every connected port is tried in turn; a device that is not a boot keyboard (a mouse, a hub,
    // a disk) is passed over, not failed: this suite runs on machines that are not gates
    // (ADR-228). A device that will not address or describe itself is a failure.
    let mut outcome: Result<Option<(UsbDevice, BootKeyboardDesc)>, u8> = Ok(None);
    let connected: Vec<u8> = (1..=x.max_ports())
        .filter(|&p| x.portsc(p) & PORTSC_CCS != 0)
        .collect();
    for port in connected {
        let Ok(mut dev) = x.address(port) else {
            outcome = Err(3);
            break;
        };
        match x.find_boot_keyboard(&mut dev) {
            Ok(desc) => {
                outcome = Ok(Some((dev, desc)));
                break;
            }
            Err(XhciError::NotKeyboard) => {}
            Err(_) => {
                outcome = Err(4);
                break;
            }
        }
    }
    if matches!(outcome, Ok(None)) {
        return Ok(0);
    }
    check!(
        "xhci: a connected root port resets enabled, a slot is enabled and the device addressed",
        !matches!(outcome, Err(3))
    );
    check!(
        "xhci: device and configuration descriptors read whole; a HID boot keyboard found",
        outcome.is_ok()
    );
    let Ok(Some((dev, desc))) = outcome else {
        unreachable!()
    };
    let kb = x.configure_keyboard(dev, desc);
    check!(
        "xhci: configured in boot protocol, interrupt-IN endpoint Running, report transfer posted",
        kb.is_ok()
    );
    let Ok(mut kb) = kb else { unreachable!() };
    // An idle keyboard: whatever arrives in 20 ms must type nothing.
    let mut typed = Vec::new();
    let mut decoder = BootKeyboard::new();
    let mut clean = true;
    let started = H::now_ns();
    while H::now_ns().wrapping_sub(started) < 20_000_000 {
        match x.poll_report(&mut kb) {
            Ok(Some(r)) => decoder.feed(&r, &mut typed),
            Ok(None) => core::hint::spin_loop(),
            Err(_) => clean = false,
        }
    }
    check!(
        "xhci: an idle keyboard types nothing",
        clean && typed.is_empty()
    );
    check!(
        "xhci: the controller halts and resets, leaving nothing in flight",
        x.halt() && x.is_halted()
    );
    Ok(n)
}
