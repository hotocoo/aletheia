//! Component signature verification — ADR-025 Phase 1, gap-register Issue 7 hosted first slice.
//!
//! A component is a content-addressed `Application` entity (ADR-014); its provenance is a detached
//! HMAC-SHA256 signature over its content hash under a trusted key. Under secure policy an unsigned or
//! tampered/untrusted component cannot launch (fail closed); with the policy off, the existing
//! unsigned install/run flow is unchanged (backward compatible). Proved through the *unchanged* runtime
//! against the same committed wasm fixture the SDK suite uses.
use aletheia::capabilities::{Constraints, Scope};
use aletheia::crypto::{hmac_sha256_hex, sha256_hex};
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

const HELLO_WASM: &[u8] = include_bytes!("fixtures/hello_component.wasm");
const TRUSTED_KEY: [u8; 32] = [42u8; 32];

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-sign-{}", aletheia::domain::new_id()))
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

/// A signed component, installed under a trusted key, launches successfully under secure policy.
#[test]
fn signed_component_launches_under_secure_policy() {
    let (mut core, owner) = open();
    core.trust_component_key(TRUSTED_KEY);
    core.set_require_signed_components(true);

    let install_cap = grant(&mut core, &owner, "human:owner", "component.install");
    let run_cap = grant(&mut core, &owner, "component:hello", "component.run");
    let write_cap = grant(&mut core, &owner, "component:hello", "entity.write");
    let emit_cap = grant(&mut core, &owner, "component:hello", "event.emit");

    let sig = core.sign_component(&sha256_hex(HELLO_WASM)).expect("trusted key can sign");
    let app = core
        .install_signed_component(&[install_cap], "human:owner", "hello", HELLO_WASM, &sig)
        .expect("a validly-signed component installs");

    let outcome = core
        .run_installed(&[run_cap], &[write_cap, emit_cap], "component:hello", &app.id, 1_000_000)
        .expect("a signed component launches under secure policy");
    assert!(outcome.ok, "component ran: {:?}", outcome.error);
    assert_eq!(outcome.exit_code, 0, "full grant -> guest exits 0");
}

/// Under secure policy, an UNSIGNED installed component is refused at launch (fail closed).
#[test]
fn unsigned_component_refused_under_secure_policy() {
    let (mut core, owner) = open();
    core.trust_component_key(TRUSTED_KEY);

    let install_cap = grant(&mut core, &owner, "human:owner", "component.install");
    let run_cap = grant(&mut core, &owner, "component:hello", "component.run");
    // Installed WITHOUT a signature via the plain path.
    let app = core
        .install_component(&[install_cap], "human:owner", "hello", HELLO_WASM)
        .expect("unsigned install itself is permitted");

    core.set_require_signed_components(true);
    let result = core.run_installed(&[run_cap], &[], "component:hello", &app.id, 1_000_000);
    assert!(result.is_err(), "an unsigned component cannot launch under secure policy");
    assert_eq!(count_events(&core, "ComponentSignatureRejected"), 1, "the rejection is recorded");
    assert_eq!(count_events(&core, "ComponentRan"), 0, "the component never ran");
}

/// A signature from an UNTRUSTED key is refused at install time — a tampered/untrusted artifact never
/// enters the store as a trusted application.
#[test]
fn install_with_untrusted_signature_is_refused() {
    let (mut core, owner) = open();
    core.trust_component_key(TRUSTED_KEY);
    let install_cap = grant(&mut core, &owner, "human:owner", "component.install");

    // An attacker signs with a DIFFERENT key; the trust anchor does not recognize it.
    let forged = hmac_sha256_hex(&[7u8; 32], sha256_hex(HELLO_WASM).as_bytes());
    let result = core.install_signed_component(&[install_cap], "human:owner", "hello", HELLO_WASM, &forged);
    assert!(result.is_err(), "an untrusted signature is refused at install");
    assert_eq!(count_events(&core, "ComponentSignatureRejected"), 1);
    assert_eq!(count_events(&core, "ComponentInstalled"), 0, "nothing is installed");
}

/// A signature valid for DIFFERENT bytes does not authorize this component (content binding).
#[test]
fn signature_is_bound_to_the_content() {
    let (mut core, owner) = open();
    core.trust_component_key(TRUSTED_KEY);
    let install_cap = grant(&mut core, &owner, "human:owner", "component.install");

    // Sign a different payload's hash with the trusted key, then present it for HELLO_WASM.
    let wrong_sig = core.sign_component(&sha256_hex(b"some other artifact")).unwrap();
    let result = core.install_signed_component(&[install_cap], "human:owner", "hello", HELLO_WASM, &wrong_sig);
    assert!(result.is_err(), "a signature over other content does not authorize this component");
}

/// Under secure policy, an AD-HOC raw-WASM `run_component` (no installed provenance) is refused
/// fail-closed — closing the bypass where unsigned code could launch without going through the
/// signed installed path.
#[test]
fn adhoc_run_component_refused_under_secure_policy() {
    let (mut core, owner) = open();
    core.trust_component_key(TRUSTED_KEY);
    core.set_require_signed_components(true);
    let run_cap = grant(&mut core, &owner, "component:hello", "component.run");

    let result = core.run_component(&[run_cap], &[], "component:hello", HELLO_WASM, 1_000_000);
    assert!(result.is_err(), "ad-hoc raw-WASM execution has no provenance and is refused under secure policy");
    assert_eq!(count_events(&core, "ComponentRan"), 0, "the component never ran");
}

/// With the policy OFF (default), the existing unsigned install/run flow is unchanged (back-compat).
#[test]
fn default_policy_runs_unsigned_component() {
    let (mut core, owner) = open();
    let install_cap = grant(&mut core, &owner, "human:owner", "component.install");
    let run_cap = grant(&mut core, &owner, "component:hello", "component.run");
    let write_cap = grant(&mut core, &owner, "component:hello", "entity.write");
    let emit_cap = grant(&mut core, &owner, "component:hello", "event.emit");

    let app = core
        .install_component(&[install_cap], "human:owner", "hello", HELLO_WASM)
        .expect("unsigned install");
    // No secure policy set -> unsigned component launches exactly as before.
    let outcome = core
        .run_installed(&[run_cap], &[write_cap, emit_cap], "component:hello", &app.id, 1_000_000)
        .expect("unsigned launch succeeds under default policy");
    assert!(outcome.ok && outcome.exit_code == 0);
}
