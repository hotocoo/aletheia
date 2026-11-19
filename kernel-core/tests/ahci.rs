//! Hosted proof of the AHCI driver (ADR-225) against a simulated HBA with a boot disk and a
//! scratch disk, plus named misbehaviours.

use kernel_core::ahci::*;
use kernel_core::devirq::{IrqSample, RunFault};
use kernel_core::storage::{BlockDevice, StorageError, BLOCK_SIZE};
use kernel_core::virtioblk::VirtioHal;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

struct HostHal;
thread_local! {
    static CLOCK: AtomicU64 = const { AtomicU64::new(0) };
    /// Interrupts the simulated HBA raised, times the platform wait halted the CPU, the HBA the
    /// wait lets answer while it sleeps, and which look at the clock a concurrent HBA answers after.
    static IRQS: Cell<u64> = const { Cell::new(0) };
    static SLEEPS: Cell<u64> = const { Cell::new(0) };
    /// Times the driver called the platform wait, ready or not.
    static WAITS: Cell<u64> = const { Cell::new(0) };
    static DEVICE: Cell<*const Sim> = const { Cell::new(std::ptr::null()) };
    static CLOCK_ANSWERS_AFTER: Cell<u32> = const { Cell::new(0) };
}
impl VirtioHal for HostHal {
    fn now_ns() -> u64 {
        let left = CLOCK_ANSWERS_AFTER.with(Cell::get);
        if left > 0 {
            CLOCK_ANSWERS_AFTER.with(|c| c.set(left - 1));
            if left == 1 {
                // SAFETY: DEVICE points at the Sim the running test owns.
                DEVICE.with(|d| unsafe { (*d.get()).deliver() });
            }
        }
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
    /// The HBA answers only while the CPU sleeps (the platform wait lets it run).
    deferred: bool,
    /// The HBA never raises its interrupt.
    no_irq: bool,
    /// An acknowledgement does not take: IS stays set once set, so no later completion raises one.
    sticky_is: bool,
    /// The HBA runs each command after PxCI is written, on the CPU's SECOND look at the clock (the
    /// driver's first is its start time), as silicon does: a command the driver stops waiting for
    /// finishes LATE, in the slot, table and data frame the next command would rewrite (ADR-266).
    late: bool,
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
    /// Ports whose command the HBA accepted but a deferred HBA has not run yet.
    pending: Vec<usize>,
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
            pending: vec![],
        }))
    }

    /// A deferred HBA runs what it was told, while the CPU sleeps.
    fn deliver(&self) {
        let mut s = self.0.borrow_mut();
        for port in std::mem::take(&mut s.pending) {
            Sim::run(&mut s, port);
        }
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
        // Completion sets PxIS (a D2H Register FIS, or the task-file error) whatever PxIE says;
        // the HBA's IS bit and the interrupt follow only when PxIE asks, and only on the IS bit's
        // 0 -> 1 edge: a message-signalled HBA says nothing while IS is still set.
        let mut is = s.r.get(&(b + PX_IS)).copied().unwrap_or(0);
        if s.f.task_file_error && port == 2 {
            is |= 1 << 30;
            s.r.insert(b + PX_TFD, 0x51);
        } else {
            is |= 1;
            s.r.insert(b + PX_TFD, 0x50);
        }
        s.r.insert(b + PX_IS, is);
        // A task-file error stops the port with the slot still set (AHCI 1.3.1 §5.3.16.5): only
        // clearing PxCMD.ST releases it. A good completion releases it.
        if !(s.f.task_file_error && port == 2) {
            s.r.insert(b + PX_CI, 0);
        }
        let ie = s.r.get(&(b + PX_IE)).copied().unwrap_or(0);
        if is & ie != 0 {
            let hba_is = s.r.get(&HBA_IS).copied().unwrap_or(0);
            if hba_is & (1 << port) == 0 {
                s.r.insert(HBA_IS, hba_is | 1 << port);
                if s.r.get(&HBA_GHC).copied().unwrap_or(0) & 2 != 0 && !s.f.no_irq {
                    IRQS.with(|i| i.set(i.get() + 1));
                }
            }
        }
    }
}

impl Regs for Sim {
    fn r32(&self, off: usize) -> u32 {
        let s = self.0.borrow();
        let v = *s.r.get(&off).unwrap_or(&0);
        if s.f.stays_busy && off >= PORT_BASE && (off - PORT_BASE) % PORT_STRIDE == PX_TFD {
            return 0x80;
        }
        // Like the spec (and QEMU): no signature until FIS receive is on and a D2H FIS arrived.
        if off >= PORT_BASE && (off - PORT_BASE) % PORT_STRIDE == PX_SIG {
            let cmd = *s.r.get(&(off - PX_SIG + PX_CMD)).unwrap_or(&0);
            return if cmd & (1 << 4) != 0 { v } else { u32::MAX };
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
                    // Write-1-to-clear (AHCI 1.3.1 section 3.3.5).
                    let cur = s.r.get(&off).copied().unwrap_or(0);
                    s.r.insert(off, cur & !v);
                    return;
                }
                PX_CMD => {
                    // Clearing ST clears PxCI and ends what the slot held (AHCI 1.3.1 §3.3.14).
                    if v & 1 == 0 {
                        s.r.insert(off - PX_CMD + PX_CI, 0);
                        s.pending.retain(|&p| p != port);
                    }
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
                    if s.f.late {
                        CLOCK_ANSWERS_AFTER.with(|c| c.set(2));
                        s.pending.push(port);
                    } else if s.f.deferred
                        && s.r.get(&(off - PX_CI + PX_IE)).copied().unwrap_or(0) != 0
                    {
                        s.pending.push(port);
                    } else {
                        Sim::run(&mut s, port);
                    }
                    return;
                }
                _ => {}
            }
        } else if off == HBA_IS {
            if !s.f.sticky_is {
                let cur = s.r.get(&off).copied().unwrap_or(0);
                s.r.insert(off, cur & !v);
            }
            return;
        }
        s.r.insert(off, v);
    }
}

/// An HBA that runs commands on the clock after PxCI, as silicon does (ADR-266).
fn late_sim(sector: usize) -> Sim {
    Sim::new(
        Faults {
            late: true,
            ..Default::default()
        },
        sector,
    )
}

fn disks(sim: &Sim) -> Vec<AhciDisk<HostHal, &Sim>> {
    DEVICE.with(|d| d.set(sim));
    disk_ports(&sim)
        .into_iter()
        .map(|p| unsafe { AhciDisk::<HostHal, &Sim>::open(sim, p) }.expect("open"))
        .collect()
}

#[test]
fn the_suite_holds_and_never_writes_the_boot_disk() {
    for sector in [512, 4096] {
        let sim = late_sim(sector);
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
fn a_machine_with_only_its_own_disk_gets_the_read_only_group_and_no_writes() {
    // An ordinary one-disk PC or the VMware package (ADR-228): no scratch disk, so the suite is
    // read-only, passes, and nothing is written.
    let sim = late_sim(512);
    DEVICE.with(|d| d.set(&sim));
    let mut d = vec![unsafe { AhciDisk::<HostHal, &Sim>::open(&sim, 0) }.expect("open")];
    let n = device_suite(&mut d, 256, &mut |i, ok, name: &str| {
        assert!(ok, "{i}: {name}")
    })
    .unwrap();
    assert_eq!(n, 6, "read-only group count changed - update the gates");
    drop(d);
    let st = sim.0.borrow();
    assert_eq!(st.disks[0].writes, 0);
    assert_eq!(
        st.r[&(PORT_BASE + PX_CMD)] & 0x11,
        0,
        "a dropped disk's port is stopped"
    );
}

#[test]
fn a_port_without_a_disk_signature_is_refused_by_name_at_open() {
    let sim = Sim::new(Faults::default(), 512);
    sim.0
        .borrow_mut()
        .r
        .insert(PORT_BASE + 2 * PORT_STRIDE + PX_SIG, 0xEB14_0101); // ATAPI
    assert_eq!(disk_ports(&&sim), vec![0, 2], "link up on both ports");
    let e = unsafe { AhciDisk::<HostHal, &Sim>::open(&sim, 2) }
        .err()
        .unwrap();
    assert_eq!(e, NOT_ATA);
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

// --- Interrupt-driven completions (ADR-264) ----------------------------------------------------

/// The platform wait the tests install: nothing ready means the CPU halts, and the HBA answers
/// while it does.
fn platform_wait(ready: &dyn Fn() -> bool) {
    WAITS.with(|c| c.set(c.get() + 1));
    if !ready() {
        SLEEPS.with(|c| c.set(c.get() + 1));
        // SAFETY: DEVICE points at the Sim the running test owns.
        DEVICE.with(|d| unsafe { (*d.get()).deliver() });
    }
}

fn sample() -> IrqSample {
    IrqSample {
        interrupts: IRQS.with(Cell::get),
        sleeps: SLEEPS.with(Cell::get),
        bound_wakes: 0,
    }
}

fn armed(sim: &Sim) -> Vec<AhciDisk<HostHal, &Sim>> {
    IRQS.with(|c| c.set(0));
    SLEEPS.with(|c| c.set(0));
    WAITS.with(|c| c.set(0));
    DEVICE.with(|d| d.set(sim));
    let mut d = disks(sim);
    for disk in d.iter_mut() {
        disk.enable_interrupts();
        disk.set_completion_wait(Some(platform_wait))
            .expect("an enabled port takes a wait");
    }
    d
}

fn deferred() -> Faults {
    Faults {
        deferred: true,
        ..Default::default()
    }
}

#[test]
fn a_wait_is_refused_until_the_ports_interrupts_are_enabled() {
    let sim = Sim::new(Faults::default(), 512);
    let mut d = disks(&sim);
    assert!(d[0].set_completion_wait(Some(platform_wait)).is_err());
    d[0].enable_interrupts();
    assert!(d[0].set_completion_wait(Some(platform_wait)).is_ok());
    let st = sim.0.borrow();
    assert_ne!(st.r[&(PORT_BASE + PX_IE)], 0, "PxIE is programmed");
    assert_ne!(st.r[&HBA_GHC] & 2, 0, "GHC.IE is set");
}

#[test]
fn the_interrupt_group_holds_over_every_disk_and_acknowledges_in_order() {
    for sector in [512, 4096] {
        let sim = Sim::new(deferred(), sector);
        let d = armed(&sim);
        let mut names = vec![];
        let n = interrupt_suite(&d, 100, &sample, &mut |i, ok, name: &str| {
            assert!(ok, "invariant {i} failed: {name}");
            names.push((i, name.to_string()));
        })
        .expect("interrupt group");
        assert_eq!(n, 7, "interrupt group count changed - update the VM gates");
        assert_eq!(names[0].0, 101);
        for disk in &d {
            assert_eq!(disk.interrupt_status(), (0, 0), "acknowledged");
            let (slept, polled) = disk.completion_counts();
            // IDENTIFY ran at open, before the wait was armed: the only polled command.
            assert!(slept > 0 && polled == 1);
        }
        assert_eq!(sim.0.borrow().disks[0].writes, 0, "reads only");
    }
}

#[test]
fn a_driver_that_polls_fails_the_group_by_name() {
    let sim = Sim::new(deferred(), 512);
    let d = disks(&sim); // no interrupts enabled, no wait
    let (idx, name) =
        interrupt_suite(&d, 0, &sample, &mut |_, _, _| {}).expect_err("a polling driver must fail");
    assert_eq!(idx, 1);
    assert!(name.contains("wait through the platform"), "{name}");
}

#[test]
fn an_hba_that_never_interrupts_is_refused_by_the_run_verdict() {
    let sim = Sim::new(
        Faults {
            deferred: true,
            no_irq: true,
            ..Default::default()
        },
        512,
    );
    let d = armed(&sim);
    let (_, name) = interrupt_suite(&d, 0, &sample, &mut |_, _, _| {})
        .expect_err("an unarmed source must fail");
    assert_eq!(name, RunFault::NoInterrupt.name());
}

/// If IS is never cleared, a message-signalled HBA raises nothing for the next completion: the
/// run shows fewer interrupts than commands.
#[test]
fn an_acknowledgement_that_does_not_take_loses_every_later_interrupt() {
    let sim = Sim::new(
        Faults {
            deferred: true,
            sticky_is: true,
            ..Default::default()
        },
        512,
    );
    let d = armed(&sim);
    let (_, name) =
        interrupt_suite(&d, 0, &sample, &mut |_, _, _| {}).expect_err("a sticky IS must fail");
    assert_eq!(name, RunFault::LostInterrupt.name());
}

#[test]
fn a_status_left_set_before_a_command_is_refused_not_wiped() {
    let sim = Sim::new(deferred(), 512);
    let d = armed(&sim);
    sim.0.borrow_mut().r.insert(PORT_BASE + PX_IS, 1); // a completion nobody acknowledged
    let mut s = [0u8; 512];
    assert_eq!(d[0].read_first_sector(&mut s), Err(StorageError::Device));
    // The refusal acknowledged it: one late FIS fails one command, the next one runs.
    d[0].read_first_sector(&mut s)
        .expect("the next command runs");
    assert_eq!(d[0].interrupt_status(), (0, 0));
}

/// The lost-wakeup window: the HBA answers (its interrupt latches) between the driver's first
/// look and the wait's check, the check reports the stale "not ready", and the command still
/// completes through that one wait.
#[test]
fn a_completion_that_races_the_wait_is_consumed_through_one_wait() {
    let sim = Sim::new(deferred(), 512);
    let d = armed(&sim);
    let mut s = [0u8; 512];
    d[0].read_first_sector(&mut s).expect("plain read");
    let (i0, s0) = (IRQS.with(Cell::get), SLEEPS.with(Cell::get));
    assert_eq!(s0, 1);
    CLOCK_ANSWERS_AFTER.with(|c| c.set(2));
    let raced = d[0].race_read_first_sector(&mut s);
    CLOCK_ANSWERS_AFTER.with(|c| c.set(0));
    raced.expect("raced read");
    assert_eq!(SLEEPS.with(Cell::get), s0 + 1);
    assert_eq!(IRQS.with(Cell::get), i0 + 1);
    assert_eq!(d[0].interrupt_status(), (0, 0));
}

/// A wait that halts and is never woken: the clock bound still ends the command.
fn deaf_wait(ready: &dyn Fn() -> bool) {
    if !ready() {
        SLEEPS.with(|c| c.set(c.get() + 1));
    }
}

#[test]
fn a_silent_hba_still_times_out_through_the_platform_wait() {
    let sim = Sim::new(deferred(), 512);
    let mut d = armed(&sim);
    d[0].set_completion_wait(Some(deaf_wait)).unwrap();
    let before = SLEEPS.with(Cell::get);
    let mut s = [0u8; 512];
    assert_eq!(d[0].read_first_sector(&mut s), Err(StorageError::Device));
    assert!(
        SLEEPS.with(Cell::get) > before,
        "the wait was asked before the bound fired"
    );
}

#[test]
fn the_fallback_group_passes_only_for_a_named_polled_reason() {
    let sim = Sim::new(Faults::default(), 512);
    let d = disks(&sim);
    let mut seen = vec![];
    let n = fallback_suite(&d, 100, "no MSI capability", &mut |i, ok, name: &str| {
        seen.push((i, ok, name.to_string()))
    })
    .expect("a named fallback");
    assert_eq!(n, 1);
    assert_eq!(seen[0].0, 101);
    // An empty reason is a silent fallback.
    assert!(fallback_suite(&d, 0, "", &mut |_, _, _| {}).is_err());
    // A wait armed while claiming a polled fallback is a lie.
    let sim = Sim::new(deferred(), 512);
    let armed_disks = armed(&sim);
    assert!(fallback_suite(&armed_disks, 0, "no MSI capability", &mut |_, _, _| {}).is_err());
}

/// The evidence for "completed through the wait" is the wait being CALLED, not configured: an HBA
/// that has already answered when the driver first looks (not deferred) still sees every command
/// enter the platform's wait once. A driver that has a wait set but spins anyway would leave the
/// wait's own counter at zero.
#[test]
fn a_command_is_credited_to_the_wait_only_when_the_wait_was_called() {
    let sim = Sim::new(Faults::default(), 512);
    let d = armed(&sim);
    let mut s = [0u8; 512];
    for _ in 0..5 {
        d[0].read_first_sector(&mut s).expect("read");
    }
    assert_eq!(
        d[0].completion_counts(),
        (5, 1),
        "IDENTIFY ran before the wait"
    );
    assert_eq!(WAITS.with(Cell::get), 5, "each command entered the wait");
    assert_eq!(
        SLEEPS.with(Cell::get),
        0,
        "an answered command never sleeps"
    );
    let (_, name) = interrupt_suite(&d, 0, &sample, &mut |_, _, _| {})
        .expect_err("a wait that never slept must fail");
    assert_eq!(name, RunFault::NeverSlept.name());
}

// --- A command left with the HBA (ADR-266) -----------------------------------------------------

/// A read the driver stopped waiting for finishes late, in the slot, table and data frame the next
/// command would rewrite. Wedged, the port refuses the next command before PxCI (nothing more is
/// issued), and a port reset, which clears PxCI, brings it back reading the right sectors.
#[test]
fn a_command_left_with_the_hba_is_never_finished_into_the_next() {
    let sim = late_sim(512);
    let mut d = disks(&sim);
    let scratch = &mut d[1];
    let (a, b) = ([0x11u8; BLOCK_SIZE], [0x22u8; BLOCK_SIZE]);
    scratch.write_block(21, &a).unwrap();
    scratch.write_block(22, &b).unwrap();

    scratch.set_completion_budget_ns(Some(0));
    let mut back = [0u8; BLOCK_SIZE];
    assert_eq!(scratch.read_block(21, &mut back), Err(StorageError::Device));
    assert!(scratch.is_wedged(), "a timed-out command wedges the port");
    scratch.set_completion_budget_ns(None);

    let issued = scratch.issued();
    assert_eq!(scratch.read_block(22, &mut back), Err(StorageError::Device));
    assert_eq!(scratch.write_block(22, &a), Err(StorageError::Device));
    assert_eq!(scratch.issued(), issued, "a wedged port issues nothing");

    scratch.reset().expect("port reset");
    assert!(!scratch.is_wedged());
    scratch.read_block(22, &mut back).unwrap();
    assert_eq!(back, b, "after the reset the read returns its own sectors");
    scratch.read_block(21, &mut back).unwrap();
    assert_eq!(back, a);
}

/// A disk still busy after its engine stopped needs a COMRESET, which is refused by name: the
/// port stays wedged rather than restarting under a busy device.
#[test]
fn a_port_whose_disk_stays_busy_is_refused_by_name_and_stays_wedged() {
    let sim = late_sim(512);
    let d = disks(&sim);
    sim.0.borrow_mut().f.stays_busy = true;
    assert_eq!(
        d[1].reset(),
        Err("ahci: the disk stayed busy after its port stopped (it needs a COMRESET)")
    );
    assert!(d[1].is_wedged());
}

/// A task-file error stops the port with its slot still set (AHCI 1.3.1 §5.3.16.5), so the port is
/// wedged, and only the reset (clearing PxCMD.ST) releases the slot for the next command.
#[test]
fn a_task_file_error_wedges_the_port_until_its_reset() {
    let sim = Sim::new(Faults::default(), 512);
    let d = disks(&sim);
    let scratch = &d[1];
    sim.0.borrow_mut().f.task_file_error = true;
    let mut b = [0u8; BLOCK_SIZE];
    assert_eq!(scratch.read_block(3, &mut b), Err(StorageError::Device));
    assert!(scratch.is_wedged(), "the slot is still the HBA's");
    sim.0.borrow_mut().f.task_file_error = false;
    let issued = scratch.issued();
    assert_eq!(scratch.read_block(3, &mut b), Err(StorageError::Device));
    assert_eq!(scratch.issued(), issued);
    scratch.reset().expect("reset");
    assert!(scratch.read_block(3, &mut b).is_ok());
}
