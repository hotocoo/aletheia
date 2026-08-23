//! P2 acceptance - component installation SUPPLY-CHAIN verification (ALET-P1-023, ADR-067).
//!
//! Provenance is a CHAIN, enforced at BOTH ends, LIVE at every launch:
//!
//! * a root key endorses component-signing keys; an endorsed signer signs artifacts; the whole
//!   chain verifies against PUBLIC keys only before anything enters the record;
//! * every artifact carries the evidence of WHAT vouched for it (root, signer, signatures), and
//!   the launch gate RE-JUDGES that evidence against CURRENT trust - so revoking a signer goes
//!   live at the next launch, including for components already admitted;
//! * the spawn path passes through the SAME gate as run_installed - a spawn request cannot be
//!   used as a side door around secure policy (ALET-P2-050);
//! * refusals name WHICH link failed (revoked signer / unendorsed signer / bad signature),
//!   because rotate-the-key and rebuild-the-artifact are different responses.
use aletheia::intelligence::DeterministicRuntime;
use aletheia::provenance::{ComponentProvenance, SigningIdentity};
use aletheia::syscore::SysCore;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-supply-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

/// A minimal current-ABI guest.
fn guest() -> Vec<u8> {
    let mut w = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32) (i32.const 0)))"#,
    )
    .expect("guest wat compiles");
    aletheia::component::stamp_abi_section(&mut w, aletheia::component::ABI_VERSION);
    w
}

/// A guest whose code DIFFERS from `guest()` - used to prove signatures cover content.
fn other_guest() -> Vec<u8> {
    let mut w = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32) (i32.const 42)))"#,
    )
    .expect("other wat compiles");
    aletheia::component::stamp_abi_section(&mut w, aletheia::component::ABI_VERSION);
    w
}

/// Direct-root provenance over `wasm`.
fn direct_prov(root: &SigningIdentity, wasm: &[u8]) -> ComponentProvenance {
    ComponentProvenance {
        signer: root.public_key(),
        component_sig: root.sign(&aletheia::crypto::sha256_hex(wasm)),
        endorsement: None,
    }
}

/// Chain provenance: `root` endorses `signer`, who signs `wasm`.
fn chain_prov(
    root: &SigningIdentity,
    signer: &SigningIdentity,
    wasm: &[u8],
) -> ComponentProvenance {
    ComponentProvenance {
        signer: signer.public_key(),
        component_sig: signer.sign(&aletheia::crypto::sha256_hex(wasm)),
        endorsement: Some(root.endorse(&signer.public_key())),
    }
}

/// A parent that spawns `child_id` - built fresh so each test embeds its own child id.
fn spawner_for(child_id: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "spawn" (func $spawn (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{child_id}")
  (data (i32.const 64) "entity.read")
  (func (export "run") (result i32)
    (drop (call $spawn (i32.const 0) (i32.const {idlen}) (i32.const 64) (i32.const 11)))
    (i32.const 0)))"#,
        child_id = child_id,
        idlen = child_id.len()
    );
    let mut w = wat::parse_str(&wat).expect("spawner wat compiles");
    aletheia::component::stamp_abi_section(&mut w, aletheia::component::ABI_VERSION);
    w
}

/// A root-signed component installs, and RUNS under secure policy - the happy path of the whole
/// supply chain, end to end.
#[test]
fn root_signed_component_installs_and_runs_under_secure_policy() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([1u8; 32]);
    core.trust_component_root(root.public_key());
    let wasm = guest();
    let app = core
        .install_verified_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "rooted",
            &wasm,
            &direct_prov(&root, &wasm),
        )
        .expect("a root-signed component is admitted");

    core.set_require_signed_components(true);
    let outcome = core
        .run_installed(
            std::slice::from_ref(&owner),
            &[],
            "operator",
            &app.id,
            100_000,
        )
        .expect("secure policy admits what the anchor vouches for");
    assert!(outcome.ok, "{:?}", outcome.error);
}

/// A signing key endorsed by NO trusted root is refused AT INSTALL, with the fault NAMED -
/// a key outside the chain never gets its artifact into the record.
#[test]
fn unendorsed_signer_is_refused_at_install_by_name() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([1u8; 32]);
    core.trust_component_root(root.public_key()); // a root exists, but never endorsed anyone below
    let rogue = SigningIdentity::from_seed([9u8; 32]);
    let wasm = guest();
    // The rogue self-consistently signs its own artifact; the chain still fails because NO trusted
    // root endorsed the rogue key.
    let prov = ComponentProvenance {
        signer: rogue.public_key(),
        component_sig: rogue.sign(&aletheia::crypto::sha256_hex(&wasm)),
        endorsement: Some(rogue.endorse(&rogue.public_key())),
    };
    let err = core
        .install_verified_component(&[owner], "human:owner", "rogue", &wasm, &prov)
        .unwrap_err();
    assert!(err.to_string().contains("not endorsed"), "named: {err}");
    assert_eq!(count(core.store(), "ComponentSignatureRejected"), 1);
}

/// A valid signature over DIFFERENT bytes is refused: the chain vouches for content, and the
/// content-addressed hash is what it covers - swap the artifact and the chain goes dark.
#[test]
fn tampered_artifact_is_refused_because_the_signature_covers_the_hash() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([2u8; 32]);
    core.trust_component_root(root.public_key());
    let signed = guest();
    let swapped = other_guest(); // different code than what was signed
    let prov = direct_prov(&root, &signed); // signature over the ORIGINAL bytes
    let err = core
        .install_verified_component(&[owner], "human:owner", "swapped", &swapped, &prov)
        .unwrap_err();
    assert!(err.to_string().contains("does not verify"), "named: {err}");
}

/// THE headline property: revocation is LIVE at launch. A chain-signed component admitted while
/// its signer was trusted runs once, then the operator revokes the SIGNER, and the very next
/// launch is refused BY NAME - without touching the stored bytes or reinstalling anything.
#[test]
fn revoking_a_signer_goes_live_at_the_next_launch() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([3u8; 32]);
    let signer = SigningIdentity::from_seed([4u8; 32]);
    core.trust_component_root(root.public_key());
    let wasm = guest();
    let app = core
        .install_verified_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "doomed",
            &wasm,
            &chain_prov(&root, &signer, &wasm),
        )
        .expect("admitted while the signer was trusted");
    core.set_require_signed_components(true);

    let first = core.run_installed(std::slice::from_ref(&owner), &[], "op", &app.id, 100_000);
    assert!(first.is_ok(), "runs while trusted: {:?}", first.err());

    core.revoke_component_signer(signer.public_key());
    assert_eq!(
        count(core.store(), "ComponentSignerRevoked"),
        1,
        "revocation is audited"
    );

    let second = core.run_installed(std::slice::from_ref(&owner), &[], "op", &app.id, 100_000);
    assert!(
        second.is_err(),
        "the SAME artifact is refused after revocation"
    );
    // And the refusal is audited by the live launch gate.
    assert!(
        core.store().events().iter().any(|e| {
            e.etype == "ComponentSignatureRejected"
                && e.payload
                    .to_string()
                    .contains("untrusted, tampered, or revoked")
        }),
        "the live-gate rejection is audited"
    );
}

/// ALET-P2-050, closed: the SPAWN path passes the SAME gate. An unsigned application that slipped
/// into the store cannot be launched by spawning it from a legitimately-running parent - the
/// child's absence is visible as an audited refusal, not silently swallowed.
#[test]
fn the_spawn_path_passes_through_the_same_provenance_gate() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([5u8; 32]);
    core.trust_component_root(root.public_key());

    // The UNSIGNED child slips into the store while policy is off (an attacker with
    // component.install can do exactly this).
    let unsigned_child = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "smuggled",
            &guest(),
        )
        .unwrap();

    // A legitimately signed parent that will try to spawn it.
    let parent_wasm = spawner_for(&unsigned_child.id);
    let parent = core
        .install_verified_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "honest-parent",
            &parent_wasm,
            &direct_prov(&root, &parent_wasm),
        )
        .unwrap();

    core.set_require_signed_components(true);
    let outcome = core
        .run_installed(
            std::slice::from_ref(&owner),
            &[],
            "op",
            &parent.id,
            1_000_000,
        )
        .unwrap();

    assert!(outcome.ok, "the parent itself ran");
    assert_eq!(outcome.spawns.len(), 1, "the spawn was queued");
    assert_eq!(
        outcome.spawned.len(),
        0,
        "but the smuggled child NEVER RAN - the gate held"
    );
    assert!(
        core.store().events().iter().any(|e| {
            e.etype == "ComponentSignatureRejected"
                && e.payload.to_string().contains("launch-provenance")
        }),
        "the side-door attempt is in the audit log"
    );
}

/// Positive control for the same gate: a properly VERIFIED child spawns and runs under secure
/// policy - the gate refuses smuggled code, not composition itself.
#[test]
fn a_verified_child_spawns_normally_under_secure_policy() {
    let (mut core, owner) = open();
    let root = SigningIdentity::from_seed([6u8; 32]);
    core.trust_component_root(root.public_key());

    let child_wasm = guest();
    let child = core
        .install_verified_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "verified-child",
            &child_wasm,
            &direct_prov(&root, &child_wasm),
        )
        .unwrap();

    let parent_wasm = spawner_for(&child.id);
    let parent = core
        .install_verified_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "honest-parent",
            &parent_wasm,
            &direct_prov(&root, &parent_wasm),
        )
        .unwrap();

    core.set_require_signed_components(true);
    let outcome = core
        .run_installed(
            std::slice::from_ref(&owner),
            &[],
            "op",
            &parent.id,
            1_000_000,
        )
        .unwrap();
    assert_eq!(outcome.spawned.len(), 1, "the verified child ran");
    assert!(outcome.spawned[0].ok);
}

/// Regression guard: the ad-hoc path keeps refusing raw wasm under secure policy.
#[test]
fn adhoc_runs_stay_refused_under_secure_policy() {
    let (mut core, owner) = open();
    core.set_require_signed_components(true);
    let err = core
        .run_component(std::slice::from_ref(&owner), &[], "c", &guest(), 100_000)
        .unwrap_err();
    assert!(err.to_string().contains("secure policy"));
}

fn count(store: &aletheia::storage::Store, etype: &str) -> usize {
    store.events().iter().filter(|e| e.etype == etype).count()
}
