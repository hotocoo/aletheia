//! What a host on the network must not be able to guess or forge (ADR-265).
//!
//! ADR-263's audit named four numbers and one answer a neighbour could predict or forge: the TCP
//! initial sequence number (the clock XOR the port), the local port (a counter from 49152), the DNS
//! query id and source port (the clock and the same counter), and ARP replies (any reply naming the
//! asked address was believed, from anyone, about anyone). This module is the arch-independent half
//! of the answer; the platforms only hand it a seed and a clock.
//!
//! * [`NetSecret`] holds 32 bytes drawn once from the machine's entropy source. The initial
//!   sequence number is RFC 6528's `M + F(4-tuple, secret)`, the local port RFC 6056's algorithm 3
//!   (a secret per-destination offset plus a counter), and the DNS id and source port a keyed
//!   counter (RFC 5452). A machine without an entropy source seeds from its clock and SAYS so
//!   ([`SecretSource::Clock`]): no weaker than what it had before, and named rather than hidden.
//! * [`arp_request`] and [`arp_reply`] are the one ARP frame writer and the one reply check every
//!   NIC driver uses. A reply is believed only when it answers this machine's question about the
//!   asked address, is addressed to this machine, and names a sender that can exist.

use crate::crypto::Sha256;

/// The first port of the IANA dynamic range (RFC 6335), where every local port this machine
/// chooses lives.
pub const EPHEMERAL_FIRST: u16 = 49152;
/// How many ports the dynamic range holds: 49152..=65535.
pub const EPHEMERAL_COUNT: u16 = 16384;
/// RFC 6528's clock `M` ticks every 4 microseconds: 250 000 ticks per second.
pub const ISS_CLOCK_HZ: u128 = 250_000;
/// The secret's length: one SHA-256 key's worth.
pub const SECRET_LEN: usize = 32;

/// Domain separators, so the three uses of one secret can never produce each other's numbers.
const DOMAIN_ISS: u8 = 1;
const DOMAIN_PORT: u8 = 2;
const DOMAIN_DRAW: u8 = 3;
const DOMAIN_CLOCK_SEED: u8 = 4;

/// Where the secret came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretSource {
    /// The machine's entropy source.
    Entropy,
    /// The clock, because the entropy source refused for the named reason. Predictable to anyone
    /// who can estimate when the machine first spoke, which is why it is reported.
    Clock(&'static str),
}

impl SecretSource {
    /// One phrase for the console's `net`.
    pub fn describe(&self) -> &'static str {
        match self {
            SecretSource::Entropy => "the entropy source",
            SecretSource::Clock(_) => "the clock (no entropy source answered)",
        }
    }
}

/// The secret behind every number this machine's network stack must keep unguessable.
pub struct NetSecret {
    key: [u8; SECRET_LEN],
    source: SecretSource,
    /// RFC 6056's `next_ephemeral`: advances once per local port handed out.
    next_ephemeral: u16,
    /// Draws taken for DNS questions, so no two draws hash the same input.
    draws: u64,
}

impl NetSecret {
    /// Seed from a draw of the entropy source, or from the clock when the draw was refused.
    /// `drawn` is the platform's own entropy call, already checked for degenerate output.
    pub fn new(drawn: Result<[u8; SECRET_LEN], &'static str>, clock_ticks: u64) -> Self {
        let (key, source) = match drawn {
            Ok(key) => (key, SecretSource::Entropy),
            Err(why) => {
                let mut h = Sha256::new();
                h.update(&[DOMAIN_CLOCK_SEED]);
                h.update(&clock_ticks.to_le_bytes());
                (h.finalize(), SecretSource::Clock(why))
            }
        };
        NetSecret {
            key,
            source,
            next_ephemeral: 0,
            draws: 0,
        }
    }

    pub fn source(&self) -> SecretSource {
        self.source
    }

    /// `SHA-256(key || domain || parts...)`: the keyed function `F` of RFC 6528 and RFC 6056.
    fn f(&self, domain: u8, parts: &[&[u8]]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.key);
        h.update(&[domain]);
        for p in parts {
            h.update(p);
        }
        h.finalize()
    }

    /// RFC 6528: `ISN = M + F(localip, localport, remoteip, remoteport, secretkey)`. `M` keeps the
    /// sequence space moving forward for one 4-tuple, so a reconnect does not land inside the
    /// last conversation's window; `F` keeps anyone without the key from knowing where it starts.
    pub fn iss(&self, local: [u8; 4], lport: u16, remote: [u8; 4], rport: u16, m: u32) -> u32 {
        let d = self.f(
            DOMAIN_ISS,
            &[&local, &lport.to_be_bytes(), &remote, &rport.to_be_bytes()],
        );
        m.wrapping_add(u32::from_be_bytes([d[0], d[1], d[2], d[3]]))
    }

    /// RFC 6056 algorithm 3: a secret offset per (local, remote, remote port) plus a counter, in
    /// the dynamic range. Knowing the port this machine used towards one host says nothing about
    /// the port it uses towards another.
    pub fn local_port(&mut self, local: [u8; 4], remote: [u8; 4], rport: u16) -> u16 {
        let d = self.f(DOMAIN_PORT, &[&local, &remote, &rport.to_be_bytes()]);
        let offset = u16::from_be_bytes([d[0], d[1]]);
        self.next_ephemeral = self.next_ephemeral.wrapping_add(1);
        // 16384 divides 65536, so the wrapping sum stays uniform modulo the range.
        EPHEMERAL_FIRST + offset.wrapping_add(self.next_ephemeral) % EPHEMERAL_COUNT
    }

    /// A DNS question's id and source port (RFC 5452): both unpredictable, so an off-path forger
    /// must guess thirty bits rather than sixteen, or none.
    pub fn dns_query(&mut self) -> (u16, u16) {
        self.draws += 1;
        let d = self.f(DOMAIN_DRAW, &[&self.draws.to_be_bytes()]);
        let id = u16::from_be_bytes([d[0], d[1]]);
        let port = EPHEMERAL_FIRST + u16::from_be_bytes([d[2], d[3]]) % EPHEMERAL_COUNT;
        (id, port)
    }
}

/// RFC 6528's `M` from a platform timer: `ticks` at `hz` in 4-microsecond units, truncated to the
/// sequence space (it wraps, as the RFC's timer does).
pub fn iss_clock(ticks: u64, hz: u64) -> u32 {
    (ticks as u128 * ISS_CLOCK_HZ / hz.max(1) as u128) as u32
}

pub const ETH_HDR_LEN: usize = 14;
pub const ARP_LEN: usize = 28;
/// An Ethernet frame carrying one ARP packet, without padding.
pub const ARP_FRAME_LEN: usize = ETH_HDR_LEN + ARP_LEN;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;
const HTYPE_ETHERNET: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_REPLY: u16 = 2;
const BROADCAST: [u8; 6] = [0xFF; 6];

/// Why an ARP frame was not believed. Each names a different lie or mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArpRefusal {
    /// Not an ARP frame, or too short to be one.
    NotArp,
    /// ARP, but not Ethernet hardware addresses for IPv4.
    WrongFormat,
    /// Not a reply.
    NotReply,
    /// A reply about some address other than the one asked.
    NotTheAskedAddress,
    /// A reply to someone else's question: its target is not this machine.
    NotForUs,
    /// A sender hardware address no station can have (zero, broadcast or multicast), or one that
    /// is not the frame's own source.
    ImpossibleSender,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// The broadcast question "who has `target`? tell `ip` at `mac`".
pub fn arp_request(mac: [u8; 6], ip: [u8; 4], target: [u8; 4]) -> [u8; ARP_FRAME_LEN] {
    let mut f = [0u8; ARP_FRAME_LEN];
    f[0..6].copy_from_slice(&BROADCAST);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    let a = &mut f[ETH_HDR_LEN..];
    a[0..2].copy_from_slice(&HTYPE_ETHERNET.to_be_bytes());
    a[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&OP_REQUEST.to_be_bytes());
    a[8..14].copy_from_slice(&mac);
    a[14..18].copy_from_slice(&ip);
    // Target hardware address stays zero: it is the question.
    a[24..28].copy_from_slice(&target);
    f
}

/// The hardware address `target` is at, if `frame` is a believable answer to this machine's
/// question about it.
pub fn arp_reply(
    frame: &[u8],
    mac: [u8; 6],
    ip: [u8; 4],
    target: [u8; 4],
) -> Result<[u8; 6], ArpRefusal> {
    if frame.len() < ARP_FRAME_LEN || be16(frame, 12) != ETHERTYPE_ARP {
        return Err(ArpRefusal::NotArp);
    }
    let a = &frame[ETH_HDR_LEN..];
    if be16(a, 0) != HTYPE_ETHERNET || be16(a, 2) != ETHERTYPE_IPV4 || a[4] != 6 || a[5] != 4 {
        return Err(ArpRefusal::WrongFormat);
    }
    if be16(a, 6) != OP_REPLY {
        return Err(ArpRefusal::NotReply);
    }
    if a[14..18] != target {
        return Err(ArpRefusal::NotTheAskedAddress);
    }
    if a[18..24] != mac || a[24..28] != ip {
        return Err(ArpRefusal::NotForUs);
    }
    let sender = &a[8..14];
    // Bit 0 of the first octet marks group addresses, broadcast included.
    if sender[0] & 1 != 0 || sender.iter().all(|&b| b == 0) || frame[6..12] != *sender {
        return Err(ArpRefusal::ImpossibleSender);
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(sender);
    Ok(m)
}

/// The contract, proved at boot on every CPU. Pure: no device, fixed keys, so every check is
/// exact rather than statistical.
pub fn netguard_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    let local = [10, 0, 2, 15];
    let peer = [10, 0, 2, 2];
    let a = NetSecret::new(Ok([0x5A; SECRET_LEN]), 0);
    let b = NetSecret::new(Ok([0xA5; SECRET_LEN]), 0);

    check!(
        a.iss(local, 50000, peer, 80, 0) != b.iss(local, 50000, peer, 80, 0)
            && a.iss(local, 50000, peer, 80, 0) != a.iss(local, 50001, peer, 80, 0),
        "netguard: the initial sequence number depends on the secret and on every part of the 4-tuple"
    );
    check!(
        a.iss(local, 50000, peer, 80, 1_000)
            .wrapping_sub(a.iss(local, 50000, peer, 80, 0))
            == 1_000
            && a.iss(local, 50000, peer, 80, 7).wrapping_sub(a.iss(local, 50000, peer, 80, u32::MAX))
                == 8,
        "netguard: one 4-tuple's initial sequence number moves forward with the clock, across the wrap"
    );

    let mut p = NetSecret::new(Ok([0x5A; SECRET_LEN]), 0);
    let mut in_range = true;
    let mut last = 0u16;
    let mut steps_by_one = true;
    // More than four times round the range: the counter wraps and the port never leaves it.
    for i in 0..70_000u32 {
        let port = p.local_port(local, peer, 80);
        in_range &= port >= EPHEMERAL_FIRST;
        if i > 0 {
            let want = if last == u16::MAX {
                EPHEMERAL_FIRST
            } else {
                last + 1
            };
            steps_by_one &= port == want;
        }
        last = port;
    }
    check!(
        in_range && steps_by_one,
        "netguard: local ports stay in the dynamic range and step by one per destination, across the wrap"
    );
    let mut q = NetSecret::new(Ok([0x5A; SECRET_LEN]), 0);
    let mut r = NetSecret::new(Ok([0xA5; SECRET_LEN]), 0);
    check!(
        q.local_port(local, peer, 80) != q.local_port(local, [10, 0, 2, 3], 80)
            && r.local_port(local, peer, 80) != NetSecret::new(Ok([0x5A; SECRET_LEN]), 0).local_port(local, peer, 80),
        "netguard: the port towards one host says nothing about the port towards another, or under another secret"
    );

    let mut d = NetSecret::new(Ok([0x5A; SECRET_LEN]), 0);
    let (id0, port0) = d.dns_query();
    let (id1, port1) = d.dns_query();
    check!(
        (id0, port0) != (id1, port1)
            && port0 >= EPHEMERAL_FIRST
            && port1 >= EPHEMERAL_FIRST
            && d.dns_query().0 != NetSecret::new(Ok([0xA5; SECRET_LEN]), 0).dns_query().0,
        "netguard: each DNS question takes a fresh id and source port from the secret"
    );
    let clocked = NetSecret::new(Err("no device"), 12_345);
    check!(
        matches!(clocked.source(), SecretSource::Clock("no device"))
            && a.source() == SecretSource::Entropy
            && clocked.iss(local, 1, peer, 1, 0)
                != NetSecret::new(Err("no device"), 12_346).iss(local, 1, peer, 1, 0),
        "netguard: a secret seeded from the clock says so, and still differs per boot moment"
    );

    let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let gw_mac = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];
    let q = arp_request(mac, local, peer);
    let mut reply = [0u8; ARP_FRAME_LEN];
    reply[0..6].copy_from_slice(&mac);
    reply[6..12].copy_from_slice(&gw_mac);
    reply[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    reply[ETH_HDR_LEN..ETH_HDR_LEN + 6].copy_from_slice(&q[ETH_HDR_LEN..ETH_HDR_LEN + 6]);
    reply[ETH_HDR_LEN + 6..ETH_HDR_LEN + 8].copy_from_slice(&OP_REPLY.to_be_bytes());
    reply[ETH_HDR_LEN + 8..ETH_HDR_LEN + 14].copy_from_slice(&gw_mac);
    reply[ETH_HDR_LEN + 14..ETH_HDR_LEN + 18].copy_from_slice(&peer);
    reply[ETH_HDR_LEN + 18..ETH_HDR_LEN + 24].copy_from_slice(&mac);
    reply[ETH_HDR_LEN + 24..ETH_HDR_LEN + 28].copy_from_slice(&local);
    check!(
        be16(&q, 12) == ETHERTYPE_ARP
            && q[0..6] == BROADCAST
            && arp_reply(&reply, mac, local, peer) == Ok(gw_mac),
        "netguard: a reply to this machine's question about the asked address is believed"
    );
    let mut others_question = reply;
    others_question[ETH_HDR_LEN + 24..ETH_HDR_LEN + 28].copy_from_slice(&[10, 0, 2, 99]);
    let mut wrong_address = reply;
    wrong_address[ETH_HDR_LEN + 14..ETH_HDR_LEN + 18].copy_from_slice(&[10, 0, 2, 3]);
    check!(
        arp_reply(&others_question, mac, local, peer) == Err(ArpRefusal::NotForUs)
            && arp_reply(&wrong_address, mac, local, peer) == Err(ArpRefusal::NotTheAskedAddress)
            && arp_reply(&q, mac, local, peer) == Err(ArpRefusal::NotReply),
        "netguard: a reply to someone else's question, about another address, or a request, is not believed"
    );
    let mut broadcast_sender = reply;
    broadcast_sender[6..12].copy_from_slice(&BROADCAST);
    broadcast_sender[ETH_HDR_LEN + 8..ETH_HDR_LEN + 14].copy_from_slice(&BROADCAST);
    let mut zero_sender = reply;
    zero_sender[6..12].copy_from_slice(&[0; 6]);
    zero_sender[ETH_HDR_LEN + 8..ETH_HDR_LEN + 14].copy_from_slice(&[0; 6]);
    let mut spoofed_source = reply;
    spoofed_source[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x66]);
    check!(
        arp_reply(&broadcast_sender, mac, local, peer) == Err(ArpRefusal::ImpossibleSender)
            && arp_reply(&zero_sender, mac, local, peer) == Err(ArpRefusal::ImpossibleSender)
            && arp_reply(&spoofed_source, mac, local, peer) == Err(ArpRefusal::ImpossibleSender),
        "netguard: a reply naming a broadcast, zero, or other-than-its-own sender is not believed"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_netguard_invariant() {
        let n = netguard_suite(|_, passed, name| assert!(passed, "{name}"));
        assert_eq!(n, Ok(9));
    }

    #[test]
    fn the_clock_for_the_sequence_number_ticks_every_four_microseconds() {
        assert_eq!(iss_clock(1_000_000, 1_000_000), 250_000);
        assert_eq!(iss_clock(62_500_000, 62_500_000), 250_000);
        assert_eq!(
            iss_clock(5, 0),
            1_250_000,
            "a zero frequency is read as 1 Hz, not a division by zero"
        );
    }
}
