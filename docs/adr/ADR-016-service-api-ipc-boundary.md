# ADR-016: Capability-gated Service API & IPC boundary + long-running Core

**Status:** Accepted · **Date:** 2026-07-21

## Context

The M1 reference was a scripted binary that called `SysCore` internals directly. The PRD/SAD (§17)
require a real boundary: the Core exposes Commands/Queries/Events, and applications are clients that
hold session/component capabilities and cannot escalate. "Applications and tests should interact
through those boundaries rather than directly calling demo internals."

## Decision

A `service` layer (`service.rs`) exposing `Request`/`Response` across the six surfaces — world,
capabilities, policy, audit, components, intents. `CoreService` owns the `SysCore` and dispatches
each request to a capability-checked Core operation; **authorization stays inside the Core**
(fail-closed), the boundary only marshals.

One request/response contract, two transports:

- **in-process** (`CoreService::handle`) — the primary, deterministic path used by apps and the
  conformance suite.
- **Unix domain socket** (`serve_unix` / `UnixClient`) — length-prefixed JSON frames, std-only,
  sequential accept loop. No async runtime, no HTTP/serialization crates beyond `serde_json`.

`aletheiad` becomes a real Core Alpha: `aletheiad serve` runs the long-running Core behind the
socket; `aletheiad demo` runs the UC-001..004 scenario **as a client** over the in-process boundary.
The M1 scenario is reproduced as conformance tests that transit the API, plus a socket round-trip.

## Consequences

- No code path in an app or test reaches around the capability engine into Core internals.
- **KC-IPC honesty:** a Unix socket path is locally connectable and the capability check runs
  per-request inside the service, not at connect time — the hosted approximation of capability-named
  IPC. The `serve_unix`/`UnixClient` seam is exactly where the native Aletheia kernel will require a
  capability to *name* the endpoint (SAD §5), with no change to the surfaces above.
- The same `Request`/`Response` contract will front the native IPC transport later; only the
  transport is replaced, not the API or the Core.
