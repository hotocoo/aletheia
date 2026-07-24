//! Cross-core synchronization primitives — arch-independent (gap-register Issue 1: defined once,
//! shared by every target; REQ-SMP-002).
//!
//! [`SpinLock`] is the kernel's mutual-exclusion primitive for real SMP: a test-and-set lock whose
//! Acquire/Release ordering makes the protected data's writes visible to the next holder. It is
//! what turns ADR-027's `with_authorization` contract ("under the engine's lock no revoke can
//! linearize between the authorization and the effect") from a borrow-checker argument into a
//! hardware guarantee once multiple cores exist. `no_std` + free of any OS dependency, so the
//! SAME type is used by the bare-metal targets and provable under host threads in `tests/sync.rs`.
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Test-and-set spinlock. `lock()` spins until it wins the flag with Acquire; dropping the guard
/// releases with Release — so everything written under the lock happens-before the next holder's
/// reads. No poisoning (a kernel panic halts the machine; there is no unwinding to poison across).
pub struct SpinLock<T> {
    locked: AtomicBool,
    cell: UnsafeCell<T>,
}

// SAFETY: the lock serializes all access to `cell`; T only needs Send to cross cores/threads.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            cell: UnsafeCell::new(value),
        }
    }

    /// Spin until the lock is acquired; the guard releases it on drop.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

/// RAII guard proving exclusive ownership of the locked value.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: guard existence proves exclusive ownership of the cell.
        unsafe { &*self.lock.cell.get() }
    }
}
impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: guard existence proves exclusive ownership of the cell.
        unsafe { &mut *self.lock.cell.get() }
    }
}
impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
