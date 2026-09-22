//! The machine's live network device, kept after the boot suite proved it (ADR-140).
//!
//! The network suite used to consume the only NIC: the kernel proved its network and then had
//! none. It now hands the device back, and this module is where the machine keeps it, so the
//! console can open a real connection with the same device the invariants were proved against.
//!
//! The concurrency posture is the console's: only the main thread touches this, and it does so
//! between keystrokes. Nothing here runs in an interrupt handler.

use kernel_core::entropy;
use kernel_core::tcpconn::Connection;
use kernel_core::tcpnet::{self, LinkError, Plan};
use kernel_core::tlsclient::{self, TlsPump, TlsReport};
use kernel_core::tlshandshake::Handshake;
use kernel_core::trust::PinnedRoot;
use kernel_core::virtionet::{NetLink, GUEST_IP};
use kernel_core::Hal;

use crate::hal::ActiveHal;
use crate::virtio::{Net, Rng};

static mut NET: Option<Net> = None;

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

/// Keep the entropy device the boot suite proved.
///
/// # Safety
/// Called once, from the boot path, before any other context can reach the static.
pub unsafe fn keep_entropy(dev: Rng) {
    (*core::ptr::addr_of_mut!(RNG)) = Some(dev);
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
    let Some(dev) = (unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }) else {
        return Err("this machine has no network device");
    };
    // SAFETY: the device was brought up by the boot and its queues are live.
    let link = unsafe { NetLink::resolve(dev, ip) }
        .map_err(|_| "no answer to the address resolution for that peer")?;

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
    let mut conn = Connection::new(GUEST_IP, lport, ip, port, rto);
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
    let Some(dev) = (unsafe { (*core::ptr::addr_of_mut!(NET)).as_mut() }) else {
        return Err("this machine has no network device");
    };
    // SAFETY: the device was brought up by the boot and its queues are live.
    let link = unsafe { NetLink::resolve(dev, ip) }
        .map_err(|_| "no answer to the address resolution for that peer")?;
    // SAFETY: as above; this is the sole writer of the port counter.
    let lport = unsafe {
        let p = NEXT_PORT;
        NEXT_PORT = if p == u16::MAX { FIRST_PORT } else { p + 1 };
        p
    };

    let clock = crate::rtc::Pl031::new();
    let verifier = kernel_core::clock::verifier_at(&clock, pin)
        .map_err(|_| "this machine's clock gave no time to judge a certificate by")?;

    let hz = ActiveHal::timer_freq_hz().max(1);
    let rto = (hz / 5).max(1);
    let mut conn = Connection::new(GUEST_IP, lport, ip, port, rto);
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
