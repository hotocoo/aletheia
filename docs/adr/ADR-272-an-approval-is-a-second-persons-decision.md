# ADR-272: An approval is a second person's decision

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-CAP-009 (new)
**Builds on:** ADR-015 (policy and approval separated from authority), ADR-263 (an
approval-constrained holder cannot approve itself; approvals are durable). Closes the last item of
ADR-263's "What this is not".

## Context

ADR-263 left one residual: "A principal holding `Allow` for a destructive action may still consent
inline or resolve its own pending approval. Telling requester from approver needs a principal
identity on the service boundary that does not exist yet." So a request that went to review because
policy demanded a human could be reviewed by the same human, which makes the review a formality.

The identity already exists, in the capability: every stored capability names its `subject`, and a
caller cannot present a capability it does not hold. The request's own `subject` field is the
caller's claim; the capability's is the system's.

## Decision

* **Who asked is recorded.** When `run_intent` sends a request to review, the pending approval stores
  `requested_by`: the subjects of every live capability in the request that covers the bound action
  (`CapEngine::covering_subjects`), approval-constrained or not. It travels in the durable
  `ApprovalRequested` record, so it survives a restart.
* **Someone else resolves.** `resolve_approval` still requires `Allow` for the bound action (ADR-263),
  and now also that at least one of the capabilities that grant it belongs to a principal NOT in
  `requested_by`. Otherwise it is refused by name, grant and deny alike, so a requester cannot deny
  its own request to clear the queue either. A second token minted for the same principal is the same
  person.
* **Inline consent is unchanged**: an `Allow` holder may still consent with `approve: true` when it
  submits, which is the one-person path. Choosing review means choosing a second person.
* Records written before this ADR carry no `requested_by` and keep the ADR-263 rule; console
  approvals (`request_console_approval`) are resolved by the console's own operator and carry none.

## Evidence

* `aletheia/tests/security.rs` `an_allow_holder_cannot_approve_its_own_request_and_the_rule_survives_restart`:
  the owner's own `Allow` is refused after a restart, a second owner token cannot even deny, and a
  reviewer's scoped grant executes the delete.
* `aletheia/tests/conformance.rs` `destructive_requires_approval_lifecycle_over_the_api`: over the
  service boundary, self-approval is refused and a delegated reviewer resolves.
* Mutation: disabling the check fails the conformance test.
* `cargo test` for `aletheia` (with `AI_PROVIDER=deterministic`; `model_registry` run plain), clippy
  under Rust 1.100, quality gate, every `check-*.sh`.

## What this is not

* **Not authentication.** A principal is whoever holds its capability tokens; two tokens handed to one
  person make two principals. The 0600 socket still bounds who can hold any.
* **No quorum**: one other principal suffices.
