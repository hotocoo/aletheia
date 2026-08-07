//! Virtual addresses that must be dead in EVERY address space (REQ-MM-007/008, ALET-P2-033).
//!
//! Two pages are deliberately given no descriptor by each target's kernel map: **VA 0**, so a null
//! dereference faults instead of reading real state, and the **ring-0 stack guard**, so a stack
//! overflow faults instead of writing whatever is below the stack. ALET-P1-006/012 proved both — but
//! only of the map the kernel built for itself.
//!
//! That is the narrower claim, and the gap it left is the reason this module exists. A *derived*
//! space — the per-process root a target builds for ring-3 — is a different tree. On x86-64 it is
//! built by COPYING the live top-level table, so it inherits whatever that table happened to hold: a
//! space copied before the kernel's own map was active mapped the guard region as one 2 MiB huge
//! page. A user space could therefore reach an address the kernel's own map deliberately cannot,
//! which inverts the property — the guard exists to protect the more privileged tree.
//!
//! So the rule is stated here once, arch-neutrally, and applied to every root rather than to one:
//!
//! * a target DECLARES its dead spans ([`DeadSet`]);
//! * [`audit`] walks them in ANY space, through caller-supplied translation and leaf lookups, and
//!   reports every page that is reachable or merely *described*; and
//! * an EMPTY declaration is itself a violation ([`DeadFault::Undeclared`]) — a target that forgets
//!   to declare fails the audit rather than passing it vacuously, the same fail-closed posture
//!   [`crate::dma`] takes toward an undeclared image span.
//!
//! Translation alone is not enough to ask. A page can be unreachable *and* still be covered by a
//! present block/huge descriptor at a higher level (an entry whose sub-range is what makes it
//! unreachable); a later split or permission change over that descriptor would silently revive the
//! address. The guard is the ABSENCE of a leaf, so the audit asks for both.
//!
//! The contract is `docs/INVARIANT-CONTRACTS.md` §INV-DEADVA.

/// A named, half-open span of virtual addresses that must have no mapping in any address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadSpan {
    pub name: &'static str,
    pub start: usize,
    /// Exclusive end. `start >= end` is an empty span: declared but not present on this target.
    pub end: usize,
}

impl DeadSpan {
    pub const fn new(name: &'static str, start: usize, end: usize) -> Self {
        DeadSpan { name, start, end }
    }
    /// A single page, the common case (VA 0, one guard page).
    pub const fn page(name: &'static str, start: usize) -> Self {
        DeadSpan {
            name,
            start,
            end: start + PAGE,
        }
    }
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
    pub const fn pages(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            (self.end - self.start).div_ceil(PAGE)
        }
    }
    pub const fn contains(&self, va: usize) -> bool {
        va >= self.start && va < self.end
    }
}

/// Page size every target uses (mirrors [`crate::layout::PAGE`]).
pub const PAGE: usize = crate::layout::PAGE;
/// Dead spans a target may declare. Small and fixed so this stays `no_std` and allocation-free.
pub const MAX_SPANS: usize = 8;
/// A ceiling on pages audited per span, so a fat or malformed declaration cannot make a boot suite
/// walk the address space forever. A span over the ceiling is reported as
/// [`DeadFault::SpanTooLarge`] rather than silently truncated — a partial walk that reported success
/// would be the vacuous pass this module exists to refuse.
pub const MAX_PAGES_PER_SPAN: usize = 1024;

/// Why a space failed the audit. Every variant names a page (or span) and the property it broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadFault {
    /// Nothing was declared, so the audit could prove nothing. Fail-closed: a target that forgets
    /// its declaration must not look identical to one that has no dead pages.
    Undeclared,
    /// A declared span is inverted or unaligned: it cannot be walked as stated.
    Malformed(&'static str),
    /// A span declares more pages than the audit will walk (see [`MAX_PAGES_PER_SPAN`]).
    SpanTooLarge(&'static str),
    /// The page TRANSLATES: the space can reach an address that must be dead.
    Reachable(&'static str, usize),
    /// The page does not translate, but a descriptor still covers it — a split or permission change
    /// over that descriptor would revive the address without anything mapping it.
    Described(&'static str, usize),
}

/// What an audit found. `pages` is what was actually walked, so a caller can refuse a report that
/// proved nothing rather than reading `violations == 0` as success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeadReport {
    /// Pages walked across every declared span.
    pub pages: usize,
    /// Spans that were non-empty and therefore contributed pages.
    pub spans: usize,
    /// Total violations found (the walk does not stop at the first).
    pub violations: usize,
    /// The first violation, for a one-line boot report.
    pub first: Option<DeadFault>,
}

impl DeadReport {
    /// A report is CLEAN only if it walked something and found nothing. Both halves matter: an audit
    /// that walked zero pages has proved nothing, and reporting that as a pass is exactly how a
    /// refactor that stopped declaring would keep every gate green.
    pub const fn clean(&self) -> bool {
        self.pages > 0 && self.violations == 0 && self.first.is_none()
    }
}

/// The spans one target declares dead.
#[derive(Clone, Copy, Debug)]
pub struct DeadSet {
    pub arch: &'static str,
    spans: [DeadSpan; MAX_SPANS],
    count: usize,
}

impl DeadSet {
    pub const fn new(arch: &'static str) -> Self {
        DeadSet {
            arch,
            spans: [DeadSpan::new("", 0, 0); MAX_SPANS],
            count: 0,
        }
    }

    /// Declare a span. Anything past [`MAX_SPANS`] is ignored, which is why [`DeadReport::spans`] is
    /// asserted by the tests rather than trusted.
    pub const fn with(mut self, s: DeadSpan) -> Self {
        if self.count < MAX_SPANS {
            self.spans[self.count] = s;
            self.count += 1;
        }
        self
    }

    pub fn live(&self) -> impl Iterator<Item = &DeadSpan> {
        self.spans[..self.count].iter().filter(|s| !s.is_empty())
    }

    /// Total pages this set claims are dead.
    pub fn pages(&self) -> usize {
        self.live().map(|s| s.pages()).sum()
    }

    /// Which declared span contains `va`, if any.
    pub fn span_of(&self, va: usize) -> Option<&DeadSpan> {
        self.live().find(|s| s.contains(va))
    }

    /// Check the declaration itself, before any space is walked. A malformed declaration is refused
    /// here so a target reports "your spans are wrong" rather than a confusing per-page verdict.
    pub fn validate(&self) -> Result<(), DeadFault> {
        if self.live().next().is_none() {
            return Err(DeadFault::Undeclared);
        }
        for s in self.spans[..self.count].iter() {
            if s.is_empty() {
                continue;
            }
            if !s.start.is_multiple_of(PAGE) || !s.end.is_multiple_of(PAGE) {
                return Err(DeadFault::Malformed(s.name));
            }
            if s.pages() > MAX_PAGES_PER_SPAN {
                return Err(DeadFault::SpanTooLarge(s.name));
            }
        }
        Ok(())
    }
}

/// Audit one address space against a declaration.
///
/// `translate(va)` answers whether the space can REACH the page; `described(va)` answers whether any
/// descriptor at any level covers it (a target's leaf lookup, which sees block/huge entries a
/// translation of an unmapped sub-range would not reveal). Both are caller-supplied because the walk
/// is the only architecture-specific part of the property.
///
/// The walk does not stop at the first violation: a space that revived two pages should say so, and
/// a caller that reports only the first would under-state the breach.
pub fn audit<T, D>(set: &DeadSet, mut translate: T, mut described: D) -> DeadReport
where
    T: FnMut(usize) -> Option<usize>,
    D: FnMut(usize) -> bool,
{
    let mut report = DeadReport::default();
    if let Err(fault) = set.validate() {
        report.violations = 1;
        report.first = Some(fault);
        return report;
    }
    for span in set.live() {
        report.spans += 1;
        let mut va = span.start;
        while va < span.end {
            report.pages += 1;
            if translate(va).is_some() {
                report.violations += 1;
                if report.first.is_none() {
                    report.first = Some(DeadFault::Reachable(span.name, va));
                }
            } else if described(va) {
                report.violations += 1;
                if report.first.is_none() {
                    report.first = Some(DeadFault::Described(span.name, va));
                }
            }
            va += PAGE;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUARD: usize = 0x8000;

    fn set() -> DeadSet {
        DeadSet::new("test")
            .with(DeadSpan::page("null", 0))
            .with(DeadSpan::page("stack-guard", GUARD))
    }

    #[test]
    fn a_space_with_neither_page_mapped_is_clean() {
        let r = audit(&set(), |_| None, |_| false);
        assert!(r.clean());
        assert_eq!(r.pages, 2);
        assert_eq!(r.spans, 2);
    }

    #[test]
    fn a_reachable_dead_page_is_a_violation() {
        let r = audit(&set(), |va| (va == GUARD).then_some(0x1000), |_| false);
        assert_eq!(r.violations, 1);
        assert_eq!(r.first, Some(DeadFault::Reachable("stack-guard", GUARD)));
        assert!(!r.clean());
    }

    #[test]
    fn a_described_but_unreachable_dead_page_is_still_a_violation() {
        // The exact shape of the x86-64 hole: the page does not translate, yet a 2 MiB descriptor
        // covers it, so one split away it is alive again.
        let r = audit(&set(), |_| None, |va| va == GUARD);
        assert_eq!(r.violations, 1);
        assert_eq!(r.first, Some(DeadFault::Described("stack-guard", GUARD)));
    }

    #[test]
    fn an_empty_declaration_proves_nothing_and_fails() {
        let r = audit(&DeadSet::new("t"), |_| None, |_| false);
        assert_eq!(r.first, Some(DeadFault::Undeclared));
        assert!(!r.clean());
        assert_eq!(r.pages, 0);
    }

    #[test]
    fn every_violation_is_counted_not_only_the_first() {
        let r = audit(&set(), |_| Some(0x2000), |_| false);
        assert_eq!(r.violations, 2);
        assert_eq!(r.first, Some(DeadFault::Reachable("null", 0)));
    }

    #[test]
    fn a_malformed_or_oversized_declaration_is_refused_before_any_walk() {
        let unaligned = DeadSet::new("t").with(DeadSpan::new("bad", 0x1001, 0x2001));
        assert_eq!(unaligned.validate(), Err(DeadFault::Malformed("bad")));

        let huge = DeadSet::new("t").with(DeadSpan::new(
            "fat",
            PAGE,
            PAGE + (MAX_PAGES_PER_SPAN + 1) * PAGE,
        ));
        assert_eq!(huge.validate(), Err(DeadFault::SpanTooLarge("fat")));

        // Refused BEFORE the walk: nothing was audited, so nothing may be claimed.
        let r = audit(&unaligned, |_| None, |_| false);
        assert_eq!(r.pages, 0);
        assert!(!r.clean());
    }
}
