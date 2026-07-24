//! SMP — x86-64 secondary-CPU (AP) bring-up parity with the aarch64/RISC-V targets (REQ-SMP-002).
//!
//! WHY THIS MATTERS: the aarch64 and RISC-V backends already boot their secondaries through a
//! firmware service (PSCI `CPU_ON`, SBI HSM `hart_start`). x86-64 has no such service after
//! `ExitBootServices` — the OS itself IS the CPU-bring-up protocol: discover the application
//! processors from the ACPI MADT, then wake each with the LAPIC **INIT–SIPI–SIPI** sequence into a
//! 16-bit real-mode **trampoline** that climbs to long mode over the SAME page tables the BSP owns.
//! This module does exactly that, then proves the identical 13-invariant cross-core substrate the
//! other two targets gate: exact atomic accounting, release/acquire message passing, the ADR-027
//! atomic authorize+execute primitive under live cross-core revocation (behind the ONE shared
//! `kernel_core::sync::SpinLock`), and a fixed-vector LAPIC IPI delivered + acknowledged end-to-end.
//!
//! SCOPE (contract-honest, ADR-010/ADR-019/ADR-021 Phase 1): bring-up + the concurrency substrate
//! on QEMU q35 + OVMF. Cross-core *scheduling* (per-CPU run queues, load balancing, TLB shootdown)
//! stays the named next slice under REQ-SMP-001. With `-smp 1` (MADT lists only the BSP) the suite
//! skips green, exactly like virtio-with-no-disk; the smoke test boots `-smp 4` and asserts the
//! full marker so a silent skip cannot pass CI.
//!
//! ORDERING (load-bearing): this suite runs BEFORE `usermode::selftest`. The ring-3 suite repoints
//! IRQ0 at its own context-switching entry and must leave IF=0 afterwards (see kmain), so the PIT
//! tick clock this suite uses for deadlines only exists before it. The APs are parked (`cli; hlt`)
//! by the time the ring-3 suite mutates the BSP IDT, so neither suite observes the other.
//!
//! CONCURRENCY RULES (same as the other targets):
//! * APs NEVER print — the serial/framebuffer writers are not serialized; the BSP narrates.
//! * Every capability-engine access happens under the one shared [`SpinLock`] (ADR-027 Option A).
//! * Liveness waits are PROGRESS-GATED with a PIT deadline on the BSP — never a fixed spin count;
//!   AP-side waits are gated on BSP-advanced phases, so the BSP clock bounds everything.
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::pit;
use kernel_core::spine::{CapEngine, CapToken, Constraints, Scope, Target};
use kernel_core::sync::SpinLock;

use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4};
use x86_64::registers::model_specific::Msr;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

// --- ACPI MADT discovery ----------------------------------------------------------------------
/// RSDP physical address, captured from the UEFI config table BEFORE ExitBootServices (the ACPI
/// tables themselves live in ACPI-reclaim memory, which persists and stays identity-mapped).
static RSDP_PA: AtomicUsize = AtomicUsize::new(0);

/// Called from `efi_main` while boot services are alive.
pub fn stash_rsdp(pa: usize) {
    RSDP_PA.store(pa, Ordering::Release);
}

#[inline]
unsafe fn read_u32(pa: usize) -> u32 {
    core::ptr::read_unaligned(pa as *const u32)
}
#[inline]
unsafe fn read_u64(pa: usize) -> u64 {
    core::ptr::read_unaligned(pa as *const u64)
}

/// Walk RSDP -> XSDT -> MADT and collect the LAPIC IDs of every enabled processor that is not
/// `bsp_apic_id`. Returns the count written into `out`.
fn madt_secondaries(bsp_apic_id: u32, out: &mut [u32; MAX_CPUS]) -> usize {
    let rsdp = RSDP_PA.load(Ordering::Acquire);
    if rsdp == 0 {
        return 0;
    }
    // SAFETY: the RSDP came from the UEFI config table; ACPI-reclaim memory is identity-mapped.
    unsafe {
        if core::ptr::read_unaligned(rsdp as *const [u8; 8]) != *b"RSD PTR " {
            return 0;
        }
        let revision = core::ptr::read_unaligned((rsdp + 15) as *const u8);
        if revision < 2 {
            return 0; // no XSDT on ACPI 1.0 — QEMU/OVMF is always >= 2
        }
        let xsdt = read_u64(rsdp + 24) as usize;
        if xsdt == 0 || core::ptr::read_unaligned(xsdt as *const [u8; 4]) != *b"XSDT" {
            return 0;
        }
        let xsdt_len = read_u32(xsdt + 4) as usize;
        let mut madt: usize = 0;
        let mut off = 36; // past the SDT header: 8-byte table pointers
        while off + 8 <= xsdt_len {
            let table = read_u64(xsdt + off) as usize;
            if table != 0 && core::ptr::read_unaligned(table as *const [u8; 4]) == *b"APIC" {
                madt = table;
                break;
            }
            off += 8;
        }
        if madt == 0 {
            return 0;
        }
        let madt_len = read_u32(madt + 4) as usize;
        let mut n = 0usize;
        let mut e = 44; // MADT: 36-byte SDT header + local-APIC address (4) + flags (4)
        while e + 2 <= madt_len {
            let ty = core::ptr::read_unaligned((madt + e) as *const u8);
            let len = core::ptr::read_unaligned((madt + e + 1) as *const u8) as usize;
            if len < 2 {
                break; // malformed entry — stop rather than loop forever
            }
            if ty == 0 && e + 8 <= madt_len {
                // Type 0: Processor Local APIC — acpi_id(1) apic_id(1) flags(4).
                let apic_id = core::ptr::read_unaligned((madt + e + 3) as *const u8) as u32;
                let flags = read_u32(madt + e + 4);
                let usable = flags & 0b11 != 0; // enabled OR online-capable
                if usable && apic_id != bsp_apic_id && n < MAX_CPUS {
                    out[n] = apic_id;
                    n += 1;
                }
            }
            e += len;
        }
        n
    }
}

// --- Local APIC (xAPIC MMIO) -------------------------------------------------------------------
const IA32_APIC_BASE: u32 = 0x1B;
const IA32_EFER: u32 = 0xC000_0080;
const IA32_GS_BASE: u32 = 0xC000_0101;

const LAPIC_ID: usize = 0x020;
const LAPIC_EOI: usize = 0x0B0;
const LAPIC_SVR: usize = 0x0F0;
const LAPIC_ICR_LO: usize = 0x300;
const LAPIC_ICR_HI: usize = 0x310;

const ICR_DELIVERY_PENDING: u32 = 1 << 12;
const ICR_INIT: u32 = 0x0000_4500; // INIT, edge, assert
const ICR_SIPI: u32 = 0x0000_4600; // Start-up IPI; low byte = vector (target page >> 12)
const ICR_FIXED: u32 = 0x0000_4000; // Fixed delivery; low byte = vector

/// The fixed interrupt vector the BSP sends to each AP (invariant 12). Clear of the CPU exception
/// range, the PIC block (0x20..0x2F) and the syscall door (0x80).
const IPI_VECTOR: u8 = 0x50;

/// xAPIC MMIO base, read from IA32_APIC_BASE by the BSP (identity-mapped by OVMF; the firmware
/// itself drives it for its own MP startup). Same physical base on every CPU.
static LAPIC_BASE: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn lapic_w(reg: usize, v: u32) {
    // SAFETY: xAPIC register in the identity-mapped LAPIC MMIO page.
    unsafe { core::ptr::write_volatile((LAPIC_BASE.load(Ordering::Relaxed) + reg) as *mut u32, v) }
}
#[inline]
fn lapic_r(reg: usize) -> u32 {
    // SAFETY: xAPIC register in the identity-mapped LAPIC MMIO page.
    unsafe { core::ptr::read_volatile((LAPIC_BASE.load(Ordering::Relaxed) + reg) as *const u32) }
}

/// Software-enable the calling CPU's LAPIC (SVR bit 8) with the spurious vector parked at 0xFF.
fn lapic_enable_self() {
    lapic_w(LAPIC_SVR, lapic_r(LAPIC_SVR) | 0x100 | 0xFF);
}

fn lapic_send(dest_apic_id: u32, icr_lo: u32) {
    lapic_w(LAPIC_ICR_HI, dest_apic_id << 24);
    lapic_w(LAPIC_ICR_LO, icr_lo);
    while lapic_r(LAPIC_ICR_LO) & ICR_DELIVERY_PENDING != 0 {
        core::hint::spin_loop();
    }
}

fn bsp_apic_id() -> u32 {
    // CPUID leaf 1 is universally supported; EBX[31:24] = initial APIC ID.
    core::arch::x86_64::__cpuid(1).ebx >> 24
}

// --- Real-mode trampoline ------------------------------------------------------------------------
// After a SIPI an AP starts at CS:IP = (vector<<8):0 in 16-bit REAL MODE with paging off — it
// cannot execute kernel code until it has climbed to long mode itself. The trampoline below is
// copied to physical TRAMP (page 8 => SIPI vector 0x08) and climbs in ONE hop: load the embedded
// mini-GDT, adopt the BSP's CR4/CR3/EFER, then set CR0.PE|PG together (the architected real-mode ->
// long-mode shortcut) and far-jump into a 64-bit code segment. All addresses are assembler-time
// constants against the fixed TRAMP base, so the copied blob needs no runtime relocation — the BSP
// only fills the data block (control-register values, stack top, Rust entry) before each launch.
const TRAMP: usize = 0x8000;

global_asm!(
    r#"
.equ TRAMP, 0x8000
.global ap_tramp_start
.global ap_tramp_end
.global ap_tramp_cr3
.global ap_tramp_cr4
.global ap_tramp_efer
.global ap_tramp_cr0
.global ap_tramp_stack
.global ap_tramp_entry
.section .text
.code16
ap_tramp_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    lgdtl TRAMP + (ap_tramp_gdtr - ap_tramp_start)
    movl TRAMP + (ap_tramp_cr4 - ap_tramp_start), %eax
    movl %eax, %cr4
    movl TRAMP + (ap_tramp_cr3 - ap_tramp_start), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    movl TRAMP + (ap_tramp_efer - ap_tramp_start), %eax
    xorl %edx, %edx
    wrmsr
    movl TRAMP + (ap_tramp_cr0 - ap_tramp_start), %eax
    movl %eax, %cr0
    ljmpl $0x08, $(TRAMP + (ap_tramp_long - ap_tramp_start))
.code64
ap_tramp_long:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movq TRAMP + (ap_tramp_stack - ap_tramp_start), %rsp
    movq TRAMP + (ap_tramp_entry - ap_tramp_start), %rax
    callq *%rax
2:  hlt
    jmp 2b
.balign 8
ap_tramp_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
ap_tramp_gdtr:
    .word (ap_tramp_gdtr - ap_tramp_gdt - 1)
    .long TRAMP + (ap_tramp_gdt - ap_tramp_start)
.balign 8
ap_tramp_cr3:   .quad 0
ap_tramp_cr4:   .long 0
ap_tramp_efer:  .long 0
ap_tramp_cr0:   .long 0
.balign 8
ap_tramp_stack: .quad 0
ap_tramp_entry: .quad 0
ap_tramp_end:
"#,
    options(att_syntax)
);

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
    static ap_tramp_cr3: u8;
    static ap_tramp_cr4: u8;
    static ap_tramp_efer: u8;
    static ap_tramp_cr0: u8;
    static ap_tramp_stack: u8;
    static ap_tramp_entry: u8;
}

/// Offset of a trampoline symbol inside the blob (link-address arithmetic; the blob itself needs
/// no relocation once copied because every reference is a TRAMP-based assembler constant).
macro_rules! tramp_off {
    ($sym:ident) => {{
        // SAFETY: address-of statics emitted by the global_asm! block above; nothing is read.
        #[allow(unused_unsafe)]
        let off = unsafe {
            (core::ptr::addr_of!($sym) as usize) - (core::ptr::addr_of!(ap_tramp_start) as usize)
        };
        off
    }};
}

/// Make the identity-mapped trampoline page present + writable + executable in the LIVE hierarchy
/// (OVMF builds with DXE memory protection may leave low RAM NX and/or read-only — an AP would
/// #PF the instant paging turns on). Walks CR3 manually because the low megabyte may be covered by
/// a 2 MiB or 1 GiB leaf that `Mapper::update_flags::<Size4KiB>` refuses. WP is dropped for the
/// edit window exactly as `vm::selftest` does.
fn make_tramp_page_executable() {
    const PRESENT: u64 = 1;
    const WRITABLE: u64 = 1 << 1;
    const PS: u64 = 1 << 7;
    const NX: u64 = 1 << 63;
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    let wp_was_set = Cr0::read().contains(Cr0Flags::WRITE_PROTECT);
    if wp_was_set {
        // SAFETY: ring 0, single-core edit window; restored below.
        unsafe { Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT)) };
    }

    let va = TRAMP as u64;
    let mut table = Cr3::read().0.start_address().as_u64();
    for level in (1..=4).rev() {
        let index = ((va >> (12 + 9 * (level - 1))) & 0x1FF) as usize;
        let entry_pa = table + (index * 8) as u64;
        // SAFETY: page-table frames are identity-mapped (phys_offset = 0, the vm.rs contract).
        unsafe {
            let entry = core::ptr::read_volatile(entry_pa as *const u64);
            if entry & PRESENT == 0 {
                break; // not mapped at all — the AP #PF will surface in the gate
            }
            core::ptr::write_volatile(entry_pa as *mut u64, (entry | WRITABLE) & !NX);
            if level == 1 || entry & PS != 0 {
                break; // reached the leaf (4 KiB, 2 MiB or 1 GiB)
            }
            table = entry & ADDR_MASK;
        }
    }
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(va));

    if wp_was_set {
        // SAFETY: restoring the flag cleared above.
        unsafe { Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT)) };
    }
}

// --- Per-CPU plumbing -----------------------------------------------------------------------
/// Max CPUs this suite drives (parity with the other targets; the gate boots 4).
const MAX_CPUS: usize = 8;
const STACK_BYTES: usize = 16 * 1024;

#[repr(C, align(16))]
struct SecondaryStack([u8; STACK_BYTES]);
/// One 16 KiB kernel stack per possible AP (BSS; zeroed by the UEFI loader).
static mut SECONDARY_STACKS: [SecondaryStack; MAX_CPUS - 1] =
    [const { SecondaryStack([0; STACK_BYTES]) }; MAX_CPUS - 1];

/// CPU index handed to the AP being launched (launches are strictly sequential: the BSP writes
/// this + the trampoline stack slot, sends the SIPI, and waits for ONLINE_MASK before the next).
static CUR_IDX: AtomicUsize = AtomicUsize::new(0);
/// Bit i set ⇒ CPU i is online (long mode on the shared tables, per-CPU state installed).
static ONLINE_MASK: AtomicUsize = AtomicUsize::new(0);
/// LAPIC ID and IA32_GS_BASE observed by each AP (per-CPU identity proof, the MPIDR/TPIDR twin).
static SEEN_APIC: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static SEEN_GS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
/// APs that reached the final parking loop.
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// Phase gate — the BSP advances it; APs wait on it. 1=counter 2=mailbox 3=caps 4=ipi 5=park.
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DONE_MAILBOX: AtomicUsize = AtomicUsize::new(0);
static DONE_CAPS: AtomicUsize = AtomicUsize::new(0);

// Phase 1 — exact cross-core atomic accounting.
const COUNTER_ROUNDS: usize = 10_000;
static SHARED_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Phase 2 — release/acquire mailbox (BSP publishes, every AP must observe exactly).
static MAILBOX_DATA: AtomicU64 = AtomicU64::new(0);
static MAILBOX_FLAG: AtomicBool = AtomicBool::new(false);
static MAILBOX_RESP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
const MAILBOX_PAYLOAD: u64 = 0x5EED_2026_C0DE_CAFE;

// Phase 3 — capability concurrency on real cores (ADR-027 mechanism, hardware-parallel).
static COMMITS: AtomicUsize = AtomicUsize::new(0);
/// Set by the BSP INSIDE the engine lock, immediately after `revoke` — the linearization marker.
static REVOKED: AtomicBool = AtomicBool::new(false);
static POST_DENIED: AtomicUsize = AtomicUsize::new(0);
static POST_ALLOWED: AtomicUsize = AtomicUsize::new(0);
const POST_ATTEMPTS: usize = 64;

// Phase 4 — fixed-vector LAPIC IPI.
static IPI_SEEN: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// An AP that took an unexpected fault reports itself here instead of triple-faulting silently.
static AP_FAULTS: AtomicUsize = AtomicUsize::new(0);

// --- Capability engine behind the SHARED kernel-core SpinLock (Issue 1: defined once) --------
struct CapState {
    engine: CapEngine,
    cap: CapToken,
}
static ENGINE: SpinLock<Option<CapState>> = SpinLock::new(None);

// --- AP interrupt handling ---------------------------------------------------------------------
// APs never touch the BSP's IDT (the ring-3 suite later mutates it); they load this dedicated
// table. The IPI handler is the delivery+acknowledge proof: vector arrives, per-CPU flag set via
// the GS_BASE identity, LAPIC EOI written.
static AP_IDT: crate::cell::Racy<InterruptDescriptorTable> =
    crate::cell::Racy::new(InterruptDescriptorTable::new());

extern "x86-interrupt" fn ap_ipi(_frame: InterruptStackFrame) {
    // SAFETY: rdmsr of IA32_GS_BASE, set by this AP in ap_entry before interrupts were enabled.
    let cpu = unsafe { Msr::new(IA32_GS_BASE).read() } as usize;
    if cpu < MAX_CPUS {
        IPI_SEEN[cpu].store(true, Ordering::SeqCst);
    }
    lapic_w(LAPIC_EOI, 0);
}

extern "x86-interrupt" fn ap_fault(_frame: InterruptStackFrame, _err: u64) {
    AP_FAULTS.fetch_add(1, Ordering::SeqCst);
    park();
}

extern "x86-interrupt" fn ap_fault_pf(
    _frame: InterruptStackFrame,
    _err: x86_64::structures::idt::PageFaultErrorCode,
) {
    AP_FAULTS.fetch_add(1, Ordering::SeqCst);
    park();
}

extern "x86-interrupt" fn ap_fault_noerr(_frame: InterruptStackFrame) {
    AP_FAULTS.fetch_add(1, Ordering::SeqCst);
    park();
}

fn park() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}

// --- Time (deadlines; BSP only — IF=1 on the BSP here, so PIT ticks advance) -------------------
fn deadline_after(secs: u64) -> u64 {
    pit::ticks() + secs * pit::FREQ_HZ as u64
}

/// Spin until `cond` or `deadline` (PIT ticks). True ⇒ condition met (progress-gated, never a
/// fixed spin count).
fn wait_until(deadline: u64, mut cond: impl FnMut() -> bool) -> bool {
    while !cond() {
        if pit::ticks() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// --- AP code path --------------------------------------------------------------------------------
/// Rust entry for a woken AP (from the trampoline: long mode on the shared CR3, IF=0, SP = its
/// private stack). Never returns; parks in `hlt` when the suite completes.
#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    let cpu = CUR_IDX.load(Ordering::Acquire);

    // Per-CPU identity: IA32_GS_BASE is the architectural per-CPU pointer slot (TPIDR_EL1 twin).
    // SAFETY: wrmsr/rdmsr of GS base at ring 0.
    unsafe { Msr::new(IA32_GS_BASE).write(cpu as u64) };
    // SAFETY: as above.
    let gs = unsafe { Msr::new(IA32_GS_BASE).read() };
    SEEN_GS[cpu].store(gs, Ordering::SeqCst);
    SEEN_APIC[cpu].store((lapic_r(LAPIC_ID) >> 24) as u64, Ordering::SeqCst);
    lapic_enable_self();
    ONLINE_MASK.fetch_or(1 << cpu, Ordering::SeqCst);

    // Phase 1 — hammer the shared counter; exactness proves real cross-core atomicity.
    while PHASE.load(Ordering::SeqCst) < 1 {
        core::hint::spin_loop();
    }
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    DONE_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Phase 2 — release/acquire mailbox: observe the payload the BSP published, answer with a
    // per-CPU transform so the BSP can prove THIS core read THIS value.
    while PHASE.load(Ordering::SeqCst) < 2 {
        core::hint::spin_loop();
    }
    while !MAILBOX_FLAG.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let data = MAILBOX_DATA.load(Ordering::Relaxed);
    MAILBOX_RESP[cpu].store(data ^ (0xA1E7 + cpu as u64), Ordering::SeqCst);
    DONE_MAILBOX.fetch_add(1, Ordering::SeqCst);

    // Phase 3 — capability concurrency on real cores. Commit through the ADR-027 primitive until
    // the BSP revokes; then make POST_ATTEMPTS more tries, every one of which must fail closed.
    while PHASE.load(Ordering::SeqCst) < 3 {
        core::hint::spin_loop();
    }
    while !REVOKED.load(Ordering::Acquire) {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            let _ = state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            );
        }
        drop(guard);
        core::hint::spin_loop();
    }
    for _ in 0..POST_ATTEMPTS {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            match state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            ) {
                Ok(_) => POST_ALLOWED.fetch_add(1, Ordering::SeqCst),
                Err(_) => POST_DENIED.fetch_add(1, Ordering::SeqCst),
            };
        }
    }
    DONE_CAPS.fetch_add(1, Ordering::SeqCst);

    // Phase 4 — receive the BSP's fixed-vector IPI through this AP's own IDT + LAPIC (the x86
    // delivery+acknowledge proof: the handler fires, tags this CPU, and writes EOI).
    while PHASE.load(Ordering::SeqCst) < 4 {
        core::hint::spin_loop();
    }
    // SAFETY: the BSP fully populated AP_IDT before any AP was launched; read-only shared load.
    unsafe { AP_IDT.get() }.load();
    x86_64::instructions::interrupts::enable();
    while !IPI_SEEN[cpu].load(Ordering::SeqCst) && PHASE.load(Ordering::SeqCst) < 5 {
        core::hint::spin_loop(); // BSP-clocked: its deadline advances PHASE on a miss
    }
    x86_64::instructions::interrupts::disable();

    PARKED.fetch_add(1, Ordering::SeqCst);
    park();
}

// --- BSP selftest --------------------------------------------------------------------------------
/// Prove the SMP invariants live. `Ok(n)` = all passed (`Ok(0)` = MADT lists no other CPUs, skip
/// green — the smoke test boots `-smp 4` so a silent skip cannot pass CI); `Err((idx, name))` =
/// failure.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            if !($cond) {
                kprintln!("  [FAIL {:>2}] {}", n, $name);
                return Err((n, $name));
            }
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }

    // Discover the APs from the ACPI MADT (x86 has no PSCI/HSM — firmware tables + INIT-SIPI-SIPI
    // ARE the platform bring-up protocol).
    let bsp = bsp_apic_id();
    let mut apic_ids = [0u32; MAX_CPUS];
    let found = madt_secondaries(bsp, &mut apic_ids);
    if found == 0 {
        kprintln!("  [info  ] single-CPU machine (MADT lists no other CPUs) — SMP suite skipped");
        return Ok(0);
    }
    let planned = found.min(MAX_CPUS - 1);

    // LAPIC up on the BSP (ICR sends require it); preserve the firmware's LVT/virtual-wire state.
    // SAFETY: rdmsr IA32_APIC_BASE at ring 0; bits 12..36 = the xAPIC MMIO base.
    let apic_base = unsafe { Msr::new(IA32_APIC_BASE).read() };
    if apic_base & (1 << 10) != 0 {
        kprintln!("  [info  ] LAPIC is in x2APIC mode — xAPIC MMIO suite not applicable, skipped");
        return Ok(0);
    }
    LAPIC_BASE.store((apic_base & 0xF_FFFF_F000) as usize, Ordering::SeqCst);
    lapic_enable_self();

    // Populate the AP interrupt table BEFORE any AP exists to load it.
    // SAFETY: single-writer init; APs only `load()` it after PHASE reaches 4.
    unsafe {
        let idt = AP_IDT.get_mut();
        idt[IPI_VECTOR].set_handler_fn(ap_ipi);
        idt.general_protection_fault.set_handler_fn(ap_fault);
        idt.page_fault.set_handler_fn(ap_fault_pf);
        idt.invalid_opcode.set_handler_fn(ap_fault_noerr);
    }

    // Stage the trampoline at TRAMP and make that page present+writable+executable.
    make_tramp_page_executable();
    let blob_len = tramp_off!(ap_tramp_end);
    // SAFETY: copying the assembled blob into the (now writable) identity-mapped trampoline page;
    // TRAMP..TRAMP+blob_len is below 0x9000 and owned by nothing else after ExitBootServices.
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(ap_tramp_start),
            TRAMP as *mut u8,
            blob_len,
        );
        // The APs clone the BSP's exact paging personality: same CR3/CR4, EFER minus the
        // CPU-owned LMA status bit (LME stays set; the trampoline's CR0 write turns it into LMA).
        let cr3 = Cr3::read().0.start_address().as_u64();
        let cr4 = Cr4::read_raw();
        let efer = Msr::new(IA32_EFER).read() & !(1 << 10);
        let cr0 = Cr0::read_raw();
        core::ptr::write_volatile((TRAMP + tramp_off!(ap_tramp_cr3)) as *mut u64, cr3);
        core::ptr::write_volatile((TRAMP + tramp_off!(ap_tramp_cr4)) as *mut u32, cr4 as u32);
        core::ptr::write_volatile((TRAMP + tramp_off!(ap_tramp_efer)) as *mut u32, efer as u32);
        core::ptr::write_volatile((TRAMP + tramp_off!(ap_tramp_cr0)) as *mut u32, cr0 as u32);
        core::ptr::write_volatile(
            (TRAMP + tramp_off!(ap_tramp_entry)) as *mut u64,
            ap_entry as *const () as usize as u64,
        );
    }

    // 1 — wake every AP with INIT-SIPI-SIPI, strictly sequentially (each reads its stack + index
    // from the shared trampoline data block before signalling online).
    let mut online = 0usize;
    for (i, &apic_id) in apic_ids.iter().enumerate().take(planned) {
        let cpu = i + 1;
        let stack_top = {
            // SAFETY: address-of a static array element; no data is read or written here.
            let base = unsafe { core::ptr::addr_of!(SECONDARY_STACKS[cpu - 1]) as usize };
            ((base + STACK_BYTES) & !0xF) as u64
        };
        // SAFETY: the trampoline page is ours; the target AP is not running yet.
        unsafe {
            core::ptr::write_volatile((TRAMP + tramp_off!(ap_tramp_stack)) as *mut u64, stack_top)
        };
        CUR_IDX.store(cpu, Ordering::Release);

        lapic_send(apic_id, ICR_INIT);
        let _ = wait_until(pit::ticks() + 2, || false); // >= 10 ms INIT settle (PIT = 100 Hz)
        let sipi = ICR_SIPI | (TRAMP >> 12) as u32;
        lapic_send(apic_id, sipi);
        if !wait_until(pit::ticks() + 20, || {
            ONLINE_MASK.load(Ordering::SeqCst) & (1 << cpu) != 0
        }) {
            lapic_send(apic_id, sipi); // architected second SIPI for slow starters
        }
        if wait_until(deadline_after(5), || {
            ONLINE_MASK.load(Ordering::SeqCst) & (1 << cpu) != 0
        }) {
            online += 1;
        } else {
            break;
        }
    }
    check!(
        online >= 1,
        "smp: MADT-discovered APs started via INIT-SIPI-SIPI"
    );
    let secondaries = online;
    let all_mask: usize = ((1 << secondaries) - 1) << 1;

    // 2 — every woken AP is online in long mode over the SHARED kernel page tables.
    check!(
        ONLINE_MASK.load(Ordering::SeqCst) == all_mask && AP_FAULTS.load(Ordering::SeqCst) == 0,
        "smp: all APs online in long mode (shared CR3, no AP faults)"
    );
    kprintln!(
        "  [info  ] {} AP(s) online, mask {:#x}",
        secondaries,
        ONLINE_MASK.load(Ordering::SeqCst)
    );

    // 3 — per-CPU identity: each AP saw a distinct LAPIC ID and its own IA32_GS_BASE value.
    let mut identity_ok = true;
    for cpu in 1..=secondaries {
        identity_ok &= SEEN_GS[cpu].load(Ordering::SeqCst) == cpu as u64;
        let apic = SEEN_APIC[cpu].load(Ordering::SeqCst);
        identity_ok &= apic == apic_ids[cpu - 1] as u64 && apic != bsp as u64;
    }
    check!(
        identity_ok,
        "smp: per-CPU identity distinct (LAPIC ID + IA32_GS_BASE per core)"
    );

    // 4 — exact atomic accounting across all cores (the BSP participates too).
    PHASE.store(1, Ordering::SeqCst);
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    check!(
        wait_until(deadline_after(10), || DONE_COUNTER.load(Ordering::SeqCst)
            == secondaries),
        "smp: counter phase completes on every core"
    );
    check!(
        SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: cross-core atomic counter is exact (no lost increments)"
    );

    // 5 — release/acquire publication: every core observed the exact payload.
    PHASE.store(2, Ordering::SeqCst);
    MAILBOX_DATA.store(MAILBOX_PAYLOAD, Ordering::Relaxed);
    MAILBOX_FLAG.store(true, Ordering::Release);
    check!(
        wait_until(deadline_after(10), || DONE_MAILBOX.load(Ordering::SeqCst)
            == secondaries),
        "smp: mailbox phase completes on every core"
    );
    let mut mailbox_ok = true;
    for (cpu, resp) in MAILBOX_RESP
        .iter()
        .enumerate()
        .take(secondaries + 1)
        .skip(1)
    {
        mailbox_ok &= resp.load(Ordering::SeqCst) == MAILBOX_PAYLOAD ^ (0xA1E7 + cpu as u64);
    }
    check!(
        mailbox_ok,
        "smp: release/acquire mailbox observed exactly by every core"
    );

    // 6..8 — ADR-027 on real silicon-parallel cores: commits flow before revoke; the revoke
    // linearizes inside the engine lock; nothing commits after it, and every retry fails closed.
    {
        let mut guard = ENGINE.lock();
        let mut engine = CapEngine::new(0x5EED_2026, 1_000);
        let cap = engine.mint("smp-worker", "data.write", Scope::All, Constraints::none());
        *guard = Some(CapState { engine, cap });
    }
    PHASE.store(3, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(10), || {
            COMMITS.load(Ordering::SeqCst) >= secondaries * 4
        }),
        "smp: cross-core commits flow through with_authorization (progress gate)"
    );
    let commits_at_revoke;
    {
        let mut guard = ENGINE.lock();
        let state = guard.as_mut().expect("engine installed above");
        let cap = state.cap;
        state.engine.revoke(cap);
        REVOKED.store(true, Ordering::Release); // linearization marker, inside the lock hold
        commits_at_revoke = COMMITS.load(Ordering::SeqCst);
    }
    check!(
        wait_until(deadline_after(10), || DONE_CAPS.load(Ordering::SeqCst)
            == secondaries),
        "smp: capability phase completes on every core"
    );
    check!(
        COMMITS.load(Ordering::SeqCst) == commits_at_revoke,
        "smp: ZERO commits after revoke linearizes (atomic authorize+execute, ADR-027)"
    );
    check!(
        POST_ALLOWED.load(Ordering::SeqCst) == 0
            && POST_DENIED.load(Ordering::SeqCst) == secondaries * POST_ATTEMPTS,
        "smp: every post-revoke attempt on every core fails closed"
    );

    // 9 — inter-processor interrupt: fixed vector from the BSP, taken through each AP's own IDT,
    // acknowledged at its LAPIC. Each AP enables interrupts on its own schedule, so re-send until
    // the target acknowledges (progress-gated; covers both orders).
    PHASE.store(4, Ordering::SeqCst);
    for (cpu, seen) in IPI_SEEN.iter().enumerate().take(secondaries + 1).skip(1) {
        let deadline = deadline_after(5);
        loop {
            lapic_send(apic_ids[cpu - 1], ICR_FIXED | IPI_VECTOR as u32);
            if seen.load(Ordering::SeqCst) || pit::ticks() > deadline {
                break;
            }
            let _ = wait_until(pit::ticks() + 1, || seen.load(Ordering::SeqCst));
        }
    }
    let ipi_all = wait_until(deadline_after(5), || {
        (1..=secondaries).all(|cpu| IPI_SEEN[cpu].load(Ordering::SeqCst))
    });
    check!(
        ipi_all && AP_FAULTS.load(Ordering::SeqCst) == 0,
        "smp: fixed-vector LAPIC IPI delivered + acknowledged on every AP"
    );

    // 10 — the machine is still coherent: every AP parked, nothing regressed.
    PHASE.store(5, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(5), || PARKED.load(Ordering::SeqCst)
            == secondaries)
            && ONLINE_MASK.load(Ordering::SeqCst) == all_mask
            && SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: all APs parked; online mask + counters stable"
    );

    Ok(n)
}
