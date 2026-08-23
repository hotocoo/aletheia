//! P2 acceptance - the component ABI is EXPLICITLY versioned (ALET-P1-022, ADR-066).
//!
//! A component must DECLARE which ABI it speaks: a custom section named "aletheia.abi" carrying
//! its version as four little-endian bytes. The runtime enforces that declaration at BOTH gates:
//!
//! * at INSTALL - undeclared, malformed or foreign-version modules never enter the record;
//! * at RUN - every execution path re-checks before any guest state exists.
//!
//! The refusals are BY NAME (undeclared / malformed / unsupported, both sides of the version
//! disagreement reported), the declared version is stamped into installed metadata as evidence of
//! what was admitted, and the SDK stamps guests automatically so an SDK build can never silently
//! outlive the interface it was written against.
use aletheia::component::{SandboxLimits, ABI_VERSION};
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

/// The example component, authored with the SDK and compiled to wasm32 (see sdk_component.rs).
const SDK_WASM: &[u8] = include_bytes!("fixtures/hello_component.wasm");

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-abi-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

/// A minimal well-formed guest with NO declaration - what every component looked like before the
/// ABI existed, and exactly what the gate must refuse.
fn unstamped_guest() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32) (i32.const 0)))"#,
    )
    .expect("unstamped wat compiles")
}

/// The same guest carrying a WELL-FORMED declaration of `version`.
fn stamped_guest(version: u32) -> Vec<u8> {
    let mut w = unstamped_guest();
    aletheia::component::stamp_abi_section(&mut w, version);
    w
}

/// A DECLARATION that exists but is garbage: three bytes where four little-endian ones belong.
fn malformed_guest() -> Vec<u8> {
    let mut w = unstamped_guest();
    let name = b"aletheia.abi";
    let mut payload = Vec::new();
    payload.push(name.len() as u8);
    payload.extend_from_slice(name);
    payload.extend_from_slice(&[0x01, 0x02, 0x03]); // wrong length on purpose
    w.push(0x00); // custom section id
    w.push(payload.len() as u8);
    w.extend_from_slice(&payload);
    w
}

/// A current-version guest runs: the gate admits what it should admit.
#[test]
fn current_version_guest_is_admitted_and_runs() {
    let (mut core, owner) = open();
    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[],
            "c",
            &stamped_guest(ABI_VERSION),
            100_000,
        )
        .unwrap();
    assert!(
        outcome.ok,
        "v{ABI_VERSION} guest finished: {:?}",
        outcome.error
    );
    assert_eq!(outcome.exit_code, 0);
}

/// An UNDECLARED guest is refused AT RUN, by name - not silently tolerated, not guessed into v1.
#[test]
fn undeclared_guest_is_refused_at_run_by_name() {
    let (mut core, owner) = open();
    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[],
            "c",
            &unstamped_guest(),
            100_000,
        )
        .unwrap();
    assert!(!outcome.ok, "an undeclared guest must not run");
    assert!(
        outcome.error.as_deref().unwrap_or("").starts_with("abi:"),
        "named refusal: {:?}",
        outcome.error
    );
    assert!(outcome
        .error
        .as_deref()
        .unwrap_or("")
        .contains("does not declare"));
}

/// The SAME guest is refused AT INSTALL - unrunnable code never enters the record, and the
/// refusal is audited as ComponentInstallRefused rather than vanishing into an error return.
#[test]
fn undeclared_guest_is_refused_at_install_and_audited() {
    let (mut core, owner) = open();
    let err = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "no-declaration",
            &unstamped_guest(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not declare"), "named: {err}");
    assert_eq!(
        count(core.store(), "ComponentInstallRefused"),
        1,
        "the refusal itself is in the audit log"
    );
    assert_eq!(
        count(core.store(), "ComponentInstalled"),
        0,
        "nothing was installed"
    );
}

/// A MALFORMED declaration (wrong byte length) is its own named refusal - ambiguity is not
/// resolved by picking an interpretation.
#[test]
fn malformed_declaration_is_refused_at_both_gates() {
    let (mut core, owner) = open();
    let run_outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[],
            "c",
            &malformed_guest(),
            100_000,
        )
        .unwrap();
    assert!(!run_outcome.ok);
    assert!(run_outcome
        .error
        .as_deref()
        .unwrap_or("")
        .contains("malformed"));

    let install_err = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "garbage-declaration",
            &malformed_guest(),
        )
        .unwrap_err();
    assert!(
        install_err.to_string().contains("malformed"),
        "named: {install_err}"
    );
}

/// A guest speaking a FUTURE version is refused, with BOTH sides of the disagreement named:
/// the operator can tell "rebuild the guest" from "upgrade the host".
#[test]
fn foreign_version_guest_names_both_sides_of_the_refusal() {
    let (mut core, owner) = open();
    for foreign in [0u32, 999] {
        let outcome = core
            .run_component(
                std::slice::from_ref(&owner),
                &[],
                "c",
                &stamped_guest(foreign),
                100_000,
            )
            .unwrap();
        assert!(!outcome.ok, "v{foreign} guest must not run");
        let msg = outcome.error.clone().unwrap_or_default();
        assert!(msg.contains("abi:"), "named: {msg}");
        assert!(
            msg.contains(&format!("v{foreign}")) && msg.contains(&format!("v{ABI_VERSION}")),
            "both versions reported: {msg}"
        );
    }
}

/// An ADMITTED component carries its declared version in the installation record - evidence of
/// WHAT was admitted, queryable later without re-parsing the bytes.
#[test]
fn installed_metadata_stamps_the_declared_version() {
    let (mut core, owner) = open();
    let app = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "versioned",
            &stamped_guest(ABI_VERSION),
        )
        .unwrap();
    let stored = core
        .store()
        .get_entity(&app.id)
        .expect("installed entity exists");
    assert_eq!(
        stored.metadata.get("abi_version").and_then(|v| v.as_u64()),
        Some(ABI_VERSION as u64),
        "the record says what it admitted: {:?}",
        stored.metadata
    );
}

/// The SDK stamps guests AUTOMATICALLY: the committed example component, built by the real
/// wasm32 toolchain from the macro, declares v1 and passes the same gate - proving the stamp
/// survives a genuine compilation, not just synthetic fixtures.
#[test]
fn sdk_built_guest_carries_the_declaration_end_to_end() {
    let declared = aletheia::component::validate_module_abi(SDK_WASM);
    assert_eq!(
        declared,
        Ok(ABI_VERSION),
        "SDK fixture declares the current ABI"
    );

    let (mut core, owner) = open();
    let app = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "sdk-hello",
            SDK_WASM,
        )
        .unwrap();
    let stored = core.store().get_entity(&app.id).expect("installed");
    assert_eq!(
        stored.metadata.get("abi_version").and_then(|v| v.as_u64()),
        Some(1)
    );
}

/// The HOST import surface of ABI v1 is pinned: a module importing all four documented
/// signatures under module "aletheia" links and starts. If anyone changes a host signature
/// without bumping ABI_VERSION, this probe fails the suite - the interface cannot drift quietly.
#[test]
fn the_v1_host_import_surface_is_pinned_by_a_live_link_probe() {
    let probe = r#"(module
  (import "aletheia" "read"  (func (param i32 i32 i32 i32) (result i64)))
  (import "aletheia" "write" (func (param i32 i32)       (result i64)))
  (import "aletheia" "emit"  (func (param i32 i32)       (result i64)))
  (import "aletheia" "spawn" (func (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (func (export "run") (result i32) (i32.const 0)))"#;
    let mut w = wat::parse_str(probe).expect("probe wat compiles");
    aletheia::component::stamp_abi_section(&mut w, ABI_VERSION);

    let (mut core, owner) = open();
    let outcome = core
        .run_component_with_limits(
            std::slice::from_ref(&owner),
            &[],
            "c",
            &w,
            100_000,
            &SandboxLimits::defaults(),
        )
        .unwrap();
    assert!(
        outcome.ok,
        "all four v1 imports linked against the live host: {:?}",
        outcome.error
    );
}

fn count(store: &aletheia::storage::Store, etype: &str) -> usize {
    store.events().iter().filter(|e| e.etype == etype).count()
}
