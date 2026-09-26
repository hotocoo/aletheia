# ADR-185 — The console names its authority and its wire

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-CON-010 (new)
**Builds on:** ADR-089 (console authority is capabilities), ADR-140 (the kept NIC), ADR-162 (boot
time is measured), ADR-184 (the console reaches the machine).

## Context

After ADR-184 the console could read the clock and drive the power governor, but three facts the
machine already holds were still invisible from it: which capabilities the console itself is
exercising, what network device it dials with, and where the boot's time went (printed once to the
serial log, then gone).

## Decision

Three report-only commands in the shared dispatcher, so every CPU gets them:

* **`caps`** — `ShellHost::capabilities` walks the console's offered tokens through the new
  `CapEngine::for_each_offered`: subject, action and live/REVOKED per capability. Tokens are never
  printed: naming a capability is not holding it. Defaulted to naming none.
* **`net`** — `ShellHost::net_facts` returns a copied `NetFacts` (MAC, address, gateway, frames
  dropped, ARP requests on the wire, DMA regions, next local port) from each target's
  `netstatic::facts()`, or `None` with no NIC. The addresses are the static plan's; there is no DHCP
  client yet, and the line says `(static)`.
* **`boot`** — `boottime::recorded()`: the suite count, total and slowest family as the boot's own
  `summary` took them, without reading a clock again. Before the summary it says so.

## Proof

`console=57` on aarch64, riscv64 and x86-64 (three new boot invariants); the heap storm's reporting
round now includes `boot`, `caps` and `net` and still requires zero bytes over 256 commands;
`scripts/console-e2e.sh` and `scripts/console-fuzz-e2e.sh` PASS on all three CPUs; the hosted planner
classifies all three `Safe` (`every_command_is_classified`).

## Non-claims

`net` reports; it configures nothing. No DHCP, no interface up/down, no routing table — the
network plan is still ADR-140's static one.
