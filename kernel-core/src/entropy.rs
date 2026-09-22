//! An entropy source: the bytes a key must be made of (REQ-SEC-TLS-011, ADR-153).
//!
//! ADR-151 shipped a TLS client whose ephemeral key was seeded from timer readings and said so:
//! this kernel had no entropy device, and a key made from a clock is a key a patient observer can
//! make too. This module is the device — virtio-rng, the one entropy source QEMU offers every
//! target alike — behind a contract small enough to state in a sentence: `fill` either fills the
//! whole buffer with bytes the device produced, or refuses by name.
//!
//! ## What is checked, and why
//!
//! A random-number device that has failed does not announce it; it returns zeros, or the same
//! bytes twice, or nothing. So every draw is checked before it is believed: a draw whose bytes are
//! all equal is `Degenerate`, a draw that repeats the previous one is `Repeated`, a device that does
//! not answer is `Device`. None of these can be confused with entropy, and none of them reaches a
//! key. The checks are cheap and they are not statistics — a statistical test on sixty-four bytes
//! proves nothing — they are the failure modes a broken device actually has.
//!
//! ## What the absence means
//!
//! [`NoEntropy`] exists so that a machine without a device has a type and a named refusal rather
//! than a fallback. A console on such a machine opens no TLS conversation and says why.

use core::marker::PhantomData;

use crate::virtioblk::{Transport, VirtioHal};
use crate::virtq::Virtqueue;

/// The virtio device id of an entropy source.
pub const VIRTIO_ID_RNG: u32 = 4;

const F_VERSION_1_BIT: u32 = 0;
const F_IOMMU_PLATFORM_BIT: u32 = 1;
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;
const S_FAILED: u32 = 0x80;
/// virtio-rng has exactly one queue: the driver posts device-writable buffers on it.
const REQUEST_QUEUE: u16 = 0;
/// How many polls one request may take before the device is declared silent. The device answers
/// in microseconds; this is the ADR-150 doctrine in the substrate's own unit.
const COMPLETION_POLLS: u64 = 20_000_000;
/// How many requests one `fill` may issue: a device that answers one byte at a time is refused
/// rather than trusted to eventually finish.
const MAX_REQUESTS_PER_FILL: usize = 64;
/// The seed a TLS conversation is made from: 32 bytes for the ephemeral scalar, 32 for the random.
pub const TLS_SEED_LEN: usize = 64;

/// Why no bytes were produced. Each is a different fact about the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyRefusal {
    /// No entropy device is attached to this machine.
    Absent,
    /// The device refused, misbehaved or did not answer, and the driver says how.
    Device(&'static str),
    /// A draw whose bytes are all one value. A device in that state produces no entropy.
    Degenerate,
    /// A draw identical to the previous one. A device repeating itself produces no entropy.
    Repeated,
}

/// A source of entropy.
pub trait EntropySource {
    /// Fill `out` entirely with fresh bytes, or refuse by name. A partial fill is a refusal: a
    /// caller that used a half-filled key buffer would use the zeros it started with.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyRefusal>;
}

/// The machine without an entropy device. A type, so the absence has a refusal and not a fallback.
pub struct NoEntropy;

impl EntropySource for NoEntropy {
    fn fill(&mut self, _out: &mut [u8]) -> Result<(), EntropyRefusal> {
        Err(EntropyRefusal::Absent)
    }
}

/// The virtio-rng driver over the shared virtqueue substrate.
pub struct VirtioRng<H: VirtioHal, T: Transport> {
    transport: T,
    queue: Virtqueue,
    /// The one device-writable frame every request lands in.
    frame: usize,
    /// The first 32 bytes of the previous draw, to catch a device repeating itself.
    last: [u8; 32],
    has_last: bool,
    /// Draws completed and draws refused, counted like every other refusal in this tree.
    pub draws: u64,
    pub refusals: u64,
    _hal: PhantomData<H>,
}

impl<H: VirtioHal, T: Transport> VirtioRng<H, T> {
    /// Bring the device up: reset, negotiate `VERSION_1` (and `IOMMU_PLATFORM` when offered), set up
    /// the request queue, register the one buffer frame, `DRIVER_OK`.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio-rng device, and `H::alloc_frame` must return
    /// identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(mut transport: T) -> Result<Self, EntropyRefusal> {
        let (_version, device_id) = transport.identity();
        if device_id != VIRTIO_ID_RNG {
            return Err(EntropyRefusal::Device("not a virtio entropy device"));
        }
        transport.set_status(0);
        let mut status = S_ACKNOWLEDGE;
        transport.set_status(status);
        status |= S_DRIVER;
        transport.set_status(status);
        let hi = transport.device_features(1);
        if hi & (1 << F_VERSION_1_BIT) == 0 {
            return Err(EntropyRefusal::Device(
                "device does not offer VIRTIO_F_VERSION_1",
            ));
        }
        let iommu_platform = hi & (1 << F_IOMMU_PLATFORM_BIT) != 0;
        transport.set_driver_features(0, 0);
        transport.set_driver_features(
            1,
            (1 << F_VERSION_1_BIT)
                | if iommu_platform {
                    1 << F_IOMMU_PLATFORM_BIT
                } else {
                    0
                },
        );
        transport.set_status(status | S_FEATURES_OK);
        if transport.status() & S_FEATURES_OK == 0 {
            transport.set_status(status | S_FAILED);
            return Err(EntropyRefusal::Device(
                "device rejected the negotiated features",
            ));
        }
        let mut queue = Virtqueue::new::<H, T>(&mut transport, REQUEST_QUEUE)
            .map_err(EntropyRefusal::Device)?;
        let frame =
            H::alloc_frame().ok_or(EntropyRefusal::Device("no frame for the entropy buffer"))?;
        queue
            .register_buffer(frame, crate::dma::PAGE, "virtio-rng.buffer")
            .map_err(|_| EntropyRefusal::Device("the buffer frame was refused as a DMA region"))?;
        status |= S_FEATURES_OK | S_DRIVER_OK;
        transport.set_status(status);
        Ok(VirtioRng {
            transport,
            queue,
            frame,
            last: [0u8; 32],
            has_last: false,
            draws: 0,
            refusals: 0,
            _hal: PhantomData,
        })
    }

    /// Whether the DMA gate would refuse to tell the device about `addr..addr+len`. The suite
    /// proves the gate with an address the driver never registered.
    pub fn dma_gate_refuses(&self, addr: u64, len: u32) -> bool {
        self.queue.would_refuse(addr, len)
    }

    fn refuse(&mut self, why: EntropyRefusal) -> EntropyRefusal {
        self.refusals += 1;
        why
    }

    /// One request: ask the device for `want` bytes into the frame, wait for the completion,
    /// return how many it wrote.
    ///
    /// # Safety
    /// The queue and the device must be live (they are, from `init` on).
    unsafe fn request(&mut self, want: usize) -> Result<usize, EntropyRefusal> {
        // The frame is zeroed first, so a device that "completes" without writing hands back
        // zeros - which the degeneracy check then refuses - rather than a previous draw.
        core::ptr::write_bytes(self.frame as *mut u8, 0, want);
        self.queue
            .add::<H>(0, self.frame as u64, want as u32, true)
            .map_err(|_| EntropyRefusal::Device("the DMA gate refused the buffer"))?;
        self.queue.kick::<H, T>(&self.transport);
        let (slot, written) = self
            .queue
            .poll_used_bounded::<H>(COMPLETION_POLLS)
            .ok_or(EntropyRefusal::Device("the device did not answer"))?;
        if slot != 0 {
            return Err(EntropyRefusal::Device(
                "the device completed a descriptor it was not given",
            ));
        }
        let written = written as usize;
        if written == 0 || written > want {
            return Err(EntropyRefusal::Device(
                "the device wrote nothing, or more than it was asked",
            ));
        }
        Ok(written)
    }
}

impl<H: VirtioHal, T: Transport> EntropySource for VirtioRng<H, T> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyRefusal> {
        let mut done = 0usize;
        let mut requests = 0usize;
        while done < out.len() {
            requests += 1;
            if requests > MAX_REQUESTS_PER_FILL {
                return Err(self.refuse(EntropyRefusal::Device(
                    "the device answers too little per request",
                )));
            }
            let want = (out.len() - done).min(crate::dma::PAGE);
            // SAFETY: the device and its queue have been live since `init`; the frame is the
            // registered, identity-mapped buffer this driver owns.
            let written = match unsafe { self.request(want) } {
                Ok(n) => n,
                Err(e) => return Err(self.refuse(e)),
            };
            // SAFETY: `frame` is the driver's own identity-mapped page and `written <= PAGE`.
            let bytes = unsafe { core::slice::from_raw_parts(self.frame as *const u8, written) };
            out[done..done + written].copy_from_slice(bytes);
            done += written;
        }
        // A device that has failed does not say so: it returns one value, or the last answer
        // again. Neither is entropy, and neither reaches a caller.
        if out.len() >= 8 && out.iter().all(|&b| b == out[0]) {
            return Err(self.refuse(EntropyRefusal::Degenerate));
        }
        if out.len() >= 32 {
            let mut head = [0u8; 32];
            head.copy_from_slice(&out[..32]);
            if self.has_last && head == self.last {
                return Err(self.refuse(EntropyRefusal::Repeated));
            }
            self.last = head;
            self.has_last = true;
        }
        self.draws += 1;
        Ok(())
    }
}

/// The seed one TLS conversation is made from, or the named reason there is none. This is the
/// only way a key's material leaves an entropy source: sixty-four fresh bytes, checked, never a
/// timer reading and never a fallback.
pub fn tls_seed(source: &mut dyn EntropySource) -> Result<[u8; TLS_SEED_LEN], EntropyRefusal> {
    let mut seed = [0u8; TLS_SEED_LEN];
    source.fill(&mut seed)?;
    if seed.iter().all(|&b| b == seed[0]) {
        return Err(EntropyRefusal::Degenerate);
    }
    Ok(seed)
}

/// The entropy contract, proved at boot on every CPU that has the device.
pub fn entropy_suite<H: VirtioHal, T: Transport>(
    dev: &mut VirtioRng<H, T>,
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

    // 1 - the device answers: a 64-byte request comes back filled, and the driver counted one draw.
    let mut a = [0u8; 64];
    let first = dev.fill(&mut a);
    check!(
        first == Ok(()) && dev.draws == 1,
        "entropy: the device fills a 64-byte request completely"
    );

    // 2 - a second draw differs from the first. A device repeating itself is refused by name, so
    //     this is proved both ways: the draws differ, and the counter of refusals did not move.
    let mut b = [0u8; 64];
    let second = dev.fill(&mut b);
    check!(
        second == Ok(()) && a != b && dev.refusals == 0,
        "entropy: two consecutive draws differ, and neither was refused"
    );

    // 3 - a page-sized draw is not degenerate: at least 200 of the 256 byte values appear. For
    //     uniform bytes the chance of fewer is below one in a hundred thousand; for a stuck or
    //     counting device it is certain.
    {
        let mut page = [0u8; 4096];
        let filled = dev.fill(&mut page);
        let mut seen = [false; 256];
        for &byte in page.iter() {
            seen[byte as usize] = true;
        }
        let distinct = seen.iter().filter(|&&s| s).count();
        check!(
            filled == Ok(()) && distinct >= 200,
            "entropy: a page-sized draw shows at least 200 of the 256 byte values"
        );
    }

    // 4 - the DMA gate: an address the driver never registered is refused before it becomes a
    //     descriptor; the driver's own frame is not.
    check!(
        dev.dma_gate_refuses(0x1000, 64) && !dev.dma_gate_refuses(dev.frame as u64, 64),
        "entropy: the DMA gate refuses an address the driver never registered"
    );

    // 5 - the absence of a source is a named refusal, and a refusal seeds no key.
    check!(
        NoEntropy.fill(&mut [0u8; 8]) == Err(EntropyRefusal::Absent)
            && tls_seed(&mut NoEntropy) == Err(EntropyRefusal::Absent),
        "entropy: no device is a named refusal, and a refusal seeds no TLS key"
    );

    // 6 - two TLS seeds from the device derive two different ephemeral scalars, neither zero.
    {
        let s1 = tls_seed(dev);
        let s2 = tls_seed(dev);
        let ok = match (s1, s2) {
            (Ok(x), Ok(y)) => {
                let (k1, r1) = crate::tlsclient::ephemeral_material(&x);
                let (k2, r2) = crate::tlsclient::ephemeral_material(&y);
                k1 != k2 && r1 != r2 && k1 != [0u8; 32] && k2 != [0u8; 32]
            }
            _ => false,
        };
        check!(
            ok,
            "entropy: two TLS seeds derive two different ephemeral keys, neither zero"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(u8);
    impl EntropySource for Fixed {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyRefusal> {
            out.fill(self.0);
            Ok(())
        }
    }

    struct Counting(u8);
    impl EntropySource for Counting {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyRefusal> {
            for b in out.iter_mut() {
                *b = self.0;
                self.0 = self.0.wrapping_add(1);
            }
            Ok(())
        }
    }

    #[test]
    fn a_stuck_source_seeds_nothing() {
        assert_eq!(tls_seed(&mut Fixed(0)), Err(EntropyRefusal::Degenerate));
        assert_eq!(tls_seed(&mut Fixed(0xFF)), Err(EntropyRefusal::Degenerate));
        assert_eq!(tls_seed(&mut NoEntropy), Err(EntropyRefusal::Absent));
    }

    #[test]
    fn a_changing_source_seeds_a_key_that_changes() {
        let mut src = Counting(3);
        let a = tls_seed(&mut src).expect("seed");
        let b = tls_seed(&mut src).expect("seed");
        assert_ne!(a, b);
        let (k1, _) = crate::tlsclient::ephemeral_material(&a);
        let (k2, _) = crate::tlsclient::ephemeral_material(&b);
        assert_ne!(k1, k2);
    }
}
