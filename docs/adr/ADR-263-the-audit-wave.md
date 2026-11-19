# ADR-263 — The audit wave: approval gates, durable approvals, device-controlled indices, fail-closed records

**Status:** Accepted (2026-10-22)
**Requirements:** REQ-CAP-007, REQ-DRV-006, REQ-SEC-TLS-005, REQ-QUAL-001
**Builds on:** ADR-015 (policy and approval separated from authority), ADR-043 (the DMA gate),
ADR-069 (encryption at rest), ADR-244/247/250 (accounts, roles, ownership), ADR-262 (`e2e-all.sh`).

## Context

A whole-repository audit read the hosted Core, the kernel's security, network and driver code, the
three architecture crates and the release and CI scripts. Every finding below was confirmed by
reading the code; each fix carries a test that fails on the old code where a host test can reach it.

## Decision

**Hosted Core**

* `RequireApproval` is not `Allow`. `revoke_capability`, `query_audit`, `list_pending_approvals`,
  `resolve_approval` and the service's capability and performance queries require `Allow`; an
  approval-constrained capability passes none of them. Inline consent (`approve: true`) applies only
  when authority is `Allow`, so an approval-constrained holder always goes pending, and it cannot
  resolve its own approval.
* `ApprovalRequested` and `ApprovalResolved` are persisted before the in-memory registry changes; a
  failed write is an error, not a pending approval that vanishes on restart or one that can be
  resolved twice.
* `Store::rewrite_log` changes `frame_seq` and the per-version counts only after the rename: a rekey
  that fails part-way leaves the old log and the counters that describe it, and removes its temp file.
* Key rotation stops at version 255: a frame names its key in one byte.
* `fsync` errors propagate from `append_frame` and `write_private`.
* The Unix endpoint sets a write timeout on each connection, creates the socket mode 0600, and
  refuses to replace a path that is not a socket.

**Kernel**

* virtio-input refuses a used-ring id at or past the queue size; `Virtqueue::add` checks the slot.
* virtio-net drops a completion longer than its receive buffer.
* `DmaRegistry` bounds checks no longer wrap on `addr + len`.
* virtio-blk: a batch that times out or names a foreign id wedges the driver until `reset()`, so a
  late completion is never taken for a new request; `reset()` zeroes the ring indices it owns.
  The x86-64 VT-d suite resets the device before each probe kick, as the aarch64 SMMU suite already
  did after restoring a grant: a kick the unit swallows leaves its request in flight.
* SMMU `drain_events` reads at most one queue's worth per call.
* LZSS refuses a declared length the payload cannot expand to before allocating.
* `.users` and `.owners`: only `NotFound` means "no record". A storage error keeps the console
  locked, refuses `passwd`, and denies ownership changes. Owner lines split on the last `:`.
* Peer and file bytes with control characters other than tab, newline and CRLF are reported as not
  text, never printed.
* An oversize datagram is dropped rather than ending a TCP exchange.
* Ed25519 refuses small-order public keys.
* `build_identity` returns every table it took when the pool runs out; two-space suites destroy the
  first space when the second fails; x86-64 `build_space_from` frees the PML4 if the PDPT fails.

**Gates**

* `release.yml` sets `REQUIRE_VERIFY=1`: a package that was not booted fails the release.
* `conformance.sh` counts each leg's exit status; CI sets `REQUIRE_X86=1`.
* `quality-gate.sh` ignores RUSTSEC-2026-0367/0368/0369 for `aletheia-laya` only, with the reason in
  the script: they are all in `zip`'s `extract`, which candle (the only path to `zip`, pinned below
  9 in every release) never calls. Accepted risk, removed when candle moves to `zip` 9.

## Evidence

* Host tests added: `an_approval_constrained_holder_cannot_approve_itself`,
  `approval_constrained_caps_do_not_pass_boundary_gates`,
  `a_rekey_that_fails_midway_leaves_the_store_openable`,
  `key_rotation_stops_at_the_last_version_a_frame_can_name`, `a_small_order_public_key_is_refused`,
  `a_declared_length_the_payload_cannot_reach_allocates_nothing`,
  `a_span_that_wraps_the_address_space_is_never_visible_or_registered`,
  `an_object_name_with_a_colon_keeps_its_owner`, `bytes_that_would_drive_the_terminal_are_not_text`,
  `an_unreadable_account_record_keeps_the_console_locked`,
  `after_a_timeout_every_request_refuses_until_a_reset`.
* `aletheia` and `kernel-core` test suites pass; the three architecture kernels build; the seven
  `check-*.sh` gates pass. `e2e-all.sh`: every leg PASS after the VT-d reset above (the first run
  failed VT-d invariant 12, which is how the wedge was found to need it), except VirtualBox (arm64
  host), the model arms (no model served) and TLS/HTTPS/browser/net-fuzz (no python
  `cryptography` on this host; CI runs them). `REQUIRE_X86=1 conformance.sh` PASS, 408 behaviors.

## What this is not

* **Approval separation of persons.** A principal holding `Allow` for a destructive action may still
  consent inline or resolve its own pending approval. Telling requester from approver needs a
  principal identity on the service boundary that does not exist yet; until then the 0600 socket
  bounds who may hold such a capability. Accepted residual.
* NVMe, GPU, HD Audio and virtio-net timeouts still leave the request in flight; each needs the
  virtio-blk wedge-until-reset treatment. Deferred.
* Local port, TCP initial sequence number and DNS id are predictable, the TCP pumps accept a reset
  without checking the sender's address, and the ARP cache never ages. Deferred to a network wave.
* The SMMU register seam carries no memory barriers; QEMU masks the ordering. Deferred with the
  emulated-only limit already recorded for the v0.8.0 drivers.
* `frames.rs`, `desktop.rs` and `heap.rs` are near-copies across the three crates (about 900 lines).
  Deferred.

## Addendum 2026-11-19: candle 0.11 closes the `zip` advisories

candle 0.11.1 (2026-10-31) depends on `zip` 9, so `aletheia-laya` moves to candle 0.11.1 and
`tokenizers` 0.23 (the version candle now carries, so the tree holds one copy). candle turns on
`tokenizers`' `fancy-regex` feature; with `onig` also on, Oniguruma stays the pre-tokenizer's regex
engine (`tokenizers` selects `fancy-regex` only without `onig`), so the pure-Rust crate is compiled
but unused. No progress bars and no C++ suffix arrays, as before. `quality-gate.sh` drops its
ignore list: RUSTSEC-2026-0367/0368/0369 and the five `zip` 7 advisories published since (0429,
0545, 0546, 0547, 0548) no longer apply. `aletheia` takes
`ctutils` 0.4.4 for RUSTSEC-2026-0385 (non-constant-time masks, reached through `poly1305`).

Parity before and after, `scripts/system1/native_parity.py` against a binary built from the
previous lockfile, same checkpoint: console v4, console corpus, 400 rows, CPU and Metal, 932/932
answers each, max |Δconf| 0.0000; scheduler run 2, `corpus-v2`, seed 31337, 300 rows, CPU, 700/700,
max |Δconf| 0.0000. Accuracy and latency unchanged (388/400 and 251/300 on both builds).
