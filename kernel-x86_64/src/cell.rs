//! `Racy<T>` — a `Sync` interior-mutability cell for kernel statics.
//!
//! The kernel boots single-core with no preemption during init, so the GDT/IDT/heap statics are
//! written exactly once (or with disciplined single-threaded access) before interrupts are on.
//! `static mut` would trip the `static_mut_refs` lint and force scattered `unsafe`; `Racy` centralizes
//! the invariant: **the caller guarantees no concurrent/aliasing access.** It is NOT a general-purpose
//! synchronization primitive — a real SMP kernel replaces this with a proper lock (deferred phase).

use core::cell::UnsafeCell;

pub struct Racy<T>(UnsafeCell<T>);

// SAFETY: single-core, init-once discipline (see module docs). No two mutable borrows are live at
// once, and no borrow crosses a point where another core or an interrupt handler touches the cell.
unsafe impl<T> Sync for Racy<T> {}

impl<T> Racy<T> {
    pub const fn new(value: T) -> Self {
        Racy(UnsafeCell::new(value))
    }

    /// Shared reference. Borrowing a `static Racy` yields a `&'static T`.
    ///
    /// # Safety
    /// No mutable borrow of this cell may be live for the duration of the returned reference.
    #[allow(clippy::should_implement_trait)]
    pub unsafe fn get(&self) -> &T {
        &*self.0.get()
    }

    /// Exclusive reference for one-time initialization.
    ///
    /// # Safety
    /// No other borrow (shared or exclusive) of this cell may be live concurrently.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.0.get()
    }
}
