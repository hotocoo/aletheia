//! P3 experience layer — capability-gated keyword search over the World Model (ADR-018 search seam).
//!
//! Search is subject to the SAME capability discipline as everything else: results are
//! authorization-before-inclusion. A caller sees only entities it may `entity.read`; an entity it
//! cannot read never appears even when it matches; a caller with no read authority sees nothing.
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

fn tmp() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-search-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(tmp(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

#[test]
fn search_is_capability_gated_ranked_and_fail_closed() {
    let (mut core, owner) = open();

    // Owner creates two documents. e1 matches two query terms, e2 matches one.
    let e1 = core
        .create_entity(std::slice::from_ref(&owner), "human:owner", EntityType::Document, b"quarterly revenue report alpha", serde_json::json!({}))
        .unwrap();
    let e2 = core
        .create_entity(std::slice::from_ref(&owner), "human:owner", EntityType::Document, b"revenue only", serde_json::json!({}))
        .unwrap();

    // Full authority: both match "revenue"; only e1 also matches "alpha" -> e1 ranks first.
    let hits = core.search(std::slice::from_ref(&owner), "revenue alpha", 10);
    assert!(hits.iter().any(|h| h.id == e1.id && h.score == 2), "e1 matches both terms");
    assert!(hits.iter().any(|h| h.id == e2.id && h.score == 1), "e2 matches one term");
    assert_eq!(hits[0].id, e1.id, "most relevant first");
    assert!(!hits[0].snippet.is_empty(), "a hit carries a match excerpt");

    // A reader authorized ONLY for e2 never sees e1, even though e1 matches the query.
    let read_e2 = core
        .grant_to(std::slice::from_ref(&owner), "agent:reader", "entity.read", Scope::Entities(vec![e2.id.clone()]), Constraints::none())
        .unwrap()
        .token;
    let scoped = core.search(&[read_e2], "revenue", 10);
    assert_eq!(scoped.len(), 1, "only the authorized entity may appear");
    assert_eq!(scoped[0].id, e2.id);

    // No capability -> nothing (fail closed; no ambient authority).
    assert!(core.search(&[], "revenue", 10).is_empty(), "no read authority => no results");

    // A query that matches nothing returns nothing.
    assert!(core.search(&[owner], "nonexistentterm", 10).is_empty());
}
