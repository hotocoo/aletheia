//! The machine's live network device, kept after the boot suite proved it (ADR-140).
//!
//! The network suite used to consume the only NIC: the kernel proved its network and then had
//! none. It now hands the device back, and this module is where the machine keeps it, so the
//! console can open a real connection with the same device the invariants were proved against.
//!
//! The concurrency posture is the console's: only the main thread touches this, and it does so
//! between keystrokes. Nothing here runs in an interrupt handler.

use kernel_core::tcpconn::Connection;
use kernel_core::tcpnet::{self, LinkError, Plan};
use kernel_core::virtionet::{NetLink, GUEST_IP};
use kernel_core::Hal;

use crate::hal::ActiveHal;
use crate::virtio::Net;

static mut NET: Option<Net> = None;

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
