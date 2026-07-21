//! # Aletheia Component SDK
//!
//! Author capability-secure Aletheia components (WASM applications) in Rust. A component reaches the
//! operating system ONLY through the four host calls wrapped here — there is deliberately **no WASI,
//! no ambient filesystem/clock/rand/env** (ADR-014). Every call is authorized by the System Core
//! against the *exact* capabilities the component was granted; nothing is inherited from the launcher.
//!
//! ```ignore
//! #![no_std]
//! use aletheia_component_sdk as sdk;
//!
//! fn main() -> i32 {
//!     if sdk::write_output(b"hello").is_err() { return 1; }
//!     if sdk::emit_event("did a thing").is_err() { return 2; }
//!     0
//! }
//! sdk::component_main!(main);
//! ```
//!
//! Build a guest with `cargo build --release --target wasm32-unknown-unknown`; the resulting `.wasm`
//! is what `SysCore::install_component` / `run_component` loads. The guest is a `cdylib` that exports
//! `run() -> i32` and its linear `memory` — `component_main!` wires both plus the mandatory
//! `#[panic_handler]`.
#![no_std]

/// The result of a host call. On failure it carries WHY, mapped from the host's ABI sentinels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    /// Fail-closed: the component holds no capability authorizing this action (ABI `-1`).
    Denied,
    /// The action is authorized but requires human approval; refused at the component boundary
    /// (ABI `-2`). Approval is a governance decision the System Core makes outside the component.
    NeedsApproval,
    /// Malformed request — bad pointer/length, missing entity, or non-UTF-8 argument (ABI `-3`).
    Bad,
}

impl HostError {
    #[inline]
    fn from_code(code: i64) -> Self {
        match code {
            -1 => HostError::Denied,
            -2 => HostError::NeedsApproval,
            _ => HostError::Bad,
        }
    }
}

/// Result of an SDK host call.
pub type Result<T> = core::result::Result<T, HostError>;

// The Aletheia host ABI (import module "aletheia"). These are the ONLY entry points from a component
// into the OS; each is capability-gated inside the System Core (see aletheia/src/component.rs).
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aletheia")]
extern "C" {
    fn read(id_ptr: i32, id_len: i32, out_ptr: i32, out_cap: i32) -> i64;
    fn write(ptr: i32, len: i32) -> i64;
    fn emit(ptr: i32, len: i32) -> i64;
    fn spawn(app_ptr: i32, app_len: i32, act_ptr: i32, act_len: i32) -> i64;
}

/// Write `bytes` as a new content-addressed `Output` entity in the semantic store. Requires an
/// `entity.write` capability. Returns `Err(Denied)` when the component was not granted one.
#[inline]
pub fn write_output(bytes: &[u8]) -> Result<()> {
    let code = host_write(bytes);
    if code == 0 {
        Ok(())
    } else {
        Err(HostError::from_code(code))
    }
}

/// Emit an event carrying `message` into the immutable event log, attributed to this component.
/// Requires an `event.emit` capability.
#[inline]
pub fn emit_event(message: &str) -> Result<()> {
    let b = message.as_bytes();
    let code = host_emit(b);
    if code == 0 {
        Ok(())
    } else {
        Err(HostError::from_code(code))
    }
}

/// Read the content of entity `id` into `buf`, returning the entity's FULL content length. If the
/// returned length exceeds `buf.len()`, the content was truncated into `buf` — call again with a
/// larger buffer. Requires an `entity.read` capability scoped to that entity. The content is DATA the
/// component is authorized to consume; the OS never interprets it as instruction (SEC-003).
#[inline]
pub fn read_entity(id: &str, buf: &mut [u8]) -> Result<usize> {
    let code = host_read(id.as_bytes(), buf);
    if code < 0 {
        Err(HostError::from_code(code))
    } else {
        Ok(code as usize)
    }
}

/// Ask the System Core to spawn installed child component `app_id`, requesting `action` authority for
/// it. The child runs AFTER this component returns, under a capability **attenuated** (delegated) from
/// this one — it can never exceed this component's authority. Requires a `component.spawn` capability.
#[inline]
pub fn spawn_child(app_id: &str, action: &str) -> Result<()> {
    let code = host_spawn(app_id.as_bytes(), action.as_bytes());
    if code == 0 {
        Ok(())
    } else {
        Err(HostError::from_code(code))
    }
}

// --- thin FFI shims: isolate the `unsafe` + the wasm32/host split in one place ---

#[cfg(target_arch = "wasm32")]
#[inline]
fn host_write(bytes: &[u8]) -> i64 {
    // SAFETY: passes a valid (ptr,len) into the guest's own linear memory; the host bounds-checks it.
    unsafe { write(bytes.as_ptr() as i32, bytes.len() as i32) }
}
#[cfg(target_arch = "wasm32")]
#[inline]
fn host_emit(bytes: &[u8]) -> i64 {
    // SAFETY: as above — the host copies `len` bytes out of guest memory at `ptr`, bounds-checked.
    unsafe { emit(bytes.as_ptr() as i32, bytes.len() as i32) }
}
#[cfg(target_arch = "wasm32")]
#[inline]
fn host_read(id: &[u8], buf: &mut [u8]) -> i64 {
    // SAFETY: id is read by the host; buf is written by the host up to buf.len(); both bounds-checked.
    unsafe {
        read(
            id.as_ptr() as i32,
            id.len() as i32,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
        )
    }
}
#[cfg(target_arch = "wasm32")]
#[inline]
fn host_spawn(app: &[u8], action: &[u8]) -> i64 {
    // SAFETY: both slices are read by the host, bounds-checked.
    unsafe {
        spawn(
            app.as_ptr() as i32,
            app.len() as i32,
            action.as_ptr() as i32,
            action.len() as i32,
        )
    }
}

// Non-wasm targets have no host to call. These stubs let the crate `cargo check` on the host (for
// docs/IDE) while making a real call on the wrong target a loud, immediate failure.
#[cfg(not(target_arch = "wasm32"))]
fn host_write(_bytes: &[u8]) -> i64 {
    panic!("aletheia-component-sdk host calls are only available on wasm32 guests")
}
#[cfg(not(target_arch = "wasm32"))]
fn host_emit(_bytes: &[u8]) -> i64 {
    panic!("aletheia-component-sdk host calls are only available on wasm32 guests")
}
#[cfg(not(target_arch = "wasm32"))]
fn host_read(_id: &[u8], _buf: &mut [u8]) -> i64 {
    panic!("aletheia-component-sdk host calls are only available on wasm32 guests")
}
#[cfg(not(target_arch = "wasm32"))]
fn host_spawn(_app: &[u8], _action: &[u8]) -> i64 {
    panic!("aletheia-component-sdk host calls are only available on wasm32 guests")
}

/// Declare the component entry point and its no_std runtime glue. Wraps a `fn() -> i32` as the WASM
/// export `run`, and provides the mandatory `#[panic_handler]` for the no_std guest — a panicking
/// component simply traps (`unreachable`) and leaves no effects, which the host's per-call
/// all-or-nothing + fuel/trap boundary already guarantees.
#[macro_export]
macro_rules! component_main {
    ($entry:ident) => {
        #[no_mangle]
        pub extern "C" fn run() -> i32 {
            $entry()
        }

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __aletheia_component_panic(_info: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}
