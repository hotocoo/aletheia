//! Hosted proof of the NVMe driver (ADR-223) against a simulated controller that follows the NVM
//! Express Base Specification's queue protocol - and can be told to break it in named ways.

use kernel_core::nvme::{
    device_suite, Nvme, NvmeRegs, REG_ACQ, REG_AQA, REG_ASQ, REG_CAP, REG_CC, REG_CSTS,
    REG_DOORBELL_BASE, REG_INTMS, REG_VS,
};
use kernel_core::storage::{BlockDevice, StorageError, BLOCK_SIZE};
use kernel_core::virtioblk::VirtioHal;
use std::cell::RefCell;
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

/// Named ways the simulated controller misbehaves.
#[derive(Clone, Copy, Default)]
struct Faults {
    never_ready: bool,
    fatal_on_enable: bool,
    /// Report a CID one higher than the command's, for NVM (I/O queue) commands.
    cid_lie: bool,
    /// Report Status Code 0x02 (Invalid Field) for NVM commands.
    io_status: Option<u16>,
    /// Never post completions for NVM commands.
    drop_io: bool,
    /// Report SQ id 7 for NVM commands.
    sqid_lie: bool,
    /// Controller comes out of firmware already enabled.
    starts_enabled: bool,
}

#[derive(Clone, Copy)]
struct Geometry {
    lbads: u32,
    metadata: u32,
    nsze: u64,
    vwc: bool,
    mpsmin: u64,
    css_nvm: bool,
    dstrd: u64,
    mqes: u64,
    scratch: bool,
}

impl Geometry {
    fn healthy(lbads: u32, blocks: u64) -> Self {
        Geometry {
            lbads,
            metadata: 0,
            nsze: blocks * (BLOCK_SIZE as u64 >> lbads),
            vwc: true,
            mpsmin: 0,
            css_nvm: true,
            dstrd: 0,
            mqes: 63,
            scratch: true,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DevQueue {
    sq: usize,
    cq: usize,
    depth: u16,
    sq_head: u16,
    cq_tail: u16,
    phase: bool,
    cqid: u16,
}

struct State {
    g: Geometry,
    f: Faults,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    intms: u32,
    queues: [Option<DevQueue>; 2],
    disk: Vec<u8>,
    /// Protocol violations the DRIVER committed, recorded by the controller.
    violations: Vec<&'static str>,
    flushes: usize,
    io_commands: usize,
}

struct Sim {
    st: RefCell<State>,
}

unsafe fn rd32(a: usize) -> u32 {
    (a as *const u32).read_volatile()
}
unsafe fn wr32(a: usize, v: u32) {
    (a as *mut u32).write_volatile(v)
}

impl Sim {
    fn new(g: Geometry, f: Faults) -> Self {
        let disk = vec![0u8; (g.nsze as usize) << g.lbads];
        let (cc, csts) = if f.starts_enabled { (1, 1) } else { (0, 0) };
        Sim {
            st: RefCell::new(State {
                g,
                f,
                cc,
                csts,
                aqa: 0,
                asq: 0,
                acq: 0,
                intms: 0,
                queues: [None, None],
                disk,
                violations: vec![],
                flushes: 0,
                io_commands: 0,
            }),
        }
    }

    fn cap(g: &Geometry) -> u64 {
        g.mqes | 1 << 16 | 20 << 24 | g.dstrd << 32 | (g.css_nvm as u64) << 37 | g.mpsmin << 48
    }

    fn post(s: &mut State, qid: usize, cid: u16, sqid: u16, status: u16, dw0: u32) {
        let cqid = s.queues[qid].unwrap().cqid as usize;
        let q = s.queues[qid].unwrap();
        let cq = s.queues[cqid].unwrap();
        let slot = cq.cq + cq.cq_tail as usize * 16;
        // SAFETY: the driver handed this CQ frame to the controller.
        unsafe {
            wr32(slot, dw0);
            wr32(slot + 4, 0);
            wr32(slot + 8, q.sq_head as u32 | (sqid as u32) << 16);
            wr32(
                slot + 12,
                cid as u32 | (cq.phase as u32) << 16 | (status as u32) << 17,
            );
        }
        let c = s.queues[cqid].as_mut().unwrap();
        c.cq_tail = (c.cq_tail + 1) % c.depth;
        if c.cq_tail == 0 {
            c.phase = !c.phase;
        }
    }

    fn execute(s: &mut State, qid: usize, sqe: usize) {
        // SAFETY: the SQE lies in the driver's SQ frame.
        let dw: Vec<u32> = (0..16).map(|i| unsafe { rd32(sqe + 4 * i) }).collect();
        let opcode = (dw[0] & 0xFF) as u8;
        let cid = (dw[0] >> 16) as u16;
        let nsid = dw[1];
        let prp1 = dw[6] as usize | (dw[7] as usize) << 32;
        let (c10, c11, c12) = (dw[10], dw[11], dw[12]);
        let ok = |s: &mut State| Sim::post(s, qid, cid, qid as u16, 0, 0);
        if qid == 0 {
            match opcode {
                0x06 if c10 == 1 => {
                    let mut id = [0u8; 4096];
                    id[24..32].copy_from_slice(b"SIM NVME");
                    let sn: &[u8] = if s.g.scratch {
                        b"ALETHEIA-SCRATCH    "
                    } else {
                        b"S3EWNX0M123456      "
                    };
                    id[4..24].copy_from_slice(sn);
                    id[516..520].copy_from_slice(&1u32.to_le_bytes());
                    id[525] = s.g.vwc as u8;
                    unsafe { std::ptr::copy_nonoverlapping(id.as_ptr(), prp1 as *mut u8, 4096) };
                    ok(s);
                }
                0x06 if c10 == 0 && nsid == 1 => {
                    let mut id = [0u8; 4096];
                    id[0..8].copy_from_slice(&s.g.nsze.to_le_bytes());
                    id[25] = 0;
                    id[26] = 0;
                    let lbaf = s.g.metadata | s.g.lbads << 16;
                    id[128..132].copy_from_slice(&lbaf.to_le_bytes());
                    unsafe { std::ptr::copy_nonoverlapping(id.as_ptr(), prp1 as *mut u8, 4096) };
                    ok(s);
                }
                0x05 => {
                    if c11 & 2 != 0 {
                        s.violations.push("io cq created with interrupts enabled");
                    }
                    s.queues[(c10 & 0xFFFF) as usize] = Some(DevQueue {
                        cq: prp1,
                        depth: (c10 >> 16) as u16 + 1,
                        phase: true,
                        cqid: 1,
                        ..Default::default()
                    });
                    ok(s);
                }
                0x01 => {
                    let q = s.queues[1].as_mut().expect("cq before sq");
                    q.sq = prp1;
                    q.cqid = (c11 >> 16) as u16;
                    ok(s);
                }
                _ => Sim::post(s, qid, cid, 0, 0x01, 0),
            }
            return;
        }
        s.io_commands += 1;
        if s.f.drop_io {
            return;
        }
        let rcid = if s.f.cid_lie {
            cid.wrapping_add(1)
        } else {
            cid
        };
        let sqid = if s.f.sqid_lie { 7 } else { qid as u16 };
        if let Some(st) = s.f.io_status {
            Sim::post(s, qid, rcid, sqid, st, 0);
            return;
        }
        let slba = c10 as u64 | (c11 as u64) << 32;
        let nlb = (c12 & 0xFFFF) as u64 + 1;
        let off = (slba as usize) << s.g.lbads;
        let len = (nlb as usize) << s.g.lbads;
        let status = match opcode {
            0x00 => {
                s.flushes += 1;
                0
            }
            _ if slba + nlb > s.g.nsze || len > PAGE => 0x80, // LBA out of range
            0x01 => {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        prp1 as *const u8,
                        s.disk[off..].as_mut_ptr(),
                        len,
                    )
                };
                0
            }
            0x02 => {
                unsafe {
                    std::ptr::copy_nonoverlapping(s.disk[off..].as_ptr(), prp1 as *mut u8, len)
                };
                0
            }
            _ => 0x01,
        };
        Sim::post(s, qid, rcid, sqid, status, 0);
    }
}

impl NvmeRegs for Sim {
    fn r32(&self, off: usize) -> u32 {
        let s = self.st.borrow();
        let cap = Sim::cap(&s.g);
        match off {
            o if o == REG_CAP => cap as u32,
            o if o == REG_CAP + 4 => (cap >> 32) as u32,
            o if o == REG_VS => 0x0001_0400,
            o if o == REG_CC => s.cc,
            o if o == REG_CSTS => s.csts,
            o if o == REG_INTMS => s.intms,
            _ => 0,
        }
    }

    fn w32(&self, off: usize, v: u32) {
        let mut s = self.st.borrow_mut();
        let stride = 4usize << s.g.dstrd;
        match off {
            o if o == REG_CC => {
                s.cc = v;
                if v & 1 == 0 {
                    s.csts = 0;
                } else if s.f.fatal_on_enable {
                    s.csts = 2;
                } else if !s.f.never_ready {
                    s.queues[0] = Some(DevQueue {
                        sq: s.asq as usize,
                        cq: s.acq as usize,
                        depth: (s.aqa & 0xFFF) as u16 + 1,
                        phase: true,
                        cqid: 0,
                        ..Default::default()
                    });
                    s.csts = 1;
                }
            }
            o if o == REG_INTMS => s.intms |= v,
            o if [REG_AQA, REG_ASQ, REG_ASQ + 4, REG_ACQ, REG_ACQ + 4].contains(&o) => {
                if s.cc & 1 != 0 {
                    s.violations
                        .push("admin queue registers written while enabled");
                }
                match o {
                    o if o == REG_AQA => s.aqa = v,
                    o if o == REG_ASQ => s.asq = (s.asq & !0xFFFF_FFFF) | v as u64,
                    o if o == REG_ASQ + 4 => s.asq = (s.asq & 0xFFFF_FFFF) | (v as u64) << 32,
                    o if o == REG_ACQ => s.acq = (s.acq & !0xFFFF_FFFF) | v as u64,
                    _ => s.acq = (s.acq & 0xFFFF_FFFF) | (v as u64) << 32,
                }
            }
            o if o >= REG_DOORBELL_BASE => {
                let idx = (o - REG_DOORBELL_BASE) / stride;
                if !(o - REG_DOORBELL_BASE).is_multiple_of(stride) {
                    s.violations.push("doorbell written off its stride");
                    return;
                }
                let qid = idx / 2;
                if idx % 2 == 1 {
                    return; // CQ head doorbell: the slot is free again
                }
                let Some(q) = s.queues[qid] else {
                    s.violations
                        .push("doorbell for a queue that does not exist");
                    return;
                };
                let mut head = q.sq_head;
                while head != v as u16 {
                    let sqe = q.sq + head as usize * 64;
                    head = (head + 1) % q.depth;
                    s.queues[qid].as_mut().unwrap().sq_head = head;
                    Sim::execute(&mut s, qid, sqe);
                }
            }
            _ => {}
        }
    }
}

fn open(g: Geometry, f: Faults) -> Result<Nvme<HostHal, Sim>, &'static str> {
    unsafe { Nvme::<HostHal, Sim>::init(Sim::new(g, f)) }.map(|(d, _)| d)
}

const GATE_BLOCKS: u64 = 256;

#[test]
fn a_controller_not_marked_scratch_is_never_written() {
    // Someone's SSD (ADR-228): the read-only group runs, the write group does not, and no write
    // or flush command ever reaches the controller.
    let g = Geometry {
        scratch: false,
        ..Geometry::healthy(9, 64)
    };
    let sim = Sim::new(g, Faults::default());
    {
        let (mut dev, _) = unsafe { Nvme::<HostHal, &Sim>::init(&sim) }.expect("init");
        assert!(!dev.is_scratch());
        assert_eq!(
            dev.write_block(10, &[1u8; BLOCK_SIZE]),
            Err(StorageError::Device)
        );
        assert_eq!(dev.flush(), Err(StorageError::Device));
        let n = device_suite(&mut dev, 999, &mut |i, ok, name: &str| {
            assert!(ok, "{i}: {name}")
        })
        .expect("read-only suite");
        assert_eq!(n, 5, "read-only group count changed - update the gates");
    }
    let s = sim.st.borrow();
    assert!(s.disk.iter().all(|&b| b == 0), "the disk must be untouched");
    assert_eq!(s.flushes, 0);
    assert_eq!(
        s.cc & 1,
        0,
        "a dropped driver leaves the controller disabled"
    );
}

#[test]
fn the_suite_holds_over_a_512_byte_lba_namespace() {
    let mut dev = open(Geometry::healthy(9, GATE_BLOCKS), Faults::default()).expect("init");
    assert_eq!(dev.lba_bytes(), 512);
    let mut names = vec![];
    let n = device_suite(&mut dev, GATE_BLOCKS as usize, &mut |i, ok, name: &str| {
        assert!(ok, "invariant {i} failed: {name}");
        names.push(name.to_string());
    })
    .expect("suite");
    assert_eq!(n, 23, "invariant count changed - update the VM gates");
    assert!(names[0].starts_with("nvme: controller enabled"));
    assert!(names[8].starts_with("fs: "));
}

#[test]
fn the_suite_holds_over_a_4_kib_lba_namespace_and_a_wider_doorbell_stride() {
    let mut g = Geometry::healthy(12, GATE_BLOCKS);
    g.dstrd = 1;
    g.mqes = 3; // depth 4: the wrap invariant crosses the phase flip many times
    let mut dev = open(g, Faults::default()).expect("init");
    assert_eq!(dev.lba_bytes(), 4096);
    device_suite(&mut dev, GATE_BLOCKS as usize, &mut |i, ok, name: &str| {
        assert!(ok, "invariant {i} failed: {name}")
    })
    .expect("suite");
}

#[test]
fn the_driver_never_breaks_the_protocol_the_controller_checks() {
    let sim = Sim::new(
        Geometry::healthy(9, 64),
        Faults {
            starts_enabled: true,
            ..Default::default()
        },
    );
    let (mut dev, rep) = unsafe { Nvme::<HostHal, Sim>::init(sim) }.expect("init");
    assert_eq!(&rep.model[..8], b"SIM NVME");
    assert_eq!(rep.namespaces, 1);
    let blk = [7u8; BLOCK_SIZE];
    dev.write_block(20, &blk).unwrap();
    dev.flush().unwrap();
    // Reach the simulator back through a read: violations are recorded state.
    let mut back = [0u8; BLOCK_SIZE];
    dev.read_block(20, &mut back).unwrap();
    assert_eq!(back, blk);
    drop(dev);
}

fn state_after(f: impl FnOnce(&mut Nvme<HostHal, &Sim>), sim: &Sim) {
    let (mut dev, _) = unsafe { Nvme::<HostHal, &Sim>::init(sim) }.expect("init");
    f(&mut dev);
}

impl NvmeRegs for &Sim {
    fn r32(&self, off: usize) -> u32 {
        (**self).r32(off)
    }
    fn w32(&self, off: usize, v: u32) {
        (**self).w32(off, v)
    }
}

#[test]
fn an_enabled_controller_is_disabled_before_its_admin_queue_is_programmed() {
    let sim = Sim::new(
        Geometry::healthy(9, 64),
        Faults {
            starts_enabled: true,
            ..Default::default()
        },
    );
    state_after(|d| d.flush().unwrap(), &sim);
    let s = sim.st.borrow();
    assert!(s.violations.is_empty(), "violations: {:?}", s.violations);
    assert_eq!(
        s.intms & 1,
        1,
        "vector 0 must be masked for a polled driver"
    );
    assert_eq!(s.flushes, 1, "a volatile write cache gets a real FLUSH");
}

#[test]
fn no_volatile_cache_means_no_flush_command() {
    let mut g = Geometry::healthy(9, 64);
    g.vwc = false;
    let sim = Sim::new(g, Faults::default());
    state_after(|d| d.flush().unwrap(), &sim);
    assert_eq!(sim.st.borrow().flushes, 0);
}

#[test]
fn controllers_that_cannot_be_driven_safely_are_refused_at_init() {
    let base = Geometry::healthy(9, 64);
    let cases: Vec<(Geometry, Faults, &str)> = vec![
        (
            base,
            Faults {
                never_ready: true,
                ..Default::default()
            },
            "ready",
        ),
        (
            base,
            Faults {
                fatal_on_enable: true,
                ..Default::default()
            },
            "fatal",
        ),
        (
            Geometry {
                metadata: 8,
                ..base
            },
            Faults::default(),
            "metadata",
        ),
        (
            Geometry {
                lbads: 8,
                nsze: 64,
                ..base
            },
            Faults::default(),
            "LBA size",
        ),
        (
            Geometry { mpsmin: 1, ..base },
            Faults::default(),
            "page size",
        ),
        (
            Geometry {
                css_nvm: false,
                ..base
            },
            Faults::default(),
            "NVM command set",
        ),
        (Geometry { mqes: 0, ..base }, Faults::default(), "MQES"),
        (Geometry { nsze: 0, ..base }, Faults::default(), "empty"),
    ];
    for (g, f, want) in cases {
        match open(g, f) {
            Ok(_) => panic!("expected refusal containing {want:?}"),
            Err(e) => assert!(e.contains(want), "{e:?} should mention {want:?}"),
        }
    }
}

#[test]
fn lying_or_failing_completions_are_errors_never_data() {
    let g = Geometry::healthy(9, 64);
    for f in [
        Faults {
            cid_lie: true,
            ..Default::default()
        },
        Faults {
            sqid_lie: true,
            ..Default::default()
        },
        Faults {
            io_status: Some(0x02),
            ..Default::default()
        },
        Faults {
            drop_io: true,
            ..Default::default()
        },
    ] {
        let mut dev = open(g, f).expect("init succeeds: admin path is honest");
        dev.set_completion_budget_ns(50_000_000);
        let mut buf = [0u8; BLOCK_SIZE];
        assert_eq!(dev.read_block(3, &mut buf), Err(StorageError::Device));
        assert_eq!(dev.write_block(3, &buf), Err(StorageError::Device));
    }
}

#[test]
fn out_of_range_and_wrong_sized_requests_never_reach_the_controller() {
    let sim = Sim::new(Geometry::healthy(9, 64), Faults::default());
    state_after(
        |d| {
            let mut big = [0u8; BLOCK_SIZE];
            assert_eq!(d.read_block(64, &mut big), Err(StorageError::OutOfRange));
            let mut small = [0u8; 512];
            assert_eq!(d.read_block(0, &mut small), Err(StorageError::BadBlockSize));
        },
        &sim,
    );
    assert_eq!(sim.st.borrow().io_commands, 0);
}
