//! Hosted proof of the AHCI driver (ADR-225) against a simulated HBA with a boot disk and a
//! scratch disk, plus named misbehaviours.

use kernel_core::ahci::*;
use kernel_core::storage::{BlockDevice, StorageError, BLOCK_SIZE};
use kernel_core::virtioblk::VirtioHal;
use std::cell::RefCell;
use std::collections::HashMap;
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

#[derive(Clone, Copy, Default)]
struct Faults {
    task_file_error: bool,
    short_transfer: bool,
    no_lba48: bool,
    stays_busy: bool,
}

struct Disk {
    serial: &'static str,
    sector: usize,
    bytes: Vec<u8>,
    writes: usize,
}

struct St {
    r: HashMap<usize, u32>,
    disks: Vec<Disk>,
    f: Faults,
}

struct Sim(RefCell<St>);

fn ident(d: &Disk, f: Faults) -> [u8; 512] {
    let mut id = [0u8; 512];
    let mut put = |w: usize, v: u16| id[2 * w..2 * w + 2].copy_from_slice(&v.to_le_bytes());
    put(83, if f.no_lba48 { 0 } else { 1 << 10 });
    let n = (d.bytes.len() / d.sector) as u64;
    for i in 0..4 {
        put(100 + i, (n >> (16 * i)) as u16);
    }
    if d.sector == 4096 {
        put(106, 0x4000 | 1 << 12);
        put(117, 2048);
    }
    let mut s = [b' '; 20];
    s[..d.serial.len()].copy_from_slice(d.serial.as_bytes());
    for i in 0..10 {
        id[20 + 2 * i] = s[2 * i + 1];
        id[20 + 2 * i + 1] = s[2 * i];
    }
    id
}

impl Sim {
    fn new(f: Faults, scratch_sector: usize) -> Self {
        let mut boot = vec![0u8; 64 * 512];
        boot[510] = 0x55;
        boot[511] = 0xAA;
        let mut r = HashMap::new();
        r.insert(HBA_PI, 0b101); // ports 0 and 2 implemented
        for p in [0usize, 2] {
            let b = PORT_BASE + p * PORT_STRIDE;
            r.insert(b + PX_SSTS, 0x123);
            r.insert(b + PX_SIG, SIG_ATA);
        }
        Sim(RefCell::new(St {
            r,
            disks: vec![
                Disk {
                    serial: "QM00001",
                    sector: 512,
                    bytes: boot,
                    writes: 0,
                },
                Disk {
                    serial: "ALETHEIA-SCRATCH",
                    sector: scratch_sector,
                    bytes: vec![0; 1 << 20],
                    writes: 0,
                },
            ],
            f,
        }))
    }

    fn run(s: &mut St, port: usize) {
        let b = PORT_BASE + port * PORT_STRIDE;
        let disk = &mut s.disks[port / 2];
        let clb = s.r[&(b + PX_CLB)] as usize | (s.r[&(b + PX_CLBU)] as usize) << 32;
        unsafe {
            let h = clb as *mut u32;
            let dw0 = h.read_volatile();
            let ctba =
                h.add(2).read_volatile() as usize | (h.add(3).read_volatile() as usize) << 32;
            let fis = ctba as *const u8;
            assert_eq!(fis.read_volatile(), 0x27, "H2D register FIS type");
            assert_eq!(dw0 & 0x1F, 5, "CFL must be 5 dwords");
            let op = fis.add(2).read_volatile();
            let mut lba = 0u64;
            for i in 0..3 {
                lba |= (fis.add(4 + i).read_volatile() as u64) << (8 * i);
                lba |= (fis.add(8 + i).read_volatile() as u64) << (8 * (i + 3));
            }
            let count =
                fis.add(12).read_volatile() as usize | (fis.add(13).read_volatile() as usize) << 8;
            let prdtl = dw0 >> 16;
            let (dba, dbc) = if prdtl > 0 {
                (
                    ((ctba + 0x80) as *const u64).read_volatile() as usize,
                    ((ctba + 0x8C) as *const u32).read_volatile() as usize + 1,
                )
            } else {
                (0, 0)
            };
            let mut moved = dbc;
            match op {
                0xEC => {
                    std::ptr::copy_nonoverlapping(ident(disk, s.f).as_ptr(), dba as *mut u8, 512)
                }
                0x25 | 0x35 => {
                    let off = lba as usize * disk.sector;
                    let len = count * disk.sector;
                    assert_eq!(len, dbc, "PRD must cover the sector count");
                    assert_eq!(
                        op == 0x35,
                        dw0 & (1 << 6) != 0,
                        "W bit must match the direction"
                    );
                    if op == 0x25 {
                        std::ptr::copy_nonoverlapping(
                            disk.bytes[off..].as_ptr(),
                            dba as *mut u8,
                            len,
                        );
                    } else {
                        disk.writes += 1;
                        std::ptr::copy_nonoverlapping(
                            dba as *const u8,
                            disk.bytes[off..].as_mut_ptr(),
                            len,
                        );
                    }
                }
                0xEA => {}
                _ => panic!("unexpected ATA command {op:#x}"),
            }
            if s.f.short_transfer && op != 0xEC {
                moved /= 2;
            }
            h.add(1).write_volatile(moved as u32);
        }
        if s.f.task_file_error && port == 2 {
            s.r.insert(b + PX_IS, 1 << 30);
            s.r.insert(b + PX_TFD, 0x51);
        } else {
            s.r.insert(b + PX_TFD, 0x50);
        }
        s.r.insert(b + PX_CI, 0);
    }
}

impl Regs for Sim {
    fn r32(&self, off: usize) -> u32 {
        let s = self.0.borrow();
        let v = *s.r.get(&off).unwrap_or(&0);
        if s.f.stays_busy && off >= PORT_BASE && (off - PORT_BASE) % PORT_STRIDE == PX_TFD {
            return 0x80;
        }
        v
    }
    fn w32(&self, off: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        if off >= PORT_BASE {
            let port = (off - PORT_BASE) / PORT_STRIDE;
            let reg = (off - PORT_BASE) % PORT_STRIDE;
            match reg {
                PX_IS | PX_SERR => {
                    s.r.insert(off, 0);
                    return;
                }
                PX_CMD => {
                    // Engines report running exactly when enabled.
                    let mut c = v & !(1 << 14 | 1 << 15);
                    if v & 1 != 0 {
                        c |= 1 << 15
                    }
                    if v & (1 << 4) != 0 {
                        c |= 1 << 14
                    }
                    s.r.insert(off, c);
                    return;
                }
                PX_CI if v & 1 != 0 => {
                    s.r.insert(off, 1);
                    Sim::run(&mut s, port);
                    return;
                }
                _ => {}
            }
        }
        s.r.insert(off, v);
    }
}

fn disks(sim: &Sim) -> Vec<AhciDisk<HostHal, &Sim>> {
    disk_ports(&sim)
        .into_iter()
        .map(|p| unsafe { AhciDisk::<HostHal, &Sim>::open(sim, p) }.expect("open"))
        .collect()
}

#[test]
fn the_suite_holds_and_never_writes_the_boot_disk() {
    for sector in [512, 4096] {
        let sim = Sim::new(Faults::default(), sector);
        let mut d = disks(&sim);
        assert_eq!(d.len(), 2);
        assert!(!d[0].is_scratch() && d[1].is_scratch());
        assert_eq!(d[1].sector_bytes(), sector);
        let n = device_suite(&mut d, 256, &mut |i, ok, name: &str| {
            assert!(ok, "{i}: {name}")
        })
        .unwrap();
        assert_eq!(n, 25, "invariant count changed - update the VM gates");
        drop(d);
        let st = sim.0.borrow();
        assert_eq!(st.disks[0].writes, 0, "the boot disk must never be written");
        assert!(st.disks[1].writes > 0);
    }
}

#[test]
fn ports_without_a_disk_signature_are_not_listed() {
    let sim = Sim::new(Faults::default(), 512);
    sim.0
        .borrow_mut()
        .r
        .insert(PORT_BASE + 2 * PORT_STRIDE + PX_SIG, 0xEB14_0101); // ATAPI
    assert_eq!(disk_ports(&&sim), vec![0]);
}

#[test]
fn unsafe_disks_are_refused_at_open() {
    let sim = Sim::new(
        Faults {
            no_lba48: true,
            ..Default::default()
        },
        512,
    );
    let e = unsafe { AhciDisk::<HostHal, &Sim>::open(&sim, 0) }
        .err()
        .unwrap();
    assert!(e.contains("48-bit"), "{e}");
    let sim = Sim::new(
        Faults {
            stays_busy: true,
            ..Default::default()
        },
        512,
    );
    let e = unsafe { AhciDisk::<HostHal, &Sim>::open(&sim, 0) }
        .err()
        .unwrap();
    assert!(e.contains("busy"), "{e}");
}

#[test]
fn errors_and_short_transfers_are_errors_never_data() {
    for f in [
        Faults {
            task_file_error: true,
            ..Default::default()
        },
        Faults {
            short_transfer: true,
            ..Default::default()
        },
    ] {
        let sim = Sim::new(f, 512);
        let d = unsafe { AhciDisk::<HostHal, &Sim>::open(&sim, 2) };
        let Ok(d) = d else { continue }; // a task-file error on IDENTIFY is also a refusal
        let mut b = [0u8; BLOCK_SIZE];
        assert_eq!(d.read_block(3, &mut b), Err(StorageError::Device));
    }
}
