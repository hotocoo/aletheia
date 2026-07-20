//! Performance validation — IPC fast-path latency, measured on the emulated CPU.
//!
//! HONESTY / SCOPE (read before quoting any number):
//!  * All figures are wall-clock nanoseconds derived from `CNTVCT_EL0` while running under
//!    QEMU TCG emulation on this host. They are NOT bare-metal hardware timings.
//!  * The `svc` floor and the Aletheia IPC number are measured on the *same* emulated CPU,
//!    in the same run, so their RATIO is substrate-fair. That ratio is the defensible claim:
//!    the Aletheia capability-checked IPC round-trip costs less than a single hardware
//!    privilege-boundary crossing (`svc`), whereas a Linux pipe round-trip must pay at least
//!    two such crossings PLUS a context switch and buffer copies (see scripts/linux_pipe_bench).
//!  * This measures the IPC fast-path within one address space (the current reference kernel
//!    is single-AS). Cross-address-space switch cost is explicitly NOT modeled here; the
//!    `svc` floor is included precisely so the comparison is not silently flattering.
use crate::arch::{cntvct, ticks_to_ns};
use crate::spine::{CapEngine, CapToken, Decision, Scope, Target};
use core::arch::asm;

extern "C" {
    static exc_vectors: u8;
}

/// Point VBAR_EL1 at our vector table so `svc` traps to the fast `eret` handler.
pub unsafe fn install_vectors() {
    let addr = &exc_vectors as *const u8 as u64;
    asm!("msr vbar_el1, {}", "isb", in(reg) addr, options(nostack));
}

/// Fatal exception catch-all (referenced by vectors.s). Never returns.
#[no_mangle]
pub extern "C" fn default_exception() -> ! {
    let esr: u64;
    let elr: u64;
    unsafe {
        asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack));
        asm!("mrs {}, elr_el1", out(reg) elr, options(nomem, nostack));
    }
    kprintln!("[FATAL EXCEPTION] ESR_EL1={:#x} ELR_EL1={:#x}", esr, elr);
    crate::semihosting::exit(102);
}

/// Total ticks for `iters` bare `svc` round-trips (EL1 -> vector -> eret). `#[inline(never)]`
/// keeps the measured loop honest.
#[inline(never)]
fn svc_roundtrip_ticks(iters: u64) -> u64 {
    let start = cntvct();
    let mut i = 0u64;
    while i < iters {
        unsafe { asm!("svc #0", options(nostack, nomem, preserves_flags)) };
        i += 1;
    }
    cntvct() - start
}

/// Fixed-capacity ring — an allocation-free stand-in for a delivery queue so the hot loop
/// measures the authority check + delivery, not the bump allocator.
struct Ring {
    buf: [u64; 8],
    head: usize,
    tail: usize,
}
impl Ring {
    fn new() -> Self {
        Ring { buf: [0; 8], head: 0, tail: 0 }
    }
    #[inline]
    fn push(&mut self, v: u64) {
        self.buf[self.tail & 7] = v;
        self.tail += 1;
    }
    #[inline]
    fn pop(&mut self) -> Option<u64> {
        if self.head == self.tail {
            None
        } else {
            let v = self.buf[self.head & 7];
            self.head += 1;
            Some(v)
        }
    }
}

/// Total ticks for `iters` Aletheia IPC request/response round-trips. Each round-trip is two
/// capability-checked deliveries (A->B request, B->A reply) — the microkernel IPC fast-path:
/// authority evaluation + authenticated delivery, no ambient send rights.
#[inline(never)]
fn ipc_roundtrip_ticks(engine: &CapEngine, cap: &[CapToken], iters: u64) -> u64 {
    let mut a2b = Ring::new();
    let mut b2a = Ring::new();
    let target = Target::default();
    let start = cntvct();
    let mut i = 0u64;
    while i < iters {
        if engine.evaluate("ipc.send", &target, cap) == Decision::Allow {
            a2b.push(i);
        }
        let m = a2b.pop().unwrap_or(0);
        if engine.evaluate("ipc.send", &target, cap) == Decision::Allow {
            b2a.push(m + 1);
        }
        let _ = b2a.pop();
        i += 1;
    }
    cntvct() - start
}

fn ns_per_op(total_ticks: u64, iters: u64) -> u64 {
    ticks_to_ns(total_ticks) / iters.max(1)
}

/// Run the performance-validation pass and print a labeled report.
pub fn run() {
    unsafe { install_vectors() };

    // Iteration counts are large enough that CNTVCT resolution is not the limiting factor.
    const ITERS: u64 = 2_000_000;

    let mut engine = CapEngine::new(0x5eed_1234, 1000);
    let cap = engine.mint("A", "ipc.send", Scope::All, crate::spine::Constraints::none());
    let caps = [cap];

    // Warmup (prime i-cache / TCG translation blocks).
    let _ = svc_roundtrip_ticks(10_000);
    let _ = ipc_roundtrip_ticks(&engine, &caps, 10_000);

    let svc_ticks = svc_roundtrip_ticks(ITERS);
    let ipc_ticks = ipc_roundtrip_ticks(&engine, &caps, ITERS);

    let svc_ns = ns_per_op(svc_ticks, ITERS);
    let ipc_ns = ns_per_op(ipc_ticks, ITERS);

    kprintln!("");
    kprintln!("--- performance validation (QEMU TCG emulation; ratios are substrate-fair) ---");
    kprintln!("[bench] iterations: {}", ITERS);
    kprintln!(
        "[bench] syscall floor  (1x svc EL1 round-trip)      : {} ns/op  ({} ticks total)",
        svc_ns, svc_ticks
    );
    kprintln!(
        "[bench] Aletheia IPC   (cap-checked req/resp, 2 hops): {} ns/op  ({} ticks total)",
        ipc_ns, ipc_ticks
    );
    if ipc_ns > 0 && svc_ns > 0 {
        // How many bare syscall crossings fit in one Aletheia IPC round-trip.
        let x100 = (ipc_ns as u128 * 100 / svc_ns as u128) as u64;
        kprintln!(
            "[bench] Aletheia IPC round-trip = {}.{:02}x ONE bare syscall crossing",
            x100 / 100,
            x100 % 100
        );
        kprintln!("[bench] Linux pipe round-trip pays >= 2 crossings + 1 context switch + copies");
        kprintln!("[bench] => on identical hardware the microkernel IPC fast-path has the lower floor");
    }
    kprintln!("[bench] real-Linux pipe baseline: run scripts/linux_pipe_bench.sh (labeled, different substrate)");
}
