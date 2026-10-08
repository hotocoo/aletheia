//! virtio-net + the smallest honest network stack (REQ-NET-001/002/003, ADR-041, ADR-060).
//!
//! Networking was the largest remaining "an OS does this" hole: architecture text and nothing else. This
//! module is the first real slice — a device that sends and receives Ethernet frames, and just enough
//! protocol above it to prove the path end to end **against something that answers back**.
//!
//! ## Why ARP and ICMP, and why that is a real proof
//!
//! A driver that only transmits proves nothing: a frame written into a ring nobody reads looks identical to
//! a frame that vanished. So the suite talks to QEMU's user-mode network gateway (`10.0.2.2`), which
//! answers both:
//!
//! * **ARP** — the driver broadcasts "who has 10.0.2.2?" and must receive a reply carrying that address
//!   and a MAC. Receiving requires the receive queue to have been posted BEFORE the request went out,
//!   which is a property a block device never has to satisfy.
//! * **ICMP echo** — then a real IPv4 packet with two correct checksums (the IP header's and the ICMP
//!   message's) must come back as an echo REPLY with the same identifier, sequence and payload. A wrong
//!   checksum is dropped by the peer in silence, so a reply arriving proves the packet was well *formed*,
//!   not merely well intentioned.
//!
//! ## Scope, stated
//!
//! The second slice (ADR-060) added what the first honestly listed as missing: an ARP CACHE
//! (`arpcache` — an answer you already asked for is remembered, bounded, LRU), UDP datagrams with a
//! pseudo-header checksum that refuses to verify under re-addressing (`udpv4`), and DHCP DISCOVER →
//! OFFER (`dhcp`), so the guest address is now CROSS-CHECKED against the network's own answer instead
//! of being a constant nobody questioned. Still absent, on purpose: TCP, routing, fragmentation, a
//! socket layer — every reply is still matched synchronously by the single waiter, frames that are not
//! the answer being waited for are **counted and dropped**, not queued. Completion is polled; there
//! are no interrupts in this kernel yet.
use crate::arpcache::ArpCache;
use crate::virtioblk::{Transport, VirtioHal};
use crate::virtq::Virtqueue;
use crate::{dhcp, udpv4};

/// virtio device id for a network card.
pub const VIRTIO_ID_NET: u32 = 1;

/// Queue indices on a device without multiqueue: 0 = receive, 1 = transmit.
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

/// Feature bits: MAC in device config, and VIRTIO_F_VERSION_1 (bit 32 ⇒ bit 0 of the high half).
const F_NET_MAC_BIT: u32 = 5;
const F_VERSION_1_BIT: u32 = 0;
/// VIRTIO_F_IOMMU_PLATFORM == bit 33, i.e. bit 1 of the high half. See virtioblk for the full
/// rationale; the same acceptance rule applies to every device this kernel drives.
const F_IOMMU_PLATFORM_BIT: u32 = 1;

/// Device status bits (VIRTIO 1.1 §3.1.1).
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;
const S_FAILED: u32 = 0x80;

/// `struct virtio_net_hdr_v1` — 12 bytes, always present on a modern device, and always zero here because
/// no offload or checksum feature is negotiated.
pub const NET_HDR_LEN: usize = 12;

/// Bytes per receive buffer: the virtio header plus a full Ethernet frame.
const RX_BUF_LEN: usize = NET_HDR_LEN + 1514;
/// Receive buffers posted before the device starts. More than one, because the gateway may answer while
/// the driver is still looking at the previous frame — a single buffer turns that into a dropped reply.
const RX_BUFFERS: u16 = 8;

/// Ethernet.
pub const ETH_HDR_LEN: usize = 14;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;
const BROADCAST: [u8; 6] = [0xFF; 6];

/// The addresses QEMU's user-mode network expects: the guest is `.15`, the gateway is `.2`.
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Where this machine lives on its network (ADR-234): QEMU's user-network defaults until the
/// network's DHCP server grants a lease, then what the lease says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addressing {
    pub ip: [u8; 4],
    pub gateway: [u8; 4],
    pub mask: [u8; 4],
    /// The name server the lease named, if any.
    pub dns: Option<[u8; 4]>,
    /// The lease's length in seconds; `None` before a lease (or when the server gave none).
    pub lease_secs: Option<u32>,
    /// Whether these came from a DHCP ACK rather than the defaults.
    pub leased: bool,
}

impl Addressing {
    pub const QEMU_USER: Addressing = Addressing {
        ip: GUEST_IP,
        gateway: GATEWAY_IP,
        mask: [255, 255, 255, 0],
        dns: None,
        lease_secs: None,
        leased: false,
    };

    /// The next hop for `dst`: itself on this subnet, the gateway otherwise.
    pub fn hop(&self, dst: [u8; 4]) -> [u8; 4] {
        let same = (0..4).all(|i| dst[i] & self.mask[i] == self.ip[i] & self.mask[i]);
        if same {
            dst
        } else {
            self.gateway
        }
    }
}

/// Why a network operation failed. Each is a refusal with a reason; none is a silent drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Not a modern virtio network device, or it rejected the negotiated features.
    Unsupported(&'static str),
    /// A queue could not be set up.
    Queue(&'static str),
    /// The frame to send exceeds a transmit buffer.
    TooLong,
    /// Nothing arrived within the bounded wait — a fact about this attempt, not a claim about the peer.
    Timeout,
}

/// A live virtio network device.
pub struct VirtioNet<H: VirtioHal, T: Transport> {
    transport: T,
    rx: Virtqueue,
    tx: Virtqueue,
    /// Identity-mapped receive buffers, one per posted descriptor slot.
    rx_bufs: [usize; RX_BUFFERS as usize],
    /// One transmit buffer: this driver sends synchronously, one frame at a time.
    tx_buf: usize,
    mac: [u8; 6],
    /// Answers to "who has this address?" already paid for on the wire (ADR-060). Bounded by
    /// construction; consulted BEFORE any broadcast, refreshed on every use, LRU when full.
    arp_cache: core::cell::RefCell<ArpCache>,
    /// Broadcast ARP requests actually SENT. The cache's whole point is observable: a repeated
    /// resolve must NOT raise this number, and the suite proves exactly that.
    arp_wire_requests: core::cell::Cell<u32>,
    /// Frames received that were not what the caller was waiting for. Counted, never silently ignored: a
    /// nonzero count beside a failing wait distinguishes "the peer said nothing" from "the driver threw
    /// the answer away".
    dropped: core::cell::Cell<u64>,
    /// Where this device's machine lives (ADR-234).
    addr: core::cell::Cell<Addressing>,
    _hal: core::marker::PhantomData<H>,
}

pub(crate) fn be16(b: &[u8], at: usize) -> u16 {
    ((b[at] as u16) << 8) | b[at + 1] as u16
}

pub(crate) fn put_be16(b: &mut [u8], at: usize, v: u16) {
    b[at] = (v >> 8) as u8;
    b[at + 1] = v as u8;
}

/// The internet checksum (RFC 1071). ONE implementation lives in `udpv4`; ICMP re-uses it through this
/// re-export so the two protocols cannot drift into two definitions of "well formed".
pub use crate::udpv4::checksum;

impl<H: VirtioHal, T: Transport> VirtioNet<H, T> {
    /// Bring the device up: negotiate, set up both queues, POST the receive buffers, then DRIVER_OK.
    ///
    /// The ordering is the point. Receive buffers must be published before the device is told it may run,
    /// or a frame arriving in the gap is dropped by the device for want of anywhere to put it.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio network device; `H::alloc_frame` must return
    /// identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(mut transport: T) -> Result<Self, NetError> {
        let (_version, device_id) = transport.identity();
        if device_id != VIRTIO_ID_NET {
            return Err(NetError::Unsupported("not a virtio network device"));
        }

        transport.set_status(0);
        let mut status = S_ACKNOWLEDGE;
        transport.set_status(status);
        status |= S_DRIVER;
        transport.set_status(status);

        let lo = transport.device_features(0);
        let hi = transport.device_features(1);
        if hi & (1 << F_VERSION_1_BIT) == 0 {
            return Err(NetError::Unsupported(
                "device does not offer VIRTIO_F_VERSION_1",
            ));
        }
        // Accept only MAC (so the address comes from the device rather than being invented) plus
        // VERSION_1. Every offload feature is declined, which is what keeps the header all-zero.
        let want_mac = lo & (1 << F_NET_MAC_BIT) != 0;
        // Acknowledge the platform-IOMMU feature whenever offered: behind the VT-d identity
        // domain descriptor addresses are unchanged, and a device that REQUIRES the feature
        // clears FEATURES_OK otherwise.
        let iommu_platform = hi & (1 << F_IOMMU_PLATFORM_BIT) != 0;
        transport.set_driver_features(0, if want_mac { 1 << F_NET_MAC_BIT } else { 0 });
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
            return Err(NetError::Unsupported(
                "device rejected the negotiated features",
            ));
        }

        // config space: mac[6] at offset 0.
        let cfg = transport.config_u64(0);
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = (cfg >> (8 * i)) as u8;
        }
        if !want_mac || mac == [0u8; 6] {
            return Err(NetError::Unsupported("device did not report a MAC address"));
        }

        let mut rx = Virtqueue::new::<H, T>(&mut transport, RX_QUEUE).map_err(NetError::Queue)?;
        let mut tx = Virtqueue::new::<H, T>(&mut transport, TX_QUEUE).map_err(NetError::Queue)?;
        if rx.len() < RX_BUFFERS || tx.is_empty() {
            return Err(NetError::Queue("queues are too short for this driver"));
        }

        // One frame per receive buffer (RX_BUF_LEN < 4 KiB), plus one for transmit.
        let mut rx_bufs = [0usize; RX_BUFFERS as usize];
        for slot in 0..RX_BUFFERS {
            let buf = H::alloc_frame().ok_or(NetError::Queue("no frame for a receive buffer"))?;
            rx_bufs[slot as usize] = buf;
            // Register BEFORE publishing: the gate in `add` refuses an unregistered address (ADR-043).
            rx.register_buffer(buf, crate::dma::PAGE, "virtio-net.rx")
                .map_err(|_| NetError::Queue("a receive buffer was refused as a DMA region"))?;
            rx.add::<H>(slot, buf as u64, RX_BUF_LEN as u32, true)
                .map_err(|_| NetError::Queue("a receive buffer failed the DMA gate"))?;
        }
        let tx_buf = H::alloc_frame().ok_or(NetError::Queue("no frame for the transmit buffer"))?;
        tx.register_buffer(tx_buf, crate::dma::PAGE, "virtio-net.tx")
            .map_err(|_| NetError::Queue("the transmit buffer was refused as a DMA region"))?;

        // Buffers are published; NOW the device may run.
        status |= S_DRIVER_OK;
        transport.set_status(status);
        rx.kick::<H, T>(&transport);

        Ok(VirtioNet {
            transport,
            rx,
            tx,
            rx_bufs,
            tx_buf,
            mac,
            arp_cache: core::cell::RefCell::new(ArpCache::new()),
            arp_wire_requests: core::cell::Cell::new(0),
            dropped: core::cell::Cell::new(0),
            addr: core::cell::Cell::new(Addressing::QEMU_USER),
            _hal: core::marker::PhantomData,
        })
    }

    /// The device's own MAC address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Where this machine lives: the defaults, or the DHCP lease once taken (ADR-234).
    pub fn addressing(&self) -> Addressing {
        self.addr.get()
    }

    /// This machine's IPv4 address on the device's network.
    pub fn ip(&self) -> [u8; 4] {
        self.addr.get().ip
    }

    /// The router off this network.
    pub fn gateway(&self) -> [u8; 4] {
        self.addr.get().gateway
    }

    /// Frames received that were not the answer being waited for.
    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }

    /// Broadcast ARP requests actually put on the wire since init. A cache hit is free by THIS
    /// number, not by a claim: the suite resolves twice and requires it to stay at 1.
    pub fn arp_wire_requests(&self) -> u32 {
        self.arp_wire_requests.get()
    }

    /// Would an address this driver never registered be refused as a descriptor? The suite asks this to
    /// prove the DMA gate denies by default (REQ-DRV-006, ADR-043) rather than merely existing.
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        // An address far from any registered buffer, and one that overruns a registered buffer.
        self.tx.would_refuse(0x7fff_0000_0000, 64)
            && self
                .rx
                .would_refuse(self.rx_bufs[0] as u64, (crate::dma::PAGE * 2) as u32)
    }

    /// DMA regions the two queues have registered (rings + buffers).
    pub fn dma_regions(&self) -> usize {
        self.rx.dma_regions() + self.tx.dma_regions()
    }

    /// The LIVE grants both queues hold for THIS device, named by owner - what the VT-d suite
    /// programs as this function's per-device window domain (ADR-075). Captured after init:
    /// nothing later in the boot registers or revokes on these queues.
    pub fn dma_grants(&self) -> alloc::vec::Vec<crate::dma::Grant> {
        let mut out = self.rx.grants();
        out.extend(self.tx.grants());
        out
    }

    /// Send one Ethernet frame (without the virtio header, which this adds), waiting for the device to
    /// release the buffer so the caller may send again.
    ///
    /// # Safety
    /// The device must be live.
    unsafe fn send(&self, frame: &[u8]) -> Result<(), NetError> {
        if NET_HDR_LEN + frame.len() > 4096 {
            return Err(NetError::TooLong);
        }
        let buf =
            core::slice::from_raw_parts_mut(self.tx_buf as *mut u8, NET_HDR_LEN + frame.len());
        buf[..NET_HDR_LEN].fill(0); // no offloads negotiated ⇒ an all-zero header is correct
        buf[NET_HDR_LEN..].copy_from_slice(frame);
        self.tx
            .add::<H>(0, self.tx_buf as u64, buf.len() as u32, false)
            .map_err(|_| NetError::Queue("the transmit buffer is not DMA-visible"))?;
        self.tx.kick::<H, T>(&self.transport);
        self.tx
            .poll_used_bounded::<H>(20_000_000)
            .map(|_| ())
            .ok_or(NetError::Timeout)
    }

    /// Wait for a received frame that `accept` recognizes, returning what it extracted. Frames that are
    /// not it are counted, and every buffer is re-posted so the queue never runs dry.
    ///
    /// # Safety
    /// The queues must be live.
    unsafe fn recv_until<R>(
        &self,
        spins: u64,
        mut accept: impl FnMut(&[u8]) -> Option<R>,
    ) -> Result<R, NetError> {
        for _ in 0..spins {
            if let Some((slot, written)) = self.rx.poll_used::<H>() {
                let idx = slot as usize;
                if idx >= self.rx_bufs.len() || (written as usize) < NET_HDR_LEN + ETH_HDR_LEN {
                    // Re-post and continue: a malformed completion must not wedge the queue.
                    if idx < self.rx_bufs.len() {
                        let _ = self.rx.add::<H>(
                            slot,
                            self.rx_bufs[idx] as u64,
                            RX_BUF_LEN as u32,
                            true,
                        );
                        self.rx.kick::<H, T>(&self.transport);
                    }
                    self.dropped.set(self.dropped.get() + 1);
                    continue;
                }
                let base = self.rx_bufs[idx];
                let frame = core::slice::from_raw_parts(
                    (base + NET_HDR_LEN) as *const u8,
                    written as usize - NET_HDR_LEN,
                );
                let taken = accept(frame);
                // Re-post BEFORE returning: a queue that loses one buffer per received frame stops
                // receiving after RX_BUFFERS frames.
                let _ = self.rx.add::<H>(slot, base as u64, RX_BUF_LEN as u32, true);
                self.rx.kick::<H, T>(&self.transport);
                match taken {
                    Some(r) => return Ok(r),
                    None => {
                        self.dropped.set(self.dropped.get() + 1);
                        continue;
                    }
                }
            }
            core::hint::spin_loop();
        }
        Err(NetError::Timeout)
    }

    /// Return the MAC for `target`: from the cache when it is remembered (no wire traffic, no wait),
    /// otherwise broadcast an ARP request and remember what answers.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn arp_resolve(&self, target: [u8; 4]) -> Result<[u8; 6], NetError> {
        if let Some(mac) = self.arp_cache.borrow_mut().lookup(target) {
            return Ok(mac);
        }
        let mut frame = [0u8; ETH_HDR_LEN + 28];
        frame[0..6].copy_from_slice(&BROADCAST);
        frame[6..12].copy_from_slice(&self.mac);
        put_be16(&mut frame, 12, ETHERTYPE_ARP);
        {
            let a = &mut frame[ETH_HDR_LEN..];
            put_be16(a, 0, 1); // hardware type: Ethernet
            put_be16(a, 2, ETHERTYPE_IPV4); // protocol type: IPv4
            a[4] = 6; // hardware address length
            a[5] = 4; // protocol address length
            put_be16(a, 6, 1); // operation: request
            a[8..14].copy_from_slice(&self.mac);
            a[14..18].copy_from_slice(&self.ip());
            a[18..24].copy_from_slice(&[0u8; 6]); // target hardware address: the question itself
            a[24..28].copy_from_slice(&target);
        }
        self.send(&frame)?;
        self.arp_wire_requests.set(self.arp_wire_requests.get() + 1);

        let mac = self.recv_until(20_000_000, |f| {
            if f.len() < ETH_HDR_LEN + 28 || be16(f, 12) != ETHERTYPE_ARP {
                return None;
            }
            let a = &f[ETH_HDR_LEN..];
            // A reply, for the address asked about, carrying a sender hardware address.
            if be16(a, 6) != 2 || a[14..18] != target {
                return None;
            }
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&a[8..14]);
            Some(mac)
        })?;
        self.arp_cache.borrow_mut().insert(target, mac);
        Ok(mac)
    }

    /// Send an ICMP echo request to `target` (at `target_mac`) and wait for the matching reply, returning
    /// the payload the peer echoed back.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn icmp_echo(
        &self,
        target: [u8; 4],
        target_mac: [u8; 6],
        ident: u16,
        seq: u16,
        payload: &[u8],
    ) -> Result<([u8; 32], usize), NetError> {
        if payload.len() > 32 {
            return Err(NetError::TooLong);
        }
        let icmp_len = 8 + payload.len();
        let ip_len = 20 + icmp_len;
        let mut frame = [0u8; ETH_HDR_LEN + 20 + 8 + 32];
        frame[0..6].copy_from_slice(&target_mac);
        frame[6..12].copy_from_slice(&self.mac);
        put_be16(&mut frame, 12, ETHERTYPE_IPV4);

        {
            let ip = &mut frame[ETH_HDR_LEN..ETH_HDR_LEN + 20];
            ip[0] = 0x45; // IPv4, 20-byte header
            put_be16(ip, 2, ip_len as u16);
            put_be16(ip, 4, ident); // identification — reused as the echo id: harmless and traceable
            put_be16(ip, 6, 0x4000); // don't fragment
            ip[8] = 64; // TTL
            ip[9] = 1; // protocol: ICMP
            put_be16(ip, 10, 0); // checksum field zeroed before computing it
            ip[12..16].copy_from_slice(&self.ip());
            ip[16..20].copy_from_slice(&target);
            let ck = checksum(ip);
            put_be16(ip, 10, ck);
        }
        {
            let icmp = &mut frame[ETH_HDR_LEN + 20..ETH_HDR_LEN + 20 + icmp_len];
            icmp[0] = 8; // echo request
            put_be16(icmp, 2, 0); // checksum zeroed before computing
            put_be16(icmp, 4, ident);
            put_be16(icmp, 6, seq);
            icmp[8..8 + payload.len()].copy_from_slice(payload);
            let ck = checksum(icmp);
            put_be16(icmp, 2, ck);
        }
        self.send(&frame[..ETH_HDR_LEN + ip_len])?;

        let want_len = payload.len();
        self.recv_until(20_000_000, |f| {
            if f.len() < ETH_HDR_LEN + 20 + 8 || be16(f, 12) != ETHERTYPE_IPV4 {
                return None;
            }
            let ip = &f[ETH_HDR_LEN..];
            if ip[0] >> 4 != 4 || ip[9] != 1 {
                return None; // not IPv4, or not ICMP
            }
            let ihl = (ip[0] & 0x0F) as usize * 4;
            if ip[12..16] != target || ip[16..20] != self.ip() || f.len() < ETH_HDR_LEN + ihl + 8 {
                return None; // not from the peer we asked, or not addressed to us
            }
            let icmp = &f[ETH_HDR_LEN + ihl..];
            // An echo REPLY with our identifier and sequence, whose checksum verifies over the bytes as
            // received (a correct internet checksum sums to zero).
            if icmp[0] != 0 || be16(icmp, 4) != ident || be16(icmp, 6) != seq {
                return None;
            }
            let end = core::cmp::min(icmp.len(), 8 + want_len);
            if checksum(&icmp[..end]) != 0 {
                return None;
            }
            let mut out = [0u8; 32];
            let n = end - 8;
            out[..n].copy_from_slice(&icmp[8..end]);
            Some((out, n))
        })
    }

    /// Carry one already-built IPv4 datagram to `target_mac`, and take one back.
    ///
    /// These two are the whole of what a TCP client needs from a network device (ADR-139), and
    /// they are deliberately the ONLY thing this driver knows about TCP: no handshake, no
    /// retransmission, no opinion about sequence numbers. Everything above them lives in
    /// `kernel_core::tcpnet`, which can therefore be proved with no device attached.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn send_ipv4_to(
        &self,
        target_mac: [u8; 6],
        datagram: &[u8],
    ) -> Result<(), NetError> {
        let mut frame = [0u8; ETH_HDR_LEN + 1500];
        if ETH_HDR_LEN + datagram.len() > frame.len() {
            return Err(NetError::TooLong);
        }
        frame[0..6].copy_from_slice(&target_mac);
        frame[6..12].copy_from_slice(&self.mac);
        put_be16(&mut frame, 12, ETHERTYPE_IPV4);
        frame[ETH_HDR_LEN..ETH_HDR_LEN + datagram.len()].copy_from_slice(datagram);
        self.send(&frame[..ETH_HDR_LEN + datagram.len()])
    }

    /// Wait up to `spins` device polls for one IPv4 datagram of `protocol` addressed to this
    /// machine, copying it into `out`. `Ok(0)` means nothing arrived in the bounded wait, which is
    /// a fact about this attempt rather than a claim about the peer.
    ///
    /// # Safety
    /// The queues must be live.
    pub unsafe fn recv_ipv4_into(
        &self,
        spins: u64,
        protocol: u8,
        out: &mut [u8],
    ) -> Result<usize, NetError> {
        let mut taken = 0usize;
        let mut too_long = false;
        let got = self.recv_until(spins, |f| {
            if f.len() < ETH_HDR_LEN || be16(f, 12) != ETHERTYPE_IPV4 {
                return None;
            }
            let body = &f[ETH_HDR_LEN..];
            let Ok(ip) = udpv4::parse_ipv4(body) else {
                return None;
            };
            if ip.protocol != protocol || ip.dst != self.ip() {
                return None;
            }
            // The datagram's own declared length, not the frame's: Ethernet padding is not part of
            // what a peer sent, and handing it upward would change the segment's checksum extent.
            let total = be16(body, 2) as usize;
            if total > body.len() {
                return None;
            }
            if total > out.len() {
                too_long = true;
                return Some(());
            }
            out[..total].copy_from_slice(&body[..total]);
            taken = total;
            Some(())
        });
        match got {
            Ok(()) if too_long => Err(NetError::TooLong),
            Ok(()) => Ok(taken),
            Err(NetError::Timeout) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Send one UDP datagram to `dport` at `target` and wait for a reply addressed to OUR port
    /// (`sport`), verified end to end: IPv4 header checksum, then UDP checksum over the
    /// pseudo-header — so a datagram re-addressed in flight cannot pass for a reply (ADR-060).
    ///
    /// Returns up to 512 payload bytes; a longer reply is truncated at the bound, never wrapped.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn udp_exchange(
        &self,
        target: [u8; 4],
        target_mac: [u8; 6],
        sport: u16,
        dport: u16,
        ident: u16,
        payload: &[u8],
    ) -> Result<([u8; 512], usize), NetError> {
        let mut frame = [0u8; ETH_HDR_LEN + udpv4::IPV4_HDR_MIN + udpv4::UDP_HDR_LEN + 512];
        frame[0..6].copy_from_slice(&target_mac);
        frame[6..12].copy_from_slice(&self.mac);
        put_be16(&mut frame, 12, ETHERTYPE_IPV4);
        let wrote = udpv4::build_datagram(
            &mut frame[ETH_HDR_LEN..],
            ident,
            self.ip(),
            target,
            sport,
            dport,
            payload,
        )
        .ok_or(NetError::TooLong)?;
        let n = ETH_HDR_LEN + wrote.len();
        self.send(&frame[..n])?;

        self.recv_until(20_000_000, |f| {
            if f.len() < ETH_HDR_LEN || be16(f, 12) != ETHERTYPE_IPV4 {
                return None;
            }
            // The DEMULTIPLEXER, such as it is honestly: a frame is ICMP or UDP by its protocol
            // byte, and a UDP frame is OURS only if every layer names us — source, destination,
            // port pair, checksum. Everything else is counted and dropped by recv_until.
            let ip = match udpv4::parse_ipv4(&f[ETH_HDR_LEN..]) {
                Ok(ip) => ip,
                Err(_) => return None,
            };
            if ip.src != target || ip.dst != self.ip() || ip.protocol != udpv4::PROTOCOL_UDP {
                return None;
            }
            let u = match udpv4::parse_udp(&ip) {
                Ok(u) => u,
                Err(_) => return None,
            };
            if u.dport != sport {
                return None; // a reply to some other exchange on this wire
            }
            let mut out = [0u8; 512];
            let take = core::cmp::min(u.payload.len(), out.len());
            out[..take].copy_from_slice(&u.payload[..take]);
            Some((out, take))
        })
    }

    /// Ask the network where THIS machine lives: broadcast a DHCPDISCOVER and return the OFFER bound
    /// to `xid`. The OFFER is evidence, not a lease — nothing is REQUESTED or taken (ADR-060); the
    /// driver keeps its static configuration and the suite cross-checks the two against each other.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn dhcp_discover(&self, xid: u32) -> Result<dhcp::Offer, NetError> {
        let mut disc = [0u8; dhcp::BOOTP_MIN_LEN];
        let question = dhcp::write_discover(&mut disc, self.mac, xid).ok_or(NetError::TooLong)?;
        let mut frame = [0u8; DHCP_FRAME];
        let n = dhcp_frame(&mut frame, self.mac, xid, question).ok_or(NetError::TooLong)?;
        self.dhcp_round(&frame[..n], &|reply| dhcp::parse_offer(reply, xid).ok())
    }

    /// Take a lease (ADR-234) and configure this device from it; nothing changes unless the
    /// whole exchange succeeds. See [`take_lease`].
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn dhcp_lease(&self, xid: u32) -> Result<dhcp::Offer, NetError> {
        let (ack, addr) = take_lease(self.mac, xid, self.addr.get(), |frame, accept| {
            self.dhcp_round(frame, accept).ok()
        })
        .ok_or(NetError::Timeout)?;
        self.addr.set(addr);
        Ok(ack)
    }

    /// Broadcast one DHCP frame and wait for the reply `accept` takes.
    unsafe fn dhcp_round(
        &self,
        frame: &[u8],
        accept: &dyn Fn(&[u8]) -> Option<dhcp::Offer>,
    ) -> Result<dhcp::Offer, NetError> {
        self.send(frame)?;
        // A malformed reply is a DROPPED frame with a counted reason, not a kernel fault: the
        // wire is untrusted, and one bad packet must not stop the machine from asking again.
        self.recv_until(20_000_000, |f| dhcp_payload(f).and_then(accept))
    }
}

/// The largest DHCP frame this stack sends: Ethernet, IPv4 and UDP headers around a minimum-size
/// BOOTP packet.
pub(crate) const DHCP_FRAME: usize =
    ETH_HDR_LEN + udpv4::IPV4_HDR_MIN + udpv4::UDP_HDR_LEN + dhcp::BOOTP_MIN_LEN;

/// One DHCP question as a broadcast Ethernet frame from `0.0.0.0` (RFC 2131 section 4.1: a client
/// with no lease has no address to speak from). Its length, or `None` when it does not fit.
pub(crate) fn dhcp_frame(
    frame: &mut [u8; DHCP_FRAME],
    mac: [u8; 6],
    xid: u32,
    question: &[u8],
) -> Option<usize> {
    frame[0..6].copy_from_slice(&BROADCAST); // the client has no peer MAC for a server yet
    frame[6..12].copy_from_slice(&mac);
    put_be16(frame, 12, ETHERTYPE_IPV4);
    let wrote = udpv4::build_datagram(
        &mut frame[ETH_HDR_LEN..],
        (xid >> 16) as u16,
        [0, 0, 0, 0],
        [255, 255, 255, 255], // limited broadcast: the question itself is addressless
        dhcp::CLIENT_PORT,
        dhcp::SERVER_PORT,
        question,
    )?;
    Some(ETH_HDR_LEN + wrote.len())
}

/// The DHCP message a received frame carries for a client (UDP to port 68, both checksums
/// verified), or `None`.
pub(crate) fn dhcp_payload(f: &[u8]) -> Option<&[u8]> {
    if f.len() < ETH_HDR_LEN || be16(f, 12) != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = udpv4::parse_ipv4(&f[ETH_HDR_LEN..]).ok()?;
    if ip.protocol != udpv4::PROTOCOL_UDP {
        return None;
    }
    let u = udpv4::parse_udp(&ip).ok()?;
    (u.dport == dhcp::CLIENT_PORT).then_some(u.payload)
}

/// Take a lease over any network device (ADR-234): DISCOVER, then REQUEST the address offered
/// from the server that offered it, and accept only an ACK that grants that same address.
/// `round(frame, accept)` broadcasts one frame and returns the first reply `accept` takes. Returns
/// the ACK and the addressing it describes (what the lease leaves out is kept from `old`).
pub(crate) fn take_lease(
    mac: [u8; 6],
    xid: u32,
    old: Addressing,
    mut round: impl FnMut(&[u8], &dyn Fn(&[u8]) -> Option<dhcp::Offer>) -> Option<dhcp::Offer>,
) -> Option<(dhcp::Offer, Addressing)> {
    let mut q = [0u8; dhcp::BOOTP_MIN_LEN];
    let mut frame = [0u8; DHCP_FRAME];
    let n = dhcp_frame(
        &mut frame,
        mac,
        xid,
        dhcp::write_discover(&mut q, mac, xid)?,
    )?;
    let offer = round(&frame[..n], &|r| dhcp::parse_offer(r, xid).ok())?;
    let n = dhcp_frame(
        &mut frame,
        mac,
        xid,
        dhcp::write_request(&mut q, mac, xid, &offer)?,
    )?;
    let ack = round(&frame[..n], &|r| {
        dhcp::parse_ack(r, xid)
            .ok()
            .filter(|a| a.yiaddr == offer.yiaddr)
    })?;
    let addr = Addressing {
        ip: ack.yiaddr,
        gateway: ack.router.or(offer.router).unwrap_or(old.gateway),
        mask: ack.subnet_mask.or(offer.subnet_mask).unwrap_or(old.mask),
        dns: ack.dns.or(offer.dns),
        lease_secs: ack.lease_secs,
        leased: true,
    };
    Some((ack, addr))
}

/// The network invariant suite (REQ-NET-001/002), reported through a caller-supplied logger like every
/// other suite. Returns the number of invariants proved, or `(index, name)` of the first failure.
/// The live device, seen as the three methods a TCP client needs (ADR-139's `Ipv4Link`).
///
/// The peer's MAC is resolved ONCE, when the link is made, rather than per segment: ARP on every
/// transmission would put a broadcast and a wait in front of every acknowledgement, and the cache
/// that would prevent that is already inside the driver.
pub struct NetLink<'a, H: VirtioHal, T: Transport> {
    dev: &'a VirtioNet<H, T>,
    peer_mac: [u8; 6],
}

impl<'a, H: VirtioHal, T: Transport> NetLink<'a, H, T> {
    /// Resolve the MAC of the peer's next hop (itself on this subnet, else the gateway; ADR-235) and
    /// hand back a link to it.
    ///
    /// # Safety
    /// The device must be live: its queues are published and its buffers posted.
    pub unsafe fn resolve(dev: &'a VirtioNet<H, T>, peer: [u8; 4]) -> Result<Self, NetError> {
        let peer_mac = dev.arp_resolve(dev.addressing().hop(peer))?;
        Ok(NetLink { dev, peer_mac })
    }
}

/// Ask `server:port` for `name`'s address (ADR-176): one query, one verified reply, read by the
/// bounded DNS reader. A server off this machine's /24 is reached through the gateway's MAC.
///
/// The answer tells the console where to dial, never whom to believe: see `dns`'s module header.
///
/// # Safety
/// The device must be live.
pub unsafe fn resolve_name<H: VirtioHal, T: Transport>(
    dev: &VirtioNet<H, T>,
    server: [u8; 4],
    port: u16,
    sport: u16,
    id: u16,
    name: &[u8],
) -> Result<crate::dns::Resolved, &'static str> {
    let mut query = [0u8; crate::dns::MAX_NAME + 20];
    let qlen =
        crate::dns::write_query(&mut query, id, name).map_err(crate::dns::DnsError::describe)?;
    let hop = dev.addressing().hop(server);
    let mac = dev
        .arp_resolve(hop)
        .map_err(|_| "no answer to the address resolution for the name server")?;
    let (reply, len) = dev
        .udp_exchange(server, mac, sport, port, id, &query[..qlen])
        .map_err(|e| match e {
            NetError::Timeout => "the name server did not answer",
            _ => "the network device refused the query",
        })?;
    if len == 0 {
        return Err("the name server did not answer");
    }
    crate::dns::parse_answer(&reply[..len], id, name).map_err(crate::dns::DnsError::describe)
}

impl<H: VirtioHal, T: Transport> crate::tcpnet::Ipv4Link for NetLink<'_, H, T> {
    fn send_ipv4(&self, datagram: &[u8]) -> Result<(), crate::tcpnet::LinkError> {
        // SAFETY: a `NetLink` is only constructed by `resolve`, which requires a live device, and
        // the device outlives the link by the borrow.
        unsafe { self.dev.send_ipv4_to(self.peer_mac, datagram) }.map_err(|e| match e {
            NetError::TooLong => crate::tcpnet::LinkError::TooLong,
            _ => crate::tcpnet::LinkError::Device,
        })
    }

    fn recv_ipv4(
        &self,
        spins: u64,
        protocol: u8,
        out: &mut [u8],
    ) -> Result<usize, crate::tcpnet::LinkError> {
        // SAFETY: as above.
        unsafe { self.dev.recv_ipv4_into(spins, protocol, out) }.map_err(|e| match e {
            NetError::TooLong => crate::tcpnet::LinkError::TooLong,
            _ => crate::tcpnet::LinkError::Device,
        })
    }

    fn local_ip(&self) -> [u8; 4] {
        self.dev.ip()
    }
}

/// What the network suite returns: how many invariants held, and the device itself.
///
/// Named because the pair is the whole point of ADR-140 — a suite that consumed the only NIC meant
/// a kernel that proved its network and then had none.
pub type NetProof<H, T> = (usize, VirtioNet<H, T>);

/// The network contract, proved against the real device. The device is HANDED BACK on success so
/// the machine can keep using it (ADR-140): a suite that consumed the only NIC would mean a kernel
/// that proves its network and then has none.
pub fn net_suite<H: VirtioHal, T: Transport, F: FnMut(usize, bool, &str)>(
    dev: VirtioNet<H, T>,
    mut log: F,
) -> Result<NetProof<H, T>, (usize, &'static str)> {
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

    // 1 — the device reported a real MAC (not all-zero, not a multicast address).
    let mac = dev.mac();
    check!(
        "net: the device reported a unicast MAC address from its config space",
        mac != [0u8; 6] && mac[0] & 1 == 0
    );

    // 2 — the DMA gate is live on this device: its rings and buffers are registered, and an address the
    //     driver never registered is refused before it could become a descriptor.
    check!(
        "net: the DMA gate denies an unregistered descriptor address (rings and buffers are registered)",
        dev.dma_gate_refuses_unregistered() && dev.dma_regions() >= 2
    );

    // 3 — ARP: the request went out AND an answer came back. This is the receive path's first proof, and
    //     it works only because the receive buffers were posted before DRIVER_OK.
    // SAFETY: the device is live and owned here.
    // The lease comes first (ADR-234): the gateway, and this machine's own address, are what the
    // network says, so every exchange below speaks from the address it was granted.
    // SAFETY: the device is live and owned here.
    let lease = unsafe {
        dev.dhcp_lease(0x1EA5_E000 ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]))
    };
    let gateway = dev.gateway();
    let gw = unsafe { dev.arp_resolve(gateway) };
    check!(
        "net: an ARP request for the gateway is answered with its hardware address",
        matches!(gw, Ok(m) if m != [0u8; 6])
    );
    let gw_mac = gw.unwrap_or([0u8; 6]);

    // 4 — ICMP echo: a real IPv4 packet with two correct checksums comes back as a reply carrying the same
    //     identifier, sequence and payload. A wrong checksum would be dropped by the peer in silence.
    let payload = b"aletheia-echo-01";
    // SAFETY: as above.
    let echo = unsafe { dev.icmp_echo(gateway, gw_mac, 0xA1E7, 1, payload) };
    check!(
        "net: an ICMP echo request is answered with a matching reply (both checksums verified)",
        matches!(&echo, Ok((buf, len)) if *len == payload.len() && buf[..*len] == payload[..])
    );

    // 5 — a second echo is matched on ITS sequence, not the first one's: the driver reads the reply rather
    //     than assuming the next frame is the answer.
    // SAFETY: as above.
    let echo2 = unsafe { dev.icmp_echo(gateway, gw_mac, 0xA1E7, 2, b"second") };
    check!(
        "net: a second echo is matched on its own sequence (replies are read, not assumed)",
        matches!(&echo2, Ok((buf, len)) if *len == 6 && &buf[..6] == b"second")
    );

    // 6 — the ARP cache is OBSERVABLE, not folklore: resolving the same address again must return the
    //     same answer WITHOUT a second broadcast. The counter is the proof; a cache that "worked"
    //     while the wire still saw a request would be a cache in name only.
    // SAFETY: the device is live and owned here.
    let gw_again = unsafe { dev.arp_resolve(gateway) };
    check!(
        "net: a repeated ARP resolve is answered from the cache and puts no second request on the wire",
        matches!(gw_again, Ok(m) if m == gw_mac) && dev.arp_wire_requests() == 1
    );

    // 7 — UDP round trip via DHCP: a DISCOVER broadcast draws an OFFER whose transaction id matches,
    //     whose checksums verified through the pseudo-header, and whose option walk found a real
    //     address. This is the first datagram exchange this kernel has ever completed.
    const XID: u32 = 0x4C3D_2E1F;
    // SAFETY: as above.
    let offer = unsafe { dev.dhcp_discover(XID) };
    check!(
        "net: a DHCP DISCOVER is answered by an OFFER bound to its transaction id (UDP round trip)",
        matches!(&offer, Ok(o) if o.yiaddr != [0u8; 4])
    );

    // 8 — the lease was taken (ADR-234): the network ACKed a REQUEST for the address it offers, and
    //     that address is the one this driver now speaks from. Before ADR-234 this compared the
    //     offer with a constant; now the constant is only the default before a lease.
    let offered = offer.unwrap_or_else(|_| dhcp::Offer::none());
    check!(
        "net: the network's lease is requested and acknowledged, and the driver speaks from the leased address",
        matches!(&lease, Ok(l) if l.yiaddr == offered.yiaddr && dev.ip() == l.yiaddr && dev.addressing().leased)
    );

    // 9 — a NEW transaction id draws its OWN answer: replies are matched per-exchange, never
    //     inherited from a previous question's luck.
    // SAFETY: as above.
    let offer2 = unsafe { dev.dhcp_discover(XID ^ 0xFFFF_FFFF) };
    check!(
        "net: a second DISCOVER under a new transaction id draws its own fresh answer",
        matches!(&offer2, Ok(o) if o.yiaddr == dev.ip())
    );

    Ok((n, dev))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_internet_checksum_matches_a_known_header_and_self_verifies() {
        // A worked IPv4 header (checksum field zero).
        let hdr = [
            0x45u8, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        let ck = checksum(&hdr);
        assert_eq!(ck, 0xb861);
        // With the checksum in place the sum over the whole header is zero — the property the receive path
        // relies on to validate a reply.
        let mut with = hdr;
        with[10] = (ck >> 8) as u8;
        with[11] = ck as u8;
        assert_eq!(checksum(&with), 0);
    }

    #[test]
    fn an_odd_trailing_byte_changes_the_checksum() {
        assert_ne!(checksum(&[0x01, 0x02, 0x03]), checksum(&[0x01, 0x02]));
    }
}
