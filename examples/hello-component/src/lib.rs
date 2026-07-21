//! Example Aletheia component, authored with `aletheia-component-sdk`.
//!
//! It does exactly two capability-gated things: write an `Output` entity, then emit an event. With no
//! capability both are denied and it changes nothing; granted `entity.write` + `event.emit` it does
//! exactly those and no more — the same invariant the runtime's own acceptance suite gates (ADR-014).
//! Build: `cargo build --release --target wasm32-unknown-unknown` (see scripts/build-example-component.sh).
#![no_std]

use aletheia_component_sdk as sdk;

/// The payload this component writes — the hosted test asserts the stored entity equals these bytes.
const OUTPUT: &[u8] = b"hello from an Aletheia component authored with the SDK";
const EVENT: &str = "hello-component: wrote its output via the SDK";

fn component() -> i32 {
    if sdk::write_output(OUTPUT).is_err() {
        return 1; // write denied (no entity.write capability)
    }
    if sdk::emit_event(EVENT).is_err() {
        return 2; // emit denied (no event.emit capability)
    }
    0
}

sdk::component_main!(component);
