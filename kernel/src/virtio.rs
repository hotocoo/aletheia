//! virtio-blk driver over virtio-mmio (modern / v2) — the first REAL hardware driver (REQ-DRV-001,
//! ADR-023), aarch64 dev backend on QEMU `virt`.
//!
//! Until now the `kernel_core::storage::BlockDevice` seam was only ever backed by an in-memory
//! `MemBlockDevice`. This module implements that same trait over a genuine emulated block device
//! reached through a split virtqueue, so the write-ahead journal (REQ-STOR-002) runs over real
//! emulated storage — closing gap-register Issue 5's "no concrete driver" hole with one contract-honest
//! brick (ADR-010: a wrong ring layout faults/hangs into the VM watchdog, never a silent pass).
//!
//! **No ambient authority (ADR-023).** The driver holds only the frames it allocated for its rings and
//! buffers; every block op is still authorized by the SAME `CapEngine` when wrapped in a
//! `kernel_core::device::DeviceGuard` (REQ-DRV-002), proved live in `selftest`.
//!
//! **Graceful probe.** Under bare `cargo run` (no `-drive`) no block transport is present, so `probe`
//! returns `None` and the kernel logs `[virtio] no device (skipped)` and boots green. The VM gate
//! (`scripts/vm-e2e.sh`) attaches a disk and asserts discovery + I/O + journal round-trip.
//!
//! **Coherency note (honest).** QEMU's virtio DMA is cache-coherent with the guest, so no explicit
//! cache maintenance is needed for the ring/buffer frames; a `dsb` orders our Normal-memory ring
//! writes before the Device-memory `QueueNotify`. Real hardware would additionally need cache
//! clean/invalidate around the DMA buffers — called out here, not silently assumed away.

use core::ptr::{read_volatile, write_volatile};

use kernel_core::device::{DeviceError, DeviceGuard};
use kernel_core::spine::{CapEngine, Constraints, Scope};
use kernel_core::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE};

use crate::frames;

// --- virtio-mmio transport layout on QEMU `virt` -------------------------------------------------

/// First virtio-mmio transport; QEMU `virt` places 32 slots, each 0x200 apart, inside the
/// Device-mapped peripheral GiB (already identity-mapped Device-nGnRnE by `vm::build_identity`).
const MMIO_BASE: usize = 0x0a00_0000;
const MMIO_STRIDE: usize = 0x200;
const MMIO_SLOTS: usize = 32;

// virtio-mmio register offsets (VIRTIO 1.1 §4.2.2).
const R_MAGIC: usize = 0x000;
const R_VERSION: usize = 0x004;
const R_DEVICE_ID: usize = 0x008;
const R_DEVICE_FEATURES: usize = 0x010;
const R_DEVICE_FEATURES_SEL: usize = 0x014;
const R_DRIVER_FEATURES: usize = 0x020;
const R_DRIVER_FEATURES_SEL: usize = 0x024;
const R_QUEUE_SEL: usize = 0x030;
const R_QUEUE_NUM_MAX: usize = 0x034;
const R_QUEUE_NUM: usize = 0x038;
const R_QUEUE_READY: usize = 0x044;
const R_QUEUE_NOTIFY: usize = 0x050;
const R_STATUS: usize = 0x070;
const R_QUEUE_DESC_LOW: usize = 0x080;
const R_QUEUE_DESC_HIGH: usize = 0x084;
const R_QUEUE_DRIVER_LOW: usize = 0x090; // avail ring
const R_QUEUE_DRIVER_HIGH: usize = 0x094;
const R_QUEUE_DEVICE_LOW: usize = 0x0a0; // used ring
const R_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const R_CONFIG: usize = 0x100; // device-specific config; blk capacity (u64 sectors) at +0

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt" (little-endian)
const VIRTIO_VERSION_MODERN: u32 = 2;
const VIRTIO_ID_BLOCK: u32 = 2;

// Device status bits.
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;
const S_FAILED: u32 = 0x80;

// Feature bits (offset within their 32-bit half).
const F_BLK_FLUSH_BIT: u32 = 9; // VIRTIO_BLK_F_FLUSH, in the low half (bits 0..31)
const F_VERSION_1_BIT: u32 = 0; // VIRTIO_F_VERSION_1 == bit 32, i.e. bit 0 of the high half

// Split-virtqueue descriptor flags.
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2; // buffer is device-writable (a READ into our memory)

// virtio-blk request types + status.
const VIRTIO_BLK_T_IN: u32 = 0; // read from device into memory
const VIRTIO_BLK_T_OUT: u32 = 1; // write from memory to device
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;

/// virtio-blk transfers in 512-byte sectors; our block is one page.
const SECTOR_SIZE: usize = 512;
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64; // 8

/// Descriptor-table entry (VIRTIO 1.1 §2.6.5): 16 bytes, packed.
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Byte offsets within our single ring frame. Desc table, avail ring, and used ring each get an
/// aligned, non-overlapping region (modern virtio allows split placement — that is what the three
/// QueueDesc/Driver/Device address registers are for). The request header and status byte reuse the
/// tail of the same frame (well clear of the rings for any queue size we pick).
const OFF_DESC: usize = 0; // desc table:  N * 16 B  (N <= 8 -> <= 128 B)
const OFF_AVAIL: usize = 256; // avail ring: 6 + 2N B
const OFF_USED: usize = 512; // used ring:  6 + 8N B
const OFF_HDR: usize = 1024; // 16-byte request header
const OFF_STATUS: usize = 1040; // 1-byte status

/// Queue size we request (capped by `QueueNumMax`). 8 is ample: we only ever have one 3-descriptor
/// request in flight and poll it to completion.
const QSIZE_WANT: u16 = 8;

// --- register + barrier helpers ------------------------------------------------------------------

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    // SAFETY: caller passes a mapped virtio-mmio register address (Device-nGnRnE, aligned).
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn w32(base: usize, off: usize, v: u32) {
    // SAFETY: as above; virtio-mmio registers are 32-bit and 4-byte aligned.
    write_volatile((base + off) as *mut u32, v);
}

#[inline]
unsafe fn r64_config(base: usize, off: usize) -> u64 {
    let lo = r32(base, R_CONFIG + off) as u64;
    let hi = r32(base, R_CONFIG + off + 4) as u64;
    (hi << 32) | lo
}

/// Full system barrier: orders our Normal-memory ring/buffer writes before the Device-memory
/// `QueueNotify`, and orders the used-ring read after we observe `used.idx` advance.
#[inline]
fn dsb() {
    // SAFETY: `dsb sy` has no operands and only enforces memory ordering.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}

// --- the driver ----------------------------------------------------------------------------------

/// A live virtio-blk device: the MMIO base, the physical addresses of its single virtqueue's rings
/// and request buffers (all identity-mapped, so VA == PA — the addresses handed to the device as DMA
/// targets), the device capacity, and whether FLUSH was negotiated.
pub struct VirtioBlk {
    base: usize,
    desc: usize,
    avail: usize,
    used: usize,
    hdr: usize,
    status: usize,
    data: usize,
    qsize: u16,
    capacity_sectors: u64,
    flush_ok: bool,
}

/// Scan the 32 virtio-mmio slots for a block device. Returns its MMIO base, or `None` when no block
/// transport is attached (bare `cargo run`) — the graceful-skip path.
pub fn probe() -> Option<usize> {
    for i in 0..MMIO_SLOTS {
        let base = MMIO_BASE + i * MMIO_STRIDE;
        // SAFETY: every slot address is inside the Device-mapped peripheral GiB.
        let magic = unsafe { r32(base, R_MAGIC) };
        if magic != VIRTIO_MAGIC {
            continue;
        }
        let dev_id = unsafe { r32(base, R_DEVICE_ID) };
        // DeviceID 0 == a present-but-empty transport slot; skip until we find the block device.
        if dev_id == VIRTIO_ID_BLOCK {
            return Some(base);
        }
    }
    None
}

impl VirtioBlk {
    /// Bring the block device up: reset → feature negotiation → queue 0 setup → DRIVER_OK. Fails
    /// closed (returns `Err`) on anything unexpected — a legacy (v1) transport, a missing queue, or a
    /// feature handshake the device rejects — so a mis-negotiation is a clean failure, never a silent
    /// wrong-mode driver. Emits a per-step marker so a hang localizes to the exact stage.
    pub fn init(base: usize) -> Result<Self, &'static str> {
        // SAFETY (whole fn): `base` came from `probe`, so it is a mapped virtio-mmio block transport;
        // all register accesses are aligned and to Device memory. The ring/buffer frames are freshly
        // allocated, identity-mapped RAM we own exclusively.
        unsafe {
            let version = r32(base, R_VERSION);
            let dev_id = r32(base, R_DEVICE_ID);
            kprintln!(
                "[virtio] block device @ {:#x} (version {}, id {})",
                base,
                version,
                dev_id
            );
            if version != VIRTIO_VERSION_MODERN {
                return Err("legacy (v1) virtio-mmio not supported — fail closed");
            }

            // 1 — reset, then ACKNOWLEDGE + DRIVER (VIRTIO 1.1 §3.1.1).
            w32(base, R_STATUS, 0);
            let mut status = S_ACKNOWLEDGE;
            w32(base, R_STATUS, status);
            status |= S_DRIVER;
            w32(base, R_STATUS, status);

            // 2 — feature negotiation. Read both 32-bit halves; accept only VIRTIO_F_VERSION_1
            //     (mandatory for modern) plus VIRTIO_BLK_F_FLUSH when offered (a real durability
            //     barrier). Everything else is cleared — we implement nothing that needs it.
            w32(base, R_DEVICE_FEATURES_SEL, 0);
            let dev_lo = r32(base, R_DEVICE_FEATURES);
            w32(base, R_DEVICE_FEATURES_SEL, 1);
            let dev_hi = r32(base, R_DEVICE_FEATURES);

            let want_version1 = (dev_hi & (1 << F_VERSION_1_BIT)) != 0;
            let flush_ok = (dev_lo & (1 << F_BLK_FLUSH_BIT)) != 0;
            kprintln!(
                "[virtio] features dev_lo={:#010x} dev_hi={:#010x} -> version1={} flush={}",
                dev_lo,
                dev_hi,
                want_version1,
                flush_ok
            );
            if !want_version1 {
                return Err("device does not offer VIRTIO_F_VERSION_1 — fail closed");
            }
            let drv_lo = if flush_ok { 1 << F_BLK_FLUSH_BIT } else { 0 };
            let drv_hi = 1 << F_VERSION_1_BIT;
            w32(base, R_DRIVER_FEATURES_SEL, 0);
            w32(base, R_DRIVER_FEATURES, drv_lo);
            w32(base, R_DRIVER_FEATURES_SEL, 1);
            w32(base, R_DRIVER_FEATURES, drv_hi);

            // 3 — FEATURES_OK, then read it back: if the device clears it, our set is unacceptable.
            status |= S_FEATURES_OK;
            w32(base, R_STATUS, status);
            if r32(base, R_STATUS) & S_FEATURES_OK == 0 {
                w32(base, R_STATUS, status | S_FAILED);
                return Err("device rejected negotiated features (FEATURES_OK cleared)");
            }

            // 4 — queue 0 setup. Allocate one frame for the rings, one for the 4 KiB data buffer.
            w32(base, R_QUEUE_SEL, 0);
            let num_max = r32(base, R_QUEUE_NUM_MAX);
            if num_max == 0 {
                return Err("queue 0 unavailable (QueueNumMax == 0)");
            }
            let qsize = core::cmp::min(QSIZE_WANT as u32, num_max) as u16;
            kprintln!("[virtio] queue0 num_max={} using N={}", num_max, qsize);

            let ring = frames::alloc_zeroed()
                .ok_or("frame allocator exhausted (ring)")?
                .addr();
            let data = frames::alloc_zeroed()
                .ok_or("frame allocator exhausted (data)")?
                .addr();

            let desc = ring + OFF_DESC;
            let avail = ring + OFF_AVAIL;
            let used = ring + OFF_USED;
            let hdr = ring + OFF_HDR;
            let status_buf = ring + OFF_STATUS;

            w32(base, R_QUEUE_NUM, qsize as u32);
            w32(base, R_QUEUE_DESC_LOW, desc as u32);
            w32(base, R_QUEUE_DESC_HIGH, (desc as u64 >> 32) as u32);
            w32(base, R_QUEUE_DRIVER_LOW, avail as u32);
            w32(base, R_QUEUE_DRIVER_HIGH, (avail as u64 >> 32) as u32);
            w32(base, R_QUEUE_DEVICE_LOW, used as u32);
            w32(base, R_QUEUE_DEVICE_HIGH, (used as u64 >> 32) as u32);
            dsb();
            w32(base, R_QUEUE_READY, 1);

            // 5 — DRIVER_OK: the device is now live.
            status |= S_DRIVER_OK;
            w32(base, R_STATUS, status);

            let capacity_sectors = r64_config(base, 0);
            kprintln!(
                "[virtio] DRIVER_OK; capacity = {} sectors ({} x {}-byte blocks)",
                capacity_sectors,
                capacity_sectors / SECTORS_PER_BLOCK,
                BLOCK_SIZE
            );

            Ok(VirtioBlk {
                base,
                desc,
                avail,
                used,
                hdr,
                status: status_buf,
                data,
                qsize,
                capacity_sectors,
                flush_ok,
            })
        }
    }

    /// Write descriptor `i` in the table.
    #[inline]
    unsafe fn set_desc(&self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        // SAFETY: `i` < qsize; the desc table is our identity-mapped ring frame.
        let d = (self.desc + i * core::mem::size_of::<VirtqDesc>()) as *mut VirtqDesc;
        write_volatile(
            d,
            VirtqDesc {
                addr,
                len,
                flags,
                next,
            },
        );
    }

    /// Post the head descriptor to the avail ring, notify the device, and poll the used ring to
    /// completion. Returns the device status byte. Bounded poll: a device that never completes makes
    /// this return `Err` rather than spin forever (contract-honest anti-hang; the outer VM watchdog is
    /// the backstop).
    unsafe fn submit(&self, head: u16) -> Result<u8, StorageError> {
        // avail ring layout: [flags:u16][idx:u16][ring:u16 * qsize][used_event:u16]
        let avail_idx_ptr = (self.avail + 2) as *mut u16;
        let avail_ring = (self.avail + 4) as *mut u16;
        // used ring layout:  [flags:u16][idx:u16][ring:{id:u32,len:u32} * qsize][avail_event:u16]
        let used_idx_ptr = (self.used + 2) as *const u16;

        let cur = read_volatile(avail_idx_ptr);
        let old_used = read_volatile(used_idx_ptr);
        write_volatile(avail_ring.add((cur % self.qsize) as usize), head);
        dsb(); // ring writes visible before we bump idx
        write_volatile(avail_idx_ptr, cur.wrapping_add(1));
        dsb(); // idx visible before we notify

        w32(self.base, R_QUEUE_NOTIFY, 0);

        // Poll for completion. The bound is generous (millions of iterations) so a healthy device
        // always finishes well within it; only a broken ring layout exhausts it.
        let mut spins: u64 = 0;
        while read_volatile(used_idx_ptr) == old_used {
            spins += 1;
            if spins > 50_000_000 {
                return Err(StorageError::Device);
            }
            core::hint::spin_loop();
        }
        dsb(); // used.idx observed before we read the status the device wrote

        Ok(read_volatile(self.status as *const u8))
    }

    /// Issue one virtio-blk request. `has_data` builds the 3-descriptor chain (header, data, status);
    /// a flush omits the data descriptor. `device_writes_data` marks the data buffer device-writable
    /// (a READ). Returns `Err(Device)` if the device reports a non-OK status.
    unsafe fn request(
        &self,
        rtype: u32,
        sector: u64,
        has_data: bool,
        device_writes_data: bool,
    ) -> Result<(), StorageError> {
        // Header: [type:le32][reserved:le32][sector:le64].
        let h = self.hdr as *mut u8;
        write_volatile((h as *mut u32).add(0), rtype);
        write_volatile((h as *mut u32).add(1), 0);
        write_volatile((self.hdr + 8) as *mut u64, sector);
        // Sentinel so a device that writes nothing is detected as a failure, not a stale OK.
        write_volatile(self.status as *mut u8, 0xff);

        if has_data {
            self.set_desc(0, self.hdr as u64, 16, VIRTQ_DESC_F_NEXT, 1);
            let data_flags = VIRTQ_DESC_F_NEXT
                | if device_writes_data {
                    VIRTQ_DESC_F_WRITE
                } else {
                    0
                };
            self.set_desc(1, self.data as u64, BLOCK_SIZE as u32, data_flags, 2);
            self.set_desc(2, self.status as u64, 1, VIRTQ_DESC_F_WRITE, 0);
        } else {
            // flush: header + status only.
            self.set_desc(0, self.hdr as u64, 16, VIRTQ_DESC_F_NEXT, 1);
            self.set_desc(1, self.status as u64, 1, VIRTQ_DESC_F_WRITE, 0);
        }

        match self.submit(0)? {
            VIRTIO_BLK_S_OK => Ok(()),
            _ => Err(StorageError::Device),
        }
    }

    #[inline]
    fn check_block(&self, idx: usize, buf_len: usize) -> Result<u64, StorageError> {
        if buf_len != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        if idx >= self.num_blocks() {
            return Err(StorageError::OutOfRange);
        }
        Ok(idx as u64 * SECTORS_PER_BLOCK)
    }
}

impl BlockDevice for VirtioBlk {
    fn num_blocks(&self) -> usize {
        (self.capacity_sectors / SECTORS_PER_BLOCK) as usize
    }

    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        let sector = self.check_block(idx, buf.len())?;
        // SAFETY: single request in flight; the data frame is ours and identity-mapped.
        unsafe {
            self.request(VIRTIO_BLK_T_IN, sector, true, true)?;
            let src = core::slice::from_raw_parts(self.data as *const u8, BLOCK_SIZE);
            buf.copy_from_slice(src);
        }
        Ok(())
    }

    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        let sector = self.check_block(idx, buf.len())?;
        // SAFETY: as above; we stage the bytes into the data frame, then hand it to the device.
        unsafe {
            let dst = core::slice::from_raw_parts_mut(self.data as *mut u8, BLOCK_SIZE);
            dst.copy_from_slice(buf);
            self.request(VIRTIO_BLK_T_OUT, sector, true, false)
        }
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        if !self.flush_ok {
            // No FLUSH feature: QEMU's default cache mode makes each write durable on completion, so
            // there is no separate barrier to issue. (Honest: on a device/host that reorders, this
            // path would need the feature; we negotiated it above whenever offered.)
            return Ok(());
        }
        // SAFETY: flush is a header+status chain with no data buffer.
        unsafe { self.request(VIRTIO_BLK_T_FLUSH, 0, false, false) }
    }
}

// --- VM-gated invariants -------------------------------------------------------------------------

const CAP_READ: &str = "dev.blk.read";
const CAP_WRITE: &str = "dev.blk.write";

macro_rules! check {
    ($n:expr, $cond:expr, $name:expr) => {{
        if $cond {
            kprintln!("  [pass {:>2}] {}", $n, $name);
            $n += 1;
        } else {
            return Err(($n, $name));
        }
    }};
}

/// Prove the driver against a real emulated device. Skips green (returns `Ok(0)`) when no block
/// device is attached, so bare `cargo run` still boots; the VM gate attaches a disk and asserts the
/// invariant marker. Failure returns `(index, name)` → the caller exits `120 + index`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let base = match probe() {
        Some(b) => b,
        None => {
            kprintln!("[virtio] no device (skipped)");
            return Ok(0);
        }
    };

    let mut dev = match VirtioBlk::init(base) {
        Ok(d) => d,
        Err(e) => {
            kprintln!("[virtio] init failed: {}", e);
            return Err((0, "virtio-blk device initialization"));
        }
    };

    let mut n: u32 = 1;

    // 1 — discovery: a real block device is present and initialized.
    check!(
        n,
        dev.num_blocks() > 0,
        "virtio-blk: device discovered + initialized"
    );

    // 2 — capacity matches the attached image (vm-e2e.sh attaches a 1 MiB disk = 2048 sectors = 256
    //     4 KiB blocks). A wrong sector/block mapping shows up here before any I/O.
    check!(
        n,
        dev.num_blocks() == 256 && dev.capacity_sectors == 2048,
        "virtio-blk: capacity read matches the 1 MiB attached image (256 blocks)"
    );

    // 3 — write then read-back over a real virtqueue round-trip returns exactly the written bytes.
    let home = kernel_core::storage::DATA_START + 5; // a fresh home block (>= 65)
    let mut pattern = [0u8; BLOCK_SIZE];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i as u8) ^ 0x5a;
    }
    let mut readback = [0u8; BLOCK_SIZE];
    let roundtrip = dev.write_block(home, &pattern).is_ok()
        && dev.flush().is_ok()
        && dev.read_block(home, &mut readback).is_ok()
        && readback == pattern;
    check!(
        n,
        roundtrip,
        "virtio-blk: write -> read-back round-trip returns the written bytes"
    );

    // 4 — end-to-end: the write-ahead journal (REQ-STOR-002) commits a transaction over the REAL
    //     device, and a FRESH journal recovers the committed state from the device bytes alone
    //     (crash-consistency over real emulated storage — the ADR-023 payoff).
    let h1 = kernel_core::storage::DATA_START + 10;
    let h2 = kernel_core::storage::DATA_START + 11;
    let (mut a, mut b) = ([0u8; BLOCK_SIZE], [0u8; BLOCK_SIZE]);
    a.fill(0xA1);
    b.fill(0xB2);
    let mut journal = Journal::new();
    let committed = journal.commit(&mut dev, &[(h1, a), (h2, b)]).is_ok();
    let mut recovered = Journal::new();
    let replayed = recovered.recover(&mut dev) == Ok(true);
    let r1 = recovered.read(&dev, h1);
    let r2 = recovered.read(&dev, h2);
    check!(
        n,
        committed && replayed && r1 == Ok(a) && r2 == Ok(b),
        "virtio-blk: journal commit + fresh recover reproduce state over real storage"
    );

    // 5 — capability-gated over the REAL device (REQ-DRV-002): no capability -> no bytes move; a
    //     write capability's bytes land. Same authority mechanism as every other Aletheia effect.
    let capblk = kernel_core::storage::DATA_START + 20;
    let mut engine = CapEngine::new(0x5171_0b1c, 1_000_000);
    let read_cap = engine.mint("virtio-test", CAP_READ, Scope::All, Constraints::none());
    let write_cap = engine.mint("virtio-test", CAP_WRITE, Scope::All, Constraints::none());
    let mut guard = DeviceGuard::new(dev, CAP_READ, CAP_WRITE);
    let mut deny_buf = [0u8; BLOCK_SIZE];
    deny_buf.fill(0xEE);
    let denied = guard.write_block(&engine, &[], capblk, &deny_buf) == Err(DeviceError::Denied);
    // With no offered cap the device was never touched; confirm via an authorized read.
    let mut after_deny = [0u8; BLOCK_SIZE];
    let read_ok = guard
        .read_block(&engine, &[read_cap], capblk, &mut after_deny)
        .is_ok();
    let unchanged = after_deny.iter().all(|&x| x != 0xEE);
    // With the write cap the bytes actually land and read back.
    let mut landed = [0u8; BLOCK_SIZE];
    landed.fill(0x7c);
    let wrote = guard
        .write_block(&engine, &[read_cap, write_cap], capblk, &landed)
        .is_ok();
    let mut verify = [0u8; BLOCK_SIZE];
    let verified = guard
        .read_block(&engine, &[read_cap], capblk, &mut verify)
        .is_ok()
        && verify == landed;
    check!(
        n,
        denied && read_ok && unchanged && wrote && verified,
        "virtio-blk: capability-gated I/O to the real device (deny moves nothing; grant lands)"
    );

    Ok(n - 1)
}
