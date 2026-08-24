//! Host proofs for the shared block-device suite (REQ-DRV-004, ADR-036), plus the
//! ALET-P1-019 driver-depth proofs: a host-side virtio-mmio DEVICE MODEL that can LIE.
//!
//! The MMIO/virtqueue half of kernel_core::virtioblk needs a real transport, which the
//! aarch64 and RISC-V VM gates provide against QEMU's honest device. The proofs below go
//! further: a synchronous mock transport runs the DEVICE side itself and is programmed to
//! MISBEHAVE - short completions, lying used-ring ids and lengths, garbage status bytes,
//! silent devices - so the driver's responses are held to their contract on the host, where
//! an exhaustive sweep costs nothing.

use kernel_core::storage::{BlockDevice, MemBlockDevice, StorageError, BLOCK_SIZE};
use kernel_core::virtioblk::{device_suite, InitReport, Transport, VirtioBlk, VirtioHal};
use kernel_core::virtioblk::{SECTORS_PER_BLOCK, SECTOR_SIZE};
use std::cell::RefCell;
extern crate alloc;
use std::rc::Rc;

/// Capacity of the mock medium, in 512-byte sectors (a 1 MiB disk).
const CAP: u64 = 2048;

/// The geometry both VM gates attach: a 1 MiB image = 2048 sectors = 256 blocks.
const GATE_BLOCKS: usize = 256;

#[test]
fn the_sector_geometry_matches_a_4_kib_block() {
    assert_eq!(SECTOR_SIZE, 512);
    assert_eq!(SECTORS_PER_BLOCK, 8);
    assert_eq!(SECTORS_PER_BLOCK as usize * SECTOR_SIZE, BLOCK_SIZE);
    assert_eq!(1024 * 1024 / BLOCK_SIZE, GATE_BLOCKS);
}
#[test]
fn the_shared_suite_proves_seventeen_invariants_over_a_device() {
    let dev = MemBlockDevice::new(GATE_BLOCKS);
    let mut seen: alloc::vec::Vec<(usize, alloc::string::String)> = alloc::vec![];
    let n = device_suite(dev, GATE_BLOCKS, |i, passed, name| {
        assert!(passed, "invariant {i} failed: {name}");
        seen.push((i, alloc::string::String::from(name)));
    })
    .expect("every invariant holds over a well-behaved device");

    assert_eq!(
        n, 21,
        "the suite's invariant count changed - update both VM gates"
    );
    assert_eq!(seen.len(), 21);
    for (idx, (i, _)) in seen.iter().enumerate() {
        assert_eq!(*i, idx + 1, "invariant numbering has a gap at {i}");
    }
    assert!(seen[0].1.starts_with("virtio-blk: device discovered"));
    assert!(
        seen[5].1.starts_with("fs: "),
        "group 6 should be the filesystem: {}",
        seen[5].1
    );
    assert!(seen[19].1.starts_with("fs: "));
    assert!(seen[20].1.contains("capability-gated I/O"));
}

#[test]
fn a_device_of_the_wrong_geometry_is_refused_before_any_data_is_trusted() {
    let dev = MemBlockDevice::new(GATE_BLOCKS / 2);
    let mut last = 0usize;
    let err = device_suite(dev, GATE_BLOCKS, |i, _, _| last = i)
        .expect_err("a mis-sized device must be refused");
    assert_eq!(
        err.0, 2,
        "geometry must be the SECOND invariant, before any I/O"
    );
    assert!(err.1.contains("capacity"), "wrong invariant: {}", err.1);
    assert_eq!(last, 2, "the failing invariant must still be logged");
}

#[test]
fn a_device_too_small_for_the_journal_fails_closed_rather_than_writing_past_its_end() {
    let dev = MemBlockDevice::new(4);
    let err = device_suite(dev, 4, |_, _, _| {}).expect_err("a device this small cannot pass");
    assert!(err.0 >= 1);
}
// ---------------------------------------------------------------------------
// The host-side virtio-mmio DEVICE MODEL (ALET-P1-019): registers from an array,
// a synchronous notify() that runs the whole device side, and a programmable
// Faults record so a test can make the device LIE in exactly one way at a time.
// ---------------------------------------------------------------------------

const PAGE: usize = 4096;

/// Host stand-in for the CPU seam: a leaked, zeroed, page-aligned buffer IS a
/// DMA-able identity-mapped frame inside a hosted process (VA == PA trivially).
struct HostHal;
impl VirtioHal for HostHal {
    fn alloc_frame() -> Option<usize> {
        unsafe {
            let layout = std::alloc::Layout::from_size_align(PAGE, PAGE).unwrap();
            let p = std::alloc::alloc_zeroed(layout);
            if p.is_null() {
                None
            } else {
                Some(p as usize)
            }
        }
    }
    fn barrier() {}
}

/// What the emulated device may do WRONG this run. Every field is one named
/// malformation: a silent device, a completion for another descriptor's request,
/// a short/padded completion byte-count, or a garbage status byte.
#[derive(Clone, Default)]
struct Faults {
    skip_completion: bool,
    id_lie: Option<u32>,
    used_len: Option<u32>,
    status_byte: Option<u8>,
}

struct MockDev {
    /// Mutable device state, behind a RefCell: the Transport trait takes &self
    /// for reads, and the device mutates itself while serving them.
    st: RefCell<MockState>,
    faults: SharedFaults,
}

struct MockState {
    regs: [u32; 64],
    feat_lo: u32,
    feat_hi: u32,
    cfg_capacity: u64,
    q: Option<(usize, usize, usize)>, // desc, avail, used
    qsize: u32,
    disk: Vec<u8>,
    dev_avail: u16,
    dev_used: u16,
}

type SharedFaults = Rc<RefCell<Faults>>;

impl MockDev {
    fn rd16(a: usize) -> u16 {
        unsafe { (a as *const u16).read_volatile() }
    }
    fn wr8(a: usize, v: u8) {
        unsafe { (a as *mut u8).write_volatile(v) }
    }
    fn rd32(a: usize) -> u32 {
        unsafe { (a as *const u32).read_volatile() }
    }
    fn wr32(a: usize, v: u32) {
        unsafe { (a as *mut u32).write_volatile(v) }
    }
    fn wr16(a: usize, v: u16) {
        unsafe { (a as *mut u16).write_volatile(v) }
    }

    fn new(capacity_sectors: u64, faults: SharedFaults) -> Self {
        MockDev {
            st: RefCell::new(MockState {
                regs: [0; 64],
                feat_lo: 1 << 9, // VIRTIO_BLK_F_FLUSH offered
                feat_hi: 1,      // VIRTIO_F_VERSION_1 (bit 0 of the high half)
                cfg_capacity: capacity_sectors,
                q: None,
                qsize: 8,
                disk: vec![0u8; capacity_sectors as usize * 512],
                dev_avail: 0,
                dev_used: 0,
            }),
            faults,
        }
    }
}

fn mk_driver(
    capacity_sectors: u64,
    faults: SharedFaults,
) -> (VirtioBlk<HostHal, MockDev>, InitReport, SharedFaults) {
    let dev = MockDev::new(capacity_sectors, faults.clone());
    let (blk, report) = unsafe { VirtioBlk::init(dev) }.expect("mock device inits");
    (blk, report, faults)
}
impl Transport for MockDev {
    fn identity(&self) -> (u32, u32) {
        let s = self.st.borrow();
        // Version register at 0x004, DeviceId at 0x008 (virtio-mmio v2 layout);
        // register offsets are word-addressed in the regs array.
        let version = s.regs[0x004 >> 2];
        let device_id = s.regs[0x008 >> 2];
        (version, device_id)
    }
    unsafe fn device_features(&self, sel: u32) -> u32 {
        let s = self.st.borrow();
        if sel == 0 {
            s.feat_lo
        } else {
            s.feat_hi
        }
    }
    unsafe fn set_driver_features(&self, _sel: u32, _value: u32) {}
    unsafe fn status(&self) -> u32 {
        self.st.borrow().regs[0x070 / 4]
    }
    unsafe fn set_status(&self, value: u32) {
        self.st.borrow_mut().regs[0x070 / 4] = value;
    }
    unsafe fn select_queue(&self, _queue: u16) {}
    unsafe fn queue_num_max(&self) -> u32 {
        8
    }
    unsafe fn set_queue_num(&self, size: u32) {
        self.st.borrow_mut().qsize = size;
    }
    unsafe fn set_queue_addrs(&self, desc: u64, avail: u64, used: u64) {
        let mut s = self.st.borrow_mut();
        s.q = Some((desc as usize, avail as usize, used as usize));
    }
    unsafe fn queue_ready(&self) {}
    unsafe fn config_u64(&self, _off: usize) -> u64 {
        self.st.borrow().cfg_capacity
    }

    // THE DEVICE SIDE of one request: consume an avail entry, walk its chain,
    // service the data against the backing medium, write the status byte, and
    // publish the used-ring entry - honouring this run's programmed fault.
    unsafe fn notify(&self, _queue: u16) {
        let f = self.faults.borrow().clone();
        if f.skip_completion {
            // A silent device never advances used.idx: the driver's poll bound fires.
            return;
        }
        let mut s = self.st.borrow_mut();
        let (desc, avail, used) = match s.q {
            Some(q) => q,
            None => return,
        };

        let head = MockDev::rd16(avail + 4 + 2 * (s.dev_avail % s.qsize as u16) as usize);
        s.dev_avail = s.dev_avail.wrapping_add(1);

        fn rd_desc(desc: usize, i: usize) -> (u64, u32, u16, u16) {
            unsafe {
                let base = desc + i * 16;
                (
                    (base as *const u64).read_volatile(),
                    (base as *const u32).add(2).read_volatile(),
                    (base as *const u16).add(6).read_volatile(),
                    (base as *const u16).add(7).read_volatile(),
                )
            }
        }

        let (h_addr_u64, _, _, mut next) = rd_desc(desc, head as usize);
        let h_addr = h_addr_u64 as usize;
        let rtype = MockDev::rd32(h_addr);
        let mut sector_bytes = [0u8; 8];
        for (i, sb) in sector_bytes.iter_mut().enumerate() {
            *sb = ((h_addr + 8 + i) as *const u8).read_volatile();
        }
        let sector = u64::from_le_bytes(sector_bytes);

        let mut data_addr = 0usize;
        let mut status_addr = 0usize;
        // Classify by CHAIN POSITION: the first descriptor after the header is the
        // data buffer (device-writable for a READ, device-readable for a WRITE),
        // and the LAST descriptor is always the one-byte status.
        while next != 0 {
            let (addr, _len, _flags, nxt) = rd_desc(desc, next as usize);
            if nxt == 0 {
                status_addr = addr as usize;
            } else {
                data_addr = addr as usize;
            }
            next = nxt;
        }

        if rtype == 1 {
            for i in 0..BLOCK_SIZE {
                let b = ((data_addr + i) as *const u8).read_volatile();
                s.disk[sector as usize * 512 + i] = b;
            }
        } else if rtype == 0 {
            for i in 0..BLOCK_SIZE {
                s.disk[sector as usize * 512 + i] = ((data_addr + i) as *const u8).read_volatile();
            }
        }

        let status_byte = f.status_byte.unwrap_or(VIRTIO_S_OK);
        MockDev::wr8(status_addr, status_byte);

        // The completion length a device HONESTLY reports is the status byte plus
        // the whole data buffer for a READ - unless a fault overrides it.
        let honest_len = if rtype == 0 { BLOCK_SIZE as u32 + 1 } else { 1 };
        let wlen = f.used_len.unwrap_or(honest_len);
        let entry = used + 4 + 8 * (s.dev_used % s.qsize as u16) as usize;
        MockDev::wr32(entry, f.id_lie.unwrap_or(head as u32));
        MockDev::wr32(entry + 4, wlen);
        s.dev_used = s.dev_used.wrapping_add(1);
        MockDev::wr16(used + 2, s.dev_used);
    }
}

const VIRTIO_S_OK: u8 = 0;

#[test]
fn clean_round_trip_over_the_mock_device() {
    let (mut blk, _r, _f) = mk_driver(CAP, Rc::new(RefCell::new(Faults::default())));
    let pattern: [u8; BLOCK_SIZE] = core::array::from_fn(|i| (i * 7 + 3) as u8);
    blk.write_block(5, &pattern).expect("write");
    let mut buf = [0u8; BLOCK_SIZE];
    blk.read_block(5, &mut buf).expect("read");
    assert_eq!(buf, pattern);
}

#[test]
fn a_short_read_is_refused_and_never_touches_the_caller_buffer() {
    // THE partial-IO contract: a device that completes a READ having written
    // only HALF the block is an error - and the caller's buffer must hold
    // exactly what it held before, never half-old-half-garbage bytes.
    let faults = Rc::new(RefCell::new(Faults {
        used_len: Some((BLOCK_SIZE / 2 + 1) as u32),
        ..Default::default()
    }));
    let (blk, report, _f) = mk_driver(CAP, faults);
    assert!(report.flush_ok);

    let sentinel = [0xA5u8; BLOCK_SIZE];
    let mut buf = sentinel;
    match blk.read_block(3, &mut buf) {
        Err(StorageError::Device) => {}
        other => panic!("expected Err(Device), got {:?}", other.map(|_| ())),
    }
    assert!(
        buf == sentinel,
        "a refused short read must not write the buffer"
    );
}

#[test]
fn a_zero_length_completion_is_refused() {
    // The device wrote NOTHING - not even the status byte.
    let faults = Rc::new(RefCell::new(Faults {
        used_len: Some(0),
        ..Default::default()
    }));
    let (blk, _r, _f) = mk_driver(CAP, faults);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(1, &mut buf),
        Err(StorageError::Device)
    ));
}

#[test]
fn an_absurdly_large_completion_length_is_refused() {
    let faults = Rc::new(RefCell::new(Faults {
        used_len: Some(u32::MAX),
        ..Default::default()
    }));
    let (blk, _r, _f) = mk_driver(CAP, faults);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(2, &mut buf),
        Err(StorageError::Device)
    ));
}

#[test]
fn garbage_status_is_refused_even_with_an_exact_byte_count() {
    // Right LENGTH, nonsense status byte: length alone must never be mistaken
    // for success.
    let faults = Rc::new(RefCell::new(Faults {
        status_byte: Some(0x7f),
        ..Default::default()
    }));
    let (blk, _r, _f) = mk_driver(CAP, faults);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(4, &mut buf),
        Err(StorageError::Device)
    ));
}

#[test]
fn a_completion_for_another_descriptor_is_refused() {
    // The used-ring entry names descriptor 5 while this driver posted head 0: a
    // completion for a request nobody made must be refused before the status
    // byte is believed.
    let faults = Rc::new(RefCell::new(Faults {
        id_lie: Some(5),
        ..Default::default()
    }));
    let (blk, _r, _f) = mk_driver(CAP, faults);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(6, &mut buf),
        Err(StorageError::Device)
    ));
}

#[test]
fn a_flush_completion_with_the_wrong_byte_count_is_refused() {
    // FLUSH chains carry no data descriptor: the honest completion writes one
    // byte. Two means the device padded it - refuse.
    let faults = Rc::new(RefCell::new(Faults {
        used_len: Some(2),
        ..Default::default()
    }));
    let (mut blk, _r, _f) = mk_driver(CAP, faults);
    assert!(matches!(blk.flush(), Err(StorageError::Device)));
}

#[test]
fn sub_sector_requests_are_refused_at_the_api_boundary() {
    // The BlockDevice seam is block-granular BY CONTRACT: partial-block calls
    // get named refusals before any descriptor exists. And out-of-range block
    // numbers are refused by the geometry check.
    let (mut blk, _r, _f) = mk_driver(CAP, Rc::new(RefCell::new(Faults::default())));
    let mut small = [0u8; 100];
    assert!(matches!(
        blk.read_block(0, &mut small),
        Err(StorageError::BadBlockSize)
    ));
    let big = [0u8; BLOCK_SIZE * 2];
    assert!(matches!(
        blk.write_block(0, &big),
        Err(StorageError::BadBlockSize)
    ));
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(9999, &mut buf),
        Err(StorageError::OutOfRange)
    ));
}

#[test]
fn a_silent_device_hits_the_poll_bound_and_still_returns() {
    let faults = Rc::new(RefCell::new(Faults {
        skip_completion: true,
        ..Default::default()
    }));
    let (blk, _r, _f) = mk_driver(CAP, faults);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(matches!(
        blk.read_block(0, &mut buf),
        Err(StorageError::Device)
    ));
}
#[test]
fn a_fuzzed_completion_surface_never_yields_data_without_an_exact_match() {
    // A seeded sweep over the malformation matrix: under EVERY malformed mode
    // both the write and the read must surface Err(Device), and only an EXACT
    // honest completion may fill the caller buffer.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for iter in 0..400usize {
        let pick = next() % 6;
        let faults = match pick {
            0 => Faults {
                used_len: Some((next() % 4096) as u32),
                ..Default::default()
            },
            1 => Faults {
                status_byte: Some((next() % 256) as u8),
                ..Default::default()
            },
            2 => Faults {
                id_lie: Some((next() % 16) as u32),
                ..Default::default()
            },
            3 => Faults {
                skip_completion: true,
                ..Default::default()
            },
            4 => Faults {
                used_len: Some(0),
                status_byte: Some(0x60),
                ..Default::default()
            },
            _ => Faults::default(), // honest completion: must SUCCEED
        };
        let honest = pick == 5;
        let f = Rc::new(RefCell::new(faults));
        let (mut blk, _r, _handle) = mk_driver(CAP, f.clone());
        let pattern: [u8; BLOCK_SIZE] =
            core::array::from_fn(|i| ((i as u64 + iter as u64) % 251) as u8);

        eprintln!("FUZZ iter={iter} pick={pick}");
        let wrote = blk.write_block(3, &pattern).is_ok();
        eprintln!("FUZZ iter={iter} wrote={wrote}");
        let mut buf = [0u8; BLOCK_SIZE]; // the zero sentinel a refused read must preserve
        let read_ok = blk.read_block(3, &mut buf).is_ok();
        eprintln!("FUZZ iter={iter} read_ok={read_ok}");

        if honest {
            assert!(wrote && read_ok, "iter {iter}: the honest case must pass");
            assert_eq!(
                buf, pattern,
                "iter {iter}: honest read returned wrong bytes"
            );
        } else {
            // A fault may ACCIDENTALLY produce an honest completion (e.g. a
            // status override that lands on OK): such a run is simply a valid
            // completion and must carry the right bytes. What is FORBIDDEN is a
            // HALF-completed run - one op succeeding while the other refuses -
            // or a refused read leaving anything in the buffer.
            if wrote && read_ok {
                assert_eq!(
                    buf, pattern,
                    "iter {iter}: accepted completion returned wrong bytes"
                );
            } else {
                assert!(
                    !wrote && !read_ok,
                    "iter {iter}: a malformed run half-succeeded (wrote={wrote}, read_ok={read_ok})"
                );
                assert!(
                    buf.iter().all(|&b| b == 0),
                    "iter {iter}: refused reads leaked bytes into the buffer"
                );
            }
        }
    }
}
