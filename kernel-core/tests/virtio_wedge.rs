//! Hosted proof of the virtqueue wedge (ADR-267) against a simulated virtio-rng device on a fake
//! transport: a request the device never answers in time wedges the driver, the late used entry is
//! never harvested as the next request's, and after `reset` a completion at used index 0 is read.
//! `Virtqueue::poll_used_within` and `Virtqueue::reset` are exercised here through the driver.

use kernel_core::entropy::{EntropySource, VirtioRng, VIRTIO_ID_RNG};
use kernel_core::virtioblk::{Transport, VirtioHal};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const PAGE: usize = 4096;
/// The clock advances this much per look, so a 2 s budget is 2000 looks.
const TICK_NS: u64 = 1_000_000;

thread_local! {
    static CLOCK: Cell<u64> = const { Cell::new(0) };
    /// The device the clock lets run, and on which look it answers what it holds.
    static DEVICE: RefCell<Option<Rc<RefCell<Dev>>>> = const { RefCell::new(None) };
}

struct HostHal;
impl VirtioHal for HostHal {
    fn now_ns() -> u64 {
        DEVICE.with(|d| {
            if let Some(dev) = d.borrow().as_ref() {
                dev.borrow_mut().tick();
            }
        });
        CLOCK.with(|c| {
            c.set(c.get() + TICK_NS);
            c.get()
        })
    }
    fn alloc_frame() -> Option<usize> {
        let l = std::alloc::Layout::from_size_align(PAGE, PAGE).unwrap();
        // SAFETY: non-zero layout; leaked for the test's life (a device frame).
        let p = unsafe { std::alloc::alloc_zeroed(l) };
        (!p.is_null()).then_some(p as usize)
    }
    fn barrier() {}
}

/// How the simulated device answers a request it was notified of.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// During the notify, as QEMU's does.
    Now,
    /// On the clock look `after` looks from the notify: later than any budget the test sets.
    Late { after: u64 },
}

struct Dev {
    mode: Mode,
    status: u32,
    desc: usize,
    avail: usize,
    used: usize,
    qsize: u16,
    /// The device's own view of the rings: avail entries consumed, used entries written.
    seen_avail: u16,
    used_idx: u16,
    /// Requests notified and not answered yet, with the look they are due on.
    pending: Vec<u64>,
    looks: u64,
    /// Each answer fills with a fresh byte, so two answers never look alike.
    serial: u8,
    resets: u32,
}

impl Dev {
    fn tick(&mut self) {
        self.looks += 1;
        while self.pending.first().is_some_and(|&due| due <= self.looks) {
            self.pending.remove(0);
            self.answer_one();
        }
    }

    /// Take the next avail entry and complete it: fill its buffer, write a used element.
    fn answer_one(&mut self) {
        // SAFETY: the rings and buffers are frames the driver handed this device.
        unsafe {
            let avail_ring = (self.avail + 4) as *const u16;
            let head = avail_ring
                .add((self.seen_avail % self.qsize) as usize)
                .read_volatile();
            self.seen_avail = self.seen_avail.wrapping_add(1);
            let d = (self.desc + 16 * head as usize) as *const u64;
            let addr = d.read_volatile() as usize;
            let len = ((self.desc + 16 * head as usize + 8) as *const u32).read_volatile();
            self.serial = self.serial.wrapping_add(1);
            for i in 0..len as usize {
                ((addr + i) as *mut u8).write_volatile(self.serial ^ (i as u8).wrapping_mul(31));
            }
            let elem = (self.used + 4 + 8 * (self.used_idx % self.qsize) as usize) as *mut u32;
            elem.write_volatile(head as u32);
            elem.add(1).write_volatile(len);
            self.used_idx = self.used_idx.wrapping_add(1);
            ((self.used + 2) as *mut u16).write_volatile(self.used_idx);
        }
    }
}

#[derive(Clone)]
struct Fake(Rc<RefCell<Dev>>);

impl Transport for Fake {
    fn identity(&self) -> (u32, u32) {
        (2, VIRTIO_ID_RNG)
    }
    unsafe fn device_features(&self, sel: u32) -> u32 {
        if sel == 1 {
            1 // VIRTIO_F_VERSION_1 (bit 32)
        } else {
            0
        }
    }
    unsafe fn set_driver_features(&self, _sel: u32, _value: u32) {}
    unsafe fn status(&self) -> u32 {
        self.0.borrow().status
    }
    unsafe fn set_status(&self, value: u32) {
        let mut d = self.0.borrow_mut();
        if value == 0 {
            // A device reset ends everything it held and restarts its view of the rings.
            d.pending.clear();
            d.seen_avail = 0;
            d.used_idx = 0;
            d.resets += 1;
        }
        d.status = value;
    }
    unsafe fn select_queue(&self, _queue: u16) {}
    unsafe fn queue_num_max(&self) -> u32 {
        32
    }
    unsafe fn set_queue_num(&self, size: u32) {
        self.0.borrow_mut().qsize = size as u16;
    }
    unsafe fn set_queue_addrs(&self, desc: u64, avail: u64, used: u64) {
        let mut d = self.0.borrow_mut();
        (d.desc, d.avail, d.used) = (desc as usize, avail as usize, used as usize);
    }
    unsafe fn queue_ready(&self) {}
    unsafe fn notify(&self, _queue: u16) {
        let mut d = self.0.borrow_mut();
        match d.mode {
            Mode::Now => d.answer_one(),
            Mode::Late { after } => {
                let due = d.looks + after;
                d.pending.push(due);
            }
        }
    }
    unsafe fn config_u64(&self, _off: usize) -> u64 {
        0
    }
}

fn device(mode: Mode) -> (Rc<RefCell<Dev>>, VirtioRng<HostHal, Fake>) {
    let dev = Rc::new(RefCell::new(Dev {
        mode,
        status: 0,
        desc: 0,
        avail: 0,
        used: 0,
        qsize: 0,
        seen_avail: 0,
        used_idx: 0,
        pending: vec![],
        looks: 0,
        serial: 0,
        resets: 0,
    }));
    DEVICE.with(|d| *d.borrow_mut() = Some(dev.clone()));
    let rng = unsafe { VirtioRng::<HostHal, Fake>::init(Fake(dev.clone())) }.expect("init");
    (dev, rng)
}

/// Bytes the driver published to the device so far (the avail index).
fn published(dev: &Rc<RefCell<Dev>>) -> u16 {
    let avail = dev.borrow().avail;
    // SAFETY: the avail ring is the driver's frame, alive for the test.
    unsafe { ((avail + 2) as *const u16).read_volatile() }
}

#[test]
fn a_healthy_device_answers_each_draw_with_fresh_bytes() {
    let (_dev, mut rng) = device(Mode::Now);
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    rng.fill(&mut a).unwrap();
    rng.fill(&mut b).unwrap();
    assert_ne!(a, b);
    assert!(!rng.is_wedged());
}

/// The hazard: a draw the device answers after the budget leaves its used entry in the ring.
/// Wedged, the driver publishes nothing more; without the wedge, the next draw would harvest that
/// late entry as its own and hand out bytes from the old request while its own is still pending.
#[test]
fn a_late_answer_is_never_harvested_as_the_next_draw() {
    // Two healthy draws first, so the driver's `last_used` is past 0 when the device resets: a
    // reset that did not restart it would read every new completion as old.
    let (dev, mut rng) = device(Mode::Now);
    let mut b = [0u8; 32];
    rng.fill(&mut b).unwrap();
    rng.fill(&mut b).unwrap();
    // Then one answered 2500 looks after the notify: past the 2 s (2000-look) budget.
    dev.borrow_mut().mode = Mode::Late { after: 2_500 };
    assert!(rng.fill(&mut b).is_err(), "the draw timed out on the clock");
    assert!(rng.is_wedged(), "an unanswered request wedges the driver");
    let before = published(&dev);
    // Let the late answer land, as it would on a device that was only slow.
    for _ in 0..1_000 {
        HostHal::now_ns();
    }
    assert_eq!(
        dev.borrow().used_idx,
        3,
        "the late completion did land in the ring"
    );
    assert!(
        rng.fill(&mut b).is_err(),
        "a wedged driver hands out nothing"
    );
    assert_eq!(published(&dev), before, "a wedged driver publishes nothing");

    // The reset restarts both sides' indices; the next draw is answered at used index 0 again and
    // read as this draw's own completion.
    dev.borrow_mut().mode = Mode::Now;
    unsafe { rng.reset() }.expect("reset");
    assert!(!rng.is_wedged());
    assert_eq!(
        dev.borrow().resets,
        2,
        "init and the reset each reset the device"
    );
    let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
    rng.fill(&mut x).unwrap();
    rng.fill(&mut y).unwrap();
    assert_ne!(x, y);
    assert_eq!(
        dev.borrow().used_idx,
        2,
        "two completions since the reset, from index 0"
    );
}

/// A device that is merely slower than its first look, but inside the budget, is waited for on
/// the clock and never wedges.
#[test]
fn an_answer_inside_the_time_budget_is_taken() {
    let (_dev, mut rng) = device(Mode::Late { after: 500 });
    let mut b = [0u8; 32];
    rng.fill(&mut b).unwrap();
    assert!(!rng.is_wedged());
}
