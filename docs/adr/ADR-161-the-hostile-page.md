# ADR-161 — The hostile page: a property campaign over the browser stack

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-007; threat-model boundary B-14
- **Supersedes:** nothing. Adds evidence under ADR-155, ADR-156, ADR-158, ADR-159.

## Context

Every byte the browser reads arrives from a peer that may be an adversary: the HTTP head and
body, the HTML, the URLs a page offers. The boot suites prove NAMED behaviours on NAMED inputs -
a chunked body, a script tag, a plaintext URL. History says browsers are broken by the inputs
nobody named: a header that lies about a length, a tag never closed, a byte that drives a
terminal, a URL that carries a line end into the request. The threat model had no row for this
boundary at all.

## Decision

`kernel-core/tests/hostile_page.rs` is a deterministic property campaign in the shape of the
existing `property_campaign.rs` - a tiny seeded generator, no dependency, a `PROPERTY FAILURE
seed=... case=...` line that reproduces any failure, the failing input shrunk before the panic
is re-raised. It runs under `scripts/property-campaign.sh` with the same seed and case count as
the soak campaign, so CI's 64-case run drives a few thousand adversarial inputs.

Four campaigns, and what must hold for EVERY generated input:

- **The renderer** (`content::render`), over documents built from known and unknown tags,
  script/style/iframe/object/embed bodies carrying a marker, unterminated tags, comments,
  declarations, entities valid and bogus, control and high bytes, deep nesting, documents past
  the input bound, into buffers of random size followed by guard bytes: nothing panics, the
  guard bytes stay, every shown byte is printable ASCII or a newline, the marker never reaches
  the page, links/hrefs/title respect their caps, no line is wider than the grid and no more
  lines than rows, a document past the bound is SAID to be cut, and rendering twice is identical.
- **The HTTP reader** (`http::parse`), over responses with honest and lying lengths, both body
  framings and both at once, chunk sizes past the digit bound, folded and colon-less headers,
  too many headers, header lines past their cap, `Set-Cookie`, flipped bytes, truncations and
  leading garbage: nothing panics, nothing is written past the body buffer, every span lies in
  the raw bytes, an honest complete answer is read exactly (or refused only for a header past
  its cap) with truncation said exactly when it happened, and parsing twice agrees.
- **URLs and requests** (`browser::parse_url`, `http::request`), over bytes of every value, hosts
  and paths at and past their bounds, ports past 65535, and a URL carrying `\r\nHost: evil`: a
  URL that parses has a host in its alphabet and a sendable path, writes back to text that parses
  to the same URL, and a request built from any host and path is either refused or carries
  exactly six line ends - the five this client writes plus the terminator - so no input can
  smuggle a header onto the wire.
- **The navigator** (`browser::Navigator`), under random sequences of `trust`, `block`, navigate,
  `back`, `forward`, `forget` and pages offering links: a host the operator did not pin never
  resolves, a blocked host never resolves (pinned or not, typed or reached by `back`), a
  resolution carries the host's own pin, history never exceeds its ring, a new navigation drops
  the forward pages, and following a link is refused exactly as typing it would be.

The threat model gains **B-14, remote page -> browser stack**, with this campaign, the live gates
and the four modules as its evidence.

## Proof

Host: four campaigns green at the default seed (32 cases, scaled 4-16x per campaign) and at CI's
64; the campaign found and fixed two wrong ORACLES in its own writing (request line count,
history after `back`) and no fault in the stack. Conformance unchanged (383): this is host
evidence over generated input, not a new boot invariant. `scripts/check-threat-model.sh` holds
with the new row.

## Alternatives considered

**proptest / a fuzzing crate.** Rejected for the reason `property_campaign.rs` rejected it: a
dependency-free generator reproduces from a seed in any hosted environment, and the shrinker is
independent of the generator.

**A coverage-guided fuzzer in CI.** Worth doing when a fuzzing target exists for the crate; not
this wave, and not instead of this: a seeded campaign is a regression gate, a fuzzer is a search.

## Consequences

The browser stack's bounds and refusals are proved over generated adversarial input on every
push, and the threat model names the boundary. What the campaign does NOT prove is named in the
threat model's residual section: that this HTML subset parses the way any other browser parses
it. It does not, by design (ADR-158).
