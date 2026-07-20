//! World model: provenance-aware relationship traversal over the store (PRD-002 §19, SAD §14).
use crate::domain::Id;
use crate::storage::Store;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir { Outgoing, Incoming }

/// Breadth-first traversal from `from` following edges of `rtype` in `dir` up to `depth`.
/// Returns the reachable entity ids (excluding the origin), preserving discovery order.
pub fn traverse(store: &Store, from: &Id, rtype: &str, dir: Dir, depth: u32) -> Vec<Id> {
    let mut seen: HashSet<Id> = HashSet::new();
    seen.insert(from.clone());
    let mut frontier = vec![from.clone()];
    let mut out = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in &frontier {
            for r in store.relationships() {
                if r.rtype != rtype { continue; }
                let hit = match dir {
                    Dir::Outgoing if &r.from == node => Some(r.to.clone()),
                    Dir::Incoming if &r.to == node => Some(r.from.clone()),
                    _ => None,
                };
                if let Some(target) = hit {
                    if seen.insert(target.clone()) {
                        out.push(target.clone());
                        next.push(target);
                    }
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    out
}
