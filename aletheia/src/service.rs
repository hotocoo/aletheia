//! Secure Service API & IPC boundary (ADR-016; SAD §17).
//!
//! The System Core exposes **Commands** (state-changing) and **Queries** (read) across the six
//! surfaces — world, capabilities, policy, audit, components, intents. Applications and tests are
//! CLIENTS: they interact through this boundary, never by calling Core internals. Every request
//! carries a subject + its offered capabilities and is authorized *inside* the Core before any
//! effect (fail-closed) — the boundary marshals, the Core decides.
//!
//! Two transports behind one request/response contract:
//! - **in-process** (`CoreService::handle`) — the primary, deterministic path (apps + conformance).
//! - **Unix domain socket** (`serve_unix` / `UnixClient`) — length-prefixed JSON frames, std-only,
//!   sequential accept loop (no async runtime, no external deps).
//!
//! HOSTED-CONTRACT HONESTY (KC-IPC): SAD §5 requires "no global connectable namespace." A Unix
//! socket path IS locally connectable, and the capability check runs per-request *inside* the
//! service, not at connect time. This is the hosted approximation of capability-named IPC; the
//! `serve_unix`/`UnixClient` seam is exactly where the native Aletheia kernel will enforce a
//! capability to *name* the endpoint. Documented, not hidden — same honesty as the CapEngine
//! unforgeability and IPC-benchmark contracts.
use crate::domain::EntityType;
use crate::intent_action::Intent;
use crate::syscore::SysCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

/// A request across the Core's service surfaces. `caps` is the caller's offered capabilities; the
/// Core authorizes against them before any effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Request {
    /// Mint the root capability for a subject (hosted-dev root of trust; recorded + audited).
    BootstrapOwner { subject: String },
    /// world (command): create an entity.
    CreateEntity { caps: Vec<String>, subject: String, etype: EntityType, content: String, metadata: Value },
    /// intents (command/query): run one intent through the full pipeline (read/derive/traverse/... ).
    SubmitIntent { caps: Vec<String>, intent: Intent, approve: bool },
    /// capabilities (command): delegate a capability to a subject.
    Grant { caps: Vec<String>, subject: String, action: String, scope_entities: Vec<String>, approval: bool },
    /// capabilities (command): revoke a capability (and its descendants). Capability-gated.
    Revoke { caps: Vec<String>, token: String },
    /// policy (query): list pending approvals awaiting a human decision. Capability-gated.
    ListApprovals { caps: Vec<String> },
    /// policy (command): a human grants or denies a pending approval (re-runs the bound intent).
    ResolveApproval { caps: Vec<String>, approval_id: String, granted: bool },
    /// audit (query): the tail of the immutable event log. Capability-gated (`audit.read`).
    QueryAudit { caps: Vec<String>, limit: usize },
    /// components (command): install untrusted WASM (hex-encoded bytes) as an Application entity.
    InstallComponent { caps: Vec<String>, subject: String, name: String, wasm_hex: String },
    /// components (command): launch an installed component with an explicit capability grant.
    RunComponent { launch_caps: Vec<String>, grant_caps: Vec<String>, subject: String, app_id: String, fuel: u64 },
}

/// A uniform response envelope (consistent success/data/error shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub data: Value,
    pub error: Option<String>,
}
impl Response {
    fn ok(data: Value) -> Self {
        Response { ok: true, data, error: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, data: Value::Null, error: Some(msg.into()) }
    }
}

/// The System Core exposed as a service. Owns the `SysCore`; every request is dispatched to a
/// capability-checked Core operation. This is the ONLY object apps and tests should touch.
pub struct CoreService {
    core: SysCore,
}

impl CoreService {
    pub fn open(dir: impl AsRef<std::path::Path>) -> crate::domain::Result<Self> {
        Ok(CoreService { core: SysCore::open_default(dir)? })
    }

    /// Dispatch one request. Authorization happens inside the Core operations, not here.
    pub fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::BootstrapOwner { subject } => match self.core.bootstrap_owner(&subject) {
                Ok(cap) => Response::ok(json!({ "token": cap.token, "subject": subject })),
                Err(e) => Response::err(e.to_string()),
            },
            Request::CreateEntity { caps, subject, etype, content, metadata } => {
                match self.core.create_entity(&caps, &subject, etype, content.as_bytes(), metadata) {
                    Ok(e) => Response::ok(json!({ "id": e.id, "type": e.etype, "version": e.version, "chain": e.version_chain })),
                    Err(e) => Response::err(e.to_string()),
                }
            }
            Request::SubmitIntent { caps, intent, approve } => {
                let trace = self.core.handle_intent(&caps, intent, approve);
                Response { ok: trace.ok, data: serde_json::to_value(&trace).unwrap_or(Value::Null), error: trace.error.as_ref().map(|e| e.to_string()) }
            }
            Request::Grant { caps, subject, action, scope_entities, approval } => {
                use crate::capabilities::{Constraints, Scope};
                let scope = if scope_entities.is_empty() { Scope::All } else { Scope::Entities(scope_entities) };
                let cons = if approval { Constraints::approval() } else { Constraints::none() };
                match self.core.grant_to(&caps, &subject, &action, scope, cons) {
                    Ok(cap) => Response::ok(json!({ "token": cap.token, "subject": subject, "action": action })),
                    Err(e) => Response::err(e.to_string()),
                }
            }
            Request::Revoke { caps, token } => match self.core.revoke_capability(&caps, &token) {
                Ok(()) => Response::ok(json!({ "revoked": token })),
                Err(e) => Response::err(e.to_string()),
            },
            Request::ListApprovals { caps } => match self.core.list_pending_approvals(&caps) {
                Ok(pending) => Response::ok(serde_json::to_value(&pending).unwrap_or(Value::Null)),
                Err(e) => Response::err(e.to_string()),
            },
            Request::ResolveApproval { caps, approval_id, granted } => {
                match self.core.resolve_approval(&caps, &approval_id, granted) {
                    Ok(trace) => Response { ok: trace.ok || !granted, data: serde_json::to_value(&trace).unwrap_or(Value::Null), error: None },
                    Err(e) => Response::err(e.to_string()),
                }
            }
            Request::QueryAudit { caps, limit } => match self.core.query_audit(&caps, limit) {
                Ok(tail) => Response::ok(serde_json::to_value(&tail).unwrap_or(Value::Null)),
                Err(e) => Response::err(e.to_string()),
            },
            Request::InstallComponent { caps, subject, name, wasm_hex } => match from_hex(&wasm_hex) {
                Some(bytes) => match self.core.install_component(&caps, &subject, &name, &bytes) {
                    Ok(e) => Response::ok(json!({ "app": e.id, "name": name })),
                    Err(e) => Response::err(e.to_string()),
                },
                None => Response::err("invalid wasm_hex"),
            },
            Request::RunComponent { launch_caps, grant_caps, subject, app_id, fuel } => {
                match self.core.run_installed(&launch_caps, &grant_caps, &subject, &app_id, fuel) {
                    Ok(out) => Response::ok(json!({ "ok": out.ok, "exit_code": out.exit_code, "wrote": out.wrote.len(), "host_calls": out.calls.len() })),
                    Err(e) => Response::err(e.to_string()),
                }
            }
        }
    }
}

// --- Unix-socket transport (hosted realization of the IPC boundary) ---

/// Serve requests on a Unix domain socket until the listener is dropped. Sequential accept loop:
/// one connection at a time, each connection may issue many requests (length-prefixed JSON frames).
/// std-only; no async runtime.
/// Maximum accepted request frame (8 MiB): bounds a malicious length prefix BEFORE allocation.
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Per-connection read timeout: a client that stalls mid-frame is dropped rather than blocking the
/// (sequential) accept loop forever (slow-loris mitigation). Bounds reading REQUEST bytes, not
/// request processing, so a long model interpretation is unaffected.
const CONN_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn serve_unix(mut svc: CoreService, socket_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    for conn in listener.incoming() {
        let mut stream = conn?;
        let _ = stream.set_read_timeout(Some(CONN_READ_TIMEOUT));
        while let Ok(Some(bytes)) = read_frame(&mut stream) {
            let resp = match serde_json::from_slice::<Request>(&bytes) {
                Ok(req) => svc.handle(req),
                Err(e) => Response::err(format!("bad request: {e}")),
            };
            let out = serde_json::to_vec(&resp).unwrap_or_default();
            if write_frame(&mut stream, &out).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// A client over the Unix-socket transport. Holds one connection; `call` is a synchronous
/// request/response round trip.
pub struct UnixClient {
    stream: UnixStream,
}
impl UnixClient {
    pub fn connect(socket_path: &str) -> std::io::Result<Self> {
        Ok(UnixClient { stream: UnixStream::connect(socket_path)? })
    }
    pub fn call(&mut self, req: &Request) -> std::io::Result<Response> {
        let out = serde_json::to_vec(req).unwrap_or_default();
        write_frame(&mut self.stream, &out)?;
        let bytes = read_frame(&mut self.stream)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "server closed"))?;
        serde_json::from_slice(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// 4-byte little-endian length prefix + payload (mirrors the store log framing). Returns Ok(None)
/// on a clean EOF at a frame boundary.
fn write_frame(w: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}
fn read_frame(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::other("frame exceeds MAX_FRAME"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Hex-encode WASM bytes for the `InstallComponent` request (JSON-safe binary marshaling).
pub fn encode_wasm(bytes: &[u8]) -> String {
    to_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let b = vec![0u8, 1, 254, 255, 16];
        assert_eq!(from_hex(&to_hex(&b)).unwrap(), b);
        assert!(from_hex("abc").is_none());
    }
}
