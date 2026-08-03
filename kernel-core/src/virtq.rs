//! A reusable split virtqueue, for devices that need MORE THAN ONE (REQ-NET-001, ADR-041).
//!
//! `virtioblk` drives one queue with a fixed layout inside a single frame, which is exactly right for a
//! block device: one request in flight, polled to completion. A **network** device cannot work that way.
//! It needs at least two queues (receive and transmit), and the receive queue must have buffers **posted
//! before the device runs** — a packet arrives whether or not the driver is ready, and a queue with no
//! buffer simply drops it.
//!
//! So the ring mechanics live here, parameterized by queue index and reusable by any device kind:
//!
//! * one frame per queue holds the descriptor table, the avail ring and the used ring at fixed offsets
//!   (VIRTIO 1.1 §2.6 allows split placement, which is what the three queue-address registers are for);
//! * [`Virtqueue::add`] publishes a single-descriptor buffer, [`Virtqueue::kick`] notifies the device,
//!   and [`Virtqueue::poll_used`] harvests completions — returning the descriptor slot so the caller can
//!   map it back to its own buffer.
//!
//! **`last_used` is the whole subtlety.** The used ring's index only ever increases (mod 2^16); the driver
//! must remember how far it has consumed, or it re-reads a completion it already handled — which on a
//! receive queue means processing one packet twice, and on a transmit queue means reusing a buffer that is
//! still in flight. It is tracked here rather than left to each caller to remember.
//!
//! Not claimed: no indirect descriptors, no event-index suppression (`used_event`/`avail_event` are left
//! zero and ignored), no chained multi-descriptor buffers (a caller needing a header plus a payload posts
//! them as one contiguous buffer), and completion is polled — there are no interrupts in this kernel yet.
use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};

use crate::dma::{DmaFault, DmaRegistry};
use crate::virtioblk::{Transport, VirtioHal};

/// Descriptor-table entry (VIRTIO 1.1 §2.6.5): 16 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// The buffer is device-WRITABLE — the device fills it (a receive buffer).
pub const DESC_F_WRITE: u16 = 2;

/// Byte offsets within one queue's frame: descriptor table, avail ring, used ring.
const OFF_DESC: usize = 0; // N * 16 B
const OFF_AVAIL: usize = 1024; // 6 + 2N B
const OFF_USED: usize = 2048; // 6 + 8N B

/// Descriptors per queue. 32 leaves room for a receive queue with many buffers posted at once while
/// keeping one frame per queue (32 × 16 = 512 B of descriptors; both rings sit well inside 4 KiB).
pub const QUEUE_LEN: u16 = 32;

/// One live split virtqueue.
pub struct Virtqueue {
    index: u16,
    desc: usize,
    avail: usize,
    used: usize,
    qsize: u16,
    /// How far this driver has consumed the used ring. See the module docs.
    last_used: Cell<u16>,
    /// What this queue may tell the device about (REQ-DRV-006, ADR-043). Owned per queue, so the check
    /// sits exactly where an address becomes a descriptor — the only place a wrong one could escape.
    dma: DmaRegistry,
}

impl Virtqueue {
    /// Set up queue `index`: allocate its frame, publish the three ring addresses, mark it ready.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio device that has this queue, and `H::alloc_frame` must
    /// return identity-mapped frames the caller owns exclusively.
    pub unsafe fn new<H: VirtioHal, T: Transport>(
        transport: &mut T,
        index: u16,
    ) -> Result<Self, &'static str> {
        transport.select_queue(index);
        transport.after_queue_select();
        let max = transport.queue_num_max();
        if max == 0 {
            return Err("virtqueue: the device does not have this queue");
        }
        let qsize = core::cmp::min(QUEUE_LEN as u32, max) as u16;

        let frame = H::alloc_frame().ok_or("virtqueue: frame allocator exhausted")?;
        let desc = frame + OFF_DESC;
        let avail = frame + OFF_AVAIL;
        let used = frame + OFF_USED;

        transport.set_queue_num(qsize as u32);
        transport.set_queue_addrs(desc as u64, avail as u64, used as u64);
        H::barrier();
        transport.queue_ready();

        let mut dma = DmaRegistry::new();
        // The ring frame itself is device-visible: the device reads the descriptor table and the avail
        // ring, and writes the used ring.
        dma.register(frame, crate::dma::PAGE, "virtq.ring")
            .map_err(|_| "virtqueue: the ring frame was refused as a DMA region")?;
        Ok(Virtqueue {
            index,
            desc,
            avail,
            used,
            qsize,
            last_used: Cell::new(0),
            dma,
        })
    }

    /// This queue's index.
    pub fn index(&self) -> u16 {
        self.index
    }
    /// Its negotiated length in descriptors.
    pub fn len(&self) -> u16 {
        self.qsize
    }
    /// A queue of length zero would accept no buffers at all.
    pub fn is_empty(&self) -> bool {
        self.qsize == 0
    }

    /// Publish one buffer as descriptor `slot`, and offer it to the device.
    ///
    /// `device_writable` marks a buffer the DEVICE fills (a receive buffer); a transmit buffer stays
    /// read-only, so a device that tried to write it would be violating the contract visibly rather than
    /// quietly corrupting driver memory.
    ///
    /// # Safety
    /// `slot` must be `< len()`, `addr` must be an identity-mapped physical address the caller owns for as
    /// long as the buffer is in flight, and `len` must not exceed that buffer.
    pub unsafe fn add<H: VirtioHal>(
        &self,
        slot: u16,
        addr: u64,
        len: u32,
        device_writable: bool,
    ) -> Result<(), DmaFault> {
        // THE GATE (REQ-DRV-006, ADR-043): a device is only ever told about memory a driver registered for
        // it. An address that was never registered — a miscalculation, a stale pointer, a corrupted field —
        // is refused HERE, before it becomes a descriptor the device will act on.
        if !self.dma.visible(addr as usize, len as usize) {
            return Err(DmaFault::Malformed);
        }
        let d = (self.desc + slot as usize * core::mem::size_of::<Desc>()) as *mut Desc;
        write_volatile(
            d,
            Desc {
                addr,
                len,
                flags: if device_writable { DESC_F_WRITE } else { 0 },
                next: 0,
            },
        );
        // avail ring: [flags u16][idx u16][ring u16 * qsize][used_event u16]
        let idx_ptr = (self.avail + 2) as *mut u16;
        let ring = (self.avail + 4) as *mut u16;
        let cur = read_volatile(idx_ptr);
        write_volatile(ring.add((cur % self.qsize) as usize), slot);
        H::barrier(); // the ring entry is visible before the index that publishes it
        write_volatile(idx_ptr, cur.wrapping_add(1));
        Ok(())
    }

    /// Register a buffer this queue will hand to the device. Must be called before [`Virtqueue::add`]
    /// names that address, which is what makes the gate above meaningful rather than decorative.
    pub fn register_buffer(
        &mut self,
        addr: usize,
        len: usize,
        owner: &'static str,
    ) -> Result<(), DmaFault> {
        self.dma.register(addr, len, owner).map(|_| ())
    }

    /// Would this address be refused as a descriptor right now? Used by the invariant suites to prove the
    /// gate denies by default rather than merely existing.
    pub fn would_refuse(&self, addr: u64, len: u32) -> bool {
        !self.dma.visible(addr as usize, len as usize)
    }

    /// DMA regions this queue has registered, and refusals it has counted.
    pub fn dma_regions(&self) -> usize {
        self.dma.live_regions()
    }

    /// Tell the device this queue has new buffers.
    ///
    /// # Safety
    /// The transport must be live; the caller must have published at least one buffer.
    pub unsafe fn kick<H: VirtioHal, T: Transport>(&self, transport: &T) {
        H::barrier(); // the published index is visible before the notify store
        transport.notify(self.index);
    }

    /// Harvest one completion: `(descriptor slot, bytes the device wrote)`, or `None` if nothing new.
    ///
    /// # Safety
    /// The queue must be live.
    pub unsafe fn poll_used<H: VirtioHal>(&self) -> Option<(u16, u32)> {
        // used ring: [flags u16][idx u16][{id u32, len u32} * qsize][avail_event u16]
        let idx_ptr = (self.used + 2) as *const u16;
        let cur = read_volatile(idx_ptr);
        let last = self.last_used.get();
        if cur == last {
            return None;
        }
        H::barrier(); // the index advance is observed before the element it publishes is read
        let elem = (self.used + 4 + (last % self.qsize) as usize * 8) as *const u32;
        let id = read_volatile(elem) as u16;
        let written = read_volatile(elem.add(1));
        self.last_used.set(last.wrapping_add(1));
        Some((id, written))
    }

    /// Poll for a completion up to `spins` times. Bounded, so a device that never completes returns
    /// `None` instead of hanging past the VM watchdog — the same doctrine as the block driver.
    ///
    /// # Safety
    /// The queue must be live.
    pub unsafe fn poll_used_bounded<H: VirtioHal>(&self, spins: u64) -> Option<(u16, u32)> {
        for _ in 0..spins {
            if let Some(done) = self.poll_used::<H>() {
                return Some(done);
            }
            core::hint::spin_loop();
        }
        None
    }
}
