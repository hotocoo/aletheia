# ADR-158 — The content renderer: a bounded, fail-closed subset of HTML into lines

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-004, Lethe integration stage N5
- **Supersedes:** nothing. Sits on ADR-156/157 (the page and the window).

## Context

Lethe's stage N5: "a bounded, fail-closed subset renderer into the window's `TextGrid`. This is
where 'a browser' starts being a real word." It is also where browsers historically get hurt:
attacker-chosen input, a forgiving grammar, and a renderer that tried to make sense of everything
became a program the page controls.

## Decision

`kernel-core/src/content.rs` makes sense of a LIST and nothing else: headings, paragraphs, line
breaks, lists, preformatted text, links, the title, horizontal rules. Nothing allocates; nothing
executes.

- **Dropped whole:** the CONTENT of `script`, `style`, `template`, `iframe`, `object`, `embed` —
  to the matching close tag or the end — and counted, so a page can say how much it refused.
- **Invisible:** any tag this renderer does not know. Its text still flows; its attributes never
  become text.
- **Bounded:** the input at 8 KiB (rendered to the bound and said to be cut), the output at the
  grid's width and rows (cut at the last row, never written past the buffer), links at sixteen,
  hrefs and the title at fixed lengths.
- **Entities:** five named ones and decimal references decode; anything else stays literal
  rather than guessed. Bytes outside printable ASCII show as `?`, so no page can drive a terminal.
- **Links** keep their text in the flow and are numbered `[n]`; their hrefs are kept in the
  navigator, made absolute against the current page when they are paths, and followed with
  `follow N` — through `navigate`, so a plaintext or unpinned link is refused for what it is.
- **Comments, declarations and an unterminated tag** end where they end: a `<` with no `>` ends
  the document rather than leaking markup into the page.

The console's `go` and the browser window render `text/html` answers through this; every other
content type is shown as text, bounded as before.

## Proof

`content=8` on all three CPUs: lines from headings, paragraphs and breaks with collapsed
whitespace and the title kept; script and style content dropped whole and counted; links
numbered with hrefs kept; lists, `pre`, known entities and literal unknown ones; output cut at the
grid and the guard bytes untouched; input cut at the bound; unknown tags invisible and an
unterminated tag ending the page; comments skipped and terminal bytes shown as `?`. Host: every
prefix of a page renders without panic or overflow. `console=50`: `follow` refuses a link the
page never offered. **Live (`scripts/https-e2e.sh`):** the real server's `/index.html` renders
its heading, its numbered link and its list, never its script; `follow 1` fetches the plain page;
`follow 7` is refused by name. Conformance contract 365 -> 374 core behaviours.

## Alternatives considered

**A DOM.** Rejected: a tree the page shapes is memory the page controls, on a heap that never
frees.

**CSS, images, forms, tables with layout.** Not this rung; each is a decision about what to
refuse, made when its rung comes.

## Consequences

Lethe's stage N5 is delivered for the subset named. Stage N6 — the policy contract as invariants
— is next.
