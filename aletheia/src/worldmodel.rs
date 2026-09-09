//! World model: provenance-aware relationship traversal over the store (PRD-002 §19, SAD §14).
use crate::domain::Id;
use crate::storage::Store;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Outgoing,
    Incoming,
}

/// Breadth-first traversal from `from` following edges of `rtype` in `dir` up to `depth`.
/// Returns the reachable entity ids (excluding the origin), preserving discovery order.
pub fn traverse(store: &Store, from: &Id, rtype: &str, dir: Dir, depth: u32) -> Vec<Id> {
    traverse_filtered(store, from, rtype, dir, depth, |_| true)
}

/// Capability/scoping seam for graph traversal. The predicate is applied to the origin and every
/// candidate before that node becomes traversable or observable. This matters because filtering
/// only the final result still permits an unauthorized node to act as a hidden bridge between two
/// authorized nodes, leaking a relationship through graph reachability.
pub fn traverse_filtered<F>(
    store: &Store,
    from: &Id,
    rtype: &str,
    dir: Dir,
    depth: u32,
    mut allowed: F,
) -> Vec<Id>
where
    F: FnMut(&Id) -> bool,
{
    if !allowed(from) {
        return Vec::new();
    }
    let mut seen: HashSet<Id> = HashSet::new();
    seen.insert(from.clone());
    let mut frontier = vec![from.clone()];
    let mut out = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in &frontier {
            for r in store.relationships() {
                if r.rtype != rtype {
                    continue;
                }
                let hit = match dir {
                    Dir::Outgoing if &r.from == node => Some(r.to.clone()),
                    Dir::Incoming if &r.to == node => Some(r.from.clone()),
                    _ => None,
                };
                if let Some(target) = hit {
                    if allowed(&target) && seen.insert(target.clone()) {
                        out.push(target.clone());
                        next.push(target);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{new_id, Entity, EntityType, Provenance, Relationship};

    fn tmp() -> String {
        std::env::temp_dir()
            .join(format!("aletheia-world-{}", new_id()))
            .to_string_lossy()
            .into_owned()
    }

    fn entity(id: Id) -> Entity {
        Entity {
            id,
            etype: EntityType::Document,
            content_ref: None,
            version: 1,
            version_chain: new_id(),
            metadata: serde_json::json!({}),
            provenance: Provenance::of("test"),
            created_at: 0,
            updated_at: 0,
            deleted: false,
        }
    }

    #[test]
    fn filtered_traversal_does_not_cross_or_reveal_forbidden_nodes() {
        let mut store = Store::open(tmp()).unwrap();
        let a = new_id();
        let hidden = new_id();
        let b = new_id();
        for id in [a.clone(), hidden.clone(), b.clone()] {
            store.put_entity(&entity(id)).unwrap();
        }
        store
            .put_relationship(&Relationship {
                id: new_id(),
                rtype: "related".into(),
                from: a.clone(),
                to: hidden.clone(),
                provenance: Provenance::of("test"),
                created_at: 0,
            })
            .unwrap();
        store
            .put_relationship(&Relationship {
                id: new_id(),
                rtype: "related".into(),
                from: hidden.clone(),
                to: b.clone(),
                provenance: Provenance::of("test"),
                created_at: 0,
            })
            .unwrap();

        let ids = traverse_filtered(&store, &a, "related", Dir::Outgoing, 4, |id| id != &hidden);
        assert!(ids.is_empty(), "forbidden bridge must stop traversal");
    }
}
