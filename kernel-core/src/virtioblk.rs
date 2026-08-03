//! virtio-blk over virtio-mmio (modern / v2) — the driver itself, defined ONCE (REQ-DRV-001, ADR-036).
//!
//! The first real driver landed inside the aarch64 target crate (REQ-DRV-003). That was the right
//! shape for one target and the wrong shape for three: a split virtqueue, a feature handshake and a
//! request layout are **bus** facts, not CPU facts, so a per-target copy would mean the two
//! *first-class* targets (AMD64, RISC-V) either had no real storage or had a second implementation of
//! the same protocol — which is exactly the duplication gap-register Issue 1 exists to prevent.
//!
//! What genuinely differs per target is small and explicit, and lives behind [`VirtioHal`]:
//!
//! * **where the transport is** — an [`MmioLayout`] (QEMU `virt` puts 32 slots 0x200 apart at
//!   `0x0a00_0000` on aarch64, and 8 slots 0x1000 apart at `0x1000_1000` on RISC-V);
//! * **how to get a DMA-able frame** — each target's own frame allocator, whose pages are
//!   identity-mapped, so the address handed to the device is both the VA we write and the PA it reads;
//! * **the barrier instruction** — `dsb sy` vs `fence rw, rw`.
//!
//! Everything else — reset, negotiation, queue setup, descriptor chains, the bounded poll, the
//! `BlockDevice` impl — is this module.
//!
//! **No ambient authority (ADR-023).** The driver holds only the frames it allocated for its ring and
//! data buffer; a block op is authorized by the same [`crate::spine::CapEngine`] when the device is
//! wrapped in a [`crate::device::DeviceGuard`] (REQ-DRV-002), which [`device_suite`] proves live.
//!
//! **Contract-honest failure.** `init` fails closed on a legacy (v1) transport, a device that does not
//! offer `VIRTIO_F_VERSION_1`, a rejected feature set, or a missing queue — a mis-negotiation is a
//! clean error, never a silently wrong-mode driver. `submit` polls with a bound, so a device that
//! never completes returns [`StorageError::Device`] instead of hanging past the VM watchdog.
//!
//! **Coherency note (honest).** QEMU's virtio DMA is coherent with the guest, so the ring/buffer
//! frames need no cache maintenance; the barrier orders our Normal-memory ring writes before the
//! Device-memory `QueueNotify`. Real hardware would additionally need clean/invalidate around the DMA
//! buffers — stated, not assumed away.
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use crate::device::{DeviceError, DeviceGuard};
use crate::spine::{CapEngine, Constraints, Scope};
use crate::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE};

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
pub const SECTOR_SIZE: usize = 512;
/// Sectors per [`BLOCK_SIZE`] block.
pub const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// Descriptor-table entry (VIRTIO 1.1 §2.6.5): 16 bytes, packed.
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Byte offsets within our single ring frame. Desc table, avail ring and used ring each get an
/// aligned, non-overlapping region (modern virtio allows split placement — that is what the three
/// QueueDesc/Driver/Device address registers are for). The request header and status byte reuse the
/// tail of the same frame, well clear of the rings at any queue size we pick.
const OFF_DESC: usize = 0; // desc table:  N * 16 B  (N <= 8 -> <= 128 B)
const OFF_AVAIL: usize = 256; // avail ring: 6 + 2N B
const OFF_USED: usize = 512; // used ring:  6 + 8N B
const OFF_HDR: usize = 1024; // 16-byte request header
const OFF_STATUS: usize = 1040; // 1-byte status

/// Queue size we request (capped by `QueueNumMax`). 8 is ample: one 3-descriptor request is ever in
/// flight and it is polled to completion.
const QSIZE_WANT: u16 = 8;

/// The per-target seam. Everything a virtio-mmio driver needs from a CPU backend, and nothing else.
pub trait VirtioHal {
    /// A zeroed 4 KiB frame the device may DMA to/from. MUST be identity-mapped (VA == PA), because
    /// the returned address is both what the driver writes through and what it hands the device.
    /// `None` means the frame allocator is exhausted — `init` then fails closed.
    fn alloc_frame() -> Option<usize>;
    /// Full system barrier: orders Normal-memory ring writes before the Device-memory notify, and the
    /// used-ring read after `used.idx` is observed to advance.
    fn barrier();
}

/// Where a target's virtio-mmio transports live.
#[derive(Clone, Copy, Debug)]
pub struct MmioLayout {
    /// Address of transport slot 0.
    pub base: usize,
    /// Bytes between slots.
    pub stride: usize,
    /// Number of slots to scan.
    pub slots: usize,
}

/// What `init` observed, so the CALLER prints it with its own console. Returning the facts instead of
/// logging them is what lets one driver serve targets whose `kprintln!` are different macros.
#[derive(Clone, Copy, Debug)]
pub struct InitReport {
    pub base: usize,
    pub version: u32,
    pub device_id: u32,
    pub features_lo: u32,
    pub features_hi: u32,
    pub flush_ok: bool,
    pub queue_num_max: u32,
    pub qsize: u16,
    pub capacity_sectors: u64,
}

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    // SAFETY: caller passes a mapped virtio-mmio register address (device memory, 4-byte aligned).
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

/// Scan `layout`'s slots for a block transport. Returns its MMIO base, or `None` when none is attached
/// (bare `cargo run` with no `-drive`) — the graceful-skip path every target shares.
///
/// # Safety
/// Every address in `layout` must be mapped as device memory in the active address space.
pub unsafe fn probe(layout: &MmioLayout) -> Option<usize> {
    for i in 0..layout.slots {
        let base = layout.base + i * layout.stride;
        if r32(base, R_MAGIC) != VIRTIO_MAGIC {
            continue;
        }
        // DeviceID 0 == a present-but-empty transport slot; keep scanning for the block device.
        if r32(base, R_DEVICE_ID) == VIRTIO_ID_BLOCK {
            return Some(base);
        }
    }
    None
}

/// A live virtio-blk device: the MMIO base, the identity-mapped addresses of its single virtqueue's
/// rings and request buffers (the DMA targets handed to the device), its capacity, and whether FLUSH
/// was negotiated.
pub struct VirtioBlk<H: VirtioHal> {
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
    _hal: PhantomData<H>,
}

impl<H: VirtioHal> VirtioBlk<H> {
    /// Bring the device up: reset → feature negotiation → queue 0 setup → DRIVER_OK. Returns the
    /// device plus an [`InitReport`] for the caller to log. Fails closed on anything unexpected.
    ///
    /// # Safety
    /// `base` must be a mapped virtio-mmio block transport (as returned by [`probe`]), and
    /// `H::alloc_frame` must return identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(base: usize) -> Result<(Self, InitReport), &'static str> {
        let version = r32(base, R_VERSION);
        let device_id = r32(base, R_DEVICE_ID);
        if version != VIRTIO_VERSION_MODERN {
            return Err("legacy (v1) virtio-mmio not supported — fail closed");
        }

        // 1 — reset, then ACKNOWLEDGE + DRIVER (VIRTIO 1.1 §3.1.1).
        w32(base, R_STATUS, 0);
        let mut status = S_ACKNOWLEDGE;
        w32(base, R_STATUS, status);
        status |= S_DRIVER;
        w32(base, R_STATUS, status);

        // 2 — feature negotiation. Accept only VIRTIO_F_VERSION_1 (mandatory for modern) plus
        //     VIRTIO_BLK_F_FLUSH when offered (a real durability barrier). Everything else is cleared:
        //     we implement nothing that needs it.
        w32(base, R_DEVICE_FEATURES_SEL, 0);
        let features_lo = r32(base, R_DEVICE_FEATURES);
        w32(base, R_DEVICE_FEATURES_SEL, 1);
        let features_hi = r32(base, R_DEVICE_FEATURES);

        let version1 = (features_hi & (1 << F_VERSION_1_BIT)) != 0;
        let flush_ok = (features_lo & (1 << F_BLK_FLUSH_BIT)) != 0;
        if !version1 {
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

        // 4 — queue 0 setup: one frame for the rings + request buffers, one for the 4 KiB data buffer.
        w32(base, R_QUEUE_SEL, 0);
        let queue_num_max = r32(base, R_QUEUE_NUM_MAX);
        if queue_num_max == 0 {
            return Err("queue 0 unavailable (QueueNumMax == 0)");
        }
        let qsize = core::cmp::min(QSIZE_WANT as u32, queue_num_max) as u16;

        let ring = H::alloc_frame().ok_or("frame allocator exhausted (ring)")?;
        let data = H::alloc_frame().ok_or("frame allocator exhausted (data)")?;

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
        H::barrier();
        w32(base, R_QUEUE_READY, 1);

        // 5 — DRIVER_OK: the device is live.
        status |= S_DRIVER_OK;
        w32(base, R_STATUS, status);

        let capacity_sectors = r64_config(base, 0);

        Ok((
            VirtioBlk {
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
                _hal: PhantomData,
            },
            InitReport {
                base,
                version,
                device_id,
                features_lo,
                features_hi,
                flush_ok,
                queue_num_max,
                qsize,
                capacity_sectors,
            },
        ))
    }

    /// Raw device capacity in 512-byte sectors (geometry, as the device reported it).
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
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
    /// completion. Returns the device status byte. The poll is BOUNDED: a device that never completes
    /// returns `Err` rather than spinning forever (the VM watchdog is only the backstop).
    unsafe fn submit(&self, head: u16) -> Result<u8, StorageError> {
        // avail ring layout: [flags:u16][idx:u16][ring:u16 * qsize][used_event:u16]
        let avail_idx_ptr = (self.avail + 2) as *mut u16;
        let avail_ring = (self.avail + 4) as *mut u16;
        // used ring layout:  [flags:u16][idx:u16][ring:{id:u32,len:u32} * qsize][avail_event:u16]
        let used_idx_ptr = (self.used + 2) as *const u16;

        let cur = read_volatile(avail_idx_ptr);
        let old_used = read_volatile(used_idx_ptr);
        write_volatile(avail_ring.add((cur % self.qsize) as usize), head);
        H::barrier(); // ring writes visible before we bump idx
        write_volatile(avail_idx_ptr, cur.wrapping_add(1));
        H::barrier(); // idx visible before we notify

        w32(self.base, R_QUEUE_NOTIFY, 0);

        // The bound is generous (millions of iterations), so a healthy device always finishes well
        // within it; only a broken ring layout exhausts it.
        let mut spins: u64 = 0;
        while read_volatile(used_idx_ptr) == old_used {
            spins += 1;
            if spins > 50_000_000 {
                return Err(StorageError::Device);
            }
            core::hint::spin_loop();
        }
        H::barrier(); // used.idx observed before we read the status the device wrote

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
        let h = self.hdr as *mut u32;
        write_volatile(h.add(0), rtype);
        write_volatile(h.add(1), 0);
        write_volatile((self.hdr + 8) as *mut u64, sector);
        // Sentinel, so a device that writes nothing is detected as a failure, not a stale OK.
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

impl<H: VirtioHal> BlockDevice for VirtioBlk<H> {
    fn num_blocks(&self) -> usize {
        (self.capacity_sectors / SECTORS_PER_BLOCK) as usize
    }

    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        let sector = self.check_block(idx, buf.len())?;
        // SAFETY: one request in flight; the data frame is ours and identity-mapped.
        unsafe {
            self.request(VIRTIO_BLK_T_IN, sector, true, true)?;
            let src = core::slice::from_raw_parts(self.data as *const u8, BLOCK_SIZE);
            buf.copy_from_slice(src);
        }
        Ok(())
    }

    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        let sector = self.check_block(idx, buf.len())?;
        // SAFETY: as above; the bytes are staged into the data frame, then handed to the device.
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
            // path would need the feature; it is negotiated above whenever offered.)
            return Ok(());
        }
        // SAFETY: flush is a header+status chain with no data buffer.
        unsafe { self.request(VIRTIO_BLK_T_FLUSH, 0, false, false) }
    }
}

const CAP_READ: &str = "dev.blk.read";
const CAP_WRITE: &str = "dev.blk.write";

/// The invariant suite for a REAL block device, shared by every target that has one (REQ-DRV-001,
/// REQ-STOR-002, REQ-FS-001). `dev` is consumed because the last group wraps it in a
/// [`DeviceGuard`]; `expect_blocks` is the geometry the caller's VM gate attached, so a wrong
/// sector/block mapping is caught before any I/O rather than surfacing later as corrupt data.
///
/// Groups: discovery → capacity → round-trip → journal commit/recover → **the whole filesystem
/// namespace** ([`crate::fs::selftest_on`], 12 behaviors, over this device) → capability gating.
/// Returns the number of invariants proved, or `(index, name)` of the first failure.
pub fn device_suite<D: BlockDevice, F: FnMut(usize, bool, &str)>(
    mut dev: D,
    expect_blocks: usize,
    mut log: F,
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

    check!(
        "virtio-blk: device discovered + initialized",
        dev.num_blocks() > 0
    );
    check!(
        "virtio-blk: capacity read matches the attached image geometry",
        dev.num_blocks() == expect_blocks
    );

    // A write → read-back round-trip over a real virtqueue returns exactly the written bytes.
    let home = crate::storage::DATA_START + 5;
    let mut pattern = [0u8; BLOCK_SIZE];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i as u8) ^ 0x5a;
    }
    let mut readback = [0u8; BLOCK_SIZE];
    check!(
        "virtio-blk: write -> read-back round-trip returns the written bytes",
        dev.write_block(home, &pattern).is_ok()
            && dev.flush().is_ok()
            && dev.read_block(home, &mut readback).is_ok()
            && readback == pattern
    );

    // The write-ahead journal commits over the REAL device, and a FRESH journal recovers the committed
    // state from the device bytes alone — crash consistency over real emulated storage.
    let h1 = crate::storage::DATA_START + 10;
    let h2 = crate::storage::DATA_START + 11;
    let (mut a, mut b) = ([0u8; BLOCK_SIZE], [0u8; BLOCK_SIZE]);
    a.fill(0xA1);
    b.fill(0xB2);
    let mut journal = Journal::new();
    let committed = journal.commit(&mut dev, &[(h1, a), (h2, b)]).is_ok();
    let mut recovered = Journal::new();
    let replayed = recovered.recover(&mut dev) == Ok(true);
    check!(
        "virtio-blk: journal commit + fresh recover reproduce state over real storage",
        committed
            && replayed
            && recovered.read(&dev, h1) == Ok(a)
            && recovered.read(&dev, h2) == Ok(b)
    );

    // The filesystem namespace (REQ-FS-001) over the REAL device: the same named behaviors every
    // target proves over a RAM disk, driven through the virtqueue — which is what makes "a create is
    // atomic across a crash" a claim about hardware. Destructive by design (it reformats).
    let fs_base = n;
    match crate::fs::selftest_on(&mut dev, |i, passed, name| log(fs_base + i, passed, name)) {
        Ok(count) => n += count,
        Err((i, name)) => return Err((fs_base + i, name)),
    }

    // Capability gating over the REAL device (REQ-DRV-002): no capability → no bytes move; a write
    // capability's bytes land. The same authority mechanism as every other Aletheia effect.
    let capblk = crate::storage::DATA_START + 20;
    let mut engine = CapEngine::new(0x5171_0b1c, 1_000_000);
    let read_cap = engine.mint("virtio-test", CAP_READ, Scope::All, Constraints::none());
    let write_cap = engine.mint("virtio-test", CAP_WRITE, Scope::All, Constraints::none());
    let mut guard = DeviceGuard::new(dev, CAP_READ, CAP_WRITE);
    let mut deny_buf = [0u8; BLOCK_SIZE];
    deny_buf.fill(0xEE);
    let denied = guard.write_block(&engine, &[], capblk, &deny_buf) == Err(DeviceError::Denied);
    // With no offered capability the device was never touched; confirm via an authorized read.
    let mut after_deny = [0u8; BLOCK_SIZE];
    let read_ok = guard
        .read_block(&engine, &[read_cap.clone()], capblk, &mut after_deny)
        .is_ok();
    let unchanged = after_deny.iter().all(|&x| x != 0xEE);
    // With the write capability the bytes actually land and read back.
    let mut landed = [0u8; BLOCK_SIZE];
    landed.fill(0x7c);
    let wrote = guard
        .write_block(&engine, &[read_cap.clone(), write_cap], capblk, &landed)
        .is_ok();
    let mut verify = [0u8; BLOCK_SIZE];
    let verified = guard
        .read_block(&engine, &[read_cap], capblk, &mut verify)
        .is_ok()
        && verify == landed;
    check!(
        "virtio-blk: capability-gated I/O to the real device (deny moves nothing; grant lands)",
        denied && read_ok && unchanged && wrote && verified
    );

    Ok(n)
}
