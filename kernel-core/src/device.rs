//! Capability-authorized device access (REQ-DRV-002, ADR-023).
//!
//! The Aletheia principle — no ambient authority — extends to hardware: a client may touch a device
//! only by presenting a capability the SAME [`CapEngine`] authorizes, never because it happens to run
//! in the kernel. [`DeviceGuard`] wraps any [`BlockDevice`] and gates every read/write/flush on a
//! capability, so device I/O is authorized exactly like an entity write or an IPC send. Read and write
//! are separate authorities, so an attenuated read-only capability genuinely cannot write.
//!
//! This is the arch-independent authority layer, hosted-proved over the real
//! [`crate::storage::MemBlockDevice`] — deny/allow decides actual bytes, not an empty registry. The
//! full device architecture (discovery, a real hardware driver, hotplug, DMA/IOMMU, restart) is
//! REQ-DRV-001 / ADR-023; the concrete virtio-blk driver — which will implement the very same
//! [`BlockDevice`] trait this guard already wraps — is the named next slice, deferred (ADR-010).
use alloc::string::{String, ToString};

use crate::spine::{CapEngine, CapToken, Decision, Target};
use crate::storage::{BlockDevice, StorageError};

/// Why a guarded device operation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
    /// The capability check did not return `Allow` — the operation never reached the device.
    Denied,
    /// The underlying device rejected an authorized operation.
    Io(StorageError),
}

/// A [`BlockDevice`] whose every access is capability-gated. `read_action` authorizes reads;
/// `write_action` authorizes writes and flush. A client presents its offered capabilities on each
/// call; without a matching live capability the call is `Denied` and NO I/O occurs.
pub struct DeviceGuard<D: BlockDevice> {
    device: D,
    read_action: String,
    write_action: String,
}

impl<D: BlockDevice> DeviceGuard<D> {
    /// Wrap `device`, gating reads behind `read_action` and writes/flush behind `write_action`.
    pub fn new(device: D, read_action: &str, write_action: &str) -> Self {
        DeviceGuard {
            device,
            read_action: read_action.to_string(),
            write_action: write_action.to_string(),
        }
    }

    fn authorized(engine: &CapEngine, action: &str, offered: &[CapToken]) -> bool {
        engine.evaluate(action, &Target::default(), offered) == Decision::Allow
    }

    /// Capability-gated read. Fail-closed: without `read_action` authority the device is not touched.
    pub fn read_block(
        &self,
        engine: &CapEngine,
        offered: &[CapToken],
        idx: usize,
        buf: &mut [u8],
    ) -> Result<(), DeviceError> {
        if !Self::authorized(engine, &self.read_action, offered) {
            return Err(DeviceError::Denied);
        }
        self.device.read_block(idx, buf).map_err(DeviceError::Io)
    }

    /// Capability-gated write. Fail-closed: without `write_action` authority NO bytes reach the device
    /// (an attenuated read-only client cannot mutate storage).
    pub fn write_block(
        &mut self,
        engine: &CapEngine,
        offered: &[CapToken],
        idx: usize,
        buf: &[u8],
    ) -> Result<(), DeviceError> {
        if !Self::authorized(engine, &self.write_action, offered) {
            return Err(DeviceError::Denied);
        }
        self.device.write_block(idx, buf).map_err(DeviceError::Io)
    }

    /// Capability-gated durability barrier (write authority).
    pub fn flush(&mut self, engine: &CapEngine, offered: &[CapToken]) -> Result<(), DeviceError> {
        if !Self::authorized(engine, &self.write_action, offered) {
            return Err(DeviceError::Denied);
        }
        self.device.flush().map_err(DeviceError::Io)
    }

    /// Number of blocks (device geometry is not sensitive; not gated).
    pub fn num_blocks(&self) -> usize {
        self.device.num_blocks()
    }
}
