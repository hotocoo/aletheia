//! The CPU's own random number generator as an entropy source (ADR-236).
//!
//! The console's TLS keys come only from an entropy source the machine can name (ADR-153). Under
//! QEMU that is virtio-rng; VMware and real x86-64 machines have no virtio-rng, so `tls` refused
//! on the artifact Aletheia ships. RDRAND is the source those machines do have.
//!
//! RDRAND reports each draw's success in CF. Intel's guidance is to retry a failed draw a small,
//! bounded number of times: a generator that keeps failing is refused by name rather than waited
//! on. `entropy::tls_seed` still rejects a seed whose bytes are all equal.

use kernel_core::entropy::{EntropyRefusal, EntropySource};

/// Retries per 64-bit draw before the generator is declared failed (Intel DRNG guide, 5.2.1).
const RETRIES: u32 = 10;

/// RDRAND, present on this CPU.
pub struct Rdrand(());

impl Rdrand {
    /// The CPU's generator, if CPUID leaf 1 reports RDRAND (ECX bit 30).
    pub fn detect() -> Option<Rdrand> {
        // CPUID leaf 1 exists on every x86-64 processor.
        let ecx = core::arch::x86_64::__cpuid(1).ecx;
        (ecx & (1 << 30) != 0).then_some(Rdrand(()))
    }

    fn draw(&self) -> Option<u64> {
        for _ in 0..RETRIES {
            let v: u64;
            let ok: u8;
            // SAFETY: `detect` proved the instruction exists; it writes two registers and flags.
            unsafe {
                core::arch::asm!(
                    "rdrand {v}",
                    "setc {ok}",
                    v = out(reg) v,
                    ok = out(reg_byte) ok,
                    options(nomem, nostack)
                )
            };
            if ok == 1 {
                return Some(v);
            }
        }
        None
    }
}

impl EntropySource for Rdrand {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyRefusal> {
        for chunk in out.chunks_mut(8) {
            let v = self
                .draw()
                .ok_or(EntropyRefusal::Device("RDRAND failed ten draws in a row"))?;
            chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
        }
        Ok(())
    }
}
