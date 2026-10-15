# ADR-262 — One command runs every end-to-end gate, and every gate covers the CPUs its feature runs on

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-QUAL-001, REQ-NET-006, REQ-DRV-017
**Builds on:** ADR-012 (the CI-coverage gate), ADR-127 (`e2e-all.sh` grows the live GUI leg),
ADR-140 (the live TCP conversation), ADR-227/235 (TCP and the console over e1000), ADR-258 (the
HD Audio tone, captured and measured).

## Context

`scripts/e2e-all.sh` says it is the unified end-to-end release gate. It ran five legs: the three
boot gates, the VirtualBox rung and the live GUI. CI runs sixteen more end-to-end gates (the
console and its hostile-input and crash campaigns, the model-driven console, TCP, TLS, HTTPS, DNS,
the browser, the hostile network peer, PS/2, USB, virtio-input, the desktop storm, HD Audio). A
release qualified locally with `e2e-all.sh` therefore skipped most of what CI proves, and nothing
noticed when a new gate landed without joining it.

Two gates also covered fewer CPUs than their feature runs on:

* `tcp-e2e.sh` drove aarch64 and riscv64 only. x86-64 has the same `tcp` command over
  virtio-net-pci and, since ADR-227/235, over an e1000 alone; neither was proved live by this gate.
* `hda-e2e.sh` measured the tone on aarch64 only. The x86-64 boot plays the same tone at 00:08.0
  (`hda=6` in its marker map) but its output went to QEMU's `none` backend, so a driver that ran
  the stream over zeroed buffers on x86-64 would pass.

`tcp-e2e.sh` had a latent hole as well: its "the peer received the request" check grepped a peer
log shared by every leg, so a later leg could pass on an earlier leg's conversation.

## Decision

* **`e2e-all.sh` leg 6** runs every CI-executed `*-e2e.sh` gate the first five legs do not. Each is
  classified from the gate's own verdict line: `NAME: PASS`, `NAME: SKIP`, or FAIL on a non-zero
  exit or a missing verdict. A PASS whose transcript shows a skipped CPU or arm is reported as
  `PASS (legs SKIPPED)`, so a host that ran half a gate never reads as one that ran all of it.
  `REQUIRE_ALL=1` makes every SKIP, whole or partial, fatal.
* **`check-ci-parity.sh` check [5]**: every `*-e2e.sh` script CI executes must be named in
  `e2e-all.sh`. A new gate added to CI without joining the aggregate fails the build. Negative
  control: dropping `hda-e2e` from the list fails with
  `CI executes scripts/hda-e2e.sh but scripts/e2e-all.sh does not run it`.
* **`tcp-e2e.sh` x86-64 legs**: the same session (a dead port refused by name, then the live peer)
  on the UEFI image, once with virtio-net-pci and once with an e1000 as the only NIC. The peer check
  now counts against the leg's own starting point.
* **`hda-e2e.sh` x86-64 leg**: `smoke-test.sh` takes `HDA_AUDIODEV` (default `none`); the gate runs
  the full x86-64 boot gate with `wav,path=FILE` and holds the capture to the aarch64 checks (signal
  above silence, at least 50 ms, 1500 zero crossings a second within 10 %).
* The CI jobs `tcp-e2e` and `hda-e2e` install `qemu-system-x86`, OVMF and mtools and add the
  `x86_64-unknown-uefi` target so those legs run, not skip.

## Evidence

* `tcp-e2e.sh`: aarch64 PASS, riscv64 PASS, x86-64 virtio-net-pci PASS, x86-64 e1000 PASS; the
  peer saw four requests, one per leg.
* `hda-e2e.sh`: aarch64 0.245 s of tone, RMS 3929, 1499 crossings/s; x86-64 (after `SMOKE TEST:
  PASS`) 0.244 s, RMS 3929, 1499 crossings/s.
* `check-ci-parity.sh` PASS with check [5]; FAIL under the negative control above.
* `e2e-all.sh` on this tree, on a host loaded by an unrelated renderer (load average 12): every leg
  PASS except VirtualBox (SKIP: arm64 host) and the model arms (no model served). Four legs failed
  on timing in the first pass and passed alone; four SKIPped without python `cryptography` and
  passed with it (STATUS, this date).

## What this is not

* `system1-storm-e2e.sh` stays outside: it needs a served System-1 model, CI does not run it, and
  `check-ci-parity.sh` lists it as exempt.
* `e2e-all.sh` is still a local release gate. CI runs the same gates as separate parallel jobs and
  does not run the aggregate.
* `usbkbd-e2e.sh` has no riscv64 leg (riscv64 has no xHCI driver) and `keyboard-e2e.sh` is
  x86-64 only (the i8042 is a PC device). `vinput-e2e.sh` is x86-64 only; the device-tree targets'
  virtio-input keyboard and tablet are driven live by `desktop-e2e-dt.sh` and `desktop-fuzz-e2e.sh`.
* The model arms of `console-ai-e2e.sh` and `console-agent-e2e.sh` SKIP on a host with no served
  model; `e2e-all.sh` reports that as `PASS (legs SKIPPED)` and fails only under `REQUIRE_ALL=1`.
