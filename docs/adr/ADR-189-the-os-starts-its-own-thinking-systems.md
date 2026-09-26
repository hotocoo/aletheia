# ADR-189 — The OS starts its own thinking systems

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-AI-014 (new)
**Builds on:** ADR-186 (roles), ADR-187 (the shipped System 1), ADR-017 (hosted model runtime).

## Context

Both thinking systems were operator-started by hand-typed commands: a `llama-server` invocation for
System 2 and a Python command line for System 1, each with a path the operator had to look up. A
system that ships a model but cannot start it has shipped a download.

## Decision

`aletheiad model serve [<id>]` builds each server's command from the manifest alone
(`runtime::serve_command`) and runs it in the foreground until one exits:

* System 2 on `llama_cpp`: `llama-server -m <weights> -c <context> --host 127.0.0.1 --port <port>
  --jinja` (the port from the manifest endpoint, or `MODEL_ENDPOINT`).
* System 1: `python3 <sidecars>/<backend>_server.py <weights dir> --serve-id <serve_id> --port
  <port>` (the port from the manifest endpoint, or `SYSTEM1_ENDPOINT`). `<sidecars>` is
  `ALETHEIA_SIDECARS`, else the tree the binary was built from.
* With no id, both selected systems. A model whose weights are absent is refused with the
  `model pull` it needs; a backend with no known server is refused by name.

## Measured

`model serve aletheia-console-s1` then `console plan --interpreter dual "list every object on this
machine"`: `route: system1 (confidence 0.98)`, `ls`. Resident memory on the workstation (MPS):
System-1 sidecar 1.4 GiB; `llama-server` with the 2.6 B System 2, 0.65 GiB resident (its weights
are memory-mapped). Serving System 1 in fp16 was tried and rejected: 0/8 on the console bench and no
drop in resident memory (the backend loads fp32 before any cast).

## Non-claims

No supervision (a crashed server is not restarted), no lazy start, no idle unload. The System-1
sidecar still needs `pip install laya`.
