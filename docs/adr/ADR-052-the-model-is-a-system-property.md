# ADR-052 — The model is a property of the system, not a constant in the source

**Status:** Accepted
**Date:** 2026-08-10
**Requirements:** REQ-AI-004 (a pinned, operator-selectable model registry), REQ-AI-005 (the
operation-surface benchmark)
**Extends:** ADR-017 (AI subsystem), ADR-018 (Context Engine)

---

## Context

ADR-017 got the shape right: the model sits behind `ModelProvider`, the Core never depends on
llama.cpp APIs, and the AI is untrusted — it proposes a plan and the deterministic pipeline decides
everything. What ADR-017 did *not* settle is which model, and how anyone changes it.

In practice the answer was two `const`s in `ai/mod.rs` plus a manifest, `models/minicpm.toml`, that
no code ever read. Three consequences, all of them the same mistake wearing different clothes:

1. **Changing the model meant editing and rebuilding the OS.** The model was a property of the
   source tree. An operating system whose intelligence can only be changed by a compiler is not
   offering the operator a choice; it is offering the developer one.
2. **The machine could not say what it was running.** `aletheiad model` printed a path derived from
   a compile-time string. If a different model was actually being served on the endpoint — trivially
   possible, since the endpoint is a *port* and any process can hold one — nothing anywhere noticed.
3. **The manifest was decoration.** `models/minicpm.toml` pinned a repo, a file, a quant and a
   checksum, and not one of those values reached the code. A pin nothing reads is a comment.

There was also a claim nobody had tested. Everything this repository proves about the AI subsystem
is a claim about *structure*: the provider is swappable, the plan is validated, the model cannot
execute. None of it says whether the resident model can produce a usable plan for the operations
Aletheia actually offers. That question had never been asked, so it had never been answered.

---

## Decision

### 1. A registry of pinned manifests, embedded in the binary

`models/*.toml`, one per model, carrying everything needed to *find*, *serve*, *verify* and *report*
it: repo, exact file, quant, measured sha256, size, context, sampling parameters, the structured
output strategy, and `serve_id` (below). They are compiled in with `include_str!`, so a binary can
never disagree with the manifests it was built from, and `aletheiad` on a machine with no checkout
still knows its own model set.

The set is closed on purpose. An operator chooses among models Aletheia has *pinned* — with a
checksum it can be held to — rather than naming an arbitrary hub path the OS has made no claim
about. `MODEL_REF` / `MODEL_PATH` remain the escape hatch for anything outside the set, and the
configuration says plainly when it has been used (`(unregistered)`).

Three entries ship:

| id | what it is |
|----|-----------|
| `lfm2.5` | **LFM2.5-2.6B (Q4_K_M)** — the new default resident model |
| `minicpm` | the previous default, retained so earlier measurements keep their baseline |
| `aletheia-lm` | Aletheia's own model, **registered before its weights exist** |

### 2. Selection is persisted, and resolution is an order

`aletheiad model use <id>` writes one id to `<data>/ai/selected-model`, so the choice survives a
reboot without an environment variable anybody has to remember. Resolution is highest-first:

1. `MODEL_REF` / `MODEL_PATH` / `MODEL_ENDPOINT` — the escape hatch
2. the persisted selection
3. the manifest marked `default`

An order rather than a merge, because an operator who exported `MODEL_PATH` to try something is
entitled to have it beat a selection they made last month.

The model commands keep that selection under `$HOME/.aletheia`, deliberately **not** the daemon's
`--data` default: `data_dir` invents a fresh temp directory per run, which is right for a demo that
should leave nothing behind and catastrophically wrong for a machine-level choice — the switch would
report success and have no effect.

`model use` is also not silent about the outcome: it persists the choice and *immediately* reports
whether the chosen model is present, whether the backend is serving it, and — for a model still in
training — that it is `NOT YET TRAINED`. The failure this guards against is a switch that appears to
work while the OS quietly keeps answering from the deterministic interpreter.

### 3. `aletheia-lm` is registered before it exists

This looks odd and is the point. The operator can line the switch up the moment pretraining
finishes, without editing source — and, more importantly, selecting it *early* produces a named
refusal rather than a fallback. `model pull` refuses it by name (there is no hub artifact), and
`model status` names the environment variable that will point at the finished weights.

### 4. The operation-surface benchmark, and the identity check that guards it

`aletheiad model bench` puts one intent per registered operation through the **same** provider the
pipeline uses, and the output through the **same** `parse_plan` + `validate_plan`. A row passes only
if the plan parses, validates, and leads with the operation the intent was about. The deterministic
interpreter runs the identical set as the control arm: correct by construction, so it is what a
perfect model would produce, and its latency is the floor.

Before any measurement is taken, the benchmark asks the backend what it is serving (`/v1/models`)
and requires the answer to contain the manifest's `serve_id`. **A mismatch is a refusal, not a
warning.** The endpoint is a port; on the machine this was developed on, another project's service
already held `:8080`. Without the check, the most likely outcome of running this is a table of
someone else's latencies published under this model's name — a wrong number that looks exactly like
a right one, and one that cannot be corrected after it has been quoted.

---

## What the benchmark found

Running it, rather than reasoning about it, produced four defects in one afternoon. Recorded here
because each is a class of bug the existing gates structurally could not see.

**The HTTP client resolved one address.** `to_socket_addrs().next()` — `localhost` resolves to `::1`
before `127.0.0.1`, and `llama-server` binds IPv4 only by default. Every probe failed, `healthy()`
returned false, and the Core fell back to the deterministic interpreter *while the model was running
the whole time*. This had been true since ADR-017; it is indistinguishable from "no model present",
which is exactly why nothing caught it.

**A GBNF grammar is not universal.** The grammar was written for MiniCPM and is the stronger
constraint — it forbids every token outside the plan. LFM2.5's chat template opens with
`<|tool_call_start|>`, which the grammar has no rule for, so generation died at token zero and
returned an **empty completion for all six operations**. A total failure that reads exactly like a
model that cannot plan. Structured output is therefore a per-manifest strategy
(`json-schema` | `gbnf-grammar`), not a constant every future model has to survive.

**The output budget was too small, and failed silently.** `n_predict = 512` looked generous for a
~50-token plan. Under a schema-constrained decode the model may emit a long run of *permitted*
whitespace before committing to the object, and `capability.grant` — the widest argument list —
reproducibly exhausted 512 that way, returning an empty completion with `finish_reason: length`.
Measured at 2048 with temperature, prompt and schema held fixed: 5/6 → 6/6.

**The prompt named operations but not their arguments.** With only operation names in the menu,
LFM2.5 answered `entity.read` for a traverse *and* for a grant: it was never told what a
`world.traverse` takes, so it fell back to the one operation whose shape it could infer. `OpMeta`
now declares each operation's argument names, and the prompt is generated from the same registry the
validator checks against — one list, so the interface the model is given cannot drift from the
interface the Core enforces.

**Measured, on this workstation** (LFM2.5-2.6B-Q4_K_M, llama.cpp, `-c 8192`): **6/6 operations
planned correctly, on two consecutive runs**, median 3.5–3.9 s per interpretation against a
deterministic control arm of 0 ms and 6/6. Before the four fixes: 0/6, then 2/6.

---

## Consequences

**Good.** The model is now something an operator sets and the machine reports, with a pin that can
be checked. The first-party model has a place to land before it exists. Four real defects are closed,
three of which had been live since ADR-017. The benchmark is a gate: it exits non-zero when any
operation fails, so it can be depended on by a script.

**The cost.** Three manifests are a third thing to keep in step with the code; a unit test holds the
compiled-in constants equal to the default manifest so the copy cannot drift silently. The subset
TOML parser is another parser in the tree — deliberately small, and it refuses what it does not
understand.

**Temperature is zero for the default model.** A plan is not creative writing. At 0.3 the same
intent planned correctly on one run and not the next, and an interpreter whose answer depends on the
roll is one whose failures cannot be investigated.

---

## What is NOT claimed

**The benchmark does not touch the kernel console.** It drives the *hosted Core's* operation surface
(`aletheia/`). The console lives in `kernel-core/src/shell.rs`, in kernel space, in a `no_std` crate,
with no inference engine underneath it and no path from one to the other in this build. Two surfaces,
two gates: the console has `scripts/console-e2e.sh` and its own live invariants. Anyone reading
"Aletheia benchmarked all its commands" should read it as the six registered *operations*, not the
console's commands.

**Six operations is the whole surface, and it is small.** `prompt::OPERATIONS` is what the Core
offers today. A model scoring 6/6 has been shown to plan those six from a structured intent — not to
be a good model, not to handle free text, and not to be safe to trust. It remains untrusted at every
downstream stage (INV-014).

**One machine, one quant, one backend.** These numbers are from one workstation running one Q4_K_M
GGUF under llama.cpp. They are a floor for reproducing the setup, not a benchmark of the model.

**A checksum is recorded, not enforced.** The manifests pin a measured sha256; nothing yet verifies
the resolved file against it at load time. That is the natural next row.

`docs/MATURITY.md` governs every claim above.
