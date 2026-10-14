//! Hosted proof of the xHCI driver (ADR-253) against a simulated controller with one USB keyboard
//! on a root port. The simulation follows the xHCI 1.2 ring, context and event protocol closely
//! enough to run the driver unchanged, and can be told to break it in named ways.

use kernel_core::nvme::NvmeRegs;
use kernel_core::virtioblk::VirtioHal;
use kernel_core::xhci::{
    device_suite, parse_boot_keyboard, usage_to_evdev, xhci_interval, BootKeyboard, Xhci,
    XhciError, CC_SHORT_PACKET, CC_STALL, CC_SUCCESS, MAP_LEN, TRB_ADDRESS_DEVICE,
    TRB_COMMAND_COMPLETION, TRB_CONFIGURE_EP, TRB_DATA, TRB_ENABLE_SLOT, TRB_EVALUATE_CTX,
    TRB_LINK, TRB_NORMAL, TRB_PORT_STATUS_CHANGE, TRB_SETUP, TRB_STATUS, TRB_TRANSFER_EVENT,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

const PAGE: usize = 4096;

struct HostHal;

thread_local! {
    static CLOCK: AtomicU64 = const { AtomicU64::new(0) };
}

impl VirtioHal for HostHal {
    fn now_ns() -> u64 {
        CLOCK.with(|c| c.fetch_add(1_000_000, Ordering::Relaxed))
    }
    fn alloc_frame() -> Option<usize> {
        let layout = std::alloc::Layout::from_size_align(PAGE, PAGE).unwrap();
        // SAFETY: non-zero layout; leaked on purpose (a device frame lives as long as the test).
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        (!p.is_null()).then_some(p as usize)
    }
    fn barrier() {}
}

// Register map of the simulation (QEMU's layout).
const CAPLEN: usize = 0x40;
const RTS: usize = 0x1000;
const DB: usize = 0x2000;
const PORTS: usize = 8;
const KBD_PORT: usize = 1;
const SLOT: u8 = 1;

/// Named ways the simulated controller or keyboard misbehaves.
#[derive(Clone, Default)]
struct Faults {
    never_leaves_reset: bool,
    page_8k: bool,
    scratchpads: u32,
    csz64: bool,
    no_device: bool,
    full_speed_mps0: Option<u8>,
    /// Command completions name the TRB after the command.
    cmd_ptr_lie: bool,
    /// This command type completes with this code.
    cmd_code: Option<(u32, u8)>,
    /// Transfer events name another slot.
    xfer_slot_lie: bool,
    /// The device descriptor is cut to ten bytes.
    short_device_desc: bool,
    /// The interface is a boot mouse, not a keyboard.
    mouse: bool,
    /// SET_PROTOCOL stalls.
    stall_set_protocol: bool,
    /// The output context never reaches Addressed.
    state_lie: bool,
}

#[derive(Default)]
struct Ring {
    deq: u64,
    ccs: bool,
}

impl Ring {
    /// The next TRB the producer has handed over, following Link TRBs.
    fn next(&mut self) -> Option<(u64, u64, u32, u32)> {
        loop {
            let t = self.deq as usize;
            let control = rd32(t + 12);
            if (control & 1 != 0) != self.ccs {
                return None;
            }
            let (param, status) = (rd64(t), rd32(t + 8));
            if (control >> 10) & 0x3F == TRB_LINK {
                self.deq = param;
                if control & 2 != 0 {
                    self.ccs = !self.ccs;
                }
                continue;
            }
            self.deq += 16;
            return Some((t as u64, param, status, control));
        }
    }
}

fn rd32(a: usize) -> u32 {
    unsafe { (a as *const u32).read_volatile() }
}
fn rd64(a: usize) -> u64 {
    unsafe { (a as *const u64).read_volatile() }
}
fn wr32(a: usize, v: u32) {
    unsafe { (a as *mut u32).write_volatile(v) }
}
fn wr64(a: usize, v: u64) {
    unsafe { (a as *mut u64).write_volatile(v) }
}

#[derive(Default)]
struct Sim {
    f: Faults,
    running: bool,
    in_reset: bool,
    config: u32,
    crcr_lo: u32,
    dcbaa: u64,
    portsc: [u32; PORTS],
    cmd: Ring,
    erstsz: u32,
    erstba: u64,
    seg: u64,
    seg_size: u32,
    enq: u32,
    pcs: bool,
    ep0: Ring,
    intr: Ring,
    intr_dci: u8,
    pending: Option<(u64, u64, u32)>,
    reports: VecDeque<[u8; 8]>,
    setup: Option<(u64, [u8; 8])>,
    data: Option<(u64, u64, u32)>,
    commands: Vec<u32>,
    config_value: u8,
    protocol: Option<u16>,
    mps0: u16,
}

impl Sim {
    fn new(f: Faults) -> Self {
        let speed = if f.full_speed_mps0.is_some() { 1 } else { 3 };
        let mut s = Sim::default();
        if !f.no_device {
            s.portsc[KBD_PORT - 1] = 1 | 1 << 9 | speed << 10;
        }
        s.f = f;
        s
    }

    fn csz(&self) -> usize {
        if self.f.csz64 {
            64
        } else {
            32
        }
    }

    fn event(&mut self, param: u64, status: u32, control: u32) {
        if !self.running || self.seg == 0 {
            return;
        }
        let t = (self.seg + 16 * self.enq as u64) as usize;
        wr64(t, param);
        wr32(t + 8, status);
        wr32(t + 12, control | self.pcs as u32);
        self.enq += 1;
        if self.enq == self.seg_size {
            self.enq = 0;
            self.pcs = !self.pcs;
        }
    }

    fn device_desc(&self) -> Vec<u8> {
        let mps0 = self.f.full_speed_mps0.unwrap_or(64);
        let mut d = vec![
            18, 1, 0x00, 0x02, 0, 0, 0, mps0, 0x27, 0x06, 0x01, 0x00, 0, 0, 1, 2, 3, 1,
        ];
        if self.f.short_device_desc {
            d.truncate(10);
        }
        d
    }

    fn config_desc(&self) -> Vec<u8> {
        let protocol = if self.f.mouse { 2 } else { 1 };
        let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, protocol, 0]);
        c.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 7]);
        let total = c.len() as u16;
        c[2..4].copy_from_slice(&total.to_le_bytes());
        c
    }

    fn command(&mut self) {
        while let Some((at, param, _status, control)) = self.cmd.next() {
            let kind = (control >> 10) & 0x3F;
            self.commands.push(kind);
            let csz = self.csz();
            let mut code = 1u8;
            match kind {
                TRB_ENABLE_SLOT => {}
                TRB_ADDRESS_DEVICE => {
                    let ictx = param as usize;
                    if rd32(ictx + 4) != 0b11 {
                        code = 5;
                    } else {
                        let ep0 = ictx + 2 * csz;
                        let dq = rd64(ep0 + 8);
                        self.ep0 = Ring {
                            deq: dq & !0xF,
                            ccs: dq & 1 != 0,
                        };
                        self.mps0 = (rd32(ep0 + 4) >> 16) as u16;
                        let octx = rd64(self.dcbaa as usize + 8 * SLOT as usize) as usize;
                        if !self.f.state_lie {
                            wr32(octx + 12, 2 << 27 | 1);
                        }
                    }
                }
                TRB_EVALUATE_CTX => {
                    let ictx = param as usize;
                    self.mps0 = (rd32(ictx + 2 * csz + 4) >> 16) as u16;
                }
                TRB_CONFIGURE_EP => {
                    let ictx = param as usize;
                    let add = rd32(ictx + 4);
                    let dci = 31 - add.leading_zeros();
                    if add & 1 == 0 || dci < 2 {
                        code = 5;
                    } else {
                        let ep = ictx + (dci as usize + 1) * csz;
                        let dq = rd64(ep + 8);
                        self.intr = Ring {
                            deq: dq & !0xF,
                            ccs: dq & 1 != 0,
                        };
                        self.intr_dci = dci as u8;
                        let octx = rd64(self.dcbaa as usize + 8 * SLOT as usize) as usize;
                        wr32(octx + dci as usize * csz, 1);
                    }
                }
                _ => code = 5,
            }
            if let Some((k, c)) = self.f.cmd_code {
                if k == kind {
                    code = c;
                }
            }
            let ptr = if self.f.cmd_ptr_lie { at + 16 } else { at };
            self.event(
                ptr,
                (code as u32) << 24,
                TRB_COMMAND_COMPLETION << 10 | (SLOT as u32) << 24,
            );
        }
    }

    fn xfer_event(&mut self, ptr: u64, residual: u32, code: u8, dci: u8) {
        let slot = if self.f.xfer_slot_lie { SLOT + 1 } else { SLOT };
        self.event(
            ptr,
            (code as u32) << 24 | residual,
            TRB_TRANSFER_EVENT << 10 | (dci as u32) << 16 | (slot as u32) << 24,
        );
    }

    fn control(&mut self) {
        while let Some((at, param, status, control)) = self.ep0.next() {
            match (control >> 10) & 0x3F {
                TRB_SETUP => {
                    self.setup = Some((at, param.to_le_bytes()));
                    self.data = None;
                }
                TRB_DATA => self.data = Some((at, param, status & 0x1FFFF)),
                TRB_STATUS => {
                    let Some((s_at, req)) = self.setup.take() else {
                        continue;
                    };
                    let (rt, rq) = (req[0], req[1]);
                    let value = u16::from_le_bytes([req[2], req[3]]);
                    let reply: Option<Vec<u8>> = match (rt, rq) {
                        (0x80, 6) => match value >> 8 {
                            1 => Some(self.device_desc()),
                            2 => Some(self.config_desc()),
                            _ => None,
                        },
                        (0x00, 9) => {
                            self.config_value = value as u8;
                            Some(vec![])
                        }
                        (0x21, 0x0B) if !self.f.stall_set_protocol => {
                            self.protocol = Some(value);
                            Some(vec![])
                        }
                        _ => None,
                    };
                    let Some(reply) = reply else {
                        self.xfer_event(s_at, 0, CC_STALL, 1);
                        continue;
                    };
                    if let Some((d_at, buf, len)) = self.data.take() {
                        let n = reply.len().min(len as usize);
                        for (i, b) in reply[..n].iter().enumerate() {
                            unsafe { ((buf as usize + i) as *mut u8).write_volatile(*b) };
                        }
                        let code = if n < len as usize {
                            CC_SHORT_PACKET
                        } else {
                            CC_SUCCESS
                        };
                        self.xfer_event(d_at, len - n as u32, code, 1);
                    }
                    self.xfer_event(at, 0, CC_SUCCESS, 1);
                }
                _ => {}
            }
        }
    }

    fn deliver(&mut self) {
        if self.pending.is_none() {
            if let Some((at, param, status, _control)) = self.intr.next() {
                self.pending = Some((at, param, status & 0x1FFFF));
            }
        }
        if let (Some((at, buf, len)), Some(r)) = (self.pending, self.reports.front().copied()) {
            self.reports.pop_front();
            self.pending = None;
            for (i, b) in r.iter().enumerate() {
                unsafe { ((buf as usize + i) as *mut u8).write_volatile(*b) };
            }
            self.xfer_event(at, len - 8, CC_SUCCESS, self.intr_dci);
        }
    }
}

#[derive(Clone)]
struct SimRegs(Rc<RefCell<Sim>>);

impl NvmeRegs for SimRegs {
    fn r32(&self, off: usize) -> u32 {
        let s = self.0.borrow();
        let op = off.wrapping_sub(CAPLEN);
        match off {
            0x00 => CAPLEN as u32 | 0x0100 << 16,
            0x04 => 64 | 1 << 8 | (PORTS as u32) << 24,
            0x08 => (s.f.scratchpads >> 5) << 21 | (s.f.scratchpads & 0x1F) << 27,
            0x10 => 1 | (s.f.csz64 as u32) << 2,
            0x14 => DB as u32,
            0x18 => RTS as u32,
            _ if op == 0x00 => (s.in_reset as u32) << 1 | s.running as u32,
            _ if op == 0x04 => !s.running as u32,
            _ if op == 0x08 => {
                if s.f.page_8k {
                    2
                } else {
                    1
                }
            }
            _ if op == 0x38 => s.config,
            _ if (0x400..0x400 + 0x10 * PORTS).contains(&op) && op.is_multiple_of(0x10) => {
                s.portsc[(op - 0x400) / 0x10]
            }
            _ => 0,
        }
    }

    fn w32(&self, off: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        let op = off.wrapping_sub(CAPLEN);
        match off {
            _ if op == 0x00 => {
                if v & 2 != 0 {
                    s.running = false;
                    s.in_reset = s.f.never_leaves_reset;
                    s.seg = 0;
                    s.pending = None;
                } else {
                    s.running = v & 1 != 0;
                }
            }
            _ if op == 0x18 => s.crcr_lo = v,
            _ if op == 0x1C => {
                let base = (v as u64) << 32 | (s.crcr_lo as u64 & !0x3F);
                s.cmd = Ring {
                    deq: base,
                    ccs: s.crcr_lo & 1 != 0,
                };
            }
            _ if op == 0x30 => s.dcbaa = (s.dcbaa & !0xFFFF_FFFF) | v as u64,
            _ if op == 0x34 => s.dcbaa = (s.dcbaa & 0xFFFF_FFFF) | (v as u64) << 32,
            _ if op == 0x38 => s.config = v,
            _ if (0x400..0x400 + 0x10 * PORTS).contains(&op) && op.is_multiple_of(0x10) => {
                let i = (op - 0x400) / 0x10;
                if v & (1 << 4) != 0 {
                    if s.portsc[i] & 1 != 0 {
                        s.portsc[i] |= 1 << 1 | 1 << 21;
                        let port = (i as u64 + 1) << 24;
                        s.event(port, 1 << 24, TRB_PORT_STATUS_CHANGE << 10);
                    }
                } else {
                    s.portsc[i] &= !(v & (0x7F << 17));
                }
            }
            x if x == RTS + 0x28 => s.erstsz = v,
            x if x == RTS + 0x30 => s.erstba = (s.erstba & !0xFFFF_FFFF) | v as u64,
            x if x == RTS + 0x34 => {
                s.erstba = (s.erstba & 0xFFFF_FFFF) | (v as u64) << 32;
                let e = s.erstba as usize;
                s.seg = rd64(e);
                s.seg_size = rd32(e + 8);
                s.enq = 0;
                s.pcs = true;
                assert_eq!(s.erstsz, 1, "one segment programmed before ERSTBA");
            }
            x if x == DB => s.command(),
            x if x == DB + 4 * SLOT as usize => {
                if v == 1 {
                    s.control();
                } else if v as u8 == s.intr_dci {
                    s.deliver();
                }
            }
            _ => {}
        }
    }
}

impl SimRegs {
    fn new(f: Faults) -> Self {
        SimRegs(Rc::new(RefCell::new(Sim::new(f))))
    }
    fn press(&self, r: [u8; 8]) {
        let mut s = self.0.borrow_mut();
        s.reports.push_back(r);
        s.deliver();
    }
}

type Driver = Xhci<HostHal, SimRegs>;

fn up(f: Faults) -> (Driver, SimRegs) {
    let regs = SimRegs::new(f);
    let (x, _r) = unsafe { Driver::init(regs.clone(), MAP_LEN) }.expect("controller up");
    (x, regs)
}

fn keyboard(x: &mut Driver) -> kernel_core::xhci::Keyboard {
    unsafe {
        let mut dev = x.address(KBD_PORT as u8).expect("addressed");
        let desc = x.find_boot_keyboard(&mut dev).expect("keyboard found");
        x.configure_keyboard(dev, desc).expect("configured")
    }
}

#[test]
fn the_boot_suite_passes_over_a_healthy_controller_and_leaves_it_halted() {
    let regs = SimRegs::new(Faults::default());
    let mut names = vec![];
    let n = unsafe {
        device_suite::<HostHal, _, _>(regs.clone(), MAP_LEN, &mut |i, ok, name: &str| {
            assert!(ok, "invariant {i} failed: {name}");
            names.push(name.to_string());
        })
    }
    .expect("every invariant holds");
    assert_eq!(
        n, 7,
        "the suite's invariant count changed - update both VM gates"
    );
    assert_eq!(names.len(), 7);
    let s = regs.0.borrow();
    assert!(!s.running, "the suite leaves the controller halted");
    assert_eq!(s.config_value, 1);
    assert_eq!(s.protocol, Some(0), "boot protocol selected");
    assert_eq!(
        s.commands,
        [TRB_ENABLE_SLOT, TRB_ADDRESS_DEVICE, TRB_CONFIGURE_EP],
        "a high-speed device needs no Evaluate Context"
    );
}

#[test]
fn a_controller_with_nothing_connected_is_a_named_skip() {
    let regs = SimRegs::new(Faults {
        no_device: true,
        ..Default::default()
    });
    let n =
        unsafe { device_suite::<HostHal, _, _>(regs, MAP_LEN, &mut |_, ok, _: &str| assert!(ok)) };
    assert_eq!(n, Ok(0));
}

#[test]
fn keystrokes_arrive_as_console_bytes() {
    let (mut x, regs) = up(Faults::default());
    let mut kb = keyboard(&mut x);
    assert_eq!(kb.dci, 3, "endpoint 0x81 is device context index 3");
    let mut dec = BootKeyboard::new();
    let mut typed = vec![];
    let mut feed = |r: [u8; 8], x: &mut Driver, kb: &mut kernel_core::xhci::Keyboard| {
        regs.press(r);
        let got = x.poll_report(kb).expect("report").expect("one report");
        assert_eq!(got, r);
        dec.feed(&got, &mut typed);
    };
    feed([0x02, 0, 0x04, 0, 0, 0, 0, 0], &mut x, &mut kb); // shift + a
    feed([0, 0, 0, 0, 0, 0, 0, 0], &mut x, &mut kb);
    feed([0, 0, 0x05, 0, 0, 0, 0, 0], &mut x, &mut kb); // b
    feed([0, 0, 0x05, 0x1E, 0, 0, 0, 0], &mut x, &mut kb); // b held, 1 pressed
    feed([0, 0, 0, 0, 0, 0, 0, 0], &mut x, &mut kb);
    feed([0, 0, 0x28, 0, 0, 0, 0, 0], &mut x, &mut kb); // Enter
    assert_eq!(typed, b"Ab1\r");
    assert_eq!(x.poll_report(&mut kb), Ok(None), "no keystroke, no report");
}

#[test]
fn a_rollover_report_types_nothing_and_is_counted() {
    let mut dec = BootKeyboard::new();
    let mut typed = vec![];
    dec.feed(&[0, 0, 0x04, 0, 0, 0, 0, 0], &mut typed);
    dec.feed(&[0, 0, 1, 1, 1, 1, 1, 1], &mut typed);
    dec.feed(&[0, 0, 0x04, 0x05, 0, 0, 0, 0], &mut typed);
    assert_eq!(
        typed, b"ab",
        "a held through the phantom state is not typed twice"
    );
    assert_eq!(dec.rollover_refusals(), 1);
}

#[test]
fn a_full_speed_device_with_a_larger_ep0_is_evaluated() {
    let (mut x, regs) = up(Faults {
        full_speed_mps0: Some(64),
        ..Default::default()
    });
    let dev = unsafe { x.address(KBD_PORT as u8) }.expect("addressed");
    assert_eq!(dev.max_packet0, 64);
    let s = regs.0.borrow();
    assert_eq!(s.mps0, 64);
    assert!(s.commands.contains(&TRB_EVALUATE_CTX));
}

#[test]
fn rings_wrap_through_their_link_trb() {
    let (mut x, _regs) = up(Faults::default());
    let mut dev = unsafe { x.address(KBD_PORT as u8) }.expect("addressed");
    // Each find is three control transfers of 3 TRBs: 30 finds lap the 255-TRB control ring and
    // the 128-entry event ring.
    let mut desc = None;
    for _ in 0..30 {
        desc = Some(unsafe { x.find_boot_keyboard(&mut dev) }.expect("descriptors"));
    }
    let kb = unsafe { x.configure_keyboard(dev, desc.unwrap()) };
    assert!(kb.is_ok());
}

#[test]
fn scratchpad_pages_are_provided_and_registered() {
    let (x, _regs) = up(Faults {
        scratchpads: 3,
        ..Default::default()
    });
    // dcbaa, 3 scratchpads, command ring, event ring.
    assert_eq!(x.dma_regions(), 6);
    assert!(x.dma_gate_refuses_unregistered());
}

#[test]
fn sixty_four_byte_contexts_are_honoured() {
    let (mut x, _regs) = up(Faults {
        csz64: true,
        ..Default::default()
    });
    let kb = keyboard(&mut x);
    assert_eq!(kb.desc.interface, 0);
}

#[test]
fn a_controller_outside_the_contract_is_refused_at_init() {
    let cases: [(Faults, usize, XhciError); 4] = [
        (
            Faults {
                never_leaves_reset: true,
                ..Default::default()
            },
            MAP_LEN,
            XhciError::Timeout,
        ),
        (
            Faults {
                page_8k: true,
                ..Default::default()
            },
            MAP_LEN,
            XhciError::PageSize,
        ),
        (
            Faults {
                scratchpads: 65,
                ..Default::default()
            },
            MAP_LEN,
            XhciError::Scratchpads,
        ),
        (Faults::default(), 0x1000, XhciError::Window),
    ];
    for (f, window, want) in cases {
        let r = unsafe { Driver::init(SimRegs::new(f), window) };
        assert_eq!(r.err(), Some(want));
    }
}

#[test]
fn completions_and_transfers_that_lie_are_refused() {
    let lies: [(Faults, XhciError); 6] = [
        (
            Faults {
                cmd_ptr_lie: true,
                ..Default::default()
            },
            XhciError::Unexpected,
        ),
        (
            Faults {
                cmd_code: Some((TRB_ADDRESS_DEVICE, 17)),
                ..Default::default()
            },
            XhciError::Command(17),
        ),
        (
            Faults {
                xfer_slot_lie: true,
                ..Default::default()
            },
            XhciError::Unexpected,
        ),
        (
            Faults {
                short_device_desc: true,
                ..Default::default()
            },
            XhciError::Descriptor,
        ),
        (
            Faults {
                state_lie: true,
                ..Default::default()
            },
            XhciError::State,
        ),
        (
            Faults {
                stall_set_protocol: true,
                ..Default::default()
            },
            XhciError::Transfer(CC_STALL),
        ),
    ];
    for (f, want) in lies {
        let (mut x, _regs) = up(f);
        let got = unsafe {
            x.address(KBD_PORT as u8).and_then(|mut dev| {
                let d = x.find_boot_keyboard(&mut dev)?;
                x.configure_keyboard(dev, d).map(|_| ())
            })
        };
        assert_eq!(got.err(), Some(want));
    }
}

#[test]
fn a_mouse_is_not_taken_for_a_keyboard() {
    let (mut x, _regs) = up(Faults {
        mouse: true,
        ..Default::default()
    });
    let mut dev = unsafe { x.address(KBD_PORT as u8) }.unwrap();
    assert_eq!(
        unsafe { x.find_boot_keyboard(&mut dev) },
        Err(XhciError::NotKeyboard)
    );
}

#[test]
fn a_configuration_whose_lengths_do_not_tile_is_refused() {
    let good = SimRegs::new(Faults::default()).0.borrow().config_desc();
    assert!(parse_boot_keyboard(&good).is_ok());
    let mut zero = good.clone();
    zero[9] = 0; // interface descriptor claims length 0
    assert_eq!(parse_boot_keyboard(&zero), Err(XhciError::Descriptor));
    let mut over = good.clone();
    let last = over.len() - 7;
    over[last] = 9; // endpoint runs past the end
    assert_eq!(parse_boot_keyboard(&over), Err(XhciError::Descriptor));
    assert_eq!(parse_boot_keyboard(&good[..5]), Err(XhciError::Descriptor));
}

#[test]
fn intervals_and_usages_follow_the_tables() {
    assert_eq!(xhci_interval(1, 10), 6); // 10 ms = 80 microframes -> 2^6
    assert_eq!(xhci_interval(1, 1), 3);
    assert_eq!(xhci_interval(3, 7), 6); // 2^(7-1) microframes
    assert_eq!(xhci_interval(3, 0), 0);
    assert_eq!(usage_to_evdev(0x04), Some(30)); // a
    assert_eq!(usage_to_evdev(0x27), Some(11)); // 0
    assert_eq!(usage_to_evdev(0x28), Some(28)); // Enter
    assert_eq!(usage_to_evdev(0x52), Some(103)); // Up
    assert_eq!(usage_to_evdev(0x58), Some(96)); // Keypad Enter
    assert_eq!(usage_to_evdev(0x01), None);
    assert_eq!(usage_to_evdev(0xE0), None, "modifiers come from the bitmap");
    let _ = TRB_NORMAL;
}

#[test]
fn a_device_that_is_not_a_keyboard_is_passed_over_not_failed() {
    let regs = SimRegs::new(Faults {
        mouse: true,
        ..Default::default()
    });
    let n = unsafe {
        device_suite::<HostHal, _, _>(regs.clone(), MAP_LEN, &mut |i, ok, name: &str| {
            assert!(ok, "invariant {i} failed: {name}")
        })
    };
    assert_eq!(n, Ok(0), "a mouse alone is a named skip");
    assert!(
        !regs.0.borrow().running,
        "and the controller is left halted"
    );
}
