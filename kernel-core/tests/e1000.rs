//! Hosted proof of the e1000 driver (ADR-224) against a simulated 8254x with an ARP-answering
//! gateway behind it, plus named misbehaviours.

use kernel_core::e1000::*;
use kernel_core::virtioblk::VirtioHal;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

struct HostHal;
thread_local! { static CLOCK: AtomicU64 = const { AtomicU64::new(0) }; }
impl VirtioHal for HostHal {
    fn now_ns() -> u64 {
        CLOCK.with(|c| c.fetch_add(1_000_000, Ordering::Relaxed))
    }
    fn alloc_frame() -> Option<usize> {
        let l = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
        // SAFETY: non-zero layout; leaked for the test's life.
        let p = unsafe { std::alloc::alloc_zeroed(l) };
        (!p.is_null()).then_some(p as usize)
    }
    fn barrier() {}
}

const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

#[derive(Default, Clone, Copy)]
struct Faults {
    reset_sticks: bool,
    no_link: bool,
    rx_error: bool,
    tx_never_done: bool,
    mac_only_in_eeprom: bool,
    no_gateway: bool,
}

struct St {
    r: std::collections::HashMap<usize, u32>,
    f: Faults,
    tdh: u32,
    rdh: u32,
    sent: usize,
}

struct Sim(RefCell<St>);

impl Sim {
    fn new(f: Faults) -> Self {
        let mut r = std::collections::HashMap::new();
        if !f.mac_only_in_eeprom {
            r.insert(
                REG_RAL0,
                u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]),
            );
            r.insert(
                REG_RAH0,
                u16::from_le_bytes([MAC[4], MAC[5]]) as u32 | 0x8000_0000,
            );
        }
        Sim(RefCell::new(St {
            r,
            f,
            tdh: 0,
            rdh: 0,
            sent: 0,
        }))
    }

    fn deliver(s: &mut St, frame: &[u8]) {
        let rdt = *s.r.get(&REG_RDT).unwrap_or(&0);
        if s.rdh == rdt {
            return; // no descriptor owned by the device: drop, like hardware
        }
        let ring =
            *s.r.get(&REG_RDBAL).unwrap() as usize | (*s.r.get(&REG_RDBAH).unwrap() as usize) << 32;
        let d = ring + s.rdh as usize * 16;
        unsafe {
            let buf = (d as *const u64).read_volatile() as usize;
            std::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len());
            ((d + 8) as *mut u16).write_volatile(frame.len() as u16);
            ((d + 13) as *mut u8).write_volatile(if s.f.rx_error { 0x01 } else { 0 });
            ((d + 12) as *mut u8).write_volatile(0x03);
        }
        s.rdh = (s.rdh + 1) % RING as u32;
    }
}

impl Regs for Sim {
    fn r32(&self, off: usize) -> u32 {
        let s = self.0.borrow();
        match off {
            REG_STATUS => {
                if s.f.no_link {
                    0
                } else {
                    2
                }
            }
            REG_CTRL => {
                let c = *s.r.get(&REG_CTRL).unwrap_or(&0);
                if s.f.reset_sticks {
                    c
                } else {
                    c & !0x0400_0000
                }
            }
            REG_EERD => {
                let v = *s.r.get(&REG_EERD).unwrap_or(&0);
                let word = (v >> 8) as usize & 0xFF;
                let w = u16::from_le_bytes([MAC[2 * word], MAC[2 * word + 1]]) as u32;
                v | 0x10 | w << 16
            }
            o => *s.r.get(&o).unwrap_or(&0),
        }
    }
    fn w32(&self, off: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        s.r.insert(off, v);
        if off != REG_TDT || s.f.tx_never_done {
            return;
        }
        let ring =
            *s.r.get(&REG_TDBAL).unwrap() as usize | (*s.r.get(&REG_TDBAH).unwrap() as usize) << 32;
        while s.tdh != v {
            let d = ring + s.tdh as usize * 16;
            let (buf, lower) = unsafe {
                (
                    (d as *const u64).read_volatile() as usize,
                    ((d + 8) as *const u32).read_volatile(),
                )
            };
            let len = (lower & 0xFFFF) as usize;
            let f = unsafe { std::slice::from_raw_parts(buf as *const u8, len) }.to_vec();
            unsafe { ((d + 12) as *mut u32).write_volatile(1) };
            s.tdh = (s.tdh + 1) % RING as u32;
            s.sent += 1;
            // Gateway: answer an ARP request for 10.0.2.2.
            if !s.f.no_gateway
                && len >= 42
                && f[12..14] == [0x08, 0x06]
                && f[20..22] == [0, 1]
                && f[38..42] == [10, 0, 2, 2]
            {
                let mut r = vec![0u8; 42];
                r[0..6].copy_from_slice(&f[6..12]);
                r[6..12].copy_from_slice(&GW_MAC);
                r[12..14].copy_from_slice(&[0x08, 0x06]);
                r[14..20].copy_from_slice(&f[14..20]);
                r[20..22].copy_from_slice(&[0, 2]);
                r[22..28].copy_from_slice(&GW_MAC);
                r[28..32].copy_from_slice(&[10, 0, 2, 2]);
                r[32..38].copy_from_slice(&f[22..28]);
                r[38..42].copy_from_slice(&f[28..32]);
                // A stray frame first: the driver must skip what it did not ask for.
                Sim::deliver(&mut s, &[0xAAu8; 60]);
                Sim::deliver(&mut s, &r);
            }
        }
    }
}

fn open(f: Faults) -> Result<E1000<HostHal, Sim>, &'static str> {
    unsafe { E1000::<HostHal, Sim>::init(Sim::new(f)) }
}

#[test]
fn the_suite_holds_over_a_healthy_controller() {
    let dev = open(Faults::default()).expect("init");
    assert_eq!(dev.mac(), MAC);
    let n = device_suite(&dev, &mut |i, ok, name: &str| assert!(ok, "{i}: {name}")).unwrap();
    assert_eq!(n, 6, "invariant count changed - update the VM gates");
}

#[test]
fn the_mac_comes_from_the_eeprom_when_the_receive_address_is_not_loaded() {
    assert_eq!(
        open(Faults {
            mac_only_in_eeprom: true,
            ..Default::default()
        })
        .unwrap()
        .mac(),
        MAC
    );
}

#[test]
fn a_reset_that_never_completes_is_refused() {
    let e = open(Faults {
        reset_sticks: true,
        ..Default::default()
    })
    .err()
    .unwrap();
    assert!(e.contains("reset"), "{e}");
}

#[test]
fn errored_frames_are_never_handed_up_and_no_answer_is_a_timeout() {
    let dev = open(Faults {
        rx_error: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(dev.arp_resolve([10, 0, 2, 2]), Err(E1000Error::Timeout));
}

#[test]
fn a_transmit_that_never_completes_is_a_timeout() {
    let dev = open(Faults {
        tx_never_done: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(dev.send(&[0u8; 60]), Err(E1000Error::Timeout));
}

#[test]
fn no_link_fails_the_suite_at_its_named_invariant() {
    let dev = open(Faults {
        no_link: true,
        ..Default::default()
    })
    .unwrap();
    let err = device_suite(&dev, &mut |_, _, _: &str| {}).unwrap_err();
    assert_eq!(err.0, 4, "link is the first wire-group invariant");
}

#[test]
fn a_network_without_the_gateway_gets_the_local_group_only() {
    // A real LAN or the VMware package (ADR-228): no 10.0.2.2, so no wire claims, and no failure.
    let dev = open(Faults {
        no_gateway: true,
        ..Default::default()
    })
    .unwrap();
    let n = device_suite(&dev, &mut |i, ok, name: &str| assert!(ok, "{i}: {name}")).unwrap();
    assert_eq!(n, 3, "local group count changed - update the gates");
}
