//! virtio-blk — the driver itself, defined ONCE, over any transport (REQ-DRV-001, ADR-036/ADR-037).
//!
//! The first real driver landed inside the aarch64 target crate (REQ-DRV-003). That was the right
//! shape for one target and the wrong shape for three: a split virtqueue, a feature handshake and a
//! request layout are **bus** facts, not CPU facts, so a per-target copy would mean the two
//! *first-class* targets (AMD64, RISC-V) either had no real storage or had a second implementation of
//! the same protocol — which is exactly the duplication gap-register Issue 1 exists to prevent.
//!
//! Two seams keep it one driver:
//!
//! * [`VirtioHal`] — what a **CPU backend** provides: a DMA-able identity-mapped frame (each target's
//!   own frame allocator) and a barrier instruction (`dsb sy` on aarch64, `fence iorw, iorw` on
//!   RISC-V, `mfence` on x86-64).
//! * [`Transport`] — how the **bus** exposes the device's registers. [`MmioTransport`] is virtio-mmio
//!   (modern/v2), used by both QEMU `virt` machines; x86-64's q35 has no MMIO window at all and speaks
//!   virtio-**pci**, whose registers live in capability-described BAR regions — so that target
//!   implements this trait instead of getting its own driver (ADR-037).
//!
//! Everything else — reset, negotiation, queue setup, descriptor chains, the bounded poll, the
//! `BlockDevice` impl — is this module, and is proved identically on every target that has a disk.
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
use crate::dma::DmaRegistry;
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
/// VIRTIO_F_IOMMU_PLATFORM == bit 33, i.e. bit 1 of the high half. Offered only when the
/// platform sits behind an IOMMU; accepting it commits descriptors to IOVAs the platform
/// translates - which is exactly what the VT-d identity domain provides, so addresses do not
/// change. Declining it makes a device that REQUIRES the feature clear FEATURES_OK.
const F_IOMMU_PLATFORM_BIT: u32 = 1;

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

/// How a bus exposes one virtio device's registers. Implemented by [`MmioTransport`] here, and by a
/// target that must reach the device over a different bus (x86-64's virtio-pci, ADR-037).
///
/// Every method is `unsafe` because each one touches device registers: the implementor promises the
/// addresses it was built from are mapped, aligned, and belong to a virtio device of the right kind.
/// The driver calls them in the order VIRTIO 1.1 §3.1.1 requires and never concurrently.
pub trait Transport {
    /// `(transport version as this bus reports it, virtio device id)` — for the caller's boot log and
    /// for the device-kind check the constructor already made.
    fn identity(&self) -> (u32, u32);
    /// Read the 32-bit half `sel` (0 = bits 0..31, 1 = bits 32..63) of the device feature bits.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn device_features(&self, sel: u32) -> u32;
    /// Write the 32-bit half `sel` of the driver (accepted) feature bits.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn set_driver_features(&self, sel: u32, value: u32);
    /// Read the device status byte.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn status(&self) -> u32;
    /// Write the device status byte.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn set_status(&self, value: u32);
    /// Select the queue subsequent queue calls refer to.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn select_queue(&self, queue: u16);
    /// Hook called ONCE, right after the driver selects the queue it will use, for a transport whose
    /// notify address depends on a per-queue register it must read while that queue is selected
    /// (virtio-pci's `queue_notify_off`). Latching it here is what lets [`Transport::notify`] take
    /// `&self` and never disturb `queue_select` with a request in flight. Default: nothing to do.
    ///
    /// # Safety
    /// A queue must be selected, and the transport's registers mapped.
    unsafe fn after_queue_select(&mut self) {}
    /// Largest queue size the selected queue supports (0 = the queue does not exist).
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn queue_num_max(&self) -> u32;
    /// Set the negotiated size of the selected queue.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn set_queue_num(&self, size: u32);
    /// Publish the selected queue's three ring addresses (physical).
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn set_queue_addrs(&self, desc: u64, avail: u64, used: u64);
    /// Mark the selected queue live.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn queue_ready(&self);
    /// Tell the device a queue has new buffers.
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn notify(&self, queue: u16);
    /// Read 8 bytes of device-specific config space at `off` (blk capacity is at 0).
    ///
    /// # Safety
    /// The transport's registers must be mapped and the caller must not call these concurrently — the
    /// driver calls them in the order VIRTIO 1.1 §3.1.1 requires, with one request in flight.
    unsafe fn config_u64(&self, off: usize) -> u64;
}

/// The per-target CPU seam. Everything a virtio driver needs from a CPU backend, and nothing else.
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
    probe_nth(layout, 0)
}

/// Scan for the `nth` (0-based) block transport in `layout`. A target that attaches more than one disk
/// — a scratch medium for the destructive suites and a PERSISTENT one the OS keeps its store on
/// (REQ-STOR-003) — needs to address them separately, and by index is the only ordering the transport
/// window gives. Returns `None` when there are fewer than `nth + 1` block devices.
///
/// # Safety
/// Every address in `layout` must be mapped as device memory in the active address space.
pub unsafe fn probe_nth(layout: &MmioLayout, nth: usize) -> Option<usize> {
    probe_nth_kind(layout, VIRTIO_ID_BLOCK, nth)
}

/// Scan for the `nth` (0-based) transport of a given virtio DEVICE KIND — block (2), network (1), … A
/// second device kind is a second `device_id`, not a second driver framework (ADR-041).
///
/// # Safety
/// Every address in `layout` must be mapped as device memory in the active address space.
pub unsafe fn probe_nth_kind(layout: &MmioLayout, device_id: u32, nth: usize) -> Option<usize> {
    let mut seen = 0usize;
    for i in 0..layout.slots {
        let base = layout.base + i * layout.stride;
        if r32(base, R_MAGIC) != VIRTIO_MAGIC {
            continue;
        }
        // DeviceID 0 == a present-but-empty transport slot; keep scanning for the kind asked for.
        if r32(base, R_DEVICE_ID) == device_id {
            if seen == nth {
                return Some(base);
            }
            seen += 1;
        }
    }
    None
}

/// The virtio-mmio (modern / v2) transport: one register window, the layout VIRTIO 1.1 §4.2.2 fixes.
pub struct MmioTransport {
    base: usize,
    version: u32,
    device_id: u32,
}

impl MmioTransport {
    /// Bind to the transport at `base`, refusing anything that is not a modern virtio **block**
    /// device: a wrong magic, a legacy (v1) transport, or another device kind. Failing here is why the
    /// driver body never has to re-check what it is talking to.
    ///
    /// # Safety
    /// `base` must be mapped as device memory (typically a base returned by [`probe`]).
    pub unsafe fn new(base: usize) -> Result<Self, &'static str> {
        Self::new_for(base, VIRTIO_ID_BLOCK)
    }

    /// Bind to the transport at `base`, requiring the given virtio device KIND — block (2), network (1), …
    /// Checking the kind here is why no driver body has to re-check what it is talking to.
    ///
    /// # Safety
    /// `base` must be mapped as device memory (typically from [`probe_nth_kind`]).
    pub unsafe fn new_for(base: usize, want_device_id: u32) -> Result<Self, &'static str> {
        if r32(base, R_MAGIC) != VIRTIO_MAGIC {
            return Err("no virtio magic at this transport address");
        }
        let version = r32(base, R_VERSION);
        let device_id = r32(base, R_DEVICE_ID);
        if version != VIRTIO_VERSION_MODERN {
            return Err("legacy (v1) virtio-mmio not supported — fail closed");
        }
        if device_id != want_device_id {
            return Err("transport is not the requested device kind — fail closed");
        }
        Ok(MmioTransport {
            base,
            version,
            device_id,
        })
    }
}

impl Transport for MmioTransport {
    fn identity(&self) -> (u32, u32) {
        (self.version, self.device_id)
    }
    unsafe fn device_features(&self, sel: u32) -> u32 {
        w32(self.base, R_DEVICE_FEATURES_SEL, sel);
        r32(self.base, R_DEVICE_FEATURES)
    }
    unsafe fn set_driver_features(&self, sel: u32, value: u32) {
        w32(self.base, R_DRIVER_FEATURES_SEL, sel);
        w32(self.base, R_DRIVER_FEATURES, value);
    }
    unsafe fn status(&self) -> u32 {
        r32(self.base, R_STATUS)
    }
    unsafe fn set_status(&self, value: u32) {
        w32(self.base, R_STATUS, value);
    }
    unsafe fn select_queue(&self, queue: u16) {
        w32(self.base, R_QUEUE_SEL, queue as u32);
    }
    unsafe fn queue_num_max(&self) -> u32 {
        r32(self.base, R_QUEUE_NUM_MAX)
    }
    unsafe fn set_queue_num(&self, size: u32) {
        w32(self.base, R_QUEUE_NUM, size);
    }
    unsafe fn set_queue_addrs(&self, desc: u64, avail: u64, used: u64) {
        w32(self.base, R_QUEUE_DESC_LOW, desc as u32);
        w32(self.base, R_QUEUE_DESC_HIGH, (desc >> 32) as u32);
        w32(self.base, R_QUEUE_DRIVER_LOW, avail as u32);
        w32(self.base, R_QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
        w32(self.base, R_QUEUE_DEVICE_LOW, used as u32);
        w32(self.base, R_QUEUE_DEVICE_HIGH, (used >> 32) as u32);
    }
    unsafe fn queue_ready(&self) {
        w32(self.base, R_QUEUE_READY, 1);
    }
    unsafe fn notify(&self, queue: u16) {
        w32(self.base, R_QUEUE_NOTIFY, queue as u32);
    }
    unsafe fn config_u64(&self, off: usize) -> u64 {
        r64_config(self.base, off)
    }
}

/// A live virtio-blk device: its transport, the identity-mapped addresses of its single virtqueue's
/// rings and request buffers (the DMA targets handed to the device), its capacity, and whether FLUSH
/// was negotiated.
pub struct VirtioBlk<H: VirtioHal, T: Transport> {
    transport: T,
    desc: usize,
    avail: usize,
    used: usize,
    hdr: usize,
    status: usize,
    data: usize,
    qsize: u16,
    capacity_sectors: u64,
    flush_ok: bool,
    /// What this device may be told about (REQ-DRV-006, ADR-043). `virtioblk` predates `virtq` and keeps its
    /// own fixed ring, so it carries its own registry — otherwise its descriptors would be the one path in
    /// the kernel that still names addresses nobody registered.
    dma: DmaRegistry,
    /// Completion-poll budget for [`Self::submit`], in spin iterations. Defaults to
    /// [`SUBMIT_SPINS`]; a caller driving PROBE kicks (the VT-d suite pays one timeout per kick
    /// when the platform loses completions) may tighten it without touching every other user.
    completion_spins: u64,
    _hal: PhantomData<H>,
}

/// The default bounded-wait budget for one request completion: millions of iterations, so a
/// healthy device always finishes well within it and only a broken ring layout exhausts it.
pub const SUBMIT_SPINS: u64 = 50_000_000;

impl<H: VirtioHal, T: Transport> VirtioBlk<H, T> {
    /// Bring the device up: reset → feature negotiation → queue 0 setup → DRIVER_OK. Returns the
    /// device plus an [`InitReport`] for the caller to log. Fails closed on anything unexpected.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio block device, and `H::alloc_frame` must return
    /// identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(mut transport: T) -> Result<(Self, InitReport), &'static str> {
        let (version, device_id) = transport.identity();

        // 1 — reset, then ACKNOWLEDGE + DRIVER (VIRTIO 1.1 §3.1.1).
        transport.set_status(0);
        let mut status = S_ACKNOWLEDGE;
        transport.set_status(status);
        status |= S_DRIVER;
        transport.set_status(status);

        // 2 — feature negotiation. Accept only VIRTIO_F_VERSION_1 (mandatory for modern) plus
        //     VIRTIO_BLK_F_FLUSH when offered (a real durability barrier). Everything else is cleared:
        //     we implement nothing that needs it.
        let features_lo = transport.device_features(0);
        let features_hi = transport.device_features(1);

        let version1 = (features_hi & (1 << F_VERSION_1_BIT)) != 0;
        let flush_ok = (features_lo & (1 << F_BLK_FLUSH_BIT)) != 0;
        // Acknowledge the platform-IOMMU feature whenever offered: behind the VT-d identity
        // domain descriptor addresses are unchanged, so acceptance costs nothing and a device
        // that REQUIRES the feature keeps FEATURES_OK.
        let iommu_platform = (features_hi & (1 << F_IOMMU_PLATFORM_BIT)) != 0;
        if !version1 {
            return Err("device does not offer VIRTIO_F_VERSION_1 — fail closed");
        }
        let drv_lo = if flush_ok { 1 << F_BLK_FLUSH_BIT } else { 0 };
        let drv_hi = 1 << F_VERSION_1_BIT
            | if iommu_platform {
                1 << F_IOMMU_PLATFORM_BIT
            } else {
                0
            };
        transport.set_driver_features(0, drv_lo);
        transport.set_driver_features(1, drv_hi);

        // 3 — FEATURES_OK, then read it back: if the device clears it, our set is unacceptable.
        status |= S_FEATURES_OK;
        transport.set_status(status);
        if transport.status() & S_FEATURES_OK == 0 {
            transport.set_status(status | S_FAILED);
            return Err("device rejected negotiated features (FEATURES_OK cleared)");
        }

        // 4 — queue 0 setup: one frame for the rings + request buffers, one for the 4 KiB data buffer.
        transport.select_queue(0);
        transport.after_queue_select();
        let queue_num_max = transport.queue_num_max();
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

        transport.set_queue_num(qsize as u32);
        transport.set_queue_addrs(desc as u64, avail as u64, used as u64);
        H::barrier();
        transport.queue_ready();

        // 5 — DRIVER_OK: the device is live.
        status |= S_DRIVER_OK;
        transport.set_status(status);

        let capacity_sectors = transport.config_u64(0);

        // Register the two frames this driver hands the device: the ring (descriptors + both rings + the
        // request header and status byte) and the 4 KiB data buffer.
        let mut dma = DmaRegistry::new();
        dma.register(ring, crate::dma::PAGE, "virtio-blk.ring")
            .map_err(|_| "virtio-blk: the ring frame was refused as a DMA region")?;
        dma.register(data, crate::dma::PAGE, "virtio-blk.data")
            .map_err(|_| "virtio-blk: the data frame was refused as a DMA region")?;

        Ok((
            VirtioBlk {
                transport,
                desc,
                avail,
                used,
                hdr,
                status: status_buf,
                data,
                qsize,
                capacity_sectors,
                flush_ok,
                dma,
                completion_spins: SUBMIT_SPINS,
                _hal: PhantomData,
            },
            InitReport {
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

    /// Tighten (or restore) the completion-poll budget for SUBSEQUENT requests. Probe callers -
    /// the VT-d suite pays one full timeout per kick when the platform loses completions - set a
    /// smaller bound here instead of paying the default millions per stimulus.
    pub fn set_completion_spins(&mut self, spins: u64) {
        self.completion_spins = spins;
    }

    /// Recover a device after an error: full reset → renegotiate → re-publish the SAME rings and
    /// buffers (the frames are still ours) → DRIVER_OK. The queue's avail/used indices restart at
    /// zero on both sides, which is exactly what a fresh init produces, so no request can be
    /// half-answered afterwards. Idempotent: resetting a healthy device is harmless.
    pub fn reset(&mut self) -> Result<(), &'static str> {
        unsafe {
            self.transport.set_status(0);
            let mut status = S_ACKNOWLEDGE;
            self.transport.set_status(status);
            status |= S_DRIVER;
            self.transport.set_status(status);

            let features_lo = self.transport.device_features(0);
            let features_hi = self.transport.device_features(1);
            let version1 = (features_hi & (1 << F_VERSION_1_BIT)) != 0;
            if !version1 {
                return Err("reset: device no longer offers VIRTIO_F_VERSION_1 — fail closed");
            }
            self.flush_ok = (features_lo & (1 << F_BLK_FLUSH_BIT)) != 0;
            let iommu_platform = (features_hi & (1 << F_IOMMU_PLATFORM_BIT)) != 0;
            let drv_lo = if self.flush_ok {
                1 << F_BLK_FLUSH_BIT
            } else {
                0
            };
            let drv_hi = 1 << F_VERSION_1_BIT
                | if iommu_platform {
                    1 << F_IOMMU_PLATFORM_BIT
                } else {
                    0
                };
            self.transport.set_driver_features(0, drv_lo);
            self.transport.set_driver_features(1, drv_hi);

            status |= S_FEATURES_OK;
            self.transport.set_status(status);
            if self.transport.status() & S_FEATURES_OK == 0 {
                self.transport.set_status(status | S_FAILED);
                return Err("reset: device rejected negotiated features (FEATURES_OK cleared)");
            }

            self.transport.select_queue(0);
            self.transport.after_queue_select();
            if self.transport.queue_num_max() == 0 {
                return Err("reset: queue 0 unavailable (QueueNumMax == 0)");
            }
            self.transport.set_queue_num(self.qsize as u32);
            self.transport
                .set_queue_addrs(self.desc as u64, self.avail as u64, self.used as u64);
            H::barrier();
            self.transport.queue_ready();

            status |= S_DRIVER_OK;
            self.transport.set_status(status);
        }
        Ok(())
    }

    /// Would an address this driver never registered be refused as a descriptor? The suite asks this to
    /// prove the DMA gate denies by default (REQ-DRV-006, ADR-043) rather than merely existing.
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        // An address far from either registered frame, and one that overruns the data frame.
        !self.dma.visible(0x7fff_0000_0000, 64)
            && !self.dma.visible(self.data, crate::dma::PAGE * 2)
    }

    /// DMA regions this driver registered (its ring and data frames).
    pub fn dma_regions(&self) -> usize {
        self.dma.live_regions()
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
    /// completion. Returns `(device status byte, bytes the device reported writing)`. The poll is
    /// BOUNDED: a device that never completes returns `Err` rather than spinning forever (the VM
    /// watchdog is only the backstop). The used-ring entry's `id` is VALIDATED — a device that
    /// names another descriptor's completion is lying about whose request finished.
    unsafe fn submit(&self, head: u16) -> Result<(u8, u32), StorageError> {
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

        self.transport.notify(0);

        // The bound is generous (millions of iterations), so a healthy device always finishes well
        // within it; only a broken ring layout exhausts it.
        let mut spins: u64 = 0;
        while read_volatile(used_idx_ptr) == old_used {
            spins += 1;
            if spins > self.completion_spins {
                return Err(StorageError::Device);
            }
            core::hint::spin_loop();
        }
        H::barrier(); // used.idx observed before we read the entry the device wrote

        // The completed entry lives at (old_used % qsize): [id: u32][len: u32]. A device that names
        // a different head is answering a request we did not make — refuse it here rather than
        // interpreting whatever status byte follows.
        let entry = (self.used + 4 + 8 * (old_used % self.qsize) as usize) as *const u32;
        let id = read_volatile(entry);
        if id != head as u32 {
            return Err(StorageError::Device);
        }
        let len = read_volatile(entry.add(1));

        Ok((read_volatile(self.status as *const u8), len))
    }

    /// Issue one virtio-blk request. `has_data` builds the 3-descriptor chain (header, data, status);
    /// a flush omits the data descriptor. `device_writes_data` marks the data buffer device-writable
    /// (a READ). The completion is validated in FULL before success is reported: status must be OK
    /// AND the used-ring byte count must be EXACTLY what the chain promised — a device that completes
    /// short (a PARTIAL read or write) or long is an error, never silently-truncated data.
    unsafe fn request(
        &self,
        rtype: u32,
        sector: u64,
        has_data: bool,
        device_writes_data: bool,
    ) -> Result<(), StorageError> {
        // THE GATE (REQ-DRV-006, ADR-043): every address about to become a descriptor must be one this
        // driver registered. The header, status byte and data buffer all live in the two registered frames,
        // so a miscalculation that walked outside them is refused here instead of reaching the device.
        for (addr, len) in [
            (self.hdr, 16usize),
            (self.status, 1),
            (self.data, if has_data { BLOCK_SIZE } else { 0 }),
        ] {
            if len > 0 && !self.dma.visible(addr, len) {
                return Err(StorageError::Device);
            }
        }
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

        // Bytes the DEVICE reports writing: the status byte always, plus the whole data buffer for
        // a READ. Anything else — a short READ (partial data), a padded WRITE, a padded FLUSH — is a
        // refused completion, checked BEFORE any data reaches the caller.
        let expect_wlen: u32 = if has_data && device_writes_data {
            BLOCK_SIZE as u32 + 1
        } else {
            1
        };
        match self.submit(0)? {
            (VIRTIO_BLK_S_OK, len) if len == expect_wlen => Ok(()),
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

impl<H: VirtioHal, T: Transport> BlockDevice for VirtioBlk<H, T> {
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
    dev: D,
    expect_blocks: usize,
    mut log: F,
) -> Result<usize, (usize, &'static str)> {
    let mut dev = dev;
    device_suite_gated(&mut dev, expect_blocks, true, &mut log)
}

/// As [`device_suite`], but the caller states whether the device's DMA gate refuses an unregistered
/// address. A `MemBlockDevice` has no DMA at all, so the host test passes `true` for the same reason the
/// geometry check takes a parameter: the suite asserts what the CALLER can vouch for, never a default.
pub fn device_suite_gated<D: BlockDevice, F: FnMut(usize, bool, &str)>(
    dev: &mut D,
    expect_blocks: usize,
    dma_gate_ok: bool,
    log: &mut F,
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

    // The DMA gate is live on THIS device: its ring and data frames are registered, and an address the
    // driver never registered is refused before it could become a descriptor.
    check!(
        "virtio-blk: the DMA gate denies an unregistered descriptor address (ring and data are registered)",
        dma_gate_ok
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
    let committed = journal.commit(&mut *dev, &[(h1, a), (h2, b)]).is_ok();
    let mut recovered = Journal::new();
    let replayed = recovered.recover(&mut *dev) == Ok(true);
    check!(
        "virtio-blk: journal commit + fresh recover reproduce state over real storage",
        committed
            && replayed
            && recovered.read(&*dev, h1) == Ok(a)
            && recovered.read(&*dev, h2) == Ok(b)
    );

    // The filesystem namespace (REQ-FS-001) over the REAL device: the same named behaviors every
    // target proves over a RAM disk, driven through the virtqueue — which is what makes "a create is
    // atomic across a crash" a claim about hardware. Destructive by design (it reformats).
    let fs_base = n;
    match crate::fs::selftest_on(&mut *dev, |i, passed, name| log(fs_base + i, passed, name)) {
        Ok(count) => n += count,
        Err((i, name)) => return Err((fs_base + i, name)),
    }

    // Capability gating over the REAL device (REQ-DRV-002): no capability → no bytes move; a write
    // capability's bytes land. The same authority mechanism as every other Aletheia effect.
    let capblk = crate::storage::DATA_START + 20;
    let mut engine = CapEngine::new(0x5171_0b1c, 1_000_000);
    let read_cap = engine.mint("virtio-test", CAP_READ, Scope::All, Constraints::none());
    let write_cap = engine.mint("virtio-test", CAP_WRITE, Scope::All, Constraints::none());
    let mut guard = DeviceGuard::new(&mut *dev, CAP_READ, CAP_WRITE);
    let mut deny_buf = [0u8; BLOCK_SIZE];
    deny_buf.fill(0xEE);
    let denied = guard.write_block(&engine, &[], capblk, &deny_buf) == Err(DeviceError::Denied);
    // With no offered capability the device was never touched; confirm via an authorized read.
    let mut after_deny = [0u8; BLOCK_SIZE];
    let read_ok = guard
        .read_block(&engine, &[read_cap], capblk, &mut after_deny)
        .is_ok();
    let unchanged = after_deny.iter().all(|&x| x != 0xEE);
    // With the write capability the bytes actually land and read back.
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
        "virtio-blk: capability-gated I/O to the real device (deny moves nothing; grant lands)",
        denied && read_ok && unchanged && wrote && verified
    );

    Ok(n)
}
