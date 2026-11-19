//! The machine's live network device, kept after the boot suite proved it (ADR-140).
//!
//! The network suite used to consume the only NIC: the kernel proved its network and then had
//! none. It now hands the device back, and this module is where the machine keeps it, so the
//! console can open a real connection with the same device the invariants were proved against.
//!
//! The concurrency posture is the console's: only the main thread touches this, and it does so
//! between keystrokes. Nothing here runs in an interrupt handler.

use kernel_core::entropy;
use kernel_core::netguard::{self, NetSecret};
use kernel_core::tcpconn::Connection;
use kernel_core::tcpnet::{self, LinkError, Plan};
use kernel_core::tlsclient::{self, TlsPump, TlsReport};
use kernel_core::tlshandshake::Handshake;
use kernel_core::trust::PinnedRoot;
use kernel_core::virtionet::NetLink;
use kernel_core::Hal;

use crate::hal::ActiveHal;
use crate::virtio::{Net, Rng};

static mut NET: Option<Net> = None;

/// The entropy device the boot suite proved, kept for the console's keys (ADR-153).
static mut RNG: Option<Rng> = None;

/// The console's one TLS pump and handshake, built on the first `tls` command and REUSED by every
/// later one: ninety kilobytes of workspace on a heap that never frees, allocated once (ADR-151).
static mut TLS: Option<(TlsPump, Handshake<PinnedRoot>)> = None;

/// The secret behind this machine's local ports, initial sequence numbers and DNS ids (ADR-265),
/// drawn on the first conversation, after the boot suite has proved the entropy source.
static mut SECRET: Option<NetSecret> = None;

/// The network secret: seeded once from the entropy source, or from the clock, by name, when there
/// is none.
fn secret() -> &'static mut NetSecret {
    // SAFETY: the console's main thread is the only context that touches `SECRET` (module
    // header), and no caller holds the reference across another call.
    unsafe {
        (*core::ptr::addr_of_mut!(SECRET)).get_or_insert_with(|| {
            let mut key = [0u8; netguard::SECRET_LEN];
            NetSecret::new(random(&mut key).map(|()| key), ActiveHal::timer_ticks())
        })
    }
}

/// The kept entropy device, reset first if an unanswered request wedged it (ADR-267): a draw that
/// timed out once must not cost the console its keys until reboot, and the reset says so.
fn rng() -> Option<&'static mut Rng> {
    // SAFETY: main thread only (the console's own thread); the device outlives the machine.
    let dev = unsafe { (*core::ptr::addr_of_mut!(RNG)).as_mut()? };
    if dev.is_wedged() {
        // SAFETY: the boot brought the device up; its queue is live.
        let back = unsafe { dev.reset() }.is_ok();
        crate::kprintln!(
            "[entropy] the device held an unanswered request; reset it: {}",
            if back {
                "answering again"
            } else {
                "still wedged"
            }
        );
    }
    Some(dev)
}

/// The kept virtio NIC, reset first if a transmit it never handed back wedged it (ADR-267). The
/// MAC, the lease and the ARP cache survive the reset; the reset says so.
fn recover_net() {
    // SAFETY: main thread only (the console's own thread); the device outlives the machine.
    if let Some(d) = unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() } {
        if d.is_wedged() {
            // SAFETY: the boot brought the device up; its queues are live.
            let back = unsafe { d.reset() }.is_ok();
            crate::kprintln!(
                "[net] the device held a transmit it never handed back; reset it: {}",
                if back {
                    "sending again"
                } else {
                    "still wedged"
                }
            );
        }
    }
}

/// Keep the device the boot suite proved.
///
/// # Safety
/// Called once, from the boot path, before any other context can reach the static.
pub unsafe fn keep(dev: Net) {
    (*core::ptr::addr_of_mut!(NET)) = Some(dev);
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
    // SAFETY: the console's main thread is the only context that touches `NET`
    // after boot (module header); this is a read between keystrokes.
    let dev = unsafe { (*core::ptr::addr_of!(NET)).as_ref()? };
    Some(kernel_core::shell::NetFacts {
        mac: dev.mac(),
        ip: dev.ip(),
        gateway: dev.gateway(),
        dns: dev.addressing().dns,
        dropped: dev.dropped(),
        arp_requests: dev.arp_wire_requests(),
        dma_regions: dev.dma_regions(),
        secret: secret().source().describe(),
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
    // SAFETY: main thread only (the console's own thread), and the device outlives the machine.
    recover_net();
    let Some(dev) = (unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }) else {
        return Err("this machine has no network device");
    };
    // SAFETY: the device was brought up by the boot and its queues are live.
    let link = unsafe { NetLink::resolve(dev, ip) }
        .map_err(|_| "no answer to the address resolution for that peer")?;

    let lport = secret().local_port(dev.ip(), ip, port);

    let hz = ActiveHal::timer_freq_hz().max(1);
    // A fifth of a second between retransmissions: long enough that a local answer arrives first,
    // short enough that a lost segment does not read as a hung console.
    let rto = (hz / 5).max(1);
    let mut conn = Connection::new(dev.ip(), lport, ip, port, rto);
    // RFC 6528: the clock moves it forward, the secret hides where it starts (ADR-265).
    let iss = secret().iss(
        dev.ip(),
        lport,
        ip,
        port,
        netguard::iss_clock(ActiveHal::timer_ticks(), hz),
    );
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
/// The query id and source port come from the network secret (ADR-265), so an off-path forger
/// must guess both.
pub fn resolve(
    server: [u8; 4],
    port: u16,
    name: &[u8],
) -> Result<kernel_core::dns::Resolved, &'static str> {
    // SAFETY: main thread only (the console's own thread), and the device outlives the machine.
    recover_net();
    let Some(dev) = (unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }) else {
        return Err("this machine has no network device");
    };
    let (id, sport) = secret().dns_query();
    // SAFETY: the device was brought up by the boot and its queues are live.
    unsafe { kernel_core::virtionet::resolve_name(dev, server, port, sport, id, name) }
}

/// Fresh bytes from the machine's entropy source (ADR-244): salts for console accounts. Refused
/// by name without a device, and when a draw is all one byte value.
pub fn random(out: &mut [u8]) -> Result<(), &'static str> {
    match rng() {
        Some(rng) => {
            entropy::EntropySource::fill(rng, out).map_err(|_| "the entropy device did not answer")
        }
        None => Err("this machine has no entropy device"),
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
    // SAFETY: main thread only (the console's own thread), and the device outlives the machine.
    recover_net();
    let Some(dev) = (unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }) else {
        return Err("this machine has no network device");
    };
    // SAFETY: the device was brought up by the boot and its queues are live.
    let link = unsafe { NetLink::resolve(dev, ip) }
        .map_err(|_| "no answer to the address resolution for that peer")?;
    let lport = secret().local_port(dev.ip(), ip, port);

    let clock = crate::rtc::GoldfishRtc::new();
    let verifier = kernel_core::clock::verifier_at(&clock, pin)
        .map_err(|_| "this machine's clock gave no time to judge a certificate by")?;

    let hz = ActiveHal::timer_freq_hz().max(1);
    let rto = (hz / 5).max(1);
    let mut conn = Connection::new(dev.ip(), lport, ip, port, rto);
    let ticks = ActiveHal::timer_ticks();
    let iss = secret().iss(dev.ip(), lport, ip, port, netguard::iss_clock(ticks, hz));
    // The key's material comes from the entropy device the boot suite proved, checked draw by
    // draw (ADR-153); a machine without one opens no conversation, because a key made from a
    // clock is a key a patient observer can make too.
    let seed =
        match rng() {
            Some(rng) => entropy::tls_seed(rng).map_err(|why| match why {
                entropy::EntropyRefusal::Degenerate | entropy::EntropyRefusal::Repeated => {
                    "the entropy device produced no entropy; refusing to make a key from it"
                }
                _ => "the entropy device did not answer; refusing to make a key without it",
            })?,
            None => return Err(
                "this machine has no entropy device; a TLS key from a predictable seed is refused",
            ),
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
