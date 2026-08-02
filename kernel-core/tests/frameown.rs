//! Frame-ownership PROPERTIES (GAPS4 ALET-P1-003, REQ-MM-002, ADR-030).
//!
//! `kernel-core/src/frameown.rs`'s unit tests state the individual transitions. This suite states
//! the properties those transitions exist to guarantee, and proves them by *enumeration over a
//! deterministic operation sequence* rather than by example — the same shape as
//! `kernel-core/tests/vmaddr.rs` (ALET-P1-001), which enumerates candidate addresses across every
//! target plan instead of asserting a handful of literals.
//!
//! The three properties, in the words of the corruption each prevents:
//!
//! 1. **No frame is ever held by two owners at once.** This is the double-allocation an intrusive
//!    free-list with a cycle in it produces: a page table and a user page over one page.
//! 2. **The books always balance.** `owned + reserved + free == frames` after EVERY operation,
//!    accepted or refused. A refused operation that still moved a counter is a slow leak of the
//!    pool, and a target's VM gate compares this `free_count()` against the allocator's own.
//! 3. **A refusal changes nothing.** Every refused claim/release/transfer leaves the entire state
//!    array byte-identical. A fail-closed check that half-applies is not fail-closed.
//!
//! Determinism matters: an OS invariant proof that depends on a random seed is not reproducible in
//! CI, so the operation sequence comes from a fixed linear-congruential generator, written here
//! rather than pulled from a crate (`kernel-core` is `no_std` and dependency-free by design).

use kernel_core::frameown::{FrameFault, FrameOwnerTable, Owner};
use kernel_core::vmaddr::{AddrPlan, PAGE_SIZE};

const BASE: usize = 0x4000_0000;
const FRAMES: usize = 64;

fn pa(i: usize) -> usize {
    BASE + i * PAGE_SIZE
}

/// Fixed LCG (Numerical Recipes constants) — reproducible in CI, unlike a seeded RNG crate.
///
/// It returns the HIGH bits: an LCG's low bits have a period as short as the modulus they span
/// (bit 0 alternates, `x % 4` cycles with period 4), so taking `next() % 4` from the raw state
/// would produce a rigidly repeating operation pattern that exercises almost nothing — which is
/// exactly what the first run of this suite showed (1 accepted operation in 20 000).
struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 16
    }
}

/// The owners the sequence draws from: the two well-known kernel tags plus two address spaces, so
/// cross-owner releases are actually attempted.
fn owners() -> [Owner; 4] {
    [
        Owner::KERNEL,
        Owner::PAGETABLE,
        Owner::address_space(0).expect("address space 0 has a tag"),
        Owner::address_space(1).expect("address space 1 has a tag"),
    ]
}

/// Drive a long, deterministic mix of legal and illegal operations, asserting all three properties
/// after every single step. This is the suite's core: the properties are checked against the state
/// the operations actually produced, not against a scenario chosen to make them true.
#[test]
fn the_three_properties_hold_after_every_operation_in_a_long_mixed_sequence() {
    let mut state = [0u8; FRAMES];
    let mut t = FrameOwnerTable::new(BASE, FRAMES, &mut state).expect("window fits state");
    let os = owners();
    // A shadow model built only from ACCEPTED operations: what ownership should be if the table is
    // correct. `None` = free. The table is compared against it after every step.
    let mut shadow: [Option<Owner>; FRAMES] = [None; FRAMES];
    let mut rng = Lcg(0xA1E7_2026);
    let mut accepted = 0usize;
    let mut refused = 0usize;

    for step in 0..20_000 {
        let idx = (rng.next() as usize) % FRAMES;
        let owner = os[(rng.next() as usize) % os.len()];
        let op = rng.next() % 4;
        let target = pa(idx);

        // Snapshot for property 3 (a refusal must change nothing observable).
        let before: Vec<Option<Owner>> = (0..FRAMES)
            .map(|i| t.owner_of(pa(i)).expect("in-window"))
            .collect();
        let before_free = t.free_count();

        let outcome = match op {
            0 => t.claim(target, owner),
            1 => t.release(target, owner),
            2 => {
                let to = os[(rng.next() as usize) % os.len()];
                t.transfer(target, owner, to).map(|()| {
                    if shadow[idx] == Some(owner) {
                        shadow[idx] = Some(to);
                    }
                })
            }
            _ => {
                // Occasionally address something that is not a frame of ours at all.
                let bogus = if step % 2 == 0 { BASE + 1 } else { pa(FRAMES) };
                t.claim(bogus, owner)
            }
        };

        match (op, outcome) {
            (0, Ok(())) => {
                assert_eq!(shadow[idx], None, "claim accepted on an owned frame");
                shadow[idx] = Some(owner);
                accepted += 1;
            }
            (1, Ok(())) => {
                assert_eq!(shadow[idx], Some(owner), "release accepted for a non-owner");
                shadow[idx] = None;
                accepted += 1;
            }
            (2, Ok(())) => accepted += 1,
            (3, Ok(())) => panic!("an address outside the window was accepted"),
            (_, Err(_)) => {
                refused += 1;
                // PROPERTY 3 — a refusal is a no-op across the whole table.
                for (i, was) in before.iter().enumerate() {
                    assert_eq!(
                        &t.owner_of(pa(i)).expect("in-window"),
                        was,
                        "refused op mutated frame {i} at step {step}"
                    );
                }
                assert_eq!(t.free_count(), before_free, "refused op moved the counters");
            }
            _ => unreachable!(),
        }

        // PROPERTY 1 — the table agrees with the shadow: every frame has exactly the one owner the
        // accepted operations gave it, and none has two.
        for (i, expected) in shadow.iter().enumerate() {
            assert_eq!(
                &t.owner_of(pa(i)).expect("in-window"),
                expected,
                "ownership diverged at frame {i}, step {step}"
            );
        }

        // PROPERTY 2 — the books balance.
        assert_eq!(
            t.owned_count() + t.reserved_count() + t.free_count(),
            FRAMES,
            "counters do not sum to the window at step {step}"
        );
        assert_eq!(
            t.owned_count(),
            shadow.iter().filter(|o| o.is_some()).count(),
            "owned_count disagrees with the shadow at step {step}"
        );
    }

    // The sequence must actually have exercised both paths, or the properties above proved nothing.
    assert!(accepted > 1_000, "sequence accepted too little: {accepted}");
    assert!(refused > 1_000, "sequence refused too little: {refused}");
}

/// A claimed frame can never be claimed again, by ANY owner — stated directly over all owners
/// rather than inferred from the mixed sequence.
#[test]
fn a_live_frame_is_unclaimable_by_every_owner() {
    let mut state = [0u8; FRAMES];
    let mut t = FrameOwnerTable::new(BASE, FRAMES, &mut state).expect("window fits state");
    for (i, holder) in owners().into_iter().enumerate() {
        t.claim(pa(i), holder).expect("free frame is claimable");
        for challenger in owners() {
            assert_eq!(
                t.claim(pa(i), challenger),
                Err(FrameFault::AlreadyOwned),
                "frame {i} held by {holder:?} was re-claimed by {challenger:?}"
            );
        }
        // ...and only the holder can give it back.
        for challenger in owners() {
            if challenger != holder {
                assert_eq!(t.release(pa(i), challenger), Err(FrameFault::WrongOwner));
            }
        }
        assert_eq!(t.release(pa(i), holder), Ok(()));
        assert_eq!(t.release(pa(i), holder), Err(FrameFault::NotOwned));
    }
}

/// Every address the ownership table accepts is a DISTINCT frame: no two accepted physical
/// addresses share an index. The physical-side twin of `vmaddr`'s "no two accepted VAs alias".
#[test]
fn accepted_physical_addresses_never_share_a_frame_index() {
    let mut state = [0u8; FRAMES];
    let t = FrameOwnerTable::new(BASE, FRAMES, &mut state).expect("window fits state");
    let mut seen = [false; FRAMES];
    // Sweep a range that runs off both ends of the window and lands off frame bases too.
    let mut candidate = BASE.saturating_sub(4 * PAGE_SIZE);
    while candidate < BASE + (FRAMES + 4) * PAGE_SIZE {
        match t.index_of(candidate) {
            Ok(idx) => {
                assert!(!seen[idx], "two accepted addresses map to frame {idx}");
                seen[idx] = true;
                assert_eq!(candidate, pa(idx), "index does not round-trip to its base");
            }
            Err(FrameFault::Unaligned) => assert_ne!(candidate % PAGE_SIZE, 0),
            Err(FrameFault::OutOfWindow) => {
                assert!(!(BASE..BASE + FRAMES * PAGE_SIZE).contains(&candidate))
            }
            Err(other) => panic!("index_of returned an unexpected fault: {other:?}"),
        }
        candidate += PAGE_SIZE / 4; // step off frame bases deliberately
    }
    assert!(seen.iter().all(|&s| s), "some frame was never reachable");
}

/// The ownership table and the address plan agree on what memory the kernel owns: every physical
/// address `AddrPlan::validate_map` accepts is a frame this table covers, and vice versa. If they
/// could disagree, a mapping could target a frame with no ownership state — the exact seam between
/// ALET-P1-001 and ALET-P1-003.
#[test]
fn the_address_plan_and_the_ownership_table_cover_the_same_physical_window() {
    let mut state = [0u8; FRAMES];
    let t = FrameOwnerTable::new(BASE, FRAMES, &mut state).expect("window fits state");
    // Built the way every target builds it: from the allocator's own base and frame count.
    let plan = AddrPlan::new(39, false, t.base(), t.frames() * PAGE_SIZE);
    let va = 0x20_0000; // a fixed, legal virtual page base
    let mut candidate = BASE.saturating_sub(4 * PAGE_SIZE);
    while candidate < BASE + (FRAMES + 4) * PAGE_SIZE {
        let plan_ok = plan.validate_map(va, candidate).is_ok();
        let table_ok = t.index_of(candidate).is_ok();
        assert_eq!(
            plan_ok, table_ok,
            "plan and ownership table disagree about {candidate:#x}"
        );
        candidate += PAGE_SIZE;
    }
}

/// Reserved memory is withheld from the pool for good: unclaimable, untransferable, and untouched
/// by an address-space teardown.
#[test]
fn reserved_frames_stay_out_of_circulation() {
    let mut state = [0u8; FRAMES];
    let mut t = FrameOwnerTable::new(BASE, FRAMES, &mut state).expect("window fits state");
    for i in 0..8 {
        t.reserve(pa(i)).expect("free frame is reservable");
    }
    let asid = Owner::address_space(7).expect("tag exists");
    for i in 8..16 {
        t.claim(pa(i), asid).expect("free frame is claimable");
    }
    assert_eq!(t.reserved_count(), 8);
    assert_eq!(t.owned_count(), 8);
    assert_eq!(t.free_count(), FRAMES - 16);

    for i in 0..8 {
        assert_eq!(t.claim(pa(i), Owner::KERNEL), Err(FrameFault::Reserved));
        assert_eq!(t.release(pa(i), Owner::KERNEL), Err(FrameFault::WrongOwner));
    }
    // A teardown of the address space frees ITS frames and leaves reserved memory reserved.
    assert_eq!(t.release_all(asid), 8);
    assert_eq!(t.reserved_count(), 8);
    assert_eq!(t.owned_count(), 0);
    assert_eq!(t.free_count(), FRAMES - 8);
}

/// Every refusal carries a distinct, stable name — the targets log these words, and
/// `scripts/conformance.sh` requires all three architectures to produce identical ones.
#[test]
fn every_fault_has_a_distinct_stable_name() {
    let all = [
        FrameFault::Unaligned,
        FrameFault::OutOfWindow,
        FrameFault::AlreadyOwned,
        FrameFault::NotOwned,
        FrameFault::WrongOwner,
        FrameFault::Reserved,
        FrameFault::CapacityExceeded,
    ];
    let names: Vec<&str> = all.iter().map(|f| f.as_str()).collect();
    for (i, a) in names.iter().enumerate() {
        assert!(a.starts_with("frame-"), "{a} is not namespaced");
        for b in names.iter().skip(i + 1) {
            assert_ne!(a, b, "two faults share the name {a}");
        }
    }
}
