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
            _hal: core::marker::PhantomData,
        })
    }

    /// The device's own MAC address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
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
            a[14..18].copy_from_slice(&GUEST_IP);
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
            ip[12..16].copy_from_slice(&GUEST_IP);
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
            if ip[12..16] != target || ip[16..20] != GUEST_IP || f.len() < ETH_HDR_LEN + ihl + 8 {
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
            GUEST_IP,
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
            if ip.src != target || ip.dst != GUEST_IP || ip.protocol != udpv4::PROTOCOL_UDP {
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

        let mut frame =
            [0u8; ETH_HDR_LEN + udpv4::IPV4_HDR_MIN + udpv4::UDP_HDR_LEN + dhcp::BOOTP_MIN_LEN];
        frame[0..6].copy_from_slice(&BROADCAST); // the client has no peer MAC for a server yet
        frame[6..12].copy_from_slice(&self.mac);
        put_be16(&mut frame, 12, ETHERTYPE_IPV4);
        let wrote = udpv4::build_datagram(
            &mut frame[ETH_HDR_LEN..],
            (xid >> 16) as u16,
            GUEST_IP,
            [255, 255, 255, 255], // limited broadcast: the question itself is addressless
            dhcp::CLIENT_PORT,
            dhcp::SERVER_PORT,
            question,
        )
        .ok_or(NetError::TooLong)?;
        let n = ETH_HDR_LEN + wrote.len();
        self.send(&frame[..n])?;

        self.recv_until(20_000_000, |f| {
            if f.len() < ETH_HDR_LEN || be16(f, 12) != ETHERTYPE_IPV4 {
                return None;
            }
            let ip = match udpv4::parse_ipv4(&f[ETH_HDR_LEN..]) {
                Ok(ip) => ip,
                Err(_) => return None,
            };
            if ip.protocol != udpv4::PROTOCOL_UDP {
                return None;
            }
            let u = match udpv4::parse_udp(&ip) {
                Ok(u) => u,
                Err(_) => return None,
            };
            if u.dport != dhcp::CLIENT_PORT {
                return None;
            }
            // A malformed offer is a DROPPED frame with a counted reason, not a kernel fault: the
            // wire is untrusted, and one bad packet must not stop the machine from asking again.
            dhcp::parse_offer(u.payload, xid).ok()
        })
    }
}

/// The network invariant suite (REQ-NET-001/002), reported through a caller-supplied logger like every
/// other suite. Returns the number of invariants proved, or `(index, name)` of the first failure.
pub fn net_suite<H: VirtioHal, T: Transport, F: FnMut(usize, bool, &str)>(
    dev: VirtioNet<H, T>,
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
    let gw = unsafe { dev.arp_resolve(GATEWAY_IP) };
    check!(
        "net: an ARP request for the gateway is answered with its hardware address",
        matches!(gw, Ok(m) if m != [0u8; 6])
    );
    let gw_mac = gw.unwrap_or([0u8; 6]);

    // 4 — ICMP echo: a real IPv4 packet with two correct checksums comes back as a reply carrying the same
    //     identifier, sequence and payload. A wrong checksum would be dropped by the peer in silence.
    let payload = b"aletheia-echo-01";
    // SAFETY: as above.
    let echo = unsafe { dev.icmp_echo(GATEWAY_IP, gw_mac, 0xA1E7, 1, payload) };
    check!(
        "net: an ICMP echo request is answered with a matching reply (both checksums verified)",
        matches!(&echo, Ok((buf, len)) if *len == payload.len() && buf[..*len] == payload[..])
    );

    // 5 — a second echo is matched on ITS sequence, not the first one's: the driver reads the reply rather
    //     than assuming the next frame is the answer.
    // SAFETY: as above.
    let echo2 = unsafe { dev.icmp_echo(GATEWAY_IP, gw_mac, 0xA1E7, 2, b"second") };
    check!(
        "net: a second echo is matched on its own sequence (replies are read, not assumed)",
        matches!(&echo2, Ok((buf, len)) if *len == 6 && &buf[..6] == b"second")
    );

    // 6 — the ARP cache is OBSERVABLE, not folklore: resolving the same address again must return the
    //     same answer WITHOUT a second broadcast. The counter is the proof; a cache that "worked"
    //     while the wire still saw a request would be a cache in name only.
    // SAFETY: the device is live and owned here.
    let gw_again = unsafe { dev.arp_resolve(GATEWAY_IP) };
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

    // 8 — the address the network OFFERS is the address this driver CLAIMS. The constant was always
    //     an assumption about the simulator; now it is cross-checked against the authority that
    //     assigns addresses, so a change on either side fails this boot instead of going silent.
    let offered = offer.unwrap_or_else(|_| dhcp::Offer::none());
    check!(
        "net: the address the network offers IS the address the driver claims",
        offered.yiaddr == GUEST_IP
    );

    // 9 — a NEW transaction id draws its OWN answer: replies are matched per-exchange, never
    //     inherited from a previous question's luck.
    // SAFETY: as above.
    let offer2 = unsafe { dev.dhcp_discover(XID ^ 0xFFFF_FFFF) };
    check!(
        "net: a second DISCOVER under a new transaction id draws its own fresh answer",
        matches!(&offer2, Ok(o) if o.yiaddr == GUEST_IP)
    );

    Ok(n)
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
