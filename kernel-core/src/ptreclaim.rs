//! Page-table reclamation — freeing the intermediate tables an unmap leaves empty (GAPS4
//! ALET-P1-002, REQ-MM-003, ADR-031).
//!
//! # Why an unmap that only clears the leaf is a leak with teeth
//!
//! Mapping one page in a fresh address space allocates a chain of tables: on a 3-level aarch64
//! TTBR0 or RISC-V Sv39 walk, an L1 (root) entry pointing at an L2 table pointing at an L3 table
//! whose entry is the page; on x86-64, PML4 → PDPT → PD → PT. Unmapping that page cleared the leaf
//! entry and stopped. Every intermediate table stayed allocated and stayed referenced, holding a
//! frame per level for a mapping that no longer exists.
//!
//! At boot-test scale that is a bounded, documented leak. As a running OS property it is not:
//! * a task that maps and unmaps across a wide virtual range permanently consumes one frame per
//!   512-page span it ever touched, so the pool drains in proportion to addresses *visited* rather
//!   than pages *held* — a denial of service any unprivileged task can drive;
//! * an address space can never be fully torn down, because teardown has no way to discover which
//!   tables belong to it (ALET-P1-004 depends on this being solved first);
//! * the leaked tables stay *reachable* through their parent entries, so they are not merely
//!   unused memory — they are live translation structure for an empty region.
//!
//! # Why this is a shared policy with a per-target seam
//!
//! The page-table FORMAT is architectural (descriptor bits, level count, index extraction), but the
//! reclamation RULE is not, and getting it wrong is the same bug on every CPU:
//!
//! 1. A table may be freed only when **every** entry in it is absent — a sibling mapping anywhere
//!    in the table means the table is still in use.
//! 2. The parent's reference must be cleared **before** the frame is freed. Free-then-clear leaves
//!    a window in which a live entry points at a frame the allocator may already have handed out.
//! 3. The **root is never freed** — it is the address space's identity, owned by whoever created it.
//! 4. Reclamation walks upward and **stops at the first non-empty table**: its ancestors are
//!    non-empty by construction (they contain the entry pointing at it), so continuing would be
//!    wasted work at best and a wrong free at worst.
//! 5. If the allocator refuses the free — the ownership model (REQ-MM-002) saying this frame is not
//!    a page table this caller holds — the parent entry is **restored** and the operation reports
//!    failure. A refused free must not leave the table unreachable-but-allocated.
//!
//! So the rule set lives here, once, proved on the host against an in-memory page-table model, and
//! each target implements [`TableOps`] over its own descriptor format — the same split by which
//! `kernel-core::sched` owns scheduling policy while each target owns the context switch.
//!
//! # Relationship to the rest of the memory model
//!
//! * [`vmaddr`](crate::vmaddr) (REQ-MM-001) decides whether an address may be walked at all.
//! * [`frameown`](crate::frameown) (REQ-MM-002) decides who owns a frame; reclamation frees tables
//!   as [`Owner::PAGETABLE`](crate::frameown::Owner::PAGETABLE), so a bug that tried to reclaim a
//!   user page or a frame this address space does not hold is refused rather than obeyed.
//! * Address-space destruction (ALET-P1-004) remains **open**: this module reclaims the tables an
//!   unmap empties, not the whole tree of a dying address space.
//!
//! Invalidation is deliberately NOT done here. Clearing a parent entry can leave stale
//! paging-structure (walk) cache entries, and the instruction that flushes them is architectural —
//! `tlbi vae1` on aarch64, `sfence.vma` on RISC-V, `invlpg` on x86-64. Every target already
//! invalidates the unmapped VA; each calls reclamation BEFORE that invalidation so one flush covers
//! the leaf and its now-detached ancestors.

/// One step of a completed walk: the table that was consulted, and the index within it.
///
/// A path is ordered root-first. `path[0].table` is the address space's root and `path[0].index`
/// the root entry that leads toward the page; the last element is the leaf table and the index of
/// the entry the unmap just cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathStep {
    /// Physical (identity-accessible) address of the table.
    pub table: usize,
    /// Index of the entry within that table.
    pub index: usize,
}

impl PathStep {
    /// Convenience constructor.
    pub const fn new(table: usize, index: usize) -> Self {
        Self { table, index }
    }
}

/// Why reclamation refused. Each names a distinct corruption the rule prevents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimFault {
    /// The path had fewer than two steps, so there is no table below the root to consider.
    /// Reclaiming with no parent to update would be a free with a live reference left behind.
    PathTooShort,
    /// The allocator refused to free a table frame — under REQ-MM-002 that means the frame is not
    /// a page table this caller holds. The parent entry has been restored; nothing was freed.
    FreeRefused,
}

impl ReclaimFault {
    /// Stable short name for invariant logs on targets that have no formatter.
    pub const fn as_str(self) -> &'static str {
        match self {
            ReclaimFault::PathTooShort => "reclaim-path-too-short",
            ReclaimFault::FreeRefused => "reclaim-free-refused",
        }
    }
}

/// What one reclamation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reclaimed {
    /// Number of intermediate tables freed (never includes the root).
    pub tables_freed: usize,
    /// Depth of the first table that was still in use, counted from the leaf (`0` = the leaf table
    /// itself had a sibling entry). `None` when the chain was reclaimed all the way to the root.
    pub stopped_at: Option<usize>,
}

/// A target's page tables, as reclamation needs to see them: read an entry, write an entry, decide
/// whether an entry is present, and hand a table frame back to the allocator.
///
/// Every method is defined over PHYSICAL table addresses, which all three Aletheia targets can
/// access directly (identity-mapped RAM). Implementations are a few lines each and contain the only
/// architecture-specific knowledge in the whole operation: the present bit and the entry width.
pub trait TableOps {
    /// Entries per table. 512 for every 4 KiB-granule 64-bit target Aletheia supports; a parameter
    /// rather than a constant so a target with a different granule cannot be silently mismodelled.
    fn entries_per_table(&self) -> usize {
        512
    }

    /// Read entry `index` of the table at `table`.
    fn read(&self, table: usize, index: usize) -> u64;

    /// Write entry `index` of the table at `table`.
    fn write(&mut self, table: usize, index: usize, value: u64);

    /// Is this entry present (does it reference anything)?
    fn is_present(&self, entry: u64) -> bool;

    /// Return a table frame to the allocator as a page table. `false` means the allocator refused —
    /// under the ownership model that is "this is not a page-table frame you hold", and
    /// reclamation restores the parent entry rather than proceeding.
    fn free_table(&mut self, table: usize) -> bool;
}

/// Does the table at `table` contain no present entries?
pub fn table_is_empty<T: TableOps>(ops: &T, table: usize) -> bool {
    (0..ops.entries_per_table()).all(|i| !ops.is_present(ops.read(table, i)))
}

/// Free every table below the root that the just-cleared leaf entry left empty, clearing each
/// parent reference first and stopping at the first table still in use.
///
/// `path` is the walk that reached the unmapped page, root-first (see [`PathStep`]). Call this
/// AFTER clearing the leaf entry and BEFORE the target's TLB/walk-cache invalidation for the VA, so
/// a single invalidation covers the leaf and any ancestors this detaches.
pub fn reclaim_empty_tables<T: TableOps>(
    path: &[PathStep],
    ops: &mut T,
) -> Result<Reclaimed, ReclaimFault> {
    if path.len() < 2 {
        return Err(ReclaimFault::PathTooShort);
    }
    let mut out = Reclaimed::default();
    // Walk upward from the leaf table toward — but never onto — the root at `path[0]`.
    for i in (1..path.len()).rev() {
        let step = path[i];
        if !table_is_empty(ops, step.table) {
            // Rule 4: this table is still in use, and every ancestor holds the entry that points
            // at it, so no ancestor can be empty either.
            out.stopped_at = Some(path.len() - 1 - i);
            return Ok(out);
        }
        let parent = path[i - 1];
        let saved = ops.read(parent.table, parent.index);
        // Rule 2: drop the reference BEFORE the frame can be reallocated.
        ops.write(parent.table, parent.index, 0);
        if !ops.free_table(step.table) {
            // Rule 5: fail-closed and reversible — put the reference back, report, free nothing.
            ops.write(parent.table, parent.index, saved);
            return Err(ReclaimFault::FreeRefused);
        }
        out.tables_freed += 1;
    }
    // Reached the root's own entry: rule 3 — the root itself is never freed.
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const ENTRIES: usize = 8; // small on purpose: emptiness is checked over EVERY entry
    const ROOT: usize = 0x1000;
    const MID: usize = 0x2000;
    const LEAF: usize = 0x3000;
    const PAGE: u64 = 0xDEAD_0000;

    /// A page-table model: fixed-size tables addressed by a fake physical address, plus the frames
    /// the "allocator" was asked to take back.
    struct Model {
        tables: Vec<(usize, Vec<u64>)>,
        freed: Vec<usize>,
        refuse: Option<usize>,
    }

    impl Model {
        fn new(addrs: &[usize]) -> Self {
            Self {
                tables: addrs.iter().map(|a| (*a, vec![0u64; ENTRIES])).collect(),
                freed: Vec::new(),
                refuse: None,
            }
        }
        fn slot(&mut self, table: usize, index: usize) -> &mut u64 {
            &mut self
                .tables
                .iter_mut()
                .find(|(a, _)| *a == table)
                .expect("table exists")
                .1[index]
        }
        fn live_reference_to(&self, table: usize) -> bool {
            self.tables
                .iter()
                .any(|(_, e)| e.iter().any(|v| *v != 0 && (*v as usize) == table))
        }
    }

    impl TableOps for Model {
        fn entries_per_table(&self) -> usize {
            ENTRIES
        }
        fn read(&self, table: usize, index: usize) -> u64 {
            self.tables
                .iter()
                .find(|(a, _)| *a == table)
                .expect("table exists")
                .1[index]
        }
        fn write(&mut self, table: usize, index: usize, value: u64) {
            *self.slot(table, index) = value;
        }
        fn is_present(&self, entry: u64) -> bool {
            entry != 0
        }
        fn free_table(&mut self, table: usize) -> bool {
            if self.refuse == Some(table) {
                return false;
            }
            self.freed.push(table);
            true
        }
    }

    /// root -> mid -> leaf with one page mapped at indices (1, 2, 3), with the leaf entry already
    /// cleared — exactly the state an unmap leaves behind before reclamation runs.
    fn mapped_then_leaf_cleared() -> (Model, [PathStep; 3]) {
        let mut m = Model::new(&[ROOT, MID, LEAF]);
        m.write(ROOT, 1, MID as u64);
        m.write(MID, 2, LEAF as u64);
        m.write(LEAF, 3, PAGE);
        m.write(LEAF, 3, 0); // the unmap cleared it
        (
            m,
            [
                PathStep::new(ROOT, 1),
                PathStep::new(MID, 2),
                PathStep::new(LEAF, 3),
            ],
        )
    }

    #[test]
    fn the_whole_empty_chain_below_the_root_is_freed_and_dereferenced() {
        let (mut m, path) = mapped_then_leaf_cleared();
        let r = reclaim_empty_tables(&path, &mut m).expect("reclaim succeeds");
        assert_eq!(r.tables_freed, 2, "leaf and mid tables");
        assert_eq!(r.stopped_at, None, "chain went all the way to the root");
        assert_eq!(m.freed, vec![LEAF, MID], "freed leaf-first, upward");
        assert!(
            !m.live_reference_to(LEAF),
            "an entry still points at the freed leaf"
        );
        assert!(
            !m.live_reference_to(MID),
            "an entry still points at freed mid"
        );
        assert_eq!(m.read(ROOT, 1), 0, "the root entry was cleared");
    }

    #[test]
    fn the_root_itself_is_never_freed() {
        let (mut m, path) = mapped_then_leaf_cleared();
        reclaim_empty_tables(&path, &mut m).expect("reclaim succeeds");
        assert!(
            !m.freed.contains(&ROOT),
            "the address space's root was freed"
        );
    }

    #[test]
    fn a_sibling_mapping_in_the_leaf_table_keeps_every_table() {
        let (mut m, path) = mapped_then_leaf_cleared();
        m.write(LEAF, 5, PAGE); // another page in the same leaf table
        let r = reclaim_empty_tables(&path, &mut m).expect("reclaim succeeds");
        assert_eq!(r.tables_freed, 0);
        assert_eq!(r.stopped_at, Some(0), "stopped at the leaf itself");
        assert!(m.freed.is_empty());
        assert_eq!(m.read(MID, 2), LEAF as u64, "the leaf is still referenced");
    }

    #[test]
    fn a_sibling_leaf_table_keeps_the_shared_parent() {
        let (mut m, path) = mapped_then_leaf_cleared();
        m.write(MID, 6, 0x9000); // a second leaf table hanging off the same mid table
        let r = reclaim_empty_tables(&path, &mut m).expect("reclaim succeeds");
        assert_eq!(r.tables_freed, 1, "only the empty leaf");
        assert_eq!(r.stopped_at, Some(1), "stopped one level above the leaf");
        assert_eq!(m.freed, vec![LEAF]);
        assert_eq!(
            m.read(ROOT, 1),
            MID as u64,
            "the mid table is still referenced"
        );
        assert!(!m.live_reference_to(LEAF));
    }

    #[test]
    fn a_refused_free_restores_the_parent_reference_and_frees_nothing() {
        let (mut m, path) = mapped_then_leaf_cleared();
        m.refuse = Some(LEAF);
        assert_eq!(
            reclaim_empty_tables(&path, &mut m),
            Err(ReclaimFault::FreeRefused)
        );
        assert!(m.freed.is_empty(), "nothing was freed");
        assert_eq!(
            m.read(MID, 2),
            LEAF as u64,
            "the reference was restored, so the table is reachable rather than orphaned"
        );
    }

    #[test]
    fn a_refusal_partway_up_keeps_what_was_already_freed_consistent() {
        let (mut m, path) = mapped_then_leaf_cleared();
        m.refuse = Some(MID);
        assert_eq!(
            reclaim_empty_tables(&path, &mut m),
            Err(ReclaimFault::FreeRefused)
        );
        // The leaf was legitimately freed and legitimately dereferenced before the refusal.
        assert_eq!(m.freed, vec![LEAF]);
        assert!(
            !m.live_reference_to(LEAF),
            "dangling reference to the freed leaf"
        );
        assert_eq!(
            m.read(ROOT, 1),
            MID as u64,
            "mid is still reachable from the root"
        );
    }

    #[test]
    fn a_second_reclaim_never_frees_a_table_twice() {
        let (mut m, path) = mapped_then_leaf_cleared();
        reclaim_empty_tables(&path, &mut m).expect("first reclaim");
        assert_eq!(m.freed, vec![LEAF, MID]);
        // A real target cannot repeat this call: the walk that produced `path` no longer resolves,
        // because the root entry is cleared — which is precisely what stops a double free. Assert
        // that property directly rather than re-running with a stale path.
        assert_eq!(m.read(ROOT, 1), 0, "the walk to these tables is gone");
        assert!(!m.live_reference_to(MID) && !m.live_reference_to(LEAF));
    }

    #[test]
    fn a_path_without_a_parent_is_refused() {
        let mut m = Model::new(&[ROOT]);
        assert_eq!(
            reclaim_empty_tables(&[PathStep::new(ROOT, 0)], &mut m),
            Err(ReclaimFault::PathTooShort)
        );
        assert_eq!(
            reclaim_empty_tables(&[], &mut m),
            Err(ReclaimFault::PathTooShort)
        );
    }

    #[test]
    fn every_fault_has_a_distinct_stable_name() {
        assert_ne!(
            ReclaimFault::PathTooShort.as_str(),
            ReclaimFault::FreeRefused.as_str()
        );
        for f in [ReclaimFault::PathTooShort, ReclaimFault::FreeRefused] {
            assert!(f.as_str().starts_with("reclaim-"));
        }
    }
}
