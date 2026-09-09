# ADR-128 — Hosted GUI Transport Hardening

**Status:** Accepted

## Context

The hosted Experience GUI is a development surface, but it is still an HTTP boundary into the
capability-gated System Core. The previous listener parsed a single 64 KiB read as if it were a
complete TCP request and did not validate browser origin or HTTP body framing. That left avoidable
slow-client, truncation, oversized-body, and cross-origin request ambiguity at the boundary.

## Decision

Keep the GUI loopback-only and make its HTTP parser fail closed without adding an async runtime:

1. Read headers incrementally into the fixed 64 KiB receive buffer and reject an unterminated header.
2. Cap request bodies at 64 KiB and reject a `Content-Length` above the cap before reading it.
3. Read exactly the declared body length; a mismatched length is refused rather than silently parsed.
4. Reject malformed/non-UTF-8 HTTP input before interpreting header offsets.
5. For browser requests carrying `Origin`, require the exact GUI origin; no CORS exception is added.
6. Apply short read/write timeouts so a stalled connection cannot hold the sequential listener forever.
7. Add `X-Frame-Options: DENY` and `Cross-Origin-Opener-Policy: same-origin` to the existing no-store,
   CSP, CORP, Permissions-Policy, and `nosniff` policy.

The GUI continues to use bearer capabilities in the request body rather than cookies, so it does not
introduce an ambient browser credential. State-changing operations remain authorized by `CoreService`.

## Consequences

The parser has a bounded memory footprint and explicit framing/origin failures. Valid browser requests
remain unchanged. The listener is still sequential and the GUI is still a development/hosted surface;
this ADR does not claim production TLS, a multi-client scheduler, or a native GPU compositor.

## Verification

- `cargo test --manifest-path aletheia/Cargo.toml --test experience_gui`: 3/3 passed.
- Live `aletheiad gui --bind 127.0.0.1:18787`: GUI returned HTTP 200 and the expected security headers.
- A request with `Origin: http://evil.example` returned HTTP 403.
- Oversized-body rejection was implemented before body allocation; the attempted client-side oversized
  probe was rejected by the local `curl` client before it sent data, so that specific live probe is not
  counted as a server-side pass.
