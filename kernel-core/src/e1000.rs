//! Intel 8254x (e1000) Ethernet driver (REQ-DRV-010, ADR-224): the first network driver for a
//! NIC family real machines and every mainstream hypervisor (QEMU `e1000`, VirtualBox, VMware)
//! ship. Legacy descriptors, one RX ring and one TX ring of 8 entries each, polled, DMA-gated.
//!
//! Register offsets and bits are from the Intel PCI/PCI-X Family of Gigabit Ethernet Controllers
//! Software Developer's Manual (8254x, section named per constant) and were checked against
//! QEMU's `hw/net/e1000_regs.h` before use.

use core::cell::Cell;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use crate::dma::DmaRegistry;
use crate::virtioblk::VirtioHal;
use crate::virtionet::{be16, dhcp_payload, put_be16, take_lease, Addressing, ETH_HDR_LEN};

// Registers (SDM §13.4).
pub const REG_CTRL: usize = 0x0000;
pub const REG_STATUS: usize = 0x0008;
pub const REG_EERD: usize = 0x0014;
pub const REG_IMC: usize = 0x00D8;
pub const REG_RCTL: usize = 0x0100;
pub const REG_TCTL: usize = 0x0400;
pub const REG_TIPG: usize = 0x0410;
pub const REG_RDBAL: usize = 0x2800;
pub const REG_RDBAH: usize = 0x2804;
pub const REG_RDLEN: usize = 0x2808;
pub const REG_RDH: usize = 0x2810;
pub const REG_RDT: usize = 0x2818;
pub const REG_TDBAL: usize = 0x3800;
pub const REG_TDBAH: usize = 0x3804;
pub const REG_TDLEN: usize = 0x3808;
pub const REG_TDH: usize = 0x3810;
pub const REG_TDT: usize = 0x3818;
pub const REG_MTA: usize = 0x5200;
pub const REG_RAL0: usize = 0x5400;
pub const REG_RAH0: usize = 0x5404;

const CTRL_SLU: u32 = 0x0000_0040; // set link up
const CTRL_ASDE: u32 = 0x0000_0020; // auto-speed detect
const CTRL_RST: u32 = 0x0400_0000; // global reset, self-clearing
const STATUS_LU: u32 = 0x0000_0002; // link up
const RAH_AV: u32 = 0x8000_0000; // address valid
const EERD_START: u32 = 1;
const EERD_DONE: u32 = 0x10;
const RCTL_EN: u32 = 0x0000_0002;
const RCTL_BAM: u32 = 0x0000_8000; // accept broadcast (ARP replies may be broadcast on some peers)
const RCTL_SECRC: u32 = 0x0400_0000; // strip CRC; BSIZE 00 = 2048 bytes
const TCTL_EN: u32 = 0x0000_0002;
const TCTL_PSP: u32 = 0x0000_0008; // pad short packets
const TCTL_CT: u32 = 0x10 << 4; // collision threshold (SDM recommended 0x10)
const TCTL_COLD: u32 = 0x40 << 12; // collision distance, full duplex (SDM recommended 0x40)
const TIPG_COPPER: u32 = 10 | 8 << 10 | 6 << 20; // IPGT 10, IPGR1 8, IPGR2 6 (SDM §13.4.34)
const TXD_CMD_EOP: u32 = 0x0100_0000;
const TXD_CMD_IFCS: u32 = 0x0200_0000;
const TXD_CMD_RS: u32 = 0x0800_0000;
const TXD_STAT_DD: u32 = 0x1;
const RXD_STAT_DD: u8 = 0x01;
const RXD_STAT_EOP: u8 = 0x02;

/// PCI ids this driver binds: 82540EM (QEMU `e1000`, VirtualBox default) and 82545EM (VMware).
pub const VENDOR_INTEL: u16 = 0x8086;
pub const DEVICE_IDS: [u16; 2] = [0x100E, 0x100F];

/// Ring entries. RDLEN/TDLEN must be multiples of 128 bytes: 8 x 16-byte descriptors.
pub const RING: usize = 8;
const DESC: usize = 16;
const RX_BUF: usize = 2048;
/// Largest frame this driver sends (no jumbo frames): one Ethernet MTU without the CRC.
pub const MAX_FRAME: usize = 1514;
const BUDGET_NS: u64 = 2_000_000_000;

/// How BAR0 is reached.
pub trait Regs {
    fn r32(&self, off: usize) -> u32;
    fn w32(&self, off: usize, v: u32);
}

/// BAR0 through an identity-mapped device window.
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
    /// # Safety
    /// `base` must be the mapped BAR0 (128 KiB) of an 8254x controller.
    pub unsafe fn new(base: usize) -> Self {
        MmioRegs { base }
    }
}

impl Regs for MmioRegs {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `new` requires a mapped BAR0; every offset used is below 0x6000.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum E1000Error {
    Timeout,
    TooLong,
    Unregistered,
}

/// A live 8254x with its rings published.
pub struct E1000<H: VirtioHal, R: Regs> {
    regs: R,
    mac: [u8; 6],
    rx_ring: usize,
    tx_ring: usize,
    rx_bufs: [usize; RING],
    tx_buf: usize,
    rx_next: Cell<usize>,
    tx_next: Cell<usize>,
    dma: DmaRegistry,
    /// Where this NIC's machine lives (ADR-234): the defaults until a DHCP lease.
    addr: Cell<Addressing>,
    _hal: PhantomData<H>,
}

fn wait<H: VirtioHal>(mut done: impl FnMut() -> bool) -> bool {
    let started = H::now_ns();
    while !done() {
        if H::now_ns().wrapping_sub(started) > BUDGET_NS {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

impl<H: VirtioHal, R: Regs> E1000<H, R> {
    /// Reset, read the MAC, publish both rings, enable RX and TX. Interrupts stay masked.
    ///
    /// # Safety
    /// `regs` must reach a live 8254x BAR0 with bus mastering enabled, and `H::alloc_frame` must
    /// return zeroed identity-mapped frames this kernel owns exclusively.
    pub unsafe fn init(regs: R) -> Result<Self, &'static str> {
        if regs.r32(REG_STATUS) == u32::MAX {
            return Err("e1000: STATUS reads all-ones (no controller behind BAR0)");
        }
        regs.w32(REG_IMC, u32::MAX);
        regs.w32(REG_CTRL, regs.r32(REG_CTRL) | CTRL_RST);
        // SDM §14.5: wait for RST to self-clear before touching anything else.
        if !wait::<H>(|| regs.r32(REG_CTRL) & CTRL_RST == 0) {
            return Err("e1000: global reset did not complete");
        }
        regs.w32(REG_IMC, u32::MAX);
        regs.w32(REG_CTRL, regs.r32(REG_CTRL) | CTRL_SLU | CTRL_ASDE);

        let mac = Self::read_mac(&regs)?;
        if mac == [0; 6] || mac[0] & 1 != 0 {
            return Err("e1000: the controller reports no unicast MAC address");
        }
        // Receive filter: our address in RA[0] (valid), multicast table cleared.
        regs.w32(
            REG_RAL0,
            u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]),
        );
        regs.w32(
            REG_RAH0,
            u16::from_le_bytes([mac[4], mac[5]]) as u32 | RAH_AV,
        );
        for i in 0..128 {
            regs.w32(REG_MTA + 4 * i, 0);
        }

        let mut dma = DmaRegistry::new();
        let mut frame = |owner| -> Result<usize, &'static str> {
            let f = H::alloc_frame().ok_or("e1000: frame allocator exhausted")?;
            dma.register(f, crate::dma::PAGE, owner)
                .map_err(|_| "e1000: a ring or buffer frame was refused as a DMA region")?;
            Ok(f)
        };
        let rx_ring = frame("e1000.rx-ring")?;
        let tx_ring = frame("e1000.tx-ring")?;
        let tx_buf = frame("e1000.tx-buf")?;
        let mut rx_bufs = [0usize; RING];
        for pair in rx_bufs.chunks_mut(2) {
            let f = frame("e1000.rx-buf")?;
            pair[0] = f;
            pair[1] = f + RX_BUF;
        }

        for (i, &b) in rx_bufs.iter().enumerate() {
            write_volatile((rx_ring + i * DESC) as *mut u64, b as u64);
        }
        H::barrier();
        regs.w32(REG_RDBAL, rx_ring as u32);
        regs.w32(REG_RDBAH, (rx_ring as u64 >> 32) as u32);
        regs.w32(REG_RDLEN, (RING * DESC) as u32);
        regs.w32(REG_RDH, 0);
        // Tail one behind head: all but one descriptor belong to the device (SDM §3.2.6).
        regs.w32(REG_RDT, (RING - 1) as u32);
        regs.w32(REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);

        regs.w32(REG_TDBAL, tx_ring as u32);
        regs.w32(REG_TDBAH, (tx_ring as u64 >> 32) as u32);
        regs.w32(REG_TDLEN, (RING * DESC) as u32);
        regs.w32(REG_TDH, 0);
        regs.w32(REG_TDT, 0);
        regs.w32(REG_TIPG, TIPG_COPPER);
        regs.w32(REG_TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

        Ok(E1000 {
            regs,
            mac,
            rx_ring,
            tx_ring,
            rx_bufs,
            tx_buf,
            rx_next: Cell::new(0),
            tx_next: Cell::new(0),
            dma,
            addr: Cell::new(Addressing::QEMU_USER),
            _hal: PhantomData,
        })
    }

    /// RA[0] when firmware or the reset loaded it (RAH.AV), otherwise EEPROM words 0..2 (SDM §5.6.1).
    fn read_mac(regs: &R) -> Result<[u8; 6], &'static str> {
        let rah = regs.r32(REG_RAH0);
        if rah & RAH_AV != 0 {
            let ral = regs.r32(REG_RAL0).to_le_bytes();
            let rah = rah.to_le_bytes();
            return Ok([ral[0], ral[1], ral[2], ral[3], rah[0], rah[1]]);
        }
        let mut mac = [0u8; 6];
        for word in 0..3u32 {
            regs.w32(REG_EERD, EERD_START | word << 8);
            let mut v = 0;
            if !wait::<H>(|| {
                v = regs.r32(REG_EERD);
                v & EERD_DONE != 0
            }) {
                return Err("e1000: EEPROM read did not complete");
            }
            let w = ((v >> 16) as u16).to_le_bytes();
            mac[2 * word as usize] = w[0];
            mac[2 * word as usize + 1] = w[1];
        }
        Ok(mac)
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Where this machine lives on the NIC's network (ADR-234).
    pub fn addressing(&self) -> Addressing {
        self.addr.get()
    }

    pub fn ip(&self) -> [u8; 4] {
        self.addr.get().ip
    }

    pub fn gateway(&self) -> [u8; 4] {
        self.addr.get().gateway
    }

    /// Stop or restart reception (ADR-235). A NIC the console keeps would otherwise go on
    /// writing received frames into memory while a later boot suite changes the machine under it;
    /// the VT-d suite needs every device quiet across the moment translation turns on.
    pub fn set_receiving(&self, on: bool) {
        let en = if on { RCTL_EN } else { 0 };
        self.regs.w32(REG_RCTL, en | RCTL_BAM | RCTL_SECRC);
    }

    /// Take a DHCP lease over this NIC and configure it from the ACK (ADR-234); `None`, with
    /// nothing changed, when the network does not complete the exchange.
    pub fn dhcp_lease(&self, xid: u32) -> Option<Addressing> {
        let (_, addr) = take_lease(self.mac, xid, self.addr.get(), |frame, accept| {
            self.send(frame).ok()?;
            self.recv_until(20_000_000, |f| dhcp_payload(f).and_then(accept))
                .ok()
        })?;
        self.addr.set(addr);
        Some(addr)
    }

    pub fn link_up(&self) -> bool {
        self.regs.r32(REG_STATUS) & STATUS_LU != 0
    }

    pub fn dma_grants(&self) -> alloc::vec::Vec<crate::dma::Grant> {
        self.dma.grants()
    }

    /// DMA regions registered, counted without building a list: the console's `net` reports it,
    /// and a reporting command allocates nothing (ADR-235).
    pub fn dma_regions(&self) -> usize {
        self.dma.live_regions()
    }

    /// Does the DMA gate refuse an address never registered, with all seven frames live?
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        !self.dma.visible(0x7fff_0000_0000, 64)
            && !self.dma.visible(self.tx_buf, crate::dma::PAGE * 2)
            && self.dma.live_regions() == 3 + RING / 2
    }

    /// Transmit one frame (no CRC; the controller appends it) and wait for Descriptor Done.
    pub fn send(&self, frame: &[u8]) -> Result<(), E1000Error> {
        if frame.len() > MAX_FRAME {
            return Err(E1000Error::TooLong);
        }
        if !self.dma.visible(self.tx_buf, frame.len().max(1)) {
            return Err(E1000Error::Unregistered);
        }
        let i = self.tx_next.get();
        let d = self.tx_ring + i * DESC;
        // SAFETY: tx_buf and the ring are our registered, identity-mapped frames; one frame in flight.
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), self.tx_buf as *mut u8, frame.len());
            write_volatile(d as *mut u64, self.tx_buf as u64);
            write_volatile(
                (d + 8) as *mut u32,
                frame.len() as u32 | TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
            );
            write_volatile((d + 12) as *mut u32, 0);
        }
        let next = (i + 1) % RING;
        self.tx_next.set(next);
        H::barrier();
        self.regs.w32(REG_TDT, next as u32);
        // SAFETY: as above.
        let done =
            wait::<H>(|| unsafe { read_volatile((d + 12) as *const u32) } & TXD_STAT_DD != 0);
        if done {
            Ok(())
        } else {
            Err(E1000Error::Timeout)
        }
    }

    /// Wait for a received frame `accept` recognises, for at most `polls` empty looks and at most
    /// the time budget; every descriptor is handed back to the device.
    pub fn recv_until<T>(
        &self,
        polls: u64,
        mut accept: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, E1000Error> {
        let started = H::now_ns();
        for _ in 0..polls {
            let i = self.rx_next.get();
            let d = self.rx_ring + i * DESC;
            // SAFETY: the descriptor lies in our RX ring frame.
            let status = unsafe { read_volatile((d + 12) as *const u8) };
            if status & RXD_STAT_DD != 0 {
                H::barrier();
                let (len, errors) = unsafe {
                    (
                        read_volatile((d + 8) as *const u16) as usize,
                        read_volatile((d + 13) as *const u8),
                    )
                };
                let whole = status & RXD_STAT_EOP != 0 && errors == 0 && len <= RX_BUF;
                let taken = if whole {
                    // SAFETY: the buffer is ours and the device wrote `len` bytes into it.
                    let f =
                        unsafe { core::slice::from_raw_parts(self.rx_bufs[i] as *const u8, len) };
                    accept(f)
                } else {
                    None
                };
                // Give the descriptor back: clear status, advance tail to it.
                unsafe { write_volatile((d + 12) as *mut u8, 0) };
                self.rx_next.set((i + 1) % RING);
                H::barrier();
                self.regs.w32(REG_RDT, i as u32);
                if let Some(t) = taken {
                    return Ok(t);
                }
                continue;
            }
            if H::now_ns().wrapping_sub(started) > BUDGET_NS {
                return Err(E1000Error::Timeout);
            }
            core::hint::spin_loop();
        }
        Err(E1000Error::Timeout)
    }

    /// Broadcast an ARP request for `target` and return the hardware address that answers.
    pub fn arp_resolve(&self, target: [u8; 4]) -> Result<[u8; 6], E1000Error> {
        let mut f = [0u8; ETH_HDR_LEN + 28];
        f[0..6].copy_from_slice(&[0xFF; 6]);
        f[6..12].copy_from_slice(&self.mac);
        put_be16(&mut f, 12, 0x0806);
        let a = &mut f[ETH_HDR_LEN..];
        put_be16(a, 0, 1); // Ethernet
        put_be16(a, 2, 0x0800); // IPv4
        a[4] = 6;
        a[5] = 4;
        put_be16(a, 6, 1); // request
        a[8..14].copy_from_slice(&self.mac);
        a[14..18].copy_from_slice(&self.ip());
        a[24..28].copy_from_slice(&target);
        self.send(&f)?;
        self.recv_until(u64::MAX, |r| {
            if r.len() < ETH_HDR_LEN + 28 || be16(r, 12) != 0x0806 {
                return None;
            }
            let a = &r[ETH_HDR_LEN..];
            if be16(a, 6) != 2 || a[14..18] != target {
                return None;
            }
            let mut m = [0u8; 6];
            m.copy_from_slice(&a[8..14]);
            Some(m)
        })
    }
}

/// A dropped driver stops the controller: receive and transmit off, interrupts masked. Without
/// this a frame arriving later is DMA nobody owns - found live when a late TCP segment from the
/// echo peer faulted under VT-d enforcement (ADR-227).
impl<H: VirtioHal, R: Regs> Drop for E1000<H, R> {
    fn drop(&mut self) {
        self.regs.w32(REG_RCTL, 0);
        self.regs.w32(REG_TCTL, 0);
        self.regs.w32(REG_IMC, u32::MAX);
    }
}

/// The TCP stack's link (ADR-227): IPv4 datagrams to one on-link peer over this NIC.
pub struct E1000Link<'a, H: VirtioHal, R: Regs> {
    pub dev: &'a E1000<H, R>,
    pub peer_mac: [u8; 6],
}

impl<H: VirtioHal, R: Regs> crate::tcpnet::Ipv4Link for E1000Link<'_, H, R> {
    fn send_ipv4(&self, datagram: &[u8]) -> Result<(), crate::tcpnet::LinkError> {
        let mut f = [0u8; MAX_FRAME];
        if ETH_HDR_LEN + datagram.len() > f.len() {
            return Err(crate::tcpnet::LinkError::TooLong);
        }
        f[0..6].copy_from_slice(&self.peer_mac);
        f[6..12].copy_from_slice(&self.dev.mac);
        put_be16(&mut f, 12, 0x0800);
        f[ETH_HDR_LEN..ETH_HDR_LEN + datagram.len()].copy_from_slice(datagram);
        self.dev
            .send(&f[..ETH_HDR_LEN + datagram.len()])
            .map_err(|_| crate::tcpnet::LinkError::Device)
    }

    fn recv_ipv4(
        &self,
        spins: u64,
        protocol: u8,
        out: &mut [u8],
    ) -> Result<usize, crate::tcpnet::LinkError> {
        let mut too_long = false;
        let got = self.dev.recv_until(spins, |f| {
            if f.len() < ETH_HDR_LEN || be16(f, 12) != 0x0800 {
                return None;
            }
            let body = &f[ETH_HDR_LEN..];
            let ip = crate::udpv4::parse_ipv4(body).ok()?;
            if ip.protocol != protocol || ip.dst != self.dev.ip() {
                return None;
            }
            // The datagram's declared length, not the frame's: Ethernet padding is not data.
            let total = be16(body, 2) as usize;
            if total > body.len() {
                return None;
            }
            if total > out.len() {
                too_long = true;
                return Some(0);
            }
            out[..total].copy_from_slice(&body[..total]);
            Some(total)
        });
        match got {
            Ok(_) if too_long => Err(crate::tcpnet::LinkError::TooLong),
            Ok(n) => Ok(n),
            Err(E1000Error::Timeout) => Ok(0),
            Err(_) => Err(crate::tcpnet::LinkError::Device),
        }
    }

    fn local_ip(&self) -> [u8; 4] {
        self.dev.ip()
    }
}

/// The TCP echo peer a VM gate places on the e1000's user-mode network (QEMU
/// `guestfwd=tcp:10.0.2.100:7-cmd:cat`). Absent on hypervisors without that feature.
pub const ECHO_PEER: [u8; 4] = [10, 0, 2, 100];
pub const ECHO_PORT: u16 = 7;

/// The NIC contract over a live 8254x with a user-mode network (gateway 10.0.2.2) behind it.
pub fn device_suite<H: VirtioHal, R: Regs, F: FnMut(usize, bool, &str)>(
    dev: &E1000<H, R>,
    log: &mut F,
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
    let mac = dev.mac();
    check!(
        "e1000: reset completed and the controller reports a unicast MAC",
        mac != [0; 6] && mac[0] & 1 == 0
    );
    check!(
        "e1000: the DMA gate denies an unregistered address (rings and buffers registered)",
        dev.dma_gate_refuses_unregistered()
    );
    check!(
        "e1000: a frame longer than one MTU is refused before the ring",
        dev.send(&[0u8; MAX_FRAME + 1]) == Err(E1000Error::TooLong)
    );
    // Wire group: only where the user-mode network's gateway (10.0.2.2) answers - the QEMU and
    // VirtualBox gates. On any other network the local group above is the whole suite (ADR-228).
    // A link that is down (no cable, or a hypervisor's link-up delay) is an absent environment.
    // The lease first (ADR-234): the gateway is whichever the network names; a network with no
    // DHCP server keeps the defaults.
    let gw = if dev.link_up() {
        let _ = dev.dhcp_lease(0xE100_0000 ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]));
        dev.arp_resolve(dev.gateway())
    } else {
        Err(E1000Error::Timeout)
    };
    if gw.is_err() {
        return Ok(n);
    }
    check!(
        "e1000: an ARP request for the gateway is answered with its hardware address",
        matches!(gw, Ok(m) if m != [0; 6])
    );
    let mut same = true;
    for _ in 0..3 * RING {
        same &= dev.arp_resolve(dev.gateway()) == gw;
    }
    check!(
        "e1000: both rings wrap in step over three ring lengths of request and answer",
        same
    );
    // The TCP stack over this NIC, when the gate placed an echo peer on the wire (ADR-227). The
    // family count says which: 7 with the peer, 6 without, and each gate pins its own.
    if let Ok(peer_mac) = dev.arp_resolve(ECHO_PEER) {
        let link = E1000Link { dev, peer_mac };
        let mut conn =
            crate::tcpconn::Connection::new(dev.ip(), 49_700, ECHO_PEER, ECHO_PORT, 200_000_000);
        let plan = crate::tcpnet::Plan {
            iss: H::now_ns() as u32,
            budget: 4_000,
            spins_per_turn: 20_000,
        };
        let mut reply = [0u8; 32];
        let got = crate::tcpnet::exchange(
            &link,
            &mut conn,
            plan,
            b"ALETHEIA-E1000",
            &mut reply,
            &mut H::now_ns,
        );
        check!(
            "e1000: a TCP conversation over the NIC reaches the echo peer and its bytes come back",
            got.is_ok_and(|e| e.received == 14) && &reply[..14] == b"ALETHEIA-E1000"
        );
    }
    Ok(n)
}
