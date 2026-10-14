//! Hosted proof of the HD Audio driver (ADR-258) against a simulated controller with one codec: an
//! audio function group, an output converter and an output pin wired to it. The simulation follows
//! the CORB/RIRB and stream-descriptor protocol QEMU's `intel-hda` implements, and can be told to
//! break it in named ways.

use kernel_core::hda::{
    device_suite, tone_sample, Hda, HdaError, HdaRegs, CORBCTL, CORBLBASE, CORBRP, CORBUBASE,
    CORBWP, FORMAT_48K_16_STEREO, GCAP, GCTL, MAP_LEN, RIRBCTL, RIRBLBASE, RIRBSTS, RIRBUBASE,
    RIRBWP, SD_BASE, SD_BDLPL, SD_BDLPU, SD_CTL, SD_CTL_RUN, SD_CTL_SRST, SD_LPIB, SD_STRIDE,
    SD_STS_BCIS, STATESTS, TONE_PERIOD,
};
use kernel_core::virtioblk::VirtioHal;
use std::cell::RefCell;
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

/// Named ways the controller or codec misbehaves.
#[derive(Clone, Copy, Default)]
struct Faults {
    no_codec: bool,
    silent: bool,
    wrong_cad: bool,
    unsolicited: bool,
    input_only: bool,
    stuck_stream: bool,
}

/// GCAP: 4 output, 4 input streams, 64-bit capable (QEMU's 0x4401).
const SIM_GCAP: u16 = 0x4401;
const OUT_SD: usize = SD_BASE + SD_STRIDE * 4;

#[derive(Default)]
struct Sim {
    f: Faults,
    regs: Vec<u8>,
    verbs: Vec<u32>,
    running: bool,
    lpib: u32,
    bdl: u64,
}

fn rd32(a: usize) -> u32 {
    unsafe { (a as *const u32).read_volatile() }
}
fn wr32(a: usize, v: u32) {
    unsafe { (a as *mut u32).write_volatile(v) }
}

impl Sim {
    fn get(&self, off: usize, n: usize) -> u32 {
        (0..n).fold(0, |a, i| a | (self.regs[off + i] as u32) << (8 * i))
    }
    fn put(&mut self, off: usize, n: usize, v: u32) {
        for i in 0..n {
            self.regs[off + i] = (v >> (8 * i)) as u8;
        }
    }

    /// The codec: three widgets answering GET_PARAMETER and GET_CONNECTION_LIST.
    fn answer(&self, verb: u32) -> u32 {
        let nid = (verb >> 20) & 0x7F;
        let payload = verb & 0xF_FFFF;
        if payload >> 8 == 0xF00 {
            let p = payload & 0xFF;
            return match (nid, p) {
                (0, 0x00) => 0x1af4_0022,
                (0, 0x04) => 0x0001_0001,
                (1, 0x05) => 0x01,
                (1, 0x04) => 0x0002_0002,
                (2, 0x09) => (self.f.input_only as u32) << 20,
                (3, 0x09) => 4 << 20,
                (3, 0x0C) => 1 << (4 + self.f.input_only as u32),
                (3, 0x0E) => 1,
                _ => 0,
            };
        }
        if payload >> 8 == 0xF02 && nid == 3 {
            return 2;
        }
        0
    }

    fn corb_run(&mut self) {
        let corb = (self.get(CORBUBASE, 4) as u64) << 32 | self.get(CORBLBASE, 4) as u64;
        let rirb = (self.get(RIRBUBASE, 4) as u64) << 32 | self.get(RIRBLBASE, 4) as u64;
        if self.regs[CORBCTL] & 2 == 0 {
            return;
        }
        let wp = self.get(CORBWP, 2) & 0xFF;
        while self.get(CORBRP, 2) & 0xFF != wp {
            let rp = (self.get(CORBRP, 2) + 1) & 0xFF;
            self.put(CORBRP, 2, rp);
            let verb = rd32(corb as usize + 4 * rp as usize);
            self.verbs.push(verb);
            if self.f.silent || self.regs[RIRBCTL] & 2 == 0 {
                continue;
            }
            let resp = self.answer(verb);
            let cad = (verb >> 28) & 0xF;
            let ex = if self.f.wrong_cad { cad + 1 } else { cad }
                | if self.f.unsolicited { 1 << 4 } else { 0 };
            let w = (self.get(RIRBWP, 2) + 1) & 0xFF;
            wr32(rirb as usize + 8 * w as usize, resp);
            wr32(rirb as usize + 8 * w as usize + 4, ex);
            self.put(RIRBWP, 2, w);
        }
    }
}

#[derive(Clone)]
struct SimRegs(Rc<RefCell<Sim>>);

impl SimRegs {
    fn new(f: Faults) -> Self {
        let mut s = Sim {
            f,
            regs: vec![0; MAP_LEN],
            ..Default::default()
        };
        s.put(GCAP, 2, SIM_GCAP as u32);
        SimRegs(Rc::new(RefCell::new(s)))
    }
}

impl HdaRegs for SimRegs {
    fn r8(&self, off: usize) -> u8 {
        self.0.borrow().regs[off]
    }
    fn r16(&self, off: usize) -> u16 {
        self.0.borrow().get(off, 2) as u16
    }
    fn r32(&self, off: usize) -> u32 {
        let mut s = self.0.borrow_mut();
        // A running stream moves a quarter buffer per look at its control register.
        if off == OUT_SD + SD_CTL && s.running && !s.f.stuck_stream {
            let before = s.lpib / 4096;
            s.lpib = (s.lpib + 1024) % 8192;
            if s.lpib / 4096 != before {
                let v = s.get(OUT_SD + SD_CTL, 4) | SD_STS_BCIS;
                s.put(OUT_SD + SD_CTL, 4, v);
            }
            let l = s.lpib;
            s.put(OUT_SD + SD_LPIB, 4, l);
        }
        s.get(off, 4)
    }
    fn w8(&self, off: usize, v: u8) {
        let mut s = self.0.borrow_mut();
        match off {
            // The status byte alone: write-1-to-clear.
            o if o == OUT_SD + 3 => s.regs[o] &= !(v & 0x1C),
            RIRBSTS => {
                s.regs[off] &= !v;
                s.corb_run();
            }
            _ => {
                s.regs[off] = v;
                if off == CORBCTL {
                    s.corb_run();
                }
            }
        }
    }
    fn w16(&self, off: usize, v: u16) {
        let mut s = self.0.borrow_mut();
        match off {
            RIRBWP => {
                if v & 0x8000 != 0 {
                    s.put(RIRBWP, 2, 0);
                }
            }
            CORBWP => {
                s.put(CORBWP, 2, v as u32);
                s.corb_run();
            }
            _ => s.put(off, 2, v as u32),
        }
    }
    fn w32(&self, off: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        if off == GCTL {
            if v & 1 == 0 {
                // Cold reset: registers clear, codecs announce themselves.
                let gcap = s.get(GCAP, 2);
                s.regs.iter_mut().for_each(|b| *b = 0);
                s.put(GCAP, 2, gcap);
                let present = if s.f.no_codec { 0 } else { 1 };
                s.put(STATESTS, 2, present);
                s.running = false;
            }
            s.put(GCTL, 4, v & 1);
        } else if off == OUT_SD + SD_CTL {
            // QEMU's CTL register: a 32-bit write stores the status bits AS GIVEN (they are in its
            // write mask, not its clear mask), so a driver that "clears" BCIS here sets it.
            let old = s.get(off, 4);
            let sts = v & 0x1C00_0000;
            let ctl = if v & SD_CTL_SRST != 0 {
                SD_CTL_SRST
            } else {
                v & 0x00FF_FFFF
            };
            if ctl & SD_CTL_RUN != 0 && old & SD_CTL_RUN == 0 {
                s.bdl =
                    (s.get(OUT_SD + SD_BDLPU, 4) as u64) << 32 | s.get(OUT_SD + SD_BDLPL, 4) as u64;
                s.lpib = 0;
            }
            s.running = ctl & SD_CTL_RUN != 0;
            s.put(off, 4, sts | ctl);
        } else {
            s.put(off, 4, v);
        }
    }
}

type Driver = Hda<HostHal, SimRegs>;

#[test]
fn the_boot_suite_plays_a_tone_over_a_healthy_controller() {
    let regs = SimRegs::new(Faults::default());
    let mut seen = None;
    let n = unsafe {
        device_suite::<HostHal, _, _>(
            regs.clone(),
            MAP_LEN,
            &mut |i, ok, name: &str| assert!(ok, "invariant {i} failed: {name}"),
            &mut |_rep, path, done| seen = Some((path.copied(), done)),
        )
    }
    .expect("every invariant holds");
    assert_eq!(
        n, 6,
        "the suite's invariant count changed - update both VM gates"
    );
    let (path, done) = seen.expect("reported");
    let path = path.expect("an output path");
    assert_eq!(
        (path.afg, path.dac, path.pin, path.vendor),
        (1, 2, 3, 0x1af4_0022)
    );
    // 250 looks at a quarter buffer each: 62 buffers. A BCIS never cleared would count every look.
    assert!((50..=70).contains(&done), "buffers completed: {done}");
    let s = regs.0.borrow();
    let fmt = 2 << 20 | 0x2 << 16 | FORMAT_48K_16_STEREO as u32;
    assert!(
        s.verbs.contains(&fmt),
        "the converter was given 48 kHz 16-bit stereo"
    );
    assert!(
        s.verbs.contains(&(3 << 20 | 0x707 << 8 | 0x40)),
        "the pin's output enabled"
    );
    assert!(
        s.verbs.contains(&(2 << 20 | 0x706 << 8 | 0x10)),
        "the converter tagged stream 1"
    );
    assert_eq!(
        s.get(GCTL, 4) & 1,
        0,
        "the suite leaves the controller in reset"
    );
    // The BDL names two registered buffers, each with IOC, holding the tone.
    let bdl = s.bdl as usize;
    for b in 0..2 {
        let addr = unsafe { ((bdl + 16 * b) as *const u64).read_volatile() } as usize;
        let len = rd32(bdl + 16 * b + 8);
        assert_eq!((len, rd32(bdl + 16 * b + 12)), (4096, 1));
        let quarter = unsafe { ((addr + 4 * (TONE_PERIOD / 4)) as *const i16).read_volatile() };
        assert_eq!(quarter, tone_sample(TONE_PERIOD / 4));
    }
}

#[test]
fn the_tone_is_a_symmetric_triangle_of_one_period() {
    let p = TONE_PERIOD;
    assert_eq!(tone_sample(0), 0);
    assert_eq!(tone_sample(p / 4), 8000);
    assert_eq!(tone_sample(p / 2), 0);
    assert_eq!(tone_sample(3 * p / 4), -8000);
    assert_eq!(tone_sample(p), 0);
    let sum: i64 = (0..p).map(|i| tone_sample(i) as i64).sum();
    assert_eq!(sum, 0, "no DC offset");
}

#[test]
fn no_codec_and_no_output_path_are_named_skips() {
    for f in [
        Faults {
            no_codec: true,
            ..Default::default()
        },
        Faults {
            input_only: true,
            ..Default::default()
        },
    ] {
        let n = unsafe {
            device_suite::<HostHal, _, _>(
                SimRegs::new(f),
                MAP_LEN,
                &mut |i, ok, name: &str| assert!(ok, "invariant {i}: {name}"),
                &mut |_, _, _| {},
            )
        };
        assert_eq!(n, Ok(0));
    }
}

#[test]
fn codec_answers_that_lie_are_refused_by_name() {
    let cases = [
        (
            Faults {
                silent: true,
                ..Default::default()
            },
            HdaError::Timeout,
        ),
        (
            Faults {
                wrong_cad: true,
                ..Default::default()
            },
            HdaError::WrongCodec,
        ),
        (
            Faults {
                unsolicited: true,
                ..Default::default()
            },
            HdaError::Unsolicited,
        ),
    ];
    for (f, want) in cases {
        let (mut h, _) = unsafe { Driver::init(SimRegs::new(f), MAP_LEN) }.expect("init");
        assert_eq!(h.find_output(0), Err(want));
    }
}

#[test]
fn a_stream_that_never_moves_is_refused_and_stopped() {
    let regs = SimRegs::new(Faults {
        stuck_stream: true,
        ..Default::default()
    });
    let (mut h, _) = unsafe { Driver::init(regs.clone(), MAP_LEN) }.expect("init");
    let p = h.find_output(0).expect("path");
    h.configure(&p).expect("configured");
    assert_eq!(h.play_tone(20), Err(HdaError::StreamStalled));
    assert!(!regs.0.borrow().running, "the stalled stream is stopped");
}

#[test]
fn a_controller_without_output_streams_or_inside_a_small_window_is_refused() {
    let regs = SimRegs::new(Faults::default());
    regs.0.borrow_mut().put(GCAP, 2, 0x0401); // inputs only
    assert_eq!(
        unsafe { Driver::init(regs, MAP_LEN) }.err(),
        Some(HdaError::Capabilities)
    );
    let regs = SimRegs::new(Faults::default());
    assert_eq!(
        unsafe { Driver::init(regs, 0x100) }.err(),
        Some(HdaError::Capabilities)
    );
}

#[test]
fn every_ring_and_buffer_is_registered() {
    let (h, _) = unsafe { Driver::init(SimRegs::new(Faults::default()), MAP_LEN) }.expect("init");
    assert_eq!(h.dma_regions(), 1);
    assert!(h.dma_gate_refuses_unregistered());
    let _ = (CORBRP, SD_LPIB);
}
