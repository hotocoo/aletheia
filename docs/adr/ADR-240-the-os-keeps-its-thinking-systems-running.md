# ADR-240 — The OS keeps its thinking systems running

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (row 3 of ADR-237's wave order)
**Builds on:** ADR-186 (System 1 and System 2 as registry roles), ADR-187 (digest-checked release asset), ADR-189 (`model serve`), ADR-237 (decision 1: the backend becomes shipped and supervised).

## Context

ADR-189 let `aletheiad model serve` start each selected model's server from its manifest and
named what it left out: a crashed server stayed down and took the session with it, nothing
stopped two servers fighting for one port, and a model that was not on the machine had to be
pulled by hand first. ADR-237 decided the Laya backend's life belongs to Aletheia. This ADR is
that decision built.

## Decision

`aletheia::ai::supervise` owns a server's life; `model serve [<id>] [--pull]` uses it.

* **Identity before trust.** Before starting anything the endpoint is asked who is serving. The
  selected model already there: nothing is started (`already served … starting nothing`). Another
  model there: refused by name (`a port is not a model`). After a start the server is ready only
  when the endpoint answers with the manifest's `serve_id`; one that answers as anything else, or
  not at all, within the ready timeout (180 s) is stopped and counts as a failed start.
* **Bounded restarts.** A server that exits is started again after a back-off doubling from
  500 ms to 8 s. More than 5 exits inside 60 s is a crash loop: the supervisor stops trying and
  says so (`crash loop: N exits in 60 s, giving up`). A server that stayed up a whole window
  earns the shortest back-off back. The rules are `Restarts`, a value with an injected clock.
* **A server never outlives its supervisor.** On Unix each server runs under a `sh` tether that
  holds the supervisor's end of a pipe on fd 3. When the supervisor goes away for any reason, a
  SIGKILL included, the kernel closes the pipe and the tether stops the server. No `unsafe` and
  no new dependency: the tether is POSIX `sh`.
* **Provisioning is asked for, never assumed.** `--pull` fetches an absent model first (the same
  hub or digest-checked archive path as `model pull`); without it an absent model is refused with
  the pull it needs. An 800 MB download never starts unasked.
* The console's "System 1 not serving" line now names the command that keeps it served.

## Evidence (2026-10-08, Apple M4 Max)

* Host: three ledger tests (back-off doubles to its ceiling and a crash loop gives up; exits wider
  apart than the window never give up; an up-for-a-window server resets its back-off). Four
  process tests against a stdlib-Python stand-in: exit, restart, crash-loop give-up with the exact
  back-offs (50, 100 ms, then give up on the third exit); another model on the port refused and
  the selected one not started twice; a server answering as the wrong model is a failed start;
  closing the supervisor's pipe stops the server.
* Live, the shipped console checkpoint (`aletheia-console-s1`, Laya backend, MPS): ready in
  21.7 s, answered `/v1/decide` (`ls`, confidence 1.0); its process killed with SIGKILL, the
  supervisor restarted it after 500 ms and it was ready again in 22.5 s; a second `model serve`
  reported it already served and started nothing; the supervisor itself killed with SIGKILL left
  nothing listening on 127.0.0.1:8091.

## What this is not

The backend's runtime dependency is still external after this wave: the Laya sidecar is Python
and needs `pip install laya` (torch, transformers). What changed is who owns its life, not what it
is made of. Whether a native Rust forward pass (no Python) can match the sidecar's answers is the
next measurement, not an assumption. The supervisor runs in the foreground of `model serve`; the
long-running Core (`aletheiad serve`) does not start models itself. Supervision on Windows starts
the server without the tether.

## Consequences

* ADR-237 row "Console decisions": the System-1 server is supervised, identity-checked and
  provisioned on request.
* Next for this row: a go/no-go measurement of a native forward pass against the sidecar on the
  existing benches (label parity, threshold side, latency, memory, binary size).
