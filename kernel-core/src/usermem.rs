//! Checked user-address ranges shared by syscall services.
//!
//! This module validates arithmetic and policy bounds before any target copies bytes. It does not
//! dereference pointers: each architecture must prove a validated range is mapped in its own page
//! table, then perform the copy through its fault-safe mechanism.

/// Maximum bytes one syscall may copy in one direction.
pub const MAX_USER_COPY: usize = 64 * 1024;

/// Why a user buffer was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserMemoryError {
    /// Non-empty buffer starts at null.
    Null,
    /// Address plus length wrapped.
    Overflow,
    /// Length exceeds the syscall copy budget.
    TooLarge,
    /// Range is outside the target's declared user VA window.
    Outside,
}

/// A validated, exclusive-end user VA range. Construction is the only place generic arithmetic is
/// performed; callers still need target page-table/mapping validation before dereferencing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserSlice {
    addr: u64,
    len: usize,
}

impl UserSlice {
    /// Validate `[addr, addr + len)` against `[user_start, user_end)`.
    pub fn validate(
        addr: u64,
        len: usize,
        user_start: u64,
        user_end: u64,
    ) -> Result<Self, UserMemoryError> {
        if len > MAX_USER_COPY {
            return Err(UserMemoryError::TooLarge);
        }
        if len != 0 && addr == 0 {
            return Err(UserMemoryError::Null);
        }
        if user_start >= user_end {
            return Err(UserMemoryError::Outside);
        }
        let end = addr
            .checked_add(len as u64)
            .ok_or(UserMemoryError::Overflow)?;
        if (len != 0 && addr < user_start) || end > user_end || (len == 0 && addr > user_end) {
            return Err(UserMemoryError::Outside);
        }
        Ok(UserSlice { addr, len })
    }

    /// Start virtual address.
    pub const fn addr(self) -> u64 {
        self.addr
    }

    /// Byte length.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Exclusive end virtual address.
    pub fn end(self) -> u64 {
        self.addr + self.len as u64
    }

    /// Empty-range predicate.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LO: u64 = 0x1000;
    const HI: u64 = 0x10_0000;

    #[test]
    fn accepts_in_window_and_exposes_exact_end() {
        let slice = UserSlice::validate(0x2000, 0x300, LO, HI).unwrap();
        assert_eq!(slice.addr(), 0x2000);
        assert_eq!(slice.len(), 0x300);
        assert_eq!(slice.end(), 0x2300);
    }

    #[test]
    fn rejects_null_underflow_overflow_and_outside_ranges() {
        assert_eq!(
            UserSlice::validate(0, 1, LO, HI),
            Err(UserMemoryError::Null)
        );
        assert_eq!(
            UserSlice::validate(LO - 1, 2, LO, HI),
            Err(UserMemoryError::Outside)
        );
        assert_eq!(
            UserSlice::validate(u64::MAX - 1, 4, LO, u64::MAX),
            Err(UserMemoryError::Overflow)
        );
        assert_eq!(
            UserSlice::validate(HI - 1, 2, LO, HI),
            Err(UserMemoryError::Outside)
        );
    }

    #[test]
    fn enforces_copy_budget_and_allows_empty_at_window_edges() {
        assert_eq!(
            UserSlice::validate(LO, MAX_USER_COPY + 1, LO, HI),
            Err(UserMemoryError::TooLarge)
        );
        assert!(UserSlice::validate(LO, 0, LO, HI).unwrap().is_empty());
        assert!(UserSlice::validate(HI, 0, LO, HI).unwrap().is_empty());
    }
}
