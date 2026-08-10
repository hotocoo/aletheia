//! 8254 PIT channel 0 as the periodic timer that drives IRQ0. Programmed to 100 Hz in mode 3
//! (square wave). The IRQ0 handler increments `TICKS`; the boot path spins until enough ticks
//! accumulate, which is the live proof that interrupts + the timer are actually firing (not merely
//! configured) after `ExitBootServices`.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

const CHANNEL0: u16 = 0x40;
const COMMAND: u16 = 0x43;
const PIT_BASE_HZ: u32 = 1_193_182;

/// Timer interrupt frequency.
pub const FREQ_HZ: u32 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let divisor = (PIT_BASE_HZ / FREQ_HZ) as u16;
    unsafe {
        // Channel 0, access lobyte/hibyte, mode 3 (square wave generator), binary.
        Port::<u8>::new(COMMAND).write(0x36u8);
        Port::<u8>::new(CHANNEL0).write((divisor & 0xFF) as u8);
        Port::<u8>::new(CHANNEL0).write((divisor >> 8) as u8);
    }
}

/// Restart channel 0's count from the top, so the caller gets a WHOLE period before the next IRQ0
/// rather than whatever remains of the current one.
///
/// The other two targets arm a one-shot timer per slice; the PIT free-runs, so a ring-3 task can be
/// resumed a microsecond before a tick that was already due and be preempted before executing a
/// single instruction of its own. Writing the control word halts and reloads the counter (8254
/// §Control Word), which is the PIT's equivalent of that arm. `TICKS` is untouched — this changes
/// the PHASE of the tick, never its rate, so every deadline computed from [`ticks`] stays valid.
pub fn rearm() {
    init();
}

/// Called from the IRQ0 handler.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
