//! virtio-net + the smallest honest network stack: ARP and ICMP echo (REQ-NET-001/002, ADR-041).
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
//! No TCP, no UDP, no DHCP, no routing, no fragmentation, no ARP cache: the guest address is the fixed
//! `10.0.2.15` QEMU's gateway expects, and every reply is matched synchronously. Frames that are not the
//! answer being waited for are **counted and dropped**, not queued — a real stack needs a receive path
//! that hands frames to sockets, and that is the next slice rather than a hidden one. Completion is polled;
//! there are no interrupts in this kernel yet.
use crate::virtioblk::{Transport, VirtioHal};
use crate::virtq::Virtqueue;

/// virtio device id for a network card.
pub const VIRTIO_ID_NET: u32 = 1;

/// Queue indices on a device without multiqueue: 0 = receive, 1 = transmit.
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

/// Feature bits: MAC in device config, and VIRTIO_F_VERSION_1 (bit 32 ⇒ bit 0 of the high half).
const F_NET_MAC_BIT: u32 = 5;
const F_VERSION_1_BIT: u32 = 0;

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
    /// Frames received that were not what the caller was waiting for. Counted, never silently ignored: a
    /// nonzero count beside a failing wait distinguishes "the peer said nothing" from "the driver threw
    /// the answer away".
    dropped: core::cell::Cell<u64>,
    _hal: core::marker::PhantomData<H>,
}

fn be16(b: &[u8], at: usize) -> u16 {
    ((b[at] as u16) << 8) | b[at + 1] as u16
}

fn put_be16(b: &mut [u8], at: usize, v: u16) {
    b[at] = (v >> 8) as u8;
    b[at + 1] = v as u8;
}

/// The internet checksum (RFC 1071): ones-complement sum of 16-bit big-endian words. Used for the IPv4
/// header and the ICMP message. A wrong value means the peer drops the packet in silence, which is exactly
/// why an echo reply arriving is evidence the packet was well formed.
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        sum += be16(bytes, i) as u32;
        i += 2;
    }
    if i < bytes.len() {
        // A trailing odd byte contributes as the HIGH byte of a 16-bit word; dropping it would make two
        // different payloads check the same.
        sum += (bytes[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

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
        transport.set_driver_features(0, if want_mac { 1 << F_NET_MAC_BIT } else { 0 });
        transport.set_driver_features(1, 1 << F_VERSION_1_BIT);
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

        let rx = Virtqueue::new::<H, T>(&mut transport, RX_QUEUE).map_err(NetError::Queue)?;
        let tx = Virtqueue::new::<H, T>(&mut transport, TX_QUEUE).map_err(NetError::Queue)?;
        if rx.len() < RX_BUFFERS || tx.is_empty() {
            return Err(NetError::Queue("queues are too short for this driver"));
        }

        // One frame per receive buffer (RX_BUF_LEN < 4 KiB), plus one for transmit.
        let mut rx_bufs = [0usize; RX_BUFFERS as usize];
        for slot in 0..RX_BUFFERS {
            let buf = H::alloc_frame().ok_or(NetError::Queue("no frame for a receive buffer"))?;
            rx_bufs[slot as usize] = buf;
            rx.add::<H>(slot, buf as u64, RX_BUF_LEN as u32, true);
        }
        let tx_buf = H::alloc_frame().ok_or(NetError::Queue("no frame for the transmit buffer"))?;

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
            .add::<H>(0, self.tx_buf as u64, buf.len() as u32, false);
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
                        self.rx
                            .add::<H>(slot, self.rx_bufs[idx] as u64, RX_BUF_LEN as u32, true);
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
                self.rx.add::<H>(slot, base as u64, RX_BUF_LEN as u32, true);
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

    /// Broadcast an ARP request for `target` and return the MAC that answers.
    ///
    /// # Safety
    /// The device must be live.
    pub unsafe fn arp_resolve(&self, target: [u8; 4]) -> Result<[u8; 6], NetError> {
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

        self.recv_until(20_000_000, |f| {
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
        })
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

    // 2 — ARP: the request went out AND an answer came back. This is the receive path's first proof, and
    //     it works only because the receive buffers were posted before DRIVER_OK.
    // SAFETY: the device is live and owned here.
    let gw = unsafe { dev.arp_resolve(GATEWAY_IP) };
    check!(
        "net: an ARP request for the gateway is answered with its hardware address",
        matches!(gw, Ok(m) if m != [0u8; 6])
    );
    let gw_mac = gw.unwrap_or([0u8; 6]);

    // 3 — ICMP echo: a real IPv4 packet with two correct checksums comes back as a reply carrying the same
    //     identifier, sequence and payload. A wrong checksum would be dropped by the peer in silence.
    let payload = b"aletheia-echo-01";
    // SAFETY: as above.
    let echo = unsafe { dev.icmp_echo(GATEWAY_IP, gw_mac, 0xA1E7, 1, payload) };
    check!(
        "net: an ICMP echo request is answered with a matching reply (both checksums verified)",
        matches!(&echo, Ok((buf, len)) if *len == payload.len() && buf[..*len] == payload[..])
    );

    // 4 — a second echo is matched on ITS sequence, not the first one's: the driver reads the reply rather
    //     than assuming the next frame is the answer.
    // SAFETY: as above.
    let echo2 = unsafe { dev.icmp_echo(GATEWAY_IP, gw_mac, 0xA1E7, 2, b"second") };
    check!(
        "net: a second echo is matched on its own sequence (replies are read, not assumed)",
        matches!(&echo2, Ok((buf, len)) if *len == 6 && &buf[..6] == b"second")
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
