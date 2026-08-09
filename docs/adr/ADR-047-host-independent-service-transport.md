# ADR-047 — The Service API boundary stops importing POSIX

**Status:** Accepted (2026-08-09)
**Context:** REQ-SVC-002 · extends ADR-016 (Service API / IPC boundary) · informed by
`docs/research/RUST-OS-DEEP-RESEARCH.md` §3.2 · precondition for ADR-046 running on a Windows host

## Context

`README.md` states the doctrine plainly: the hosted Rust implementation is a *temporary development
environment*, and "every interface is an Aletheia-owned abstraction designed to be re-implemented
natively later." `docs/Aletheia_Software_Architecture_Document.md` repeats it as an architectural
constraint — no Linux/macOS/POSIX imports.

`aletheia/src/service.rs` imported `std::os::unix::net::{UnixListener, UnixStream}` at module scope.

That is not a cosmetic violation. It sits at the **Service API / IPC boundary** (ADR-016) — the single
seam the SAD names as the place the native Aletheia kernel will later enforce a capability to *name*
an endpoint. A POSIX type at that seam means the seam is shaped like POSIX, and the two consequences
showed up immediately:

1. **The hosted Core does not compile on a Windows host at all.** `cargo test --manifest-path
   aletheia/Cargo.toml` fails with `error[E0433]: cannot find 'unix' in 'os'` — not one test failing,
   the crate failing to build. The "temporary development environment" was in fact a *Unix* development
   environment, which was nowhere stated.
2. **The boundary could not be moved.** A future native transport (a capability-named kernel endpoint)
   has no seam to be substituted at, because the transport type is the API type: `UnixClient` is a
   public struct and `serve_unix` takes a path.

This surfaced while building the ADR-046 VirtualBox gate, which exists partly to make x86-64
qualification runnable on a Windows workstation. A qualification rung that is host-independent, sitting
above a Core that is not, is incoherent.

## Decision

**Introduce an Aletheia-owned transport seam and put every platform type behind it.**

1. **`aletheia/src/transport.rs` owns the contract.** Two traits and one address type — a
   `Listener` that accepts `Conn`s, a `Conn` that is `Read + Write` with a read timeout, and an
   `Endpoint` that names a rendezvous location. `service.rs` speaks only these; it contains no
   `std::os::*` import on any platform. The framing (4-byte little-endian length prefix, 8 MiB
   `MAX_FRAME` bound, 30 s per-connection read timeout) is transport-independent and stays where it
   is — it is a *protocol* property, not a platform one.

2. **`serve_unix` / `UnixClient` become `serve` / `ServiceClient`.** The names stop asserting a
   platform. The old names remain as deprecated aliases on Unix only, so existing callers and the
   `serve` subcommand keep working while the rename propagates; nothing new uses them.

3. **Unix backend: unchanged behavior.** `UnixListener::bind` on the given path, `remove_file` first,
   sequential accept loop. On Unix, an Aletheia endpoint *is* a filesystem path, exactly as before —
   the abstraction adds no indirection cost and changes no wire bytes.

4. **Windows backend: loopback TCP behind a rendezvous file, and the rendezvous file is the
   authority.** Rust's `std` exposes no `UnixListener` on Windows, and this crate's transport is
   deliberately dependency-free ("std-only; no async runtime, no external deps"), so a third-party
   AF_UNIX shim was rejected. Instead:
   - the listener binds `127.0.0.1:0` (an ephemeral port chosen by the OS),
   - it writes a small rendezvous file **at the endpoint path** containing the port and a
     freshly-generated 32-byte connect token,
   - the client reads that file, connects, and presents the token as the first frame,
   - the server closes any connection whose first frame is not the token, before parsing anything.

   The rendezvous file is created with `CREATE_NEW` semantics and removed on drop.

5. **The honesty note is extended, not quietly dropped.** The existing KC-IPC note already concedes
   that a Unix socket path IS locally connectable and that the capability check runs per-request
   inside the service rather than at connect time. The Windows backend is **weaker still** and says
   so: loopback TCP is reachable by any local process, so the token is what binds connect authority to
   *permission to read the rendezvous file* — which is filesystem permission, i.e. the same authority
   the Unix backend gets from socket file mode. It is a closer approximation than an unauthenticated
   port would be, and a weaker one than AF_UNIX. Neither backend is the native capability-named
   endpoint; both are hosted approximations, and this ADR is where the difference between them is
   written down.

6. **Token comparison is constant-time.** A byte-by-byte early-exit compare on a locally-observable
   loopback socket is a timing oracle for a 32-byte secret. The comparison accumulates a difference
   over the whole length.

## Consequences

**What this buys.**

* The hosted Core builds and its full suite runs on Linux, macOS, **and Windows**. The x86-64
  qualification story (ADR-046) is now host-independent from the Core down to the boot gate.
* The doctrine in `README.md` and the SAD is true at the one seam where it was most load-bearing.
* There is now an actual substitution point for the native transport. Adding a capability-named kernel
  endpoint later means adding a third `Listener`/`Conn` implementation, not editing the service.

**What it costs, stated plainly.**

* **The Windows backend is a weaker security posture than the Unix one**, per §5, and no amount of
  token discipline makes loopback TCP equal to a filesystem-scoped socket. It is a development-host
  transport for a development-host Core.
* **The token adds a handshake frame on Windows only**, so the two backends are not byte-identical on
  the wire at connection setup. Everything after the handshake is identical, and the conformance suite
  runs over the in-process transport, so no semantic claim depends on which backend ran.
* **The rendezvous file is a new failure mode** — a stale file from a crashed server names a dead port.
  The client treats a connect failure against a rendezvous file as "endpoint not live" and reports it
  as such, rather than retrying into a port some other process may since have taken.
* **No test proves the Windows backend and the Unix backend behave identically on the same host**,
  because no host has both. What is proved on each host is the same round-trip suite against whichever
  backend that host provides, plus the platform-independent framing tests that run everywhere.
