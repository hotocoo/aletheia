//! A scripted fault-injecting block device — durability claims tested against an adversary
//! (ALET-P2-008, REQ-STOR-00x coverage, ADR-062).
//!
//! Every "the transaction is all-or-nothing across a crash" claim in this crate was, until now,
//! proved against devices that FAIL CLEANLY ON DEMAND only in the specific shapes the proofs
//! asked for. This module is the general adversary: a wrapper that plays a SCRIPTED PROGRAM of
//! operations — each underlying read/write/flush either allowed or REFUSED BY THE DEVICE — so a
//! proof can place a fault at ANY position in a multi-step protocol and hold the protocol to its
//! contract: an error is SURFACED, never swallowed, and afterwards the durable state is exactly
//! one of the outcomes the protocol promises (never a mixture).
//!
//! ## Refusal semantics, stated once
//!
//! * A refused WRITE mutates NOTHING (the device said no before touching the medium).
//! * A refused READ leaves the caller's buffer UNWRITTEN and returns `Err(Device)`.
//! * A refused FLUSH reports the durability barrier FAILED — the caller cannot assume anything
//!   reached stable storage, which is precisely what a caller must cope with to be correct.
//! * When the script runs out, every operation passes through untouched.
//!
//! The wrapper implements [`BlockDevice`] for any wrapped device, so it composes with the real
//! suites, the journal, and the filesystem without those layers knowing an adversary is present.

use crate::storage::{BlockDevice, StorageError};

/// One scripted operation. The device pops these in order; running out of script means pass-through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Allow the next read.
    ReadOk,
    /// Refuse the next read with `Err(Device)`.
    ReadFail,
    /// Allow the next write.
    WriteOk,
    /// Refuse the next write — the medium is NOT touched.
    WriteFail,
    /// Allow the next flush (durability barrier holds).
    FlushOk,
    /// Fail the next flush — the durability barrier did NOT hold.
    FlushFail,
}

/// A block device that follows a fault script until the script runs out. The script sits in a
/// RefCell because the BlockDevice trait takes `&self` for reads — and a device that refuses
/// reads must advance its script through that shared borrow. Single-threaded use only; the kernel
/// calls this from one context at a time.
pub struct FaultInject<D: BlockDevice> {
    inner: D,
    script: core::cell::RefCell<alloc::vec::Vec<Op>>,
}

impl<D: BlockDevice> FaultInject<D> {
    /// Wrap `inner` with the given script.
    pub fn new(inner: D, script: alloc::vec::Vec<Op>) -> Self {
        FaultInject {
            inner,
            script: core::cell::RefCell::new(script),
        }
    }

    /// How much of the script remains unplayed (a proof uses this to confirm the adversary
    /// actually fired where intended — a script that never ran proves nothing).
    pub fn remaining(&self) -> usize {
        self.script.borrow().len()
    }

    /// The adversary disarms: hand the wrapped device back once the proof is done with it.
    pub fn into_inner(self) -> D {
        self.inner
    }

    fn next_op(&self, allow: Op, refuse: Op) -> Result<(), StorageError> {
        let mut script = self.script.borrow_mut();
        let op = if script.is_empty() {
            allow
        } else {
            script.remove(0)
        };
        drop(script);
        if op == refuse {
            Err(StorageError::Device)
        } else {
            Ok(())
        }
    }
}

impl<D: BlockDevice> BlockDevice for FaultInject<D> {
    fn num_blocks(&self) -> usize {
        self.inner.num_blocks()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.next_op(Op::ReadOk, Op::ReadFail)?;
        // Only AFTER the read is allowed do the bytes move — a refused read writes no garbage.
        self.inner.read_block(idx, buf)
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        self.next_op(Op::WriteOk, Op::WriteFail)?;
        self.inner.write_block(idx, buf)
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        self.next_op(Op::FlushOk, Op::FlushFail)
    }
}
