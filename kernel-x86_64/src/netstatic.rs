//! The machine's live network device, kept after the boot suite proved it (ADR-140).
//!
//! The network suite used to consume the only NIC: the kernel proved its network and then had
//! none. It now hands the device back, and this module is where the machine keeps it, so the
//! console can open a real connection with the same device the invariants were proved against.
//!
//! The concurrency posture is the console's: only the main thread touches this, and it does so
//! between keystrokes. Nothing here runs in an interrupt handler.

use kernel_core::e1000::E1000Link;
use kernel_core::entropy;
use kernel_core::tcpconn::Connection;
use kernel_core::tcpnet::Ipv4Link;
use kernel_core::tcpnet::{self, LinkError, Plan};
use kernel_core::tlsclient::{self, TlsPump, TlsReport};
use kernel_core::tlshandshake::Handshake;
use kernel_core::trust::PinnedRoot;
use kernel_core::virtionet::{Addressing, NetLink};
use kernel_core::Hal;

use crate::hal::ActiveHal;
use crate::virtio::{Net, Rng};

static mut NET: Option<Net> = None;

/// The e1000 the boot suite proved, kept for the console when there is no virtio-net (ADR-235):
/// the NIC a VMware or a real machine has.
static mut E1000: Option<crate::pci::E1000> = None;

/// Whichever NIC the boot kept (ADR-235): virtio-net when there is one, else the e1000.
enum Nic {
    Virtio(&'static Net),
    E1000(&'static crate::pci::E1000),
}

impl Nic {
    fn kept() -> Result<Nic, &'static str> {
        // SAFETY: main thread only (the console's own thread), and the devices outlive the machine.
        unsafe {
            if let Some(d) = (*core::ptr::addr_of!(NET)).as_ref() {
                return Ok(Nic::Virtio(d));
            }
            if let Some(d) = (*core::ptr::addr_of!(E1000)).as_ref() {
                return Ok(Nic::E1000(d));
            }
        }
        Err("this machine has no network device")
    }

    fn addressing(&self) -> Addressing {
        match self {
            Nic::Virtio(d) => d.addressing(),
            Nic::E1000(d) => d.addressing(),
        }
    }

    fn ip(&self) -> [u8; 4] {
        self.addressing().ip
    }

    /// The link to `peer`, its MAC resolved for the next hop (itself on the leased subnet, else
    /// the gateway).
    fn link(&self, peer: [u8; 4]) -> Result<NicLink, &'static str> {
        let why = "no answer to the address resolution for that peer";
        match *self {
            // SAFETY: the device was brought up by the boot and its queues are live.
            Nic::Virtio(d) => unsafe { NetLink::resolve(d, peer) }
                .map(NicLink::Virtio)
                .map_err(|_| why),
            Nic::E1000(d) => d
                .arp_resolve(d.addressing().hop(peer))
                .map(|peer_mac| NicLink::E1000(E1000Link { dev: d, peer_mac }))
                .map_err(|_| why),
        }
    }
}

/// A link over either NIC; the TCP, TLS and DNS clients see one `Ipv4Link`.
enum NicLink {
    Virtio(NetLink<'static, crate::virtio::X86Virtio, crate::pci::PciTransport>),
    E1000(E1000Link<'static, crate::virtio::X86Virtio, kernel_core::e1000::MmioRegs>),
}

impl Ipv4Link for NicLink {
    fn send_ipv4(&self, datagram: &[u8]) -> Result<(), LinkError> {
        match self {
            NicLink::Virtio(l) => l.send_ipv4(datagram),
            NicLink::E1000(l) => l.send_ipv4(datagram),
        }
    }

    fn recv_ipv4(&self, spins: u64, protocol: u8, out: &mut [u8]) -> Result<usize, LinkError> {
        match self {
            NicLink::Virtio(l) => l.recv_ipv4(spins, protocol, out),
            NicLink::E1000(l) => l.recv_ipv4(spins, protocol, out),
        }
    }

    fn local_ip(&self) -> [u8; 4] {
        match self {
            NicLink::Virtio(l) => l.local_ip(),
            NicLink::E1000(l) => l.local_ip(),
        }
    }
}

/// The entropy device the boot suite proved, kept for the console's keys (ADR-153).
static mut RNG: Option<Rng> = None;

/// The console's one TLS pump and handshake, built on the first `tls` command and REUSED by every
/// later one: ninety kilobytes of workspace on a heap that never frees, allocated once (ADR-151).
static mut TLS: Option<(TlsPump, Handshake<PinnedRoot>)> = None;

/// The first ephemeral port this machine offers. Walked upward per connection so two conversations
/// in one session cannot collide in the peer's table.
const FIRST_PORT: u16 = 49152;
static mut NEXT_PORT: u16 = FIRST_PORT;

/// Keep the device the boot suite proved.
///
/// # Safety
/// Called once, from the boot path, before any other context can reach the static.
pub unsafe fn keep(dev: Net) {
    (*core::ptr::addr_of_mut!(NET)) = Some(dev);
}

/// Pause or resume the kept e1000's receiver (ADR-235), around the VT-d enable.
pub fn e1000_receiving(on: bool) {
    // SAFETY: boot path, single-threaded; read of a static written once before.
    if let Some(d) = unsafe { (*core::ptr::addr_of!(E1000)).as_ref() } {
        d.set_receiving(on);
    }
}

/// The DMA the kept e1000 may do, for the VT-d suite to map (ADR-235): a NIC that is kept keeps
/// receiving, so its rings and buffers must stay reachable once translation is on.
pub fn e1000_grants() -> Option<alloc::vec::Vec<kernel_core::dma::Grant>> {
    // SAFETY: boot path, single-threaded; read of a static written once before.
    unsafe { (*core::ptr::addr_of!(E1000)).as_ref() }.map(|d| d.dma_grants())
}

/// Keep the entropy device the boot suite proved.
///
/// # Safety
/// Called once, from the boot path, before any other context can reach the static.
pub unsafe fn keep_e1000(dev: crate::pci::E1000) {
    (*core::ptr::addr_of_mut!(E1000)) = Some(dev);
}

/// Keep the entropy device the boot suite proved.
///
/// # Safety
/// Called once, from the boot path, before any other context can reach the static.
pub unsafe fn keep_entropy(dev: Rng) {
    (*core::ptr::addr_of_mut!(RNG)) = Some(dev);
}

/// The device's addresses and counters for the console's `net` (ADR-185), `None` without a NIC.
pub fn facts() -> Option<kernel_core::shell::NetFacts> {
    // SAFETY: the console's main thread is the only context that touches `NET`/`NEXT_PORT`
    // after boot (module header); this is a read between keystrokes.
    let nic = Nic::kept().ok()?;
    let next_port = unsafe { *core::ptr::addr_of!(NEXT_PORT) };
    let a = nic.addressing();
    let (mac, dropped, arp_requests, dma_regions) = match nic {
        Nic::Virtio(d) => (d.mac(), d.dropped(), d.arp_wire_requests(), d.dma_regions()),
        Nic::E1000(d) => (d.mac(), 0, 0, d.dma_regions()),
    };
    Some(kernel_core::shell::NetFacts {
        mac,
        ip: a.ip,
        gateway: a.gateway,
        dns: a.dns,
        dropped,
        arp_requests,
        dma_regions,
        next_port,
    })
}

/// Open a TCP connection to `ip:port`, send `request`, and copy the peer's answer into `reply`.
///
/// Every bound here is this machine's, not the peer's: the local port, the initial sequence
/// number, the retransmission timeout, the poll budget and the reply buffer. A peer can make this
/// slow; it cannot make it unbounded.
pub fn fetch(
    ip: [u8; 4],
    port: u16,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, &'static str> {
    let nic = Nic::kept()?;
    let link = nic.link(ip)?;

    // SAFETY: as above; this is the sole writer of the port counter.
    let lport = unsafe {
        let p = NEXT_PORT;
        NEXT_PORT = if p == u16::MAX { FIRST_PORT } else { p + 1 };
        p
    };

    let hz = ActiveHal::timer_freq_hz().max(1);
    // A fifth of a second between retransmissions: long enough that a local answer arrives first,
    // short enough that a lost segment does not read as a hung console.
    let rto = (hz / 5).max(1);
    let mut conn = Connection::new(nic.ip(), lport, ip, port, rto);
    // The initial sequence number must be unpredictable on a real network, so it comes from the
    // machine's clock rather than from a constant this kernel ships.
    let iss = (ActiveHal::timer_ticks() as u32) ^ ((lport as u32) << 16);
    let plan = Plan {
        iss,
        budget: 4_000,
        spins_per_turn: 20_000,
    };
    match tcpnet::exchange(&link, &mut conn, plan, request, reply, &mut || {
        ActiveHal::timer_ticks()
    }) {
        Ok(done) => Ok(done.received),
        Err(LinkError::BudgetSpent) => Err("the peer did not answer inside this machine's budget"),
        Err(LinkError::ConnectionEnded) => Err("the peer refused or reset the connection"),
        Err(LinkError::TooLong) => Err("the answer did not fit this console's buffer"),
        Err(LinkError::Device) => Err("the network device refused the frame"),
    }
}

/// Ask the DNS server at `server:port` for `name`'s addresses (ADR-176), over the same device.
/// The query id and source port come from the machine's clock, so an off-path guess must hit both.
pub fn resolve(
    server: [u8; 4],
    port: u16,
    name: &[u8],
) -> Result<kernel_core::dns::Resolved, &'static str> {
    let nic = Nic::kept()?;
    let t = ActiveHal::timer_ticks();
    // SAFETY: as above; this is the sole writer of the port counter.
    let sport = unsafe {
        let p = NEXT_PORT;
        NEXT_PORT = if p == u16::MAX { FIRST_PORT } else { p + 1 };
        p
    };
    let link = nic.link(server)?;
    kernel_core::dns::resolve_over(&link, server, port, sport, (t ^ (t >> 17)) as u16, name)
}

/// Fresh bytes from the machine's entropy source (ADR-244): salts for console accounts. Refused
/// by name without a device, and when a draw is all one byte value.
pub fn random(out: &mut [u8]) -> Result<(), &'static str> {
    // SAFETY: main thread only (the console's own thread); the device outlives the machine.
    match unsafe { (*core::ptr::addr_of_mut!(RNG)).as_mut() } {
        Some(rng) => {
            entropy::EntropySource::fill(rng, out).map_err(|_| "the entropy device did not answer")
        }
        // No virtio-rng: the CPU's own generator (ADR-236).
        None => match crate::rdrand::Rdrand::detect() {
            Some(mut cpu) => entropy::EntropySource::fill(&mut cpu, out)
                .map_err(|_| "the CPU's random number generator gave no entropy"),
            None => Err("this machine has no entropy device and no RDRAND"),
        },
    }?;
    if out.len() > 1 && out.iter().all(|&b| b == out[0]) {
        return Err("the entropy source produced no entropy");
    }
    Ok(())
}

/// Open a TLS 1.3 conversation with `ip:port` as `server_name`, trusting exactly the Ed25519 root
/// `pin`, at the time this machine's own clock reads (ADR-148), and carry `request` and its
/// answer protected (ADR-151).
///
/// The ephemeral key and the client random are derived from sixty-four bytes of the entropy device
/// the boot suite proved (ADR-153), checked before use; a machine without that device opens no
/// conversation and says so. ADR-151 seeded from timer readings and named that as a gap.
pub fn fetch_tls(
    ip: [u8; 4],
    port: u16,
    server_name: &[u8],
    pin: [u8; 32],
    request: &[u8],
    reply: &mut [u8],
) -> Result<TlsReport, &'static str> {
    let nic = Nic::kept()?;
    let link = nic.link(ip)?;
    // SAFETY: as above; this is the sole writer of the port counter.
    let lport = unsafe {
        let p = NEXT_PORT;
        NEXT_PORT = if p == u16::MAX { FIRST_PORT } else { p + 1 };
        p
    };

    let clock = crate::rtc::CmosRtc::new();
    let verifier = kernel_core::clock::verifier_at(&clock, pin)
        .map_err(|_| "this machine's clock gave no time to judge a certificate by")?;

    let hz = ActiveHal::timer_freq_hz().max(1);
    let rto = (hz / 5).max(1);
    let mut conn = Connection::new(nic.ip(), lport, ip, port, rto);
    let ticks = ActiveHal::timer_ticks();
    let iss = (ticks as u32) ^ ((lport as u32) << 16);
    // The key's material comes from the entropy device the boot suite proved, checked draw by
    // draw (ADR-153); a machine without one opens no conversation, because a key made from a
    // clock is a key a patient observer can make too.
    // SAFETY: main thread only (the console's own thread); the device outlives the machine.
    let seed =
        match unsafe { (*core::ptr::addr_of_mut!(RNG)).as_mut() } {
            Some(rng) => entropy::tls_seed(rng).map_err(|why| match why {
                entropy::EntropyRefusal::Degenerate | entropy::EntropyRefusal::Repeated => {
                    "the entropy device produced no entropy; refusing to make a key from it"
                }
                _ => "the entropy device did not answer; refusing to make a key without it",
            })?,
            // No virtio-rng (VMware, a real machine): the CPU's own generator (ADR-236).
            None => match crate::rdrand::Rdrand::detect() {
                Some(mut cpu) => entropy::tls_seed(&mut cpu).map_err(|_| {
                    "the CPU's random number generator gave no entropy; refusing to make a key from it"
                })?,
                None => return Err(
                    "this machine has no entropy device and no RDRAND; a TLS key from a predictable seed is refused",
                ),
            },
        };
    let (private, random) = tlsclient::ephemeral_material(&seed);
    // SAFETY: main thread only (the console's own thread); the pump and the handshake are built on
    // the first command and rebound, never rebuilt, on every later one.
    let slot = unsafe { &mut *core::ptr::addr_of_mut!(TLS) };
    let (pump, hs) = match slot {
        Some(pair) => {
            pair.1
                .rebind(verifier, server_name, private, random)
                .map_err(|_| "that server name does not fit a ClientHello")?;
            pair
        }
        None => {
            let hs = Handshake::new(verifier, server_name, private, random)
                .map_err(|_| "that server name does not fit a ClientHello")?;
            *slot = Some((TlsPump::new(), hs));
            slot.as_mut().ok_or("the TLS workspace could not be kept")?
        }
    };
    // A protected conversation is several round trips, so the budget is three times the plain
    // TCP command's; every turn is still bounded on the device.
    let plan = Plan {
        iss,
        budget: 12_000,
        spins_per_turn: 20_000,
    };
    match tlsclient::exchange(
        &link,
        &mut conn,
        plan,
        pump,
        hs,
        request,
        reply,
        &mut || ActiveHal::timer_ticks(),
    ) {
        Ok(done) => Ok(TlsReport::from(done)),
        Err(why) => {
            // The reason goes to the operator; the counters go to the boot log, where a person
            // reading a gate transcript can see how far the conversation got before it ended.
            let (last, stage) = pump.last();
            crate::kprintln!(
                "[tls] ended: {:?} at stage {:?}; {} record(s) in, {} out; {} segment(s) in, {} out; {} turn(s)",
                why,
                stage,
                last.records_in,
                last.records_out,
                last.recv_segments,
                last.sent_segments,
                last.turns
            );
            Err(why.describe())
        }
    }
}
