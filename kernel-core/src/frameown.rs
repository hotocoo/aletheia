//! Physical frame ownership — the arch-independent answer to "who owns this frame?" that every
//! target's allocator consults before it hands a frame out or takes one back (GAPS4 ALET-P1-003,
//! REQ-MM-002, ADR-030).
//!
//! # Why an allocator's bounds check is not an ownership model
//!
//! All three Aletheia targets run the same intrusive LIFO free-list: a free frame stores the next
//! free frame's physical address in its own first word. That design is fast and needs no side
//! table, but it knows only two things about an address handed to `free`: is it aligned, and is it
//! inside the managed window. Both are true of a frame that is *already on the free list*. So:
//!
//! * **Double free.** `free(f)` twice pushes `f` onto the list twice. The list now contains a
//!   cycle through `f`, and two later `alloc` calls return the SAME frame to two different owners —
//!   a page table and a user page over one another. Nothing faults; the address space quietly
//!   aliases.
//! * **Free of a frame that was never allocated.** Any aligned in-window address is accepted, so a
//!   caller can "free" a frame that is currently live in another address space, handing an
//!   in-use page to the next allocation.
//! * **Use after free.** A frame handle is `Copy`. Freeing one copy leaves every other copy looking
//!   like a valid handle, and the allocator cannot tell the difference.
//!
//! [`AddrPlan`](crate::vmaddr::AddrPlan) (ALET-P1-001) closed the *address admission* half of this:
//! a `pa` outside the allocator's window is refused. It deliberately did not answer ownership —
//! being inside the window says the kernel owns the memory, not that THIS caller may use or return
//! this frame. That is what this module adds, and the two compose: `AddrPlan` says the address is
//! one of ours, [`FrameOwnerTable`] says who currently holds it.
//!
//! # The model
//!
//! One byte of state per frame in the window: `0` = free, `1..=254` = an owner tag, `255` =
//! permanently reserved (firmware/MMIO memory the pool must never hand out). State transitions are
//! the whole model, and each illegal one is a distinct named refusal:
//!
//! ```text
//!            claim(owner)                    release(owner)
//!   FREE ────────────────────► OWNED(owner) ────────────────────► FREE
//!    │                             │  ▲                              │
//!    │ release  -> NotOwned        │  └──── transfer(from,to) ───────┘
//!    │ (double free)               │
//!    └── claim on OWNED            └── release/transfer by a non-owner -> WrongOwner
//!        -> AlreadyOwned
//! ```
//!
//! `transfer` exists because frame ownership legitimately moves — a kernel-allocated page becomes a
//! user page when it is mapped into an address space — and doing that as `release` + `claim` would
//! leave the frame momentarily free, i.e. allocatable by someone else. It is one atomic step here
//! for the same reason `CapEngine` authorizes-and-executes atomically (REQ-CAP-006).
//!
//! # What this module is (and is not)
//!
//! Pure arithmetic and a caller-supplied `&mut [u8]`: no allocation, no architecture registers, no
//! `unsafe`. Each target owns a `static` state array sized for its own RAM window and threads it
//! through its `frames.rs`, so the rules are written once and proved once on the host
//! (`kernel-core/tests/frameown.rs`), with each target's VM gate then proving its own allocator is
//! actually wired to them.
//!
//! It is NOT page-table reclamation (ALET-P1-002) and NOT address-space teardown (ALET-P1-004);
//! both are separate findings that will be built ON this model — they are the two operations that
//! free frames in bulk, and neither is safe without an owner to check. Those rows stay `open` in
//! `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md` rather than being implied by this one.

use crate::vmaddr::PAGE_SIZE;

/// State byte meaning "no owner — this frame is on the free list".
const FREE: u8 = 0;
/// State byte meaning "permanently withheld from the pool".
const RESERVED_TAG: u8 = 255;

/// Who holds a frame. A tag, not a pointer: the kernel's own structures, a page table, and each
/// user address space get distinct tags, so a release by the wrong holder is refusable.
///
/// Tag `0` is not constructible — it is the free state, and "owned by nobody" must not be
/// expressible as an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner(u8);

impl Owner {
    /// Kernel-internal memory (structures the kernel itself holds).
    pub const KERNEL: Owner = Owner(1);
    /// A frame in use as a page table / intermediate translation table.
    pub const PAGETABLE: Owner = Owner(2);
    /// A page mapped into a user address space that has no distinct tag of its own.
    pub const USER: Owner = Owner(3);
    /// Memory permanently withheld from the pool (firmware tables, MMIO). Never allocatable.
    pub const RESERVED: Owner = Owner(RESERVED_TAG);

    /// First tag available for per-address-space identities, so a teardown can free exactly the
    /// frames belonging to one address space (ALET-P1-004 builds on this).
    pub const FIRST_ADDRESS_SPACE: u8 = 4;

    /// Build an owner from a raw tag. `0` is the free state and `None` is returned for it, so an
    /// owner value can never mean "unowned".
    pub const fn new(tag: u8) -> Option<Owner> {
        if tag == FREE {
            None
        } else {
            Some(Owner(tag))
        }
    }

    /// Tag for address space `id`, mapped into `FIRST_ADDRESS_SPACE..=254`. `None` when `id` is
    /// beyond the number of distinguishable address spaces (fail-closed: a teardown that cannot
    /// name its own frames must not silently share a tag with another address space).
    pub const fn address_space(id: u32) -> Option<Owner> {
        let first = Owner::FIRST_ADDRESS_SPACE as u32;
        let last = (RESERVED_TAG - 1) as u32; // 254; 255 is RESERVED
        if id > last - first {
            None
        } else {
            Some(Owner((first + id) as u8))
        }
    }

    /// Raw tag byte.
    pub const fn tag(self) -> u8 {
        self.0
    }

    /// Is this the permanently-withheld tag?
    pub const fn is_reserved(self) -> bool {
        self.0 == RESERVED_TAG
    }
}

/// Why an ownership transition was refused. Each variant names a distinct corruption prevented, so
/// a target reports which rule it broke rather than a bare `false` — and the same words appear in
/// every target's invariant log, which is what `scripts/conformance.sh` compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFault {
    /// The address is not a 4 KiB frame base, so it names no single frame.
    Unaligned,
    /// The address is outside the window this table covers — not a frame the pool owns.
    OutOfWindow,
    /// Claiming a frame that is already owned: it is live somewhere else, and handing it out again
    /// would alias two owners onto one page.
    AlreadyOwned,
    /// Releasing (or transferring) a frame that is currently free — the double-free that would
    /// push the same frame onto the free list twice and later hand it to two callers.
    NotOwned,
    /// Releasing or transferring a frame held by a DIFFERENT owner: the classic
    /// free-someone-else's-memory, and the use-after-free that follows it.
    WrongOwner,
    /// Claiming a frame that is permanently withheld (firmware/MMIO).
    Reserved,
    /// The state slice is too small for the declared window; the table would leave the tail of the
    /// pool unprotected, so construction fails instead.
    CapacityExceeded,
}

impl FrameFault {
    /// Stable short name, for invariant logs on targets that have no formatter.
    pub const fn as_str(self) -> &'static str {
        match self {
            FrameFault::Unaligned => "frame-unaligned",
            FrameFault::OutOfWindow => "frame-out-of-window",
            FrameFault::AlreadyOwned => "frame-already-owned",
            FrameFault::NotOwned => "frame-not-owned",
            FrameFault::WrongOwner => "frame-wrong-owner",
            FrameFault::Reserved => "frame-reserved",
            FrameFault::CapacityExceeded => "frame-capacity-exceeded",
        }
    }
}

/// Ownership state for every frame in one physical window `[base, base + frames * PAGE_SIZE)`.
///
/// The backing store is caller-supplied (`&mut [u8]`, one byte per frame) so this works in a
/// `no_std` kernel with no heap: each target declares a `static` array sized for its own RAM.
pub struct FrameOwnerTable<'a> {
    base: usize,
    frames: usize,
    state: &'a mut [u8],
    owned: usize,
    reserved: usize,
}

impl<'a> FrameOwnerTable<'a> {
    /// Cover `frames` frames starting at `base`, all initially free.
    ///
    /// `base` must be frame-aligned and `state` must have at least one byte per frame; otherwise
    /// the tail of the window would be unprotected, so construction is refused rather than
    /// silently covering a prefix. The state bytes are reset here, so a stale `static` from a
    /// previous boot stage cannot be read as live ownership.
    pub fn new(base: usize, frames: usize, state: &'a mut [u8]) -> Result<Self, FrameFault> {
        if !base.is_multiple_of(PAGE_SIZE) {
            return Err(FrameFault::Unaligned);
        }
        if base.checked_add(frames.saturating_mul(PAGE_SIZE)).is_none() {
            return Err(FrameFault::OutOfWindow);
        }
        if state.len() < frames {
            return Err(FrameFault::CapacityExceeded);
        }
        state[..frames].fill(FREE);
        Ok(Self {
            base,
            frames,
            state,
            owned: 0,
            reserved: 0,
        })
    }

    /// First physical address covered.
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Number of frames covered.
    pub const fn frames(&self) -> usize {
        self.frames
    }

    /// Index of the frame containing `pa`, or the reason `pa` names no frame of ours.
    pub fn index_of(&self, pa: usize) -> Result<usize, FrameFault> {
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(FrameFault::Unaligned);
        }
        if pa < self.base {
            return Err(FrameFault::OutOfWindow);
        }
        let idx = (pa - self.base) / PAGE_SIZE;
        if idx >= self.frames {
            return Err(FrameFault::OutOfWindow);
        }
        Ok(idx)
    }

    /// Current owner of `pa`, or `None` when the frame is free. `Err` when `pa` is not a frame in
    /// this window at all — "not ours" is deliberately distinct from "ours and free".
    pub fn owner_of(&self, pa: usize) -> Result<Option<Owner>, FrameFault> {
        let idx = self.index_of(pa)?;
        Ok(Owner::new(self.state[idx]))
    }

    /// Is `pa` a frame of ours that is currently owned by someone?
    pub fn is_owned(&self, pa: usize) -> bool {
        matches!(self.owner_of(pa), Ok(Some(_)))
    }

    /// Take ownership of a free frame. Refuses a frame that is already owned (the alias) or
    /// permanently reserved.
    pub fn claim(&mut self, pa: usize, owner: Owner) -> Result<(), FrameFault> {
        let idx = self.index_of(pa)?;
        match self.state[idx] {
            FREE => {
                self.state[idx] = owner.tag();
                if owner.is_reserved() {
                    self.reserved += 1;
                } else {
                    self.owned += 1;
                }
                Ok(())
            }
            RESERVED_TAG => Err(FrameFault::Reserved),
            _ => Err(FrameFault::AlreadyOwned),
        }
    }

    /// Withhold a frame from the pool permanently (firmware tables, MMIO, the running image).
    /// Refuses a frame that is currently owned — reserving live memory would strand it.
    pub fn reserve(&mut self, pa: usize) -> Result<(), FrameFault> {
        self.claim(pa, Owner::RESERVED)
    }

    /// Give a frame back. The caller must be the CURRENT owner: releasing a free frame is the
    /// double free, and releasing someone else's frame is the cross-owner free — both refused,
    /// with different names so the log says which happened.
    pub fn release(&mut self, pa: usize, owner: Owner) -> Result<(), FrameFault> {
        let idx = self.index_of(pa)?;
        match self.state[idx] {
            FREE => Err(FrameFault::NotOwned),
            tag if tag == owner.tag() => {
                self.state[idx] = FREE;
                if owner.is_reserved() {
                    self.reserved -= 1;
                } else {
                    self.owned -= 1;
                }
                Ok(())
            }
            _ => Err(FrameFault::WrongOwner),
        }
    }

    /// Hand a frame from one owner to another without it becoming free in between — the frame is
    /// never allocatable mid-transfer, so no third party can claim it. Refuses when `from` is not
    /// the current owner (including when the frame is free), and refuses moving a reserved frame.
    pub fn transfer(&mut self, pa: usize, from: Owner, to: Owner) -> Result<(), FrameFault> {
        let idx = self.index_of(pa)?;
        match self.state[idx] {
            FREE => Err(FrameFault::NotOwned),
            RESERVED_TAG => Err(FrameFault::Reserved),
            tag if tag == from.tag() => {
                if to.is_reserved() {
                    return Err(FrameFault::Reserved);
                }
                self.state[idx] = to.tag();
                Ok(())
            }
            _ => Err(FrameFault::WrongOwner),
        }
    }

    /// Frames currently owned (excluding permanently reserved ones).
    pub const fn owned_count(&self) -> usize {
        self.owned
    }

    /// Frames permanently withheld from the pool.
    pub const fn reserved_count(&self) -> usize {
        self.reserved
    }

    /// Frames neither owned nor reserved — what the free list should contain. A target's VM gate
    /// compares this against its allocator's own `free_count()`: a divergence means the free list
    /// and the ownership model disagree, which is exactly the corruption this module prevents.
    pub const fn free_count(&self) -> usize {
        self.frames - self.owned - self.reserved
    }

    /// Release every frame held by `owner`, returning how many were freed. This is the primitive an
    /// address-space teardown needs (ALET-P1-004): "free exactly the frames belonging to this
    /// address space", with no list of addresses to lose track of.
    pub fn release_all(&mut self, owner: Owner) -> usize {
        if owner.is_reserved() {
            return 0; // reserved memory is never reclaimed by a teardown
        }
        let tag = owner.tag();
        let mut n = 0;
        for slot in self.state[..self.frames].iter_mut() {
            if *slot == tag {
                *slot = FREE;
                n += 1;
            }
        }
        self.owned -= n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: usize = 0x4000_0000;

    fn table(state: &mut [u8]) -> FrameOwnerTable<'_> {
        let frames = state.len();
        FrameOwnerTable::new(BASE, frames, state).expect("window fits the state slice")
    }

    fn pa(i: usize) -> usize {
        BASE + i * PAGE_SIZE
    }

    #[test]
    fn a_claimed_frame_reports_its_owner_and_a_free_one_reports_none() {
        let mut s = [0u8; 8];
        let mut t = table(&mut s);
        assert_eq!(t.owner_of(pa(0)), Ok(None));
        assert_eq!(t.claim(pa(0), Owner::KERNEL), Ok(()));
        assert_eq!(t.owner_of(pa(0)), Ok(Some(Owner::KERNEL)));
        assert_eq!(t.owned_count(), 1);
        assert_eq!(t.free_count(), 7);
    }

    #[test]
    fn claiming_a_live_frame_is_refused_so_two_owners_never_alias_one_page() {
        let mut s = [0u8; 4];
        let mut t = table(&mut s);
        t.claim(pa(1), Owner::PAGETABLE).unwrap();
        assert_eq!(t.claim(pa(1), Owner::USER), Err(FrameFault::AlreadyOwned));
        assert_eq!(t.owner_of(pa(1)), Ok(Some(Owner::PAGETABLE)));
    }

    #[test]
    fn double_free_is_refused() {
        let mut s = [0u8; 4];
        let mut t = table(&mut s);
        t.claim(pa(2), Owner::KERNEL).unwrap();
        assert_eq!(t.release(pa(2), Owner::KERNEL), Ok(()));
        assert_eq!(t.release(pa(2), Owner::KERNEL), Err(FrameFault::NotOwned));
        assert_eq!(t.free_count(), 4);
    }

    #[test]
    fn releasing_someone_elses_frame_is_refused() {
        let mut s = [0u8; 4];
        let mut t = table(&mut s);
        t.claim(pa(0), Owner::PAGETABLE).unwrap();
        assert_eq!(t.release(pa(0), Owner::USER), Err(FrameFault::WrongOwner));
        assert_eq!(t.owner_of(pa(0)), Ok(Some(Owner::PAGETABLE)));
    }

    #[test]
    fn transfer_moves_ownership_without_the_frame_ever_being_free() {
        let mut s = [0u8; 4];
        let mut t = table(&mut s);
        t.claim(pa(3), Owner::KERNEL).unwrap();
        let before = t.free_count();
        assert_eq!(t.transfer(pa(3), Owner::KERNEL, Owner::USER), Ok(()));
        assert_eq!(t.owner_of(pa(3)), Ok(Some(Owner::USER)));
        assert_eq!(t.free_count(), before, "never became allocatable mid-move");
        assert_eq!(
            t.transfer(pa(3), Owner::KERNEL, Owner::USER),
            Err(FrameFault::WrongOwner)
        );
    }

    #[test]
    fn reserved_memory_is_never_handed_out_or_reclaimed() {
        let mut s = [0u8; 4];
        let mut t = table(&mut s);
        t.reserve(pa(0)).unwrap();
        assert_eq!(t.claim(pa(0), Owner::KERNEL), Err(FrameFault::Reserved));
        assert_eq!(
            t.transfer(pa(0), Owner::RESERVED, Owner::USER),
            Err(FrameFault::Reserved)
        );
        assert_eq!(t.release_all(Owner::RESERVED), 0);
        assert_eq!(t.reserved_count(), 1);
        assert_eq!(t.free_count(), 3);
    }

    #[test]
    fn addresses_outside_the_window_or_off_a_frame_base_name_no_frame() {
        let mut s = [0u8; 2];
        let mut t = table(&mut s);
        assert_eq!(
            t.claim(BASE - PAGE_SIZE, Owner::KERNEL),
            Err(FrameFault::OutOfWindow)
        );
        assert_eq!(t.claim(pa(2), Owner::KERNEL), Err(FrameFault::OutOfWindow));
        assert_eq!(t.claim(BASE + 1, Owner::KERNEL), Err(FrameFault::Unaligned));
    }

    #[test]
    fn a_window_larger_than_its_state_slice_is_refused_not_truncated() {
        let mut s = [0u8; 4];
        assert_eq!(
            FrameOwnerTable::new(BASE, 5, &mut s).err(),
            Some(FrameFault::CapacityExceeded)
        );
    }

    #[test]
    fn release_all_frees_exactly_one_owners_frames() {
        let mut s = [0u8; 8];
        let mut t = table(&mut s);
        let a = Owner::address_space(0).unwrap();
        let b = Owner::address_space(1).unwrap();
        t.claim(pa(0), a).unwrap();
        t.claim(pa(1), a).unwrap();
        t.claim(pa(2), b).unwrap();
        assert_eq!(t.release_all(a), 2);
        assert_eq!(t.owner_of(pa(0)), Ok(None));
        assert_eq!(t.owner_of(pa(1)), Ok(None));
        assert_eq!(t.owner_of(pa(2)), Ok(Some(b)));
        assert_eq!(t.owned_count(), 1);
    }

    #[test]
    fn owner_tags_are_distinct_and_zero_is_not_an_owner() {
        assert_eq!(Owner::new(0), None);
        assert_eq!(
            Owner::address_space(0),
            Owner::new(Owner::FIRST_ADDRESS_SPACE)
        );
        assert_ne!(Owner::address_space(0), Owner::address_space(1));
        assert_eq!(Owner::address_space(u32::MAX), None);
        // Address-space tags never collide with the well-known ones or with RESERVED.
        for id in 0..(255 - Owner::FIRST_ADDRESS_SPACE as u32) {
            let o = Owner::address_space(id).expect("id within range");
            assert!(!o.is_reserved());
            assert_ne!(o, Owner::KERNEL);
            assert_ne!(o, Owner::PAGETABLE);
            assert_ne!(o, Owner::USER);
        }
    }
}
