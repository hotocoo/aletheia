//! Address-space destruction — returning everything a dying address space owns (GAPS4 ALET-P1-004,
//! REQ-MM-004, ADR-032).
//!
//! # What was missing
//!
//! [`ptreclaim`](crate::ptreclaim) (REQ-MM-003) reclaims the tables an *unmap* empties. That covers
//! a space that tidies up after itself, page by page. It does nothing for a space that simply
//! **dies** — a task that faults, is killed, or exits without unmapping. Everything it held stayed
//! allocated: its user pages, every intermediate table, and its root.
//!
//! An OS that cannot destroy an address space cannot reclaim a crashed task, so process lifetime
//! becomes a one-way ratchet on physical memory. This module is the other half: given a root, free
//! everything that address space owns, and nothing else.
//!
//! # "And nothing else" is the entire difficulty
//!
//! A page-table tree is not a private forest. Depending on the target it contains:
//!
//! * **Shared kernel structure.** x86-64 builds a per-process PML4 by *copying* the live one, so
//!   almost every top-level slot points at firmware and kernel tables that other spaces — and the
//!   running kernel — depend on. Freeing those would take the machine down.
//! * **Block/huge leaves that are not pool frames.** aarch64 and RISC-V per-process roots carry an
//!   identity map built from 2 MiB block / megapage descriptors covering RAM and MMIO. Those
//!   descriptor addresses are not frames the allocator ever handed out.
//! * **Shared pages.** A frame handed to another endpoint through the grant table (REQ-IPC-008) is
//!   mapped here but not owned here.
//!
//! So teardown is governed by two independent guards, and both must hold before anything is freed:
//!
//! 1. **Privacy** — [`SpaceOps::is_private`] lets each target say which `(level, index)` slots
//!    belong to this space. Teardown never descends into, and never frees, anything else. x86-64
//!    scopes this to its dedicated user region; the QEMU `virt` targets, whose per-process roots are
//!    built whole, declare every slot private.
//! 2. **Ownership** — every free goes through the ownership model (REQ-MM-002). A leaf is freed only
//!    if the allocator agrees this space's tag holds that frame; a block descriptor over RAM, a
//!    device mapping, or a granted page is refused and *counted as skipped* rather than freed.
//!
//! The result is fail-closed by construction: if the privacy predicate is wrong, ownership still
//! refuses; if a frame is shared, ownership still refuses. A teardown that frees nothing is a leak;
//! a teardown that frees the wrong thing is a corrupted kernel, so the design errs at the first.
//!
//! # Order, and why entries are cleared first
//!
//! Depth-first: a table's children are freed before the table itself, and the root last, so no freed
//! frame is ever still reachable through a live entry. Each entry is zeroed before its target is
//! freed — the same rule as `ptreclaim`, for the same reason. Unlike `ptreclaim` there is no
//! restore-on-refusal: the space is being destroyed, so a cleared entry to a frame we could not free
//! is the correct end state (the frame stays owned by whoever the ownership model says owns it, and
//! nothing dangles).
//!
//! The caller must not be *running in* the space it destroys. That is a target-level precondition
//! (each `vm.rs` refuses to destroy the active root), not something this arithmetic can check.

use crate::ptreclaim::TableOps;

/// What one teardown returned to the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Teardown {
    /// Page tables freed, including the root.
    pub tables_freed: usize,
    /// Leaf pages freed (frames the ownership model agreed this space held).
    pub leaves_freed: usize,
    /// Leaves left alone: block/huge descriptors over RAM or MMIO, granted pages, anything the
    /// ownership model says this space does not hold. Reported, never silently dropped.
    pub leaves_skipped: usize,
    /// Tables the allocator refused to take back. Non-zero means the tree contained a table this
    /// space did not own — a bug worth surfacing rather than a condition to ignore.
    pub tables_refused: usize,
}

/// Everything teardown needs beyond [`TableOps`]: how deep the walk goes, how to tell a leaf from a
/// table pointer, where an entry points, which slots are this space's own, and how to hand a leaf
/// page back.
pub trait SpaceOps: TableOps {
    /// Number of paging levels (3 for aarch64 TTBR0 / RISC-V Sv39, 4 for x86-64).
    fn levels(&self) -> usize;

    /// Does this entry map memory directly (a page, block, megapage or huge page) rather than
    /// point at the next-level table? Level is 0-based from the root.
    fn is_leaf(&self, entry: u64, level: usize) -> bool;

    /// Physical address an entry points at (its next-level table, or its mapped frame).
    fn entry_addr(&self, entry: u64) -> usize;

    /// Is slot `index` at `level` private to this address space? Defaults to "yes" for targets
    /// whose per-process trees are built whole; x86-64 overrides it to protect the copied kernel
    /// slots. Teardown neither descends into nor frees a slot this rejects.
    fn is_private(&self, _level: usize, _index: usize) -> bool {
        true
    }

    /// Return a leaf page to the allocator as memory this space held. `false` means the ownership
    /// model refused — the page is not this space's to free — and teardown counts it as skipped.
    fn free_leaf(&mut self, pa: usize) -> bool;
}

/// Free everything the address space rooted at `root` privately owns: its leaf pages, its
/// intermediate tables, and finally the root itself.
///
/// The caller must not be executing in this address space. Returns what was freed and what was
/// deliberately left alone.
pub fn destroy_address_space<T: SpaceOps>(root: usize, ops: &mut T) -> Teardown {
    let mut out = Teardown::default();
    destroy_level(root, 0, ops, &mut out);
    // The root goes last: by now nothing it referenced is still live, so freeing it cannot leave a
    // reachable-but-freed frame behind.
    if ops.free_table(root) {
        out.tables_freed += 1;
    } else {
        out.tables_refused += 1;
    }
    out
}

fn destroy_level<T: SpaceOps>(table: usize, level: usize, ops: &mut T, out: &mut Teardown) {
    for index in 0..ops.entries_per_table() {
        if !ops.is_private(level, index) {
            continue; // shared kernel/firmware structure — not ours to walk or free
        }
        let entry = ops.read(table, index);
        if !ops.is_present(entry) {
            continue;
        }
        let target = ops.entry_addr(entry);
        if ops.is_leaf(entry, level) {
            ops.write(table, index, 0);
            if ops.free_leaf(target) {
                out.leaves_freed += 1;
            } else {
                // A block descriptor over RAM, a device mapping, or a page this space does not own.
                out.leaves_skipped += 1;
            }
        } else if level + 1 < ops.levels() {
            destroy_level(target, level + 1, ops, out);
            ops.write(table, index, 0);
            if ops.free_table(target) {
                out.tables_freed += 1;
            } else {
                out.tables_refused += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const ENTRIES: usize = 4;
    const LEVELS: usize = 3;
    const ROOT: usize = 0x1000;
    const MID: usize = 0x2000;
    const LEAFT: usize = 0x3000;
    const PAGE_A: usize = 0x9000;
    const PAGE_B: usize = 0xA000;
    const SHARED_MID: usize = 0xB000; // a table another space also uses
    const BLOCK_RAM: usize = 0xC000; // a block descriptor, not a pool frame
    const LEAF_BIT: u64 = 1 << 62;

    /// Model: tables plus an "allocator" that owns a known set of frames and refuses anything else —
    /// the stand-in for the real ownership model (REQ-MM-002).
    struct Model {
        tables: Vec<(usize, Vec<u64>)>,
        owned_tables: Vec<usize>,
        owned_leaves: Vec<usize>,
        freed: Vec<usize>,
        private_root_only: Option<usize>,
    }

    impl Model {
        fn new(tables: &[usize]) -> Self {
            Self {
                tables: tables.iter().map(|a| (*a, vec![0u64; ENTRIES])).collect(),
                owned_tables: Vec::new(),
                owned_leaves: Vec::new(),
                freed: Vec::new(),
                private_root_only: None,
            }
        }
        fn table_entry(&mut self, table: usize, index: usize, child: usize) {
            self.write(table, index, child as u64);
        }
        fn leaf_entry(&mut self, table: usize, index: usize, page: usize) {
            self.write(table, index, page as u64 | LEAF_BIT);
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
            self.tables
                .iter_mut()
                .find(|(a, _)| *a == table)
                .expect("table exists")
                .1[index] = value;
        }
        fn is_present(&self, entry: u64) -> bool {
            entry != 0
        }
        fn free_table(&mut self, table: usize) -> bool {
            if self.owned_tables.contains(&table) {
                self.freed.push(table);
                true
            } else {
                false
            }
        }
    }

    impl SpaceOps for Model {
        fn levels(&self) -> usize {
            LEVELS
        }
        fn is_leaf(&self, entry: u64, _level: usize) -> bool {
            entry & LEAF_BIT != 0
        }
        fn entry_addr(&self, entry: u64) -> usize {
            (entry & !LEAF_BIT) as usize
        }
        fn is_private(&self, level: usize, index: usize) -> bool {
            match self.private_root_only {
                Some(slot) if level == 0 => index == slot,
                _ => true,
            }
        }
        fn free_leaf(&mut self, pa: usize) -> bool {
            if self.owned_leaves.contains(&pa) {
                self.freed.push(pa);
                self.owned_leaves.retain(|p| *p != pa); // freeing it twice must not succeed twice
                true
            } else {
                false
            }
        }
    }

    /// root -> mid -> leaf-table with two owned pages.
    fn space() -> Model {
        let mut m = Model::new(&[ROOT, MID, LEAFT]);
        m.table_entry(ROOT, 1, MID);
        m.table_entry(MID, 2, LEAFT);
        m.leaf_entry(LEAFT, 0, PAGE_A);
        m.leaf_entry(LEAFT, 3, PAGE_B);
        m.owned_tables = vec![ROOT, MID, LEAFT];
        m.owned_leaves = vec![PAGE_A, PAGE_B];
        m
    }

    #[test]
    fn a_dying_space_returns_every_page_every_table_and_its_root() {
        let mut m = space();
        let t = destroy_address_space(ROOT, &mut m);
        assert_eq!(t.leaves_freed, 2);
        assert_eq!(t.tables_freed, 3, "leaf table, mid table, and the root");
        assert_eq!(t.leaves_skipped, 0);
        assert_eq!(t.tables_refused, 0);
        // Children before parents, root last: nothing freed was still reachable when it was freed.
        assert_eq!(m.freed, vec![PAGE_A, PAGE_B, LEAFT, MID, ROOT]);
        assert!(
            m.tables.iter().all(|(_, e)| e.iter().all(|v| *v == 0)),
            "every entry was cleared before its target was freed"
        );
    }

    #[test]
    fn memory_the_space_does_not_own_is_skipped_not_freed() {
        let mut m = space();
        // A block descriptor over RAM (identity map) and a granted page the space does not hold.
        m.leaf_entry(LEAFT, 1, BLOCK_RAM);
        m.leaf_entry(LEAFT, 2, 0xD000);
        let t = destroy_address_space(ROOT, &mut m);
        assert_eq!(t.leaves_freed, 2, "only the two pages it owns");
        assert_eq!(
            t.leaves_skipped, 2,
            "block mapping and granted page left alone"
        );
        assert!(!m.freed.contains(&BLOCK_RAM));
        assert_eq!(t.tables_freed, 3);
    }

    #[test]
    fn shared_kernel_slots_are_never_walked_or_freed() {
        let mut m = space();
        // Slot 0 of the root is a COPIED kernel entry pointing at a table this space does not own —
        // exactly the x86-64 shape. Teardown must not descend into it or free it.
        m.tables.push((SHARED_MID, vec![0u64; ENTRIES]));
        m.table_entry(ROOT, 0, SHARED_MID);
        m.leaf_entry(SHARED_MID, 0, 0xE000);
        m.private_root_only = Some(1); // only slot 1 is this space's own
        let t = destroy_address_space(ROOT, &mut m);
        assert!(
            !m.freed.contains(&SHARED_MID),
            "freed a shared kernel table"
        );
        assert!(!m.freed.contains(&0xE000), "freed a page in a shared table");
        assert_eq!(
            m.read(ROOT, 0),
            SHARED_MID as u64,
            "the shared slot must be left intact — other spaces still use it"
        );
        assert_eq!(
            m.read(SHARED_MID, 0) & !LEAF_BIT,
            0xE000,
            "shared subtree untouched"
        );
        assert_eq!(t.leaves_freed, 2);
        assert_eq!(t.tables_freed, 3);
    }

    #[test]
    fn a_table_the_space_does_not_own_is_refused_and_reported() {
        let mut m = space();
        m.owned_tables = vec![ROOT, LEAFT]; // MID is somehow not ours
        let t = destroy_address_space(ROOT, &mut m);
        assert_eq!(
            t.tables_refused, 1,
            "the refusal is surfaced, not swallowed"
        );
        assert_eq!(
            t.tables_freed, 2,
            "the leaf table and the root still came back"
        );
        assert!(!m.freed.contains(&MID));
        assert_eq!(
            m.read(ROOT, 1),
            0,
            "no dangling reference to the table we could not free"
        );
    }

    #[test]
    fn an_empty_space_still_returns_its_root() {
        let mut m = Model::new(&[ROOT]);
        m.owned_tables = vec![ROOT];
        let t = destroy_address_space(ROOT, &mut m);
        assert_eq!(t.tables_freed, 1);
        assert_eq!(t.leaves_freed, 0);
        assert_eq!(m.freed, vec![ROOT]);
    }

    #[test]
    fn a_leaf_at_an_upper_level_is_freed_without_being_walked_as_a_table() {
        // A 2 MiB block / huge page hanging directly off the mid level: it maps memory, so it must
        // never be descended into as if it were a table.
        let mut m = space();
        m.leaf_entry(MID, 1, PAGE_A);
        let t = destroy_address_space(ROOT, &mut m);
        // PAGE_A is reachable at two levels here; the ownership model lets it be freed exactly once
        // and refuses the second attempt — the double-free defense doing its job during teardown.
        assert_eq!(t.leaves_freed, 2, "PAGE_A once, PAGE_B once");
        assert_eq!(t.leaves_skipped, 1, "the second sighting of PAGE_A refused");
        assert_eq!(t.tables_freed, 3);
    }
}
