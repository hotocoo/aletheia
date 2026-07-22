//! Zero-copy shared-memory grant-table (gap-register Issue 2 / REQ-IPC-008, ADR-020).
//!
//! The synchronous IPC fast-path ([`crate::ipc::Channel`]) *copies* a message body into the receiver's
//! inbox. That is correct for control messages but wrong for bulk data: a service handing a page of
//! bytes to another must not pay a copy per transfer. A real microkernel solves this with **shared
//! memory under explicit authority** — one physical frame region mapped into two address spaces, the
//! sharing itself gated by a capability so no endpoint can reach memory it was not granted.
//!
//! This module is the **arch-independent** half of that mechanism, so all three CPU targets inherit it
//! from one source (ADR-019). It owns the *authority + lifecycle* of a share:
//!
//! * **Capability-gated establishment.** Creating a grant requires `memory.share` authority, checked
//!   through the SAME [`CapEngine`] the deterministic pipeline uses. No capability ⇒ no grant
//!   (fail-closed) — an endpoint cannot conjure a shared region out of nothing.
//! * **Attenuation, never amplification.** A grant can only *narrow* the grantor's own access: a
//!   holder of a read-only region can never mint a read-write grant. This mirrors
//!   [`CapEngine::delegate`]'s attenuation, applied to memory access instead of an action string.
//! * **Zero-copy backing.** The region's bytes live exactly **once** (`Rc<RefCell<[u8]>>`); every grant
//!   references the SAME store, so a write by a read-write holder is observed by every reader with no
//!   copy through any queue. [`GrantTable::region_refcount`] makes the sharing observable in tests.
//! * **Revocation unmaps.** Revoking a grant drops that endpoint's handle to the region (fail-closed:
//!   subsequent reads/writes through the grant are denied) and releases its share of the backing —
//!   the arch-independent analogue of tearing the page-table entry down.
//! * **Bounded access.** Every read/write is confined to `[0, len)` of the region — the model of the
//!   MMU refusing an access past the shared frame; an out-of-range access is refused, never wraps.
//!
//! What stays a **per-target seam** (ADR-010: no blind hardware code here): turning a granted region
//! into a real page-table mapping in each endpoint's address space is the job of each target's
//! `vm.rs` (aarch64/x86-64/RISC-V map/unmap is already delivered). The `Rc<RefCell<[u8]>>` here is the
//! hosted MODEL of one physical frame shared by mapping — exactly the way [`crate::sched`] owns the
//! scheduling *policy* while each target's assembly owns the actual context switch. The grant-table is
//! the authority/lifecycle layer that sits above the target mapping and is the same on every CPU.
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::spine::{CapEngine, CapToken, Decision, Target};

/// The single backing store of a shared region. Cloning the [`Rc`] shares the SAME bytes (zero-copy);
/// dropping the last clone frees them. `RefCell` gives interior mutability without a lock — the kernel
/// model is single-core (SMP shared memory is REQ-SMP-001, tracked separately).
type Backing = Rc<RefCell<Vec<u8>>>;

/// Access mode of a shared-memory grant. `ReadWrite` strictly dominates `Read`: a grant can only
/// **attenuate** the grantor's own access, never amplify it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareMode {
    /// The grantee may read the region but not modify it.
    Read,
    /// The grantee may read and write the region.
    ReadWrite,
}

impl ShareMode {
    fn allows_write(self) -> bool {
        matches!(self, ShareMode::ReadWrite)
    }

    /// True iff a grantor holding `self` may mint a grant of `requested` mode without amplifying.
    /// `ReadWrite` covers both; `Read` covers only `Read`.
    fn covers(self, requested: ShareMode) -> bool {
        matches!(
            (self, requested),
            (ShareMode::ReadWrite, _) | (ShareMode::Read, ShareMode::Read)
        )
    }
}

/// Why a grant-table operation was refused. Every failure is fail-closed: nothing is shared, mapped,
/// read, or written when one of these is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// The `memory.share` capability check did not return `Allow` — no grant is minted.
    Unauthorized,
    /// The requested mode exceeds the grantor's own access on the region (would amplify).
    Amplify,
    /// The grantor holds no live access to the region it is trying to share.
    NoAccess,
    /// The grant has been revoked (its mapping was torn down).
    Revoked,
    /// The access falls outside `[0, len)` of the region.
    OutOfBounds,
    /// A write was attempted through a read-only grant.
    ReadOnly,
    /// No region/grant with that id exists.
    NotFound,
}

struct RegionRecord {
    id: u64,
    #[allow(dead_code)] // physical frame base: model fidelity (what a target's vm.rs would map)
    base: u64,
    bytes: Backing,
    owner: String,
}

struct GrantRecord {
    id: u64,
    region: u64,
    #[allow(dead_code)] // retained for audit fidelity (who shared it); not read on the access path
    grantor: String,
    grantee: String,
    mode: ShareMode,
    /// The endpoint's mapped handle to the region. `Some` while mapped; `None` once revoked
    /// (revocation drops this clone, releasing the endpoint's share of the backing).
    bytes: Option<Backing>,
}

/// The capability-authorized registry of shared-memory regions and the grants that map them into
/// endpoints. Establishing a grant is gated by `share_action` through the [`CapEngine`]; every access
/// is confined to the granted region and mode, and revocation unmaps fail-closed.
pub struct GrantTable {
    share_action: String,
    regions: Vec<RegionRecord>,
    grants: Vec<GrantRecord>,
    next_region: u64,
    next_grant: u64,
}

impl GrantTable {
    /// A grant-table whose sharing is gated by the capability `share_action` (e.g. `"memory.share"`).
    pub fn new(share_action: &str) -> Self {
        GrantTable {
            share_action: share_action.to_string(),
            regions: Vec::new(),
            grants: Vec::new(),
            next_region: 1,
            next_grant: 1,
        }
    }

    /// Establish a fresh shared region of `len` zero bytes owned by `owner`, at physical frame `base`
    /// (the address a target's `vm.rs` would map). The owner implicitly holds read-write access; it
    /// shares attenuated grants of this region to other endpoints via [`GrantTable::share`]. Returns
    /// the region id.
    pub fn create_region(&mut self, owner: &str, base: u64, len: usize) -> u64 {
        let id = self.next_region;
        self.next_region += 1;
        self.regions.push(RegionRecord {
            id,
            base,
            bytes: Rc::new(RefCell::new(vec![0u8; len])),
            owner: owner.to_string(),
        });
        id
    }

    fn region(&self, region: u64) -> Option<&RegionRecord> {
        self.regions.iter().find(|r| r.id == region)
    }

    /// The grantor's live access to a region: read-write if it owns the region, otherwise the widest
    /// mode of a still-mapped grant it holds on that region. `None` if it has no live access.
    fn effective_mode(&self, region: u64, principal: &str) -> Option<ShareMode> {
        let rec = self.region(region)?;
        if rec.owner == principal {
            return Some(ShareMode::ReadWrite);
        }
        self.grants
            .iter()
            .filter(|g| g.region == region && g.grantee == principal && g.bytes.is_some())
            .map(|g| g.mode)
            .max_by_key(|m| m.allows_write() as u8)
    }

    /// Share `region` from `grantor` to `grantee` at `mode`, authorized by `memory.share`.
    ///
    /// Fail-closed and all-or-nothing: if the `memory.share` capability check is not `Allow`, the
    /// grantor holds no live access to the region, or `mode` would amplify the grantor's own access,
    /// NOTHING is mapped and no grant id is minted. On success the grantee receives a mapped handle to
    /// the SAME backing store (zero-copy) and the grant id is returned.
    pub fn share(
        &mut self,
        engine: &CapEngine,
        region: u64,
        grantor: &str,
        grantee: &str,
        mode: ShareMode,
        offered: &[CapToken],
    ) -> Result<u64, GrantError> {
        // 1. Authority to share at all (fail-closed).
        if engine.evaluate(&self.share_action, &Target::default(), offered) != Decision::Allow {
            return Err(GrantError::Unauthorized);
        }
        // 2. The grantor must actually hold live access to the region…
        let grantor_mode = self
            .effective_mode(region, grantor)
            .ok_or(GrantError::NoAccess)?;
        // 3. …and may only attenuate it, never amplify.
        if !grantor_mode.covers(mode) {
            return Err(GrantError::Amplify);
        }
        let backing = self
            .region(region)
            .ok_or(GrantError::NotFound)?
            .bytes
            .clone();
        let id = self.next_grant;
        self.next_grant += 1;
        self.grants.push(GrantRecord {
            id,
            region,
            grantor: grantor.to_string(),
            grantee: grantee.to_string(),
            mode,
            bytes: Some(backing),
        });
        Ok(id)
    }

    fn grant(&self, grant: u64) -> Option<&GrantRecord> {
        self.grants.iter().find(|g| g.id == grant)
    }

    /// Read `len` bytes at `offset` through a grant. Denied fail-closed if the grant is unknown or
    /// revoked, or if `[offset, offset+len)` leaves the region. A `Read` grant may read.
    pub fn read(&self, grant: u64, offset: usize, len: usize) -> Result<Vec<u8>, GrantError> {
        let g = self.grant(grant).ok_or(GrantError::NotFound)?;
        let backing = g.bytes.as_ref().ok_or(GrantError::Revoked)?;
        let end = offset.checked_add(len).ok_or(GrantError::OutOfBounds)?;
        let store = backing.borrow();
        if end > store.len() {
            return Err(GrantError::OutOfBounds);
        }
        Ok(store[offset..end].to_vec())
    }

    /// Write `data` at `offset` through a grant. Requires `ReadWrite` mode (a `Read` grant is refused
    /// [`GrantError::ReadOnly`]); denied fail-closed if unknown/revoked or out of `[0, len)`. Because
    /// the backing is shared, the bytes are immediately visible to every other live grant on the
    /// region with no copy — the zero-copy property.
    pub fn write(&mut self, grant: u64, offset: usize, data: &[u8]) -> Result<(), GrantError> {
        let g = self.grant(grant).ok_or(GrantError::NotFound)?;
        if !g.mode.allows_write() {
            return Err(GrantError::ReadOnly);
        }
        let backing = g.bytes.as_ref().ok_or(GrantError::Revoked)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(GrantError::OutOfBounds)?;
        let mut store = backing.borrow_mut();
        if end > store.len() {
            return Err(GrantError::OutOfBounds);
        }
        store[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Revoke a grant: tear down the endpoint's mapping. The grant's handle to the backing is dropped
    /// (releasing its share of the region), and every later read/write through this grant id is denied
    /// [`GrantError::Revoked`]. Returns true if the grant existed and was live; false otherwise.
    pub fn revoke(&mut self, grant: u64) -> bool {
        if let Some(g) = self.grants.iter_mut().find(|g| g.id == grant) {
            if g.bytes.take().is_some() {
                return true;
            }
        }
        false
    }

    /// Number of live handles to a region's backing store — the region itself plus every still-mapped
    /// grant. A test observing this rise on [`GrantTable::share`] and fall on [`GrantTable::revoke`]
    /// proves the sharing is genuine (zero-copy) and that revocation actually releases the mapping.
    pub fn region_refcount(&self, region: u64) -> usize {
        match self.region(region) {
            Some(r) => Rc::strong_count(&r.bytes),
            None => 0,
        }
    }

    /// The mode a live grant confers, or `None` if unknown or revoked.
    pub fn grant_mode(&self, grant: u64) -> Option<ShareMode> {
        self.grant(grant)
            .and_then(|g| g.bytes.as_ref().map(|_| g.mode))
    }
}
