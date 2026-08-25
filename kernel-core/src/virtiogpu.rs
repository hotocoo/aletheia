//! virtio-gpu: the machine's first real graphics device (REQ-GFX-001).
//!
//! Until now Aletheia's only face was a serial line: every gate asserted its markers over UART, and
//! the framebuffer half of "an OS you can sit in front of" existed nowhere outside architecture
//! prose (ALET-P2-021). This module is the first real slice — a driver for QEMU's virtio-gpu
//! device over the SAME substrate the block and network drivers share ([`VirtioHal`],
//! [`Transport`], [`Virtqueue`] — the chain support this module needed entered through [`Virtqueue::add_chain`]),
//! plus enough of the 2D protocol to prove the path end to end against something that ANSWERS.
//!
//! ## Why display info, flushes and error codes are a real proof
//!
//! A driver that only pushes pixels proves nothing: bytes written into a ring nobody reads look
//! identical to bytes that vanished. So the suite talks to the device three ways:
//!
//! * **GET_DISPLAY_INFO** — the device must WRITE structured geometry into driver memory
//!   (per-scanout rects and enabled flags). Reading back a sane table exercises the request path
//!   AND the response path in one round trip.
//! * **the full resource lifecycle** — CREATE_2D, ATTACH_BACKING (sixteen DMA-gated pages),
//!   SET_SCANOUT, TRANSFER_TO_HOST_2D, RESOURCE_FLUSH, DETACH_BACKING, UNREF — each answered
//!   OK_NODATA by a device that validated what we sent.
//! * **the device's own ERROR grammar** — after UNREF, the SAME flush is sent again and the device
//!   answers ERR_INVALID_RESOURCE_ID. An echo would answer OK; only a device that PARSES our
//!   commands can reject one. That is the proof the conversation is real in BOTH directions.
//!
//! ## Scope, stated
//!
//! One control queue (the cursor queue is untouched), polled completion (this kernel has no
//! interrupts yet), one outstanding command at a time. Transferred pixels land on the HOST
//! surface — which under `-display none` is nowhere visible; what this slice proves is the
//! protocol path, not a picture. No VIRGL/3D, no contexts, no EDID, no blob resources: every
//! optional feature is declined at negotiation, because a feature the driver does not understand
//! is a behavior it cannot be proved against. Backing pages stay registered in the queue's DMA
//! registry for the queue's lifetime (bounded by MAX_REGIONS); revocation-on-detach is named
//! follow-on work, not a hidden leak. The next slice renders text into a real framebuffer and
//! puts it on the scanout.

use alloc::vec::Vec;
use core::cell::Cell;
use core::marker::PhantomData;

use crate::dma::PAGE;
use crate::virtioblk::{Transport, VirtioHal};
use crate::virtq::Virtqueue;

/// virtio device id for a GPU (VIRTIO 1.1 §5.7).
pub const VIRTIO_ID_GPU: u32 = 16;

/// The control queue. Queue 1 is the cursor queue; this driver moves no cursor.
const CTRL_QUEUE: u16 = 0;

/// Scanouts the protocol can name (VIRTIO_GPU_MAX_SCANOUTS).
pub const MAX_SCANOUTS: usize = 16;

/// `struct virtio_gpu_ctrl_hdr` — 24 bytes: type, flags, fence_id(u64), ctx_id, ring_idx+pad[3].
pub const CTRL_HDR_LEN: usize = 24;

/// One `virtio_gpu_display_one`: a rect plus enabled and flags words.
pub const DISPLAY_ONE_LEN: usize = 24;

/// The GET_DISPLAY_INFO response: header plus MAX_SCANOUTS entries.
pub const DISPLAY_INFO_RESP_LEN: usize = CTRL_HDR_LEN + MAX_SCANOUTS * DISPLAY_ONE_LEN;

/// Device-writable half of the control chain. The longest answer this driver asks for is the
/// display-info table (408 B); half a page covers it with room to NOTICE an overrun attempt.
const RESP_CAP: usize = 512;

/// Bounded completion wait — the same doctrine as the block and network drivers: a device that
/// never completes yields an error instead of hanging past the VM watchdog.
const TIMEOUT_SPINS: u64 = 20_000_000;

/// Feature bit for VIRTIO_F_VERSION_1 (bit 32 ⇒ bit 0 of the high word). Every GPU-specific
/// feature (VIRGL, EDID, blobs) is DECLINED — see the scope note above.
const F_VERSION_1_BIT: u32 = 0;
/// VIRTIO_F_IOMMU_PLATFORM == bit 33, i.e. bit 1 of the high half. ACCEPTED whenever offered:
/// behind the VT-d identity domain descriptor addresses are unchanged, and a device that
/// REQUIRES the feature clears FEATURES_OK otherwise. See virtioblk for the full rationale.
const F_IOMMU_PLATFORM_BIT: u32 = 1;

/// Device status bits (VIRTIO 1.1 §3.1.1).
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;
const S_FAILED: u32 = 0x80;

/// `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM` — the one pixel format this driver speaks.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 1;

// Command codes (include/uapi/linux/virtio_gpu.h, enum virtio_gpu_ctrl_type). Values are pinned
// against the UAPI header by host tests, because a typo in one of these is a conversation with
// nobody.
pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_SET_SCANOUT: u32 = 0x0103;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;

/// Success responses.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// Error responses — the device's OWN grammar, the thing that proves it parses us.
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// Bounds a 2D resource may have. A resource is backed by guest frames this kernel owns, so its
/// worst-case size is a memory-safety fact, not a preference.
pub const MAX_RESOURCE_EXTENT_PX: u32 = 4096;
pub const MAX_RESOURCE_AREA_PX: u64 = 4 * 1024 * 1024;

/// Geometric sanity ANY rectangle must satisfy before this driver sends or believes it — applied
/// to rects the driver transmits (fail closed on nonsense) and rects a device reports (fail
/// closed on a liar) alike. Wider than the resource bound above: a display can be bigger than a
/// resource this kernel is willing to back.
pub const MAX_RECT_EXTENT_PX: u32 = 16384;

/// Resources tracked at once, and backing pages per attach. Both bounds are refusal rules, not
/// capacity claims: the table exists so every rect argument is validated BEFORE the device hears
/// about it.
const MAX_RESOURCES: usize = 8;
/// Backing pages per attach. The console surface (640x240 BGRA) is 150 single-frame entries in
/// ONE attach command — 32 + 150*16 = 2432 bytes of request, inside one DMA frame. QEMU accepts
/// up to 16384 entries; OUR bound is the command buffer, not the device.
pub const MAX_BACKING_ENTRIES: usize = 160;

/// The console surface this kernel ships (REQ-GFX-002): 640 x 240 BGRA pixels — 80 columns of 8px
/// glyphs, 15 rows of 16px lines — 600 KiB, 150 backing pages.
pub const CONSOLE_FB_WIDTH: u32 = 640;
pub const CONSOLE_FB_HEIGHT: u32 = 240;
/// Backing pages the console surface needs: width * height * 4 bytes, page-rounded.
pub const CONSOLE_FB_PAGES: usize =
    (CONSOLE_FB_WIDTH as usize * CONSOLE_FB_HEIGHT as usize * 4).div_ceil(crate::dma::PAGE);

/// Why a graphics operation failed. Refusals are the DRIVER's own, issued before any device
/// traffic and counted; Device errors carry the device's own response code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuError {
    Unsupported(&'static str),
    Queue(&'static str),
    Refused(&'static str),
    Device(u32),
    Timeout,
}

/// A protocol rectangle (`struct virtio_gpu_rect`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The full-extent rect of a w×h resource — what SET_SCANOUT binds and the suite flushes.
    pub fn covering(width: u32, height: u32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Does `self` fully contain `inner`? Computed in 64 bits so no coordinate can wrap: the
    /// classic u32 overflow at x=4 294 967 295 must read as "outside", never as "inside".
    /// Containment is the rule every rect-carrying command validates BEFORE the device hears
    /// anything — a transfer or flush past a resource's extents would touch backing the
    /// resource does not have.
    pub fn contains(&self, inner: Rect) -> bool {
        if inner.width == 0 || inner.height == 0 {
            return false;
        }
        let sx = self.x as u64;
        let sy = self.y as u64;
        let ex = sx + self.width as u64;
        let ey = sy + self.height as u64;
        let ix = inner.x as u64;
        let iy = inner.y as u64;
        ix >= sx && iy >= sy && ix + inner.width as u64 <= ex && iy + inner.height as u64 <= ey
    }

    /// Nonzero, within the geometric bound, and free of u32 overflow at its edges.
    pub fn is_sane(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_RECT_EXTENT_PX
            && self.height <= MAX_RECT_EXTENT_PX
            && self.x as u64 + self.width as u64 <= u32::MAX as u64
            && self.y as u64 + self.height as u64 <= u32::MAX as u64
    }
}

/// One entry of the display-info table, as the DEVICE reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scanout {
    pub rect: Rect,
    pub enabled: bool,
}

/// May a 2D resource of this geometry be created? Zero displays nothing; past either bound the
/// backing could exceed what this kernel is willing to owe a device.
pub fn validate_create(width: u32, height: u32) -> Result<(), GpuError> {
    if width == 0 || height == 0 {
        return Err(GpuError::Refused("a zero-extent resource displays nothing"));
    }
    if width > MAX_RESOURCE_EXTENT_PX || height > MAX_RESOURCE_EXTENT_PX {
        return Err(GpuError::Refused("a resource extent over the bound"));
    }
    if width as u64 * height as u64 > MAX_RESOURCE_AREA_PX {
        return Err(GpuError::Refused("a resource area over the bound"));
    }
    Ok(())
}

/// The name of a response code — refusals and logs name their causes, never print hex alone.
pub fn resp_name(code: u32) -> &'static str {
    match code {
        RESP_OK_NODATA => "OK_NODATA",
        RESP_OK_DISPLAY_INFO => "OK_DISPLAY_INFO",
        RESP_ERR_UNSPEC => "ERR_UNSPEC",
        RESP_ERR_OUT_OF_MEMORY => "ERR_OUT_OF_MEMORY",
        RESP_ERR_INVALID_SCANOUT_ID => "ERR_INVALID_SCANOUT_ID",
        RESP_ERR_INVALID_RESOURCE_ID => "ERR_INVALID_RESOURCE_ID",
        RESP_ERR_INVALID_CONTEXT_ID => "ERR_INVALID_CONTEXT_ID",
        RESP_ERR_INVALID_PARAMETER => "ERR_INVALID_PARAMETER",
        _ => "UNKNOWN",
    }
}

/// Read a little-endian u32 — the wire is LE on all three targets, and the encoders below pin
/// the offsets host-side so a typo cannot hide.
pub fn le32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn put_le32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_le64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Zero the request span and stamp the command type — every encoder starts here, so no stale
/// byte of a previous command can survive into the next one's descriptor.
fn encode_req(buf: &mut [u8], cmd: u32, total: usize) {
    for b in buf[..total].iter_mut() {
        *b = 0;
    }
    put_le32(buf, 0, cmd);
}

fn encode_rect(buf: &mut [u8], at: usize, r: Rect) {
    put_le32(buf, at, r.x);
    put_le32(buf, at + 4, r.y);
    put_le32(buf, at + 8, r.width);
    put_le32(buf, at + 12, r.height);
}

/// RESOURCE_CREATE_2D: hdr + rid@24 + format@28 + width@32 + height@36.
pub const CREATE_2D_LEN: usize = CTRL_HDR_LEN + 16;
pub fn encode_create_2d(buf: &mut [u8], rid: u32, format: u32, width: u32, height: u32) -> usize {
    encode_req(buf, CMD_RESOURCE_CREATE_2D, CREATE_2D_LEN);
    put_le32(buf, 24, rid);
    put_le32(buf, 28, format);
    put_le32(buf, 32, width);
    put_le32(buf, 36, height);
    CREATE_2D_LEN
}

/// RESOURCE_UNREF: hdr + rid@24 + padding.
pub const UNREF_LEN: usize = CTRL_HDR_LEN + 8;
pub fn encode_unref(buf: &mut [u8], rid: u32) -> usize {
    encode_req(buf, CMD_RESOURCE_UNREF, UNREF_LEN);
    put_le32(buf, 24, rid);
    UNREF_LEN
}

/// SET_SCANOUT: hdr + rect@24 + scanout_id@40 + rid@44.
pub const SET_SCANOUT_LEN: usize = CTRL_HDR_LEN + 24;
pub fn encode_set_scanout(buf: &mut [u8], scanout_id: u32, rid: u32, r: Rect) -> usize {
    encode_req(buf, CMD_SET_SCANOUT, SET_SCANOUT_LEN);
    encode_rect(buf, 24, r);
    put_le32(buf, 40, scanout_id);
    put_le32(buf, 44, rid);
    SET_SCANOUT_LEN
}

/// RESOURCE_FLUSH: hdr + rect@24 + rid@40 + padding.
pub const FLUSH_LEN: usize = CTRL_HDR_LEN + 24;
pub fn encode_flush(buf: &mut [u8], rid: u32, r: Rect) -> usize {
    encode_req(buf, CMD_RESOURCE_FLUSH, FLUSH_LEN);
    encode_rect(buf, 24, r);
    put_le32(buf, 40, rid);
    FLUSH_LEN
}

/// TRANSFER_TO_HOST_2D: hdr + rect@24 + offset(u64)@40 + rid@48 + padding.
pub const TRANSFER_2D_LEN: usize = CTRL_HDR_LEN + 32;
pub fn encode_transfer_to_host_2d(buf: &mut [u8], rid: u32, r: Rect, offset: u64) -> usize {
    encode_req(buf, CMD_TRANSFER_TO_HOST_2D, TRANSFER_2D_LEN);
    encode_rect(buf, 24, r);
    put_le64(buf, 40, offset);
    put_le32(buf, 48, rid);
    TRANSFER_2D_LEN
}

/// RESOURCE_ATTACH_BACKING: hdr + rid@24 + nr_entries@28, then 16-byte mem entries (addr u64,
/// length u32, padding). `None` when the entries would not fit the bound — a caller that ignored
/// that would truncate a descriptor mid-entry.
pub fn encode_attach_backing(buf: &mut [u8], rid: u32, entries: &[(u64, u32)]) -> Option<usize> {
    if entries.is_empty() || entries.len() > MAX_BACKING_ENTRIES {
        return None;
    }
    let total = CTRL_HDR_LEN + 8 + entries.len() * 16;
    encode_req(buf, CMD_RESOURCE_ATTACH_BACKING, total);
    put_le32(buf, 24, rid);
    put_le32(buf, 28, entries.len() as u32);
    for (i, (addr, len)) in entries.iter().enumerate() {
        let base = CTRL_HDR_LEN + 8 + i * 16;
        put_le64(buf, base, *addr);
        put_le32(buf, base + 8, *len);
    }
    Some(total)
}

/// RESOURCE_DETACH_BACKING: hdr + rid@24 + padding.
pub const DETACH_BACKING_LEN: usize = CTRL_HDR_LEN + 8;
pub fn encode_detach_backing(buf: &mut [u8], rid: u32) -> usize {
    encode_req(buf, CMD_RESOURCE_DETACH_BACKING, DETACH_BACKING_LEN);
    put_le32(buf, 24, rid);
    DETACH_BACKING_LEN
}

/// Parse a GET_DISPLAY_INFO response into the scanout table. Fail-closed: a malformed enabled
/// flag or an insane ENABLED rect makes the whole answer unusable rather than letting one bad
/// field through. Answers shorter than the full 408-byte table parse the entries present (a
/// device may write less; it may not write nonsense).
pub fn parse_display_info(resp: &[u8]) -> Option<Vec<Scanout>> {
    if resp.len() < CTRL_HDR_LEN + DISPLAY_ONE_LEN {
        return None;
    }
    let mut out = Vec::new();
    for i in 0..MAX_SCANOUTS {
        let base = CTRL_HDR_LEN + i * DISPLAY_ONE_LEN;
        if base + DISPLAY_ONE_LEN > resp.len() {
            break;
        }
        let r = Rect {
            x: le32(resp, base),
            y: le32(resp, base + 4),
            width: le32(resp, base + 8),
            height: le32(resp, base + 12),
        };
        let enabled_raw = le32(resp, base + 16);
        if enabled_raw > 1 {
            return None;
        }
        let enabled = enabled_raw == 1;
        if enabled && !r.is_sane() {
            return None;
        }
        out.push(Scanout { rect: r, enabled });
    }
    Some(out)
}
/// One entry of the driver's local resource table — the reality every rect argument is validated
/// against BEFORE the device hears about it.
struct Resource {
    id: u32,
    width: u32,
    height: u32,
    attached: bool,
    /// One DMA-registry handle per backing page, held so DETACH can REVOKE. A registration that
    /// outlives its buffer is a gate vouching for memory that may already be reused (REQ-GFX-002).
    backing: alloc::vec::Vec<crate::dma::Handle>,
}

/// A live virtio-gpu device: one control queue, two DMA-gated buffers, and the local table of
/// resources THIS driver created.
pub struct VirtioGpu<H: VirtioHal, T: Transport> {
    transport: T,
    ctrl: Virtqueue,
    cmd_buf: usize,
    resp_buf: usize,
    resources: Vec<Resource>,
    num_scanouts: u32,
    version1: bool,
    status_at_ok: u32,
    /// Commands actually published to the device. The counter the suite compares across refusal
    /// batteries: silence is MEASURED, not assumed.
    cmds_sent: Cell<u64>,
    local_refusals: Cell<u64>,
    _hal: PhantomData<H>,
}

/// The resource the suite drives through its whole lifecycle.
const RESOURCE_UNDER_TEST: u32 = 7;
/// An id the suite asks the device about WITHOUT ever creating — the error-grammar probe.
const NEVER_CREATED_RID: u32 = 0xDEAD;

impl<H: VirtioHal, T: Transport> VirtioGpu<H, T> {
    /// Bring the device up: negotiate VERSION_1 and NOTHING else, read the config-space scanout
    /// count, set up the control queue and its two buffers, THEN DRIVER_OK. The ordering is the
    /// VIRTIO 1.1 §3.1.1 dance the other drivers already perform, and the buffers are registered
    /// with the queue's DMA gate before any descriptor can name them.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio GPU device; `H::alloc_frame` must return
    /// identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(mut transport: T) -> Result<Self, GpuError> {
        let (_version, device_id) = transport.identity();
        if device_id != VIRTIO_ID_GPU {
            return Err(GpuError::Unsupported("not a virtio GPU device"));
        }

        transport.set_status(0);
        let mut status = S_ACKNOWLEDGE;
        transport.set_status(status);
        status |= S_DRIVER;
        transport.set_status(status);

        let hi = transport.device_features(1);
        if hi & (1 << F_VERSION_1_BIT) == 0 {
            return Err(GpuError::Unsupported(
                "device does not offer VIRTIO_F_VERSION_1",
            ));
        }
        // Accept ONLY VERSION_1 - plus the platform-IOMMU feature whenever the device offers it:
        // behind the VT-d identity domain descriptor addresses are unchanged, and a device that
        // REQUIRES the feature clears FEATURES_OK otherwise. VIRGL, EDID and blob resources stay
        // declined; they are behaviors this driver has no proofs for.
        let iommu_platform = hi & (1 << F_IOMMU_PLATFORM_BIT) != 0;
        transport.set_driver_features(0, 0);
        transport.set_driver_features(
            1,
            1 << F_VERSION_1_BIT
                | if iommu_platform {
                    1 << F_IOMMU_PLATFORM_BIT
                } else {
                    0
                },
        );
        status |= S_FEATURES_OK;
        transport.set_status(status);
        if transport.status() & S_FEATURES_OK == 0 {
            transport.set_status(status | S_FAILED);
            return Err(GpuError::Unsupported(
                "device rejected the negotiated features",
            ));
        }

        // Config space (`struct virtio_gpu_config`): num_scanouts lives at offset 8. Read after
        // feature negotiation, when the config layout is settled.
        let num_scanouts = (transport.config_u64(8) & 0xFFFF_FFFF) as u32;
        if num_scanouts == 0 || num_scanouts as usize > MAX_SCANOUTS {
            transport.set_status(status | S_FAILED);
            return Err(GpuError::Unsupported(
                "device reports an impossible scanout count",
            ));
        }

        let mut ctrl =
            Virtqueue::new::<H, T>(&mut transport, CTRL_QUEUE).map_err(GpuError::Queue)?;
        if ctrl.len() < 8 {
            return Err(GpuError::Queue("control queue too short for this driver"));
        }
        let cmd_buf = H::alloc_frame().ok_or(GpuError::Queue("no frame for the command buffer"))?;
        let resp_buf =
            H::alloc_frame().ok_or(GpuError::Queue("no frame for the response buffer"))?;
        ctrl.register_buffer(cmd_buf, PAGE, "virtio-gpu.cmd")
            .map_err(|_| GpuError::Queue("the command buffer was refused as a DMA region"))?;
        ctrl.register_buffer(resp_buf, PAGE, "virtio-gpu.resp")
            .map_err(|_| GpuError::Queue("the response buffer was refused as a DMA region"))?;

        status |= S_DRIVER_OK;
        transport.set_status(status);

        Ok(VirtioGpu {
            transport,
            ctrl,
            cmd_buf,
            resp_buf,
            resources: Vec::new(),
            num_scanouts,
            version1: true,
            status_at_ok: status,
            cmds_sent: Cell::new(0),
            local_refusals: Cell::new(0),
            _hal: PhantomData,
        })
    }

    /// Scanouts the device's config space reports.
    pub fn num_scanouts(&self) -> u32 {
        self.num_scanouts
    }

    /// Control queue length negotiated at init.
    pub fn queue_len(&self) -> u16 {
        self.ctrl.len()
    }

    /// VERSION_1 was offered AND accepted — recorded at init, checked by the suite.
    pub fn version1_negotiated(&self) -> bool {
        self.version1
    }

    /// DRIVER_OK was reached — recorded at init, checked by the suite.
    pub fn reached_driver_ok(&self) -> bool {
        self.status_at_ok & S_DRIVER_OK != 0
    }

    /// Commands actually published to the device.
    pub fn cmds_sent(&self) -> u64 {
        self.cmds_sent.get()
    }

    /// Local refusals counted — each was, by construction, a no-op.
    pub fn local_refusals(&self) -> u64 {
        self.local_refusals.get()
    }

    /// Extents of a live resource, for callers that validate against reality.
    pub fn resource_extents(&self, rid: u32) -> Option<(u32, u32)> {
        self.resources
            .iter()
            .find(|r| r.id == rid)
            .map(|r| (r.width, r.height))
    }

    fn slot_mut(&mut self, rid: u32) -> Option<&mut Resource> {
        self.resources.iter_mut().find(|r| r.id == rid)
    }

    /// Would an address this driver never registered be refused as a chain address? The suite
    /// asks, so the gate is proved to DENY BY DEFAULT rather than merely exist (REQ-DRV-006).
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        self.ctrl.would_refuse(0x7fff_0000_0000, 64)
            && self
                .ctrl
                .would_refuse(self.resp_buf as u64, (PAGE * 2) as u32)
    }

    /// DMA regions the control queue has registered (ring + command + response buffers).
    pub fn dma_regions(&self) -> usize {
        self.ctrl.dma_regions()
    }

    /// LIVE DMA regions right now — the number detach's revocation must return to 3.
    pub fn live_dma_regions(&self) -> usize {
        self.ctrl.live_regions()
    }

    fn refused(&self, why: &'static str) -> GpuError {
        self.local_refusals.set(self.local_refusals.get() + 1);
        GpuError::Refused(why)
    }

    /// Publish one request/response CHAIN on the control queue, kick, and harvest the answer.
    /// Exactly one command is in flight; descriptor slots 0 and 1 belong to it exclusively.
    ///
    /// # Safety
    /// The device must be live; both buffers were registered at init.
    unsafe fn command(&mut self, req_len: usize) -> Result<(u32, usize), GpuError> {
        self.ctrl
            .add_chain::<H>(
                0,
                self.cmd_buf as u64,
                req_len as u32,
                1,
                self.resp_buf as u64,
                RESP_CAP as u32,
            )
            .map_err(|_| GpuError::Queue("a control buffer failed the DMA gate"))?;
        // Counted only once actually published: the silence-proof below compares this counter
        // across refusal batteries, and a refused chain never became a descriptor.
        self.cmds_sent.set(self.cmds_sent.get() + 1);
        self.ctrl.kick::<H, T>(&self.transport);
        match self.ctrl.poll_used_bounded::<H>(TIMEOUT_SPINS) {
            Some((_head, written)) => {
                H::barrier(); // the response bytes precede the used-ring advance just observed
                let n = core::cmp::min(written as usize, RESP_CAP);
                let resp = core::slice::from_raw_parts(self.resp_buf as *const u8, n);
                if resp.len() < 4 {
                    return Err(GpuError::Device(RESP_ERR_UNSPEC));
                }
                Ok((le32(resp, 0), resp.len()))
            }
            None => Err(GpuError::Timeout),
        }
    }

    /// Ask the device what it will display. Idempotent and read-only.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn get_display_info(&mut self) -> Result<Vec<Scanout>, GpuError> {
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        encode_req(buf, CMD_GET_DISPLAY_INFO, CTRL_HDR_LEN);
        let (ty, written) = self.command(CTRL_HDR_LEN)?;
        if ty != RESP_OK_DISPLAY_INFO {
            return Err(GpuError::Device(ty));
        }
        let resp = core::slice::from_raw_parts(self.resp_buf as *const u8, written);
        parse_display_info(resp).ok_or(GpuError::Device(RESP_ERR_UNSPEC))
    }

    /// Create a 2D BGRA resource of some geometry. Locally validated FIRST: zero extents,
    /// oversized extents/area, id 0, a duplicate id and a full table are refusals that never
    /// reach the device (the suite's traffic counter proves that).
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn create_resource_2d(
        &mut self,
        rid: u32,
        width: u32,
        height: u32,
    ) -> Result<(), GpuError> {
        if rid == 0 {
            return Err(self.refused("resource id 0 is reserved"));
        }
        if let Err(e) = validate_create(width, height) {
            let why = match e {
                GpuError::Refused(w) => w,
                _ => "invalid geometry",
            };
            return Err(self.refused(why));
        }
        if self.resources.len() >= MAX_RESOURCES {
            return Err(self.refused("the resource table is full"));
        }
        if self.resources.iter().any(|r| r.id == rid) {
            return Err(self.refused("a duplicate resource id"));
        }
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_create_2d(buf, rid, FORMAT_B8G8R8A8_UNORM, width, height);
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        self.resources.push(Resource {
            id: rid,
            width,
            height,
            attached: false,
            backing: alloc::vec::Vec::new(),
        });
        Ok(())
    }

    /// Destroy a resource. The DEVICE requires detach-before-unref; enforcing that order locally
    /// means a caller bug surfaces as a named refusal instead of a device-side surprise.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn unref_resource(&mut self, rid: u32) -> Result<(), GpuError> {
        let attached = match self.slot_mut(rid) {
            None => return Err(self.refused("unref of an unknown resource")),
            Some(r) => r.attached,
        };
        if attached {
            return Err(self.refused("detach the backing before unref"));
        }
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_unref(buf, rid);
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        self.resources.retain(|r| r.id != rid);
        Ok(())
    }

    /// Give a resource its backing store: one mem entry per frame. Every frame is registered with
    /// the control queue's DMA gate BEFORE the descriptor naming it can exist (REQ-DRV-006) — an
    /// unregistered page is refused by name, never silently published.
    ///
    /// # Safety
    /// The device must be live; each frame must be identity-mapped memory this caller owns for as
    /// long as the backing is attached.
    pub unsafe fn attach_backing(&mut self, rid: u32, frames: &[usize]) -> Result<(), GpuError> {
        let attached = match self.slot_mut(rid) {
            None => return Err(self.refused("attach to an unknown resource")),
            Some(r) => r.attached,
        };
        if attached {
            return Err(self.refused("backing already attached"));
        }
        if frames.is_empty() || frames.len() > MAX_BACKING_ENTRIES {
            return Err(self.refused("a backing entry count outside the bound"));
        }
        // Register every frame and KEEP ITS HANDLE: detach must be able to revoke. A failure
        // mid-battery revokes what this call already claimed, so a refused attach leaves nothing
        // registered — the same no-partial-state rule the chain publisher follows.
        let mut handles: Vec<crate::dma::Handle> = Vec::with_capacity(frames.len());
        for f in frames {
            match self.ctrl.register_buffer_h(*f, PAGE, "virtio-gpu.backing") {
                Ok(h) => handles.push(h),
                Err(_) => {
                    for h in handles.drain(..) {
                        self.ctrl.revoke_buffer(h);
                    }
                    return Err(self.refused("a backing frame was refused as a DMA region"));
                }
            }
        }
        let entries: Vec<(u64, u32)> = frames.iter().map(|f| (*f as u64, PAGE as u32)).collect();
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = match encode_attach_backing(buf, rid, &entries) {
            Some(l) => l,
            None => {
                for h in handles.drain(..) {
                    self.ctrl.revoke_buffer(h);
                }
                return Err(self.refused("backing entries do not fit the command buffer"));
            }
        };
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            for h in handles.drain(..) {
                self.ctrl.revoke_buffer(h);
            }
            return Err(GpuError::Device(ty));
        }
        if let Some(r) = self.slot_mut(rid) {
            r.attached = true;
            r.backing = handles;
        }
        Ok(())
    }

    /// Take a resource's backing away, and REVOKE every page's DMA registration while doing it
    /// (REQ-GFX-002): after detach returns, the gate no longer vouches for frames the driver is
    /// about to release — a device pointed at them again is refused at the descriptor, not
    /// discovered in someone else's buffer.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn detach_backing(&mut self, rid: u32) -> Result<(), GpuError> {
        let attached = match self.slot_mut(rid) {
            None => return Err(self.refused("detach from an unknown resource")),
            Some(r) => r.attached,
        };
        if !attached {
            return Err(self.refused("no backing to detach"));
        }
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_detach_backing(buf, rid);
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        let handles = match self.slot_mut(rid) {
            None => return Err(self.refused("detach lost the resource mid-call")),
            Some(r) => {
                r.attached = false;
                core::mem::take(&mut r.backing)
            }
        };
        for h in handles {
            self.ctrl.revoke_buffer(h);
        }
        Ok(())
    }

    /// Bind a live resource to a scanout, at its FULL extent — the standard scanout shape, and
    /// the rect later flushes are checked against.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn set_scanout(&mut self, scanout_id: u32, rid: u32) -> Result<(), GpuError> {
        if scanout_id >= self.num_scanouts {
            return Err(self.refused("no such scanout"));
        }
        let geom = match self.resource_extents(rid) {
            None => return Err(self.refused("scanout of an unknown resource")),
            Some(g) => g,
        };
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_set_scanout(buf, scanout_id, rid, Rect::covering(geom.0, geom.1));
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        Ok(())
    }

    /// Copy a rect of a resource's backing into the host surface. The rect must sit inside the
    /// resource, and the byte range its origin implies must fit the backing — both checked here,
    /// before any descriptor exists.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn transfer_to_host_2d(&mut self, rid: u32, rect: Rect) -> Result<(), GpuError> {
        if !rect.is_sane() {
            return Err(self.refused("an insane transfer rect"));
        }
        let geom = match self.resource_extents(rid) {
            None => return Err(self.refused("transfer of an unknown resource")),
            Some(g) => g,
        };
        if !Rect::covering(geom.0, geom.1).contains(rect) {
            return Err(self.refused("a transfer rect outside the resource"));
        }
        // The offset a transfer implies: origin row + column, in bytes — computed in 64 bits,
        // where it cannot wrap.
        let stride = geom.0 as u64 * 4;
        let off = rect.y as u64 * stride + rect.x as u64 * 4;
        let needed = rect.width as u64 * rect.height as u64 * 4;
        if off + needed > geom.0 as u64 * geom.1 as u64 * 4 {
            return Err(self.refused("a transfer past the backing store"));
        }
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_transfer_to_host_2d(buf, rid, rect, off);
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        Ok(())
    }

    /// Hand a rect of a resource to the display: the pixels become visible on whatever the host
    /// shows (or nowhere, under `-display none` — the protocol path is real either way).
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn resource_flush(&mut self, rid: u32, rect: Rect) -> Result<(), GpuError> {
        if !rect.is_sane() {
            return Err(self.refused("an insane flush rect"));
        }
        let geom = match self.resource_extents(rid) {
            None => return Err(self.refused("flush of an unknown resource")),
            Some(g) => g,
        };
        if !Rect::covering(geom.0, geom.1).contains(rect) {
            return Err(self.refused("a flush rect outside the resource"));
        }
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_flush(buf, rid, rect);
        let (ty, _) = self.command(len)?;
        if ty != RESP_OK_NODATA {
            return Err(GpuError::Device(ty));
        }
        Ok(())
    }

    /// Send RESOURCE_FLUSH for a resource id this driver NEVER created, and hand back the
    /// device's own response code. SUITE-ONLY: the local table exists to refuse exactly such a
    /// command, so reaching the device at all means deliberately bypassing it — justified because
    /// the device's ERROR grammar is the only proof it PARSES our commands rather than echoing
    /// them, and the probe names nothing the driver owns. The DMA gate still applies; nothing
    /// else about the path differs from [`VirtioGpu::resource_flush`].
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn probe_device_error(&mut self, never_created_rid: u32) -> Result<u32, GpuError> {
        let buf = core::slice::from_raw_parts_mut(self.cmd_buf as *mut u8, PAGE);
        let len = encode_flush(buf, never_created_rid, Rect::covering(1, 1));
        let (ty, _) = self.command(len)?;
        Ok(ty)
    }
}
/// The graphics invariant suite (REQ-GFX-001), reported through a caller-supplied logger like
/// every other suite. Returns the number of invariants proved, or `(index, name)` of the first
/// failure. The count is what the VM gates grep for ("ALL 13 ..."), so it is pinned by the host
/// tests in `tests/virtiogpu.rs` only as a NUMBER — the proofs themselves run against the real
/// device, in the VM.
pub fn gpu_suite<H: VirtioHal, T: Transport, F: FnMut(usize, bool, &str)>(
    dev: &mut VirtioGpu<H, T>,
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

    // 1 — the init facts, recorded at bring-up: DRIVER_OK, VERSION_1, a usable control queue and
    //     a scanout count that fits the protocol's table.
    check!(
        "gpu: init reached DRIVER_OK with VIRTIO_F_VERSION_1 and a live control queue",
        dev.reached_driver_ok()
            && dev.version1_negotiated()
            && dev.queue_len() >= 8
            && dev.num_scanouts() >= 1
            && dev.num_scanouts() as usize <= MAX_SCANOUTS
    );

    // 2 — the DMA gate denies by default on the control queue, whose ring and two buffers ARE
    //     registered: an address far from everything, and an overrun of the response buffer.
    check!("gpu: the control queue DMA gate denies unregistered addresses (ring and buffers registered)",
        dev.dma_gate_refuses_unregistered() && dev.dma_regions() == 3
    );

    // 3 — GET_DISPLAY_INFO round trip: the device WROTE structured geometry into driver memory,
    //     at least one scanout is enabled, and every enabled rect is sane.
    // SAFETY: the device is live and owned here.
    let scans = unsafe { dev.get_display_info() };
    let scans_ok = match &scans {
        Ok(v) => {
            let enabled = v.iter().filter(|s| s.enabled).count();
            enabled >= 1
                && enabled <= dev.num_scanouts() as usize
                && v.iter().all(|s| !s.enabled || s.rect.is_sane())
        }
        Err(_) => false,
    };
    check!(
        "gpu: GET_DISPLAY_INFO is answered with a structurally valid, sane scanout table",
        scans_ok
    );

    // 4 — the geometry is PINNED, not merely sane: the qualified QEMU reports its head as
    //     1280x800, enabled (MEASURED — the first boot of this suite answered exactly that; do not
    //     "fix" this number from spec knowledge). A silent change here (different -device flags,
    //     a different emulator or version) is a different machine than the one these gates qualify.
    let pinned = match &scans {
        Ok(v) => matches!(
            v.first(),
            Some(s) if s.enabled && s.rect == Rect { x: 0, y: 0, width: 1280, height: 800 }
        ),
        Err(_) => false,
    };
    check!(
        "gpu: scanout 0 reports the machine display geometry (1280x800, enabled)",
        pinned
    );

    // 5 — invalid requests are refused BY NAME before any device traffic. The counter comparison
    //     is the proof of silence: not one descriptor was published across the whole battery.
    let before = dev.cmds_sent();
    // SAFETY: the device is live; the battery asserts every op lands in the LOCAL refusal path,
    // which is exactly what makes it legal to run them all.
    let battery = unsafe {
        [
            dev.create_resource_2d(0, 256, 64),
            dev.create_resource_2d(RESOURCE_UNDER_TEST, 0, 64),
            dev.create_resource_2d(RESOURCE_UNDER_TEST, 256, 0),
            dev.create_resource_2d(RESOURCE_UNDER_TEST, MAX_RESOURCE_EXTENT_PX + 1, 1),
            dev.create_resource_2d(
                RESOURCE_UNDER_TEST,
                MAX_RESOURCE_EXTENT_PX,
                MAX_RESOURCE_EXTENT_PX,
            ),
            dev.unref_resource(RESOURCE_UNDER_TEST),
            // SAFETY: every op below reaches at most the local refusal path — the battery asserts
            // exactly that, and the counter proves none became device traffic.
            dev.attach_backing(RESOURCE_UNDER_TEST, &[]),
            dev.set_scanout(dev.num_scanouts(), RESOURCE_UNDER_TEST),
            dev.set_scanout(0, RESOURCE_UNDER_TEST),
            dev.transfer_to_host_2d(RESOURCE_UNDER_TEST, Rect::covering(4, 4)),
            dev.resource_flush(RESOURCE_UNDER_TEST, Rect::covering(4, 4)),
            dev.detach_backing(RESOURCE_UNDER_TEST),
        ]
    };
    let all_refused = battery
        .iter()
        .all(|r| matches!(r, Err(GpuError::Refused(_))));
    check!("gpu: twelve invalid requests are refused by NAME before any device traffic (silence measured)",
        all_refused && dev.cmds_sent() == before && dev.local_refusals() == battery.len() as u64
    );

    // 6 — CREATE_2D accepted; the table records the extents every later rect validates against.
    // SAFETY: the device is live.
    let created = unsafe { dev.create_resource_2d(RESOURCE_UNDER_TEST, 256, 64) };
    check!(
        "gpu: RESOURCE_CREATE_2D is accepted and its extents recorded",
        created.is_ok() && dev.resource_extents(RESOURCE_UNDER_TEST) == Some((256, 64))
    );

    // 7 — NOW the containment rule has something to bite on: rects outside the live resource are
    //     refused by name, still with zero device traffic.
    let before = dev.cmds_sent();
    // SAFETY: as in invariant 5 — these must all be LOCAL refusals.
    let battery2 = unsafe {
        [
            dev.transfer_to_host_2d(
                RESOURCE_UNDER_TEST,
                Rect {
                    x: 200,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            ),
            dev.transfer_to_host_2d(
                RESOURCE_UNDER_TEST,
                Rect {
                    x: 0,
                    y: 60,
                    width: 256,
                    height: 8,
                },
            ),
            dev.resource_flush(RESOURCE_UNDER_TEST, Rect::covering(257, 1)),
            dev.transfer_to_host_2d(RESOURCE_UNDER_TEST, Rect::covering(0, 4)),
        ]
    };
    let contained_refused = battery2
        .iter()
        .all(|r| matches!(r, Err(GpuError::Refused(_))));
    check!(
        "gpu: rects outside a live resource are refused by name, still with zero device traffic",
        contained_refused && dev.cmds_sent() == before
    );

    // 8 — ATTACH_BACKING of sixteen DMA-gated pages is accepted (a 64 KiB backing, one entry per
    //     frame, exactly the multi-entry shape the protocol defines).
    // SAFETY: alloc_frame hands out identity-mapped frames this kernel owns exclusively.
    // Sixteen pages — this invariant proves MULTI-entry attach works at all; the big scatter-
    // gather backing is the console suite's business (150 pages).
    const ATTACH_PAGES: usize = 16;
    let frames: Vec<usize> = (0..ATTACH_PAGES).filter_map(|_| H::alloc_frame()).collect();
    let attached = if frames.len() == ATTACH_PAGES {
        unsafe { dev.attach_backing(RESOURCE_UNDER_TEST, &frames) }.is_ok()
    } else {
        false
    };
    check!(
        "gpu: RESOURCE_ATTACH_BACKING of sixteen DMA-gated pages is accepted",
        attached
    );

    // 9 — SET_SCANOUT binds the resource to scanout 0.
    // SAFETY: the device is live.
    let bound = unsafe { dev.set_scanout(0, RESOURCE_UNDER_TEST) };
    check!(
        "gpu: SET_SCANOUT binds the resource to scanout 0",
        bound.is_ok()
    );

    // 10 — TRANSFER_TO_HOST_2D of the full extent is accepted.
    // SAFETY: the device is live.
    let moved = unsafe { dev.transfer_to_host_2d(RESOURCE_UNDER_TEST, Rect::covering(256, 64)) };
    check!(
        "gpu: TRANSFER_TO_HOST_2D of the full resource is accepted",
        moved.is_ok()
    );

    // 11 — RESOURCE_FLUSH of the full extent is accepted: the pixels were handed to the host.
    // SAFETY: the device is live.
    let flushed = unsafe { dev.resource_flush(RESOURCE_UNDER_TEST, Rect::covering(256, 64)) };
    check!(
        "gpu: RESOURCE_FLUSH of the full resource is accepted",
        flushed.is_ok()
    );

    // 12 — the device PARSES commands: a flush for an id it NEVER knew draws its own error
    //      grammar, not an echo (an echo would answer OK).
    // SAFETY: the device is live.
    let err_never = unsafe { dev.probe_device_error(NEVER_CREATED_RID) };
    // The probe hands back the device's own response CODE (Ok on any completed round trip); the
    // ERROR lives in that code, not in our Result.
    check!("gpu: a flush for a never-created resource is answered INVALID_RESOURCE_ID by the DEVICE itself",
        err_never == Ok(RESP_ERR_INVALID_RESOURCE_ID)
    );

    // 13 — the lifecycle ENDS: detach + unref are accepted, the table forgets the resource, and
    //      the SAME probe that would have drawn OK a moment ago now draws
    //      INVALID_RESOURCE_ID — gone at the device, by the device's own word.
    // SAFETY: the device is live.
    let detached = unsafe { dev.detach_backing(RESOURCE_UNDER_TEST) }.is_ok();
    let unreffed = unsafe { dev.unref_resource(RESOURCE_UNDER_TEST) }.is_ok();
    let forgotten = dev.resource_extents(RESOURCE_UNDER_TEST).is_none();
    let really_gone =
        unsafe { dev.probe_device_error(RESOURCE_UNDER_TEST) } == Ok(RESP_ERR_INVALID_RESOURCE_ID);
    check!("gpu: DETACH_BACKING plus UNREF end the lifecycle — the device itself confirms the resource is gone",
        detached && unreffed && forgotten && really_gone
    );

    Ok(n)
}

/// The framebuffer-console suite (REQ-GFX-002): render text into a REAL scatter-gather backing
/// store, hand the whole frame to the display device, and prove DETACH revokes every page. Runs
/// on the same device AFTER [`gpu_suite`], whose resource is destroyed and whose registry is back
/// to ring + two buffers. Six invariants; marker `ALL 6 FRAMEBUFFER-CONSOLE INVARIANTS HOLD`.
pub fn console_suite<H: VirtioHal, T: Transport, F: FnMut(usize, bool, &str)>(
    dev: &mut VirtioGpu<H, T>,
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

    const CONSOLE_RID: u32 = 9;

    // 1 — the console surface exists as a real resource, extents recorded.
    // SAFETY: live device.
    let created =
        unsafe { dev.create_resource_2d(CONSOLE_RID, CONSOLE_FB_WIDTH, CONSOLE_FB_HEIGHT) };
    check!(
        "fbconsole: the 640x240 console resource is created and recorded",
        created.is_ok()
            && dev.resource_extents(CONSOLE_RID) == Some((CONSOLE_FB_WIDTH, CONSOLE_FB_HEIGHT))
    );

    // 2 — CONSOLE_FB_PAGES single-frame entries attach in ONE command; every page registers with
    //     a held handle. The registry grows by exactly the backing — measured, not assumed.
    // SAFETY: alloc_frame hands out identity-mapped frames this kernel owns exclusively.
    let mut pages: Vec<usize> = Vec::with_capacity(CONSOLE_FB_PAGES);
    for _ in 0..CONSOLE_FB_PAGES {
        match H::alloc_frame() {
            Some(f) => pages.push(f),
            None => break,
        }
    }
    let attached = if pages.len() == CONSOLE_FB_PAGES {
        unsafe { dev.attach_backing(CONSOLE_RID, &pages) }.is_ok()
    } else {
        false
    };
    check!(
        "fbconsole: 150 backing pages attach in ONE command and all register",
        attached && dev.live_dma_regions() == 3 + CONSOLE_FB_PAGES
    );

    // 3 — RENDER, then read our own memory back: the ink-pixel count of cell (0,0) must equal
    //     the FONT table's own popcount for 'A' (doubled per drawn row). The renderer is proved
    //     against the very table it renders from, over hardware-owned backing frames.
    let rendered = if let (Ok(mut surf), Ok(mut con)) = (
        crate::fbcon::Surface::new(&pages, CONSOLE_FB_WIDTH, CONSOLE_FB_HEIGHT),
        crate::fbcon::TextConsole::new(CONSOLE_FB_WIDTH, CONSOLE_FB_HEIGHT),
    ) {
        con.clear(&mut surf);
        let printed = con.print(&mut surf, b"Aletheia OS");
        let want: u32 = crate::font8x8::FONT8X8[65]
            .iter()
            .map(|r| r.count_ones())
            .sum::<u32>()
            * 2;
        let mut got = 0u32;
        for y in 0..crate::fbcon::CELL_H {
            for x in 0..crate::fbcon::CELL_W {
                if surf.get(x, y) == Ok(true) {
                    got += 1;
                }
            }
        }
        let bg_dark = surf.get(CONSOLE_FB_WIDTH - 1, CONSOLE_FB_HEIGHT - 1) == Ok(false);
        printed.is_ok() && want > 0 && got == want && bg_dark
    } else {
        false
    };
    check!(
        "fbconsole: the rendered text is IN OUR MEMORY — font-exact pixel counts at known cells",
        rendered
    );

    // 4 — the whole frame goes to the display: transfer AND flush of the full extent, accepted.
    // SAFETY: live device.
    let whole = Rect::covering(CONSOLE_FB_WIDTH, CONSOLE_FB_HEIGHT);
    let moved = unsafe { dev.transfer_to_host_2d(CONSOLE_RID, whole) };
    let flushed = unsafe { dev.resource_flush(CONSOLE_RID, whole) };
    check!(
        "fbconsole: TRANSFER plus FLUSH hand the whole rendered frame to the display",
        moved.is_ok() && flushed.is_ok()
    );

    // 5 — DETACH REVOKES: after detach returns, the registry holds exactly ring + cmd + resp. The
    //     limitation last wave named as follow-on is now closed by counter.
    // SAFETY: live device.
    let detached = unsafe { dev.detach_backing(CONSOLE_RID) };
    check!(
        "fbconsole: DETACH revokes every page's DMA registration (back to 3 live regions)",
        detached.is_ok() && dev.live_dma_regions() == 3
    );

    // 6 — unref; the DEVICE confirms the surface is gone.
    // SAFETY: live device.
    let unreffed = unsafe { dev.unref_resource(CONSOLE_RID) };
    let gone = unsafe { dev.probe_device_error(CONSOLE_RID) } == Ok(RESP_ERR_INVALID_RESOURCE_ID);
    check!(
        "fbconsole: UNREF destroys the surface — the device confirms INVALID_RESOURCE_ID",
        unreffed.is_ok() && gone
    );

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uapi_wire_constants_are_what_the_header_says() {
        assert_eq!(VIRTIO_ID_GPU, 16);
        assert_eq!(CTRL_HDR_LEN, 24);
        assert_eq!(MAX_SCANOUTS, 16);
        assert_eq!(DISPLAY_INFO_RESP_LEN, 408);
        assert_eq!(CMD_GET_DISPLAY_INFO, 0x0100);
        assert_eq!(CMD_RESOURCE_CREATE_2D, 0x0101);
        assert_eq!(CMD_RESOURCE_UNREF, 0x0102);
        assert_eq!(CMD_SET_SCANOUT, 0x0103);
        assert_eq!(CMD_RESOURCE_FLUSH, 0x0104);
        assert_eq!(CMD_TRANSFER_TO_HOST_2D, 0x0105);
        assert_eq!(CMD_RESOURCE_ATTACH_BACKING, 0x0106);
        assert_eq!(CMD_RESOURCE_DETACH_BACKING, 0x0107);
        assert_eq!(RESP_OK_NODATA, 0x1100);
        assert_eq!(RESP_OK_DISPLAY_INFO, 0x1101);
        assert_eq!(RESP_ERR_INVALID_RESOURCE_ID, 0x1203);
        assert_eq!(RESP_ERR_INVALID_PARAMETER, 0x1205);
        assert_eq!(FORMAT_B8G8R8A8_UNORM, 1);
    }

    #[test]
    fn rect_containment_survives_the_u32_edges() {
        let full = Rect::covering(256, 64);
        assert!(full.contains(Rect {
            x: 255,
            y: 63,
            width: 1,
            height: 1
        }));
        assert!(!full.contains(Rect {
            x: 255,
            y: 0,
            width: 2,
            height: 1
        }));
        assert!(!full.contains(Rect::covering(0, 1)));
        // The overflow case: u32 arithmetic would wrap x=MAX+2 into "inside".
        let huge = Rect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert!(huge.contains(Rect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX
        }));
        assert!(!huge.contains(Rect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: 0, // zero height is never contained
        }));
        assert!(!Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 2
        }
        .is_sane());
        assert!(Rect {
            x: 0,
            y: 0,
            width: MAX_RECT_EXTENT_PX,
            height: MAX_RECT_EXTENT_PX
        }
        .is_sane());
    }
}
