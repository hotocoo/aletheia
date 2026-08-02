//! Memory attributes: W^X and per-architecture permission validation (GAPS4 ALET-P1-007 /
//! ALET-P1-008, REQ-MM-006, ADR-034).
//!
//! # The rule and why it is not a style preference
//!
//! A page that is both **writable and executable** turns any memory-corruption bug into code
//! execution: an attacker who can write bytes into such a page has, by construction, written
//! instructions the CPU will run. W^X (write XOR execute) is the invariant that removes that step,
//! and it only helps if it holds on *every* mapping path — one API that forgets it is the one an
//! exploit will find.
//!
//! Two neighbouring rules are enforced with it, because each is a distinct real failure:
//!
//! * **Device memory is never executable.** MMIO registers are not instructions; an executable
//!   device mapping means a speculative or mispredicted fetch can hit a register with read side
//!   effects, and it gives an attacker a jump target whose contents the device — not the kernel —
//!   controls.
//! * **A user page is never kernel-executable.** If a page the user can write is executable at
//!   kernel privilege, the classic ret2usr attack applies: the user writes a payload and tricks the
//!   kernel into jumping to it, running with full authority. (This is what hardware SMEP/PXN exist
//!   to prevent; Aletheia refuses to create such a mapping in the first place, so the property does
//!   not depend on a chip feature being present and enabled.)
//!
//! # What this module is, and what it deliberately does not claim
//!
//! It is pure arithmetic over a decoded [`PageAttrs`], plus an [`audit`] that walks live page tables
//! through the same [`TableOps`](crate::ptreclaim::TableOps) seam reclamation uses. Each target
//! decodes its own descriptor bits — AP/UXN/PXN/AttrIndx on aarch64, R/W/X/U on RISC-V,
//! WRITABLE/NO_EXECUTE/USER_ACCESSIBLE on x86-64 — and calls [`PageAttrs::validate`] at the entry of
//! every dynamic mapping API.
//!
//! **Scope, stated up front (this is deliberately a `partial` requirement):** the rules are enforced
//! on every *dynamic* mapping path, and the audit walks every mapping a target declares in scope
//! (the whole tree by default; on x86-64 the region this kernel actually mapped, since the live root
//! is the one OVMF built). The bootstrap
//! identity map is a different matter. aarch64 and RISC-V map the kernel image in 2 MiB block /
//! megapage descriptors that cover text, rodata, data, stack and heap in one span, so those blocks
//! are unavoidably writable-and-executable until the image is split at page granularity with linker
//! symbols; on x86-64 the firmware's own mappings are inherited from OVMF. [`audit`] therefore
//! reports violations *by class* — `dynamic` and `bootstrap` — and each target's VM gate requires
//! `dynamic == 0` while pinning the bootstrap count, so the exception is measured rather than
//! hidden, and shrinking it is a visible change. ALET-P1-007 stays **open** in the GAPS4 register
//! until the bootstrap count reaches zero.

use crate::ptreclaim::TableOps;

/// Cacheability class of a mapping. Device memory has side effects on access and must never be
/// speculatively fetched as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemKind {
    /// Ordinary cacheable RAM.
    Normal,
    /// MMIO / device registers.
    Device,
}

/// The permissions of one mapping, decoded from a target's descriptor bits into arch-neutral terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageAttrs {
    /// Cacheability class.
    pub kind: MemKind,
    /// Writable at the privilege level(s) that can access it.
    pub write: bool,
    /// Executable at user privilege (EL0 / U-mode / ring 3).
    pub exec_user: bool,
    /// Executable at kernel privilege (EL1 / S-mode / ring 0).
    pub exec_kernel: bool,
    /// Accessible to user privilege at all.
    pub user: bool,
}

impl PageAttrs {
    /// Is this mapping executable at any privilege level?
    pub const fn executable(&self) -> bool {
        self.exec_user || self.exec_kernel
    }

    /// Check the rules. Order is diagnostic only — any failure refuses the mapping.
    pub fn validate(&self) -> Result<(), AttrFault> {
        if self.write && self.executable() {
            return Err(AttrFault::WriteExecute);
        }
        if matches!(self.kind, MemKind::Device) && self.executable() {
            return Err(AttrFault::ExecutableDevice);
        }
        if self.user && self.exec_kernel {
            return Err(AttrFault::UserPageKernelExecutable);
        }
        if self.exec_user && !self.user {
            // A page the user cannot reach cannot meaningfully be user-executable; a descriptor
            // saying both is a mis-encoding, and mis-encoded permissions are how W^X gets lost.
            return Err(AttrFault::InconsistentUserExec);
        }
        Ok(())
    }
}

/// Why a mapping's attributes were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrFault {
    /// Writable AND executable — the mapping that turns a write primitive into code execution.
    WriteExecute,
    /// An executable device/MMIO mapping.
    ExecutableDevice,
    /// A user-accessible page that is executable at kernel privilege (ret2usr).
    UserPageKernelExecutable,
    /// Marked user-executable while not user-accessible: a mis-encoded descriptor.
    InconsistentUserExec,
}

impl AttrFault {
    /// Stable short name, for invariant logs on targets with no formatter.
    pub const fn as_str(self) -> &'static str {
        match self {
            AttrFault::WriteExecute => "attr-write-exec",
            AttrFault::ExecutableDevice => "attr-exec-device",
            AttrFault::UserPageKernelExecutable => "attr-user-kernel-exec",
            AttrFault::InconsistentUserExec => "attr-user-exec-inconsistent",
        }
    }
}

/// What an audit of a live address space found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuditReport {
    /// Leaf mappings examined.
    pub leaves: usize,
    /// Violations among dynamically-created last-level leaves. A gate requires this to be ZERO.
    pub dynamic_violations: usize,
    /// Violations among bootstrap block/huge/firmware descriptors. Pinned by each gate, not hidden.
    pub bootstrap_violations: usize,
    /// The first fault found, for a log line that says what broke rather than that something did.
    pub first_fault: Option<AttrFault>,
}

/// A target's ability to decode one of its own leaf descriptors.
pub trait AttrOps: TableOps {
    /// Paging levels (3 for aarch64 TTBR0 / RISC-V Sv39, 4 for x86-64).
    fn levels(&self) -> usize;
    /// Does this entry map memory directly rather than point at a table?
    fn is_leaf(&self, entry: u64, level: usize) -> bool;
    /// Physical address an entry points at.
    fn entry_addr(&self, entry: u64) -> usize;
    /// Decode a leaf's permissions and cacheability.
    fn decode(&self, entry: u64, level: usize) -> PageAttrs;

    /// Is slot `index` at `level` part of what THIS kernel mapped? Defaults to the whole tree.
    ///
    /// x86-64 needs it: the live root is the one OVMF built, and its half-million firmware leaves —
    /// writable and executable, inherited, not ours to fix — would otherwise drown the audit and
    /// make "zero violations among our own mappings" unprovable. Scoping is the honest way to state
    /// a property about the mappings a target actually controls; the firmware's are reported
    /// separately by auditing without a scope.
    fn in_scope(&self, _level: usize, _index: usize) -> bool {
        true
    }
}

/// Walk every mapping reachable from `root` and check each leaf against the rules.
///
/// A leaf at the last level is a page some mapping API produced (counted as `dynamic`); a leaf
/// higher up is a block/huge descriptor from the bootstrap map (counted as `bootstrap`). The split
/// is what lets a gate demand zero dynamic violations while stating the bootstrap count as a known,
/// pinned number instead of quietly averaging the two together.
pub fn audit<T: AttrOps>(root: usize, ops: &T) -> AuditReport {
    let mut out = AuditReport::default();
    audit_level(root, 0, ops, &mut out);
    out
}

fn audit_level<T: AttrOps>(table: usize, level: usize, ops: &T, out: &mut AuditReport) {
    for index in 0..ops.entries_per_table() {
        if !ops.in_scope(level, index) {
            continue;
        }
        let entry = ops.read(table, index);
        if !ops.is_present(entry) {
            continue;
        }
        if ops.is_leaf(entry, level) {
            out.leaves += 1;
            if let Err(f) = ops.decode(entry, level).validate() {
                if level + 1 == ops.levels() {
                    out.dynamic_violations += 1;
                } else {
                    out.bootstrap_violations += 1;
                }
                if out.first_fault.is_none() {
                    out.first_fault = Some(f);
                }
            }
        } else if level + 1 < ops.levels() {
            audit_level(ops.entry_addr(entry), level + 1, ops, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    const RX_KERNEL: PageAttrs = PageAttrs {
        kind: MemKind::Normal,
        write: false,
        exec_user: false,
        exec_kernel: true,
        user: false,
    };
    const RW_KERNEL: PageAttrs = PageAttrs {
        kind: MemKind::Normal,
        write: true,
        exec_user: false,
        exec_kernel: false,
        user: false,
    };
    const RX_USER: PageAttrs = PageAttrs {
        kind: MemKind::Normal,
        write: false,
        exec_user: true,
        exec_kernel: false,
        user: true,
    };

    #[test]
    fn the_three_legal_shapes_are_accepted() {
        assert_eq!(RX_KERNEL.validate(), Ok(()));
        assert_eq!(RW_KERNEL.validate(), Ok(()));
        assert_eq!(RX_USER.validate(), Ok(()));
    }

    #[test]
    fn writable_and_executable_is_refused_at_either_privilege_level() {
        let mut a = RX_KERNEL;
        a.write = true;
        assert_eq!(a.validate(), Err(AttrFault::WriteExecute));
        let mut b = RX_USER;
        b.write = true;
        assert_eq!(b.validate(), Err(AttrFault::WriteExecute));
    }

    #[test]
    fn an_executable_device_mapping_is_refused() {
        let a = PageAttrs {
            kind: MemKind::Device,
            write: false,
            exec_user: false,
            exec_kernel: true,
            user: false,
        };
        assert_eq!(a.validate(), Err(AttrFault::ExecutableDevice));
        // ...while a non-executable device mapping is exactly what MMIO should look like.
        let b = PageAttrs {
            kind: MemKind::Device,
            write: true,
            exec_kernel: false,
            ..a
        };
        assert_eq!(b.validate(), Ok(()));
    }

    #[test]
    fn a_user_page_that_is_kernel_executable_is_refused() {
        let a = PageAttrs {
            exec_kernel: true,
            ..RX_USER
        };
        assert_eq!(a.validate(), Err(AttrFault::UserPageKernelExecutable));
    }

    #[test]
    fn user_executable_without_user_access_is_a_mis_encoding() {
        let a = PageAttrs {
            user: false,
            ..RX_USER
        };
        assert_eq!(a.validate(), Err(AttrFault::InconsistentUserExec));
    }

    #[test]
    fn every_fault_has_a_distinct_stable_name() {
        let all = [
            AttrFault::WriteExecute,
            AttrFault::ExecutableDevice,
            AttrFault::UserPageKernelExecutable,
            AttrFault::InconsistentUserExec,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(a.as_str().starts_with("attr-"));
            for b in all.iter().skip(i + 1) {
                assert_ne!(a.as_str(), b.as_str());
            }
        }
    }

    // --- the audit, over a model tree -------------------------------------------------------

    const ENTRIES: usize = 4;
    const LEVELS: usize = 3;
    const ROOT: usize = 0x1000;
    const MID: usize = 0x2000;
    const LEAFT: usize = 0x3000;
    const LEAF_BIT: u64 = 1 << 62;
    const WRITE_BIT: u64 = 1 << 60;
    const EXEC_BIT: u64 = 1 << 59;

    struct Model {
        tables: Vec<(usize, Vec<u64>)>,
    }

    impl TableOps for Model {
        fn entries_per_table(&self) -> usize {
            ENTRIES
        }
        fn read(&self, table: usize, index: usize) -> u64 {
            self.tables
                .iter()
                .find(|(a, _)| *a == table)
                .expect("table")
                .1[index]
        }
        fn write(&mut self, table: usize, index: usize, value: u64) {
            self.tables
                .iter_mut()
                .find(|(a, _)| *a == table)
                .expect("table")
                .1[index] = value;
        }
        fn is_present(&self, entry: u64) -> bool {
            entry != 0
        }
        fn free_table(&mut self, _table: usize) -> bool {
            unreachable!("the audit never frees")
        }
    }

    impl AttrOps for Model {
        fn levels(&self) -> usize {
            LEVELS
        }
        fn is_leaf(&self, entry: u64, _level: usize) -> bool {
            entry & LEAF_BIT != 0
        }
        fn entry_addr(&self, entry: u64) -> usize {
            (entry & 0xFFF_FFFF) as usize
        }
        fn decode(&self, entry: u64, _level: usize) -> PageAttrs {
            PageAttrs {
                kind: MemKind::Normal,
                write: entry & WRITE_BIT != 0,
                exec_user: false,
                exec_kernel: entry & EXEC_BIT != 0,
                user: false,
            }
        }
    }

    fn tree() -> Model {
        let mut m = Model {
            tables: vec![
                (ROOT, vec![0u64; ENTRIES]),
                (MID, vec![0u64; ENTRIES]),
                (LEAFT, vec![0u64; ENTRIES]),
            ],
        };
        m.write(ROOT, 0, MID as u64);
        m.write(MID, 0, LEAFT as u64);
        m
    }

    #[test]
    fn a_clean_tree_audits_with_no_violations() {
        let mut m = tree();
        m.write(LEAFT, 0, 0x5000 | LEAF_BIT | WRITE_BIT); // RW data page
        m.write(LEAFT, 1, 0x6000 | LEAF_BIT | EXEC_BIT); // RX code page
        let r = audit(ROOT, &m);
        assert_eq!(r.leaves, 2);
        assert_eq!(r.dynamic_violations, 0);
        assert_eq!(r.bootstrap_violations, 0);
        assert_eq!(r.first_fault, None);
    }

    #[test]
    fn a_writable_executable_leaf_is_counted_by_where_it_came_from() {
        let mut m = tree();
        // A dynamic (last-level) W+X page: the class a gate requires to be empty.
        m.write(LEAFT, 0, 0x5000 | LEAF_BIT | WRITE_BIT | EXEC_BIT);
        // A bootstrap (upper-level block) W+X descriptor: known, counted separately.
        m.write(MID, 1, 0x7000 | LEAF_BIT | WRITE_BIT | EXEC_BIT);
        let r = audit(ROOT, &m);
        assert_eq!(r.dynamic_violations, 1);
        assert_eq!(r.bootstrap_violations, 1);
        assert_eq!(r.first_fault, Some(AttrFault::WriteExecute));
        assert_eq!(r.leaves, 2);
    }

    #[test]
    fn the_audit_reaches_every_branch_of_the_tree() {
        let mut m = tree();
        let second_leaf = 0x4000usize;
        m.tables.push((second_leaf, vec![0u64; ENTRIES]));
        m.write(MID, 2, second_leaf as u64);
        m.write(LEAFT, 0, 0x5000 | LEAF_BIT | WRITE_BIT);
        m.write(second_leaf, 3, 0x8000 | LEAF_BIT | WRITE_BIT | EXEC_BIT);
        let r = audit(ROOT, &m);
        assert_eq!(r.leaves, 2, "both branches were walked");
        assert_eq!(
            r.dynamic_violations, 1,
            "the violation in the far branch was found"
        );
    }
}
