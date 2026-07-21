//! P2 acceptance — the component SDK (ADR-014, PRD §41 "component SDK").
//!
//! Proves the SDK is real by the SAME bar as everything else: a component AUTHORED IN RUST WITH THE
//! SDK (`aletheia-component-sdk`), compiled to wasm32 and run through the *unchanged* runtime, is
//! exactly capability-bounded — no capability ⇒ it changes nothing; granted exactly its actions ⇒ it
//! does exactly those and no more; every allowed effect lands in the one immutable event log.
//!
//! The guest under test is `examples/hello-component`, prebuilt to a committed fixture by
//! `scripts/build-example-component.sh` so this test needs NO wasm toolchain (it just `include_bytes!`s
//! the `.wasm`). Regenerate the fixture with that script whenever the SDK or the example changes.
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

/// The example component, authored with the SDK and compiled to wasm32-unknown-unknown.
const HELLO_WASM: &[u8] = include_bytes!("fixtures/hello_component.wasm");

/// The exact bytes `examples/hello-component` writes — kept in sync with its `OUTPUT` const.
const EXPECTED_OUTPUT: &[u8] = b"hello from an Aletheia component authored with the SDK";

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-sdk-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

fn grant(core: &mut SysCore, owner: &str, subject: &str, action: &str) -> String {
    core.grant_to(&[owner.to_string()], subject, action, Scope::All, Constraints::none())
        .expect("grant")
        .token
}

fn count_events(core: &SysCore, etype: &str) -> usize {
    core.store().events().iter().filter(|e| e.etype == etype).count()
}

/// The fixture is a real, non-empty WASM module (guards against a stale/empty checkout).
#[test]
fn sdk_fixture_is_present() {
    assert!(HELLO_WASM.len() > 8, "fixture missing — run scripts/build-example-component.sh");
    assert_eq!(&HELLO_WASM[0..4], b"\0asm", "fixture is not a WASM module");
}

/// No capability ⇒ the SDK-authored component changes nothing. Its first host call (`write_output`)
/// is denied, so it returns exit code 1 and never reaches `emit_event`; the store gains no effects.
#[test]
fn sdk_component_with_no_capability_can_do_nothing() {
    let (mut core, owner) = open();

    let outcome = core.run_component(&[owner], &[], "component:sdk-hello", HELLO_WASM, 1_000_000).unwrap();

    assert!(outcome.ok, "guest itself ran fine: {:?}", outcome.error);
    assert_eq!(outcome.exit_code, 1, "write_output returned Err -> guest returns 1");
    assert!(outcome.denied("write"), "write must be denied with no capability");
    assert!(outcome.wrote.is_empty(), "no entity may be created");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 0);
    assert_eq!(count_events(&core, "ComponentEmitted"), 0);
}

/// Granted exactly its two actions, the SDK-authored component writes its output and emits its event,
/// exits 0, and the write is a real, verifiable effect: the stored entity holds the SDK's payload.
#[test]
fn sdk_component_runs_with_full_grant() {
    let (mut core, owner) = open();
    let write_cap = grant(&mut core, &owner, "component:sdk-hello", "entity.write");
    let emit_cap = grant(&mut core, &owner, "component:sdk-hello", "event.emit");

    let outcome = core
        .run_component(&[owner], &[write_cap, emit_cap], "component:sdk-hello", HELLO_WASM, 1_000_000)
        .unwrap();

    assert!(outcome.ok, "component ran: {:?}", outcome.error);
    assert_eq!(outcome.exit_code, 0, "both host calls succeeded -> guest returns 0");
    assert!(outcome.allowed("write") && outcome.allowed("emit"));
    assert_eq!(outcome.wrote.len(), 1, "exactly one entity created");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 1);
    assert_eq!(count_events(&core, "ComponentEmitted"), 1);

    // The SDK's write path produced a real Output entity carrying exactly the bytes it wrote.
    let e = core.store().get_entity(&outcome.wrote[0]).expect("written entity persisted");
    assert_eq!(e.etype, EntityType::Output);
    let blob = core.store().get_blob(e.content_ref.as_ref().unwrap()).expect("content stored");
    assert_eq!(blob, EXPECTED_OUTPUT, "stored bytes match what the SDK wrote");
}

/// Authority is EXACTLY the grant: given only `entity.write`, the component writes, but its
/// `emit_event` is denied — it returns exit code 2 and no event is emitted. Attenuation holds for an
/// SDK-authored guest just as it does for the hand-written WAT components in the runtime suite.
#[test]
fn sdk_component_authority_is_exactly_its_grant() {
    let (mut core, owner) = open();
    let write_cap = grant(&mut core, &owner, "component:sdk-hello", "entity.write");

    let outcome = core
        .run_component(&[owner], &[write_cap], "component:sdk-hello", HELLO_WASM, 1_000_000)
        .unwrap();

    assert_eq!(outcome.exit_code, 2, "write allowed, emit denied -> guest returns 2");
    assert!(outcome.allowed("write"), "write is granted -> allowed");
    assert!(outcome.denied("emit"), "emit is NOT granted -> denied");
    assert_eq!(outcome.wrote.len(), 1, "the one authorized write still happened");
    assert_eq!(count_events(&core, "ComponentWroteEntity"), 1);
    assert_eq!(count_events(&core, "ComponentEmitted"), 0, "no event without event.emit");
}
