# Try Aletheia yourself

**As of:** 2026-08-10 · REQ-CON-001/006, REQ-AI-007..010, REQ-PERF-001 · ADR-044, ADR-054/055/056

This is the operator's path: boot Aletheia in a VM, type at it, then let the resident model type at
it for you — and re-run every gate whose numbers this repository quotes, so you are checking rather
than believing.

`docs/MATURITY.md` grades every subsystem and says plainly that **nothing here is production-ready**.
Read it before quoting anything below.

---

## 0. Which path works on your machine

The one thing that trips people up first, so it is first:

| Your machine | Use | Why |
|---|---|---|
| **Apple Silicon Mac (arm64)** | **QEMU** — sections 1–4 | VirtualBox virtualizes the *host* architecture. It installs and answers `--version` on arm64, then fails at `startvm` for an x86-64 guest. `scripts/vm-e2e-vbox.sh` detects this and SKIPs, saying so. |
| **Intel Mac / Windows / Linux (x86-64)** | QEMU **or** VirtualBox | VirtualBox is the no-QEMU path — see [VIRTUALBOX.md](VIRTUALBOX.md). |

Check with `uname -m`. `arm64`/`aarch64` means QEMU.

### What you need

```bash
# macOS
brew install qemu mtools zstd            # + Xcode CLT
# Debian/Ubuntu
sudo apt-get install -y qemu-system-arm qemu-system-misc qemu-system-x86 ovmf mtools dosfstools zstd
```

Rust nightly, pinned — the toolchain file does this for you on first build:

```bash
rustup toolchain install nightly-2026-08-09 --profile minimal
rustup component add rust-src --toolchain nightly-2026-08-09
rustup target add aarch64-unknown-none-softfloat riscv64gc-unknown-none-elf x86_64-unknown-uefi \
  --toolchain nightly-2026-08-09
```

> **If a build says `can't find crate for core`:** a Homebrew `cargo` is shadowing rustup's. Fix the
> session with `export PATH="$HOME/.cargo/bin:$PATH"`. Every script in `scripts/` already does this.

---

## 1. Boot it and type at it

```bash
./scripts/run-interactive.sh aarch64     # or: riscv64, x86_64
```

The machine boots, runs its invariant suites in kernel space, and hands you a prompt. This is a real
OS on a real (virtual) machine, not a simulator of one:

```
aletheia> help
aletheia> write manifesto the OS you can sit in front of
aletheia> ls
aletheia> grep front manifesto
aletheia> cp manifesto backup
aletheia> wc backup
aletheia> halt
```

Twenty-seven commands, arrow keys, `Home`/`End`, history on the up arrow, `Tab` completion over names
that actually exist. The disk persists between runs at
`kernel*/target/interactive-persistent.img` — boot again and `ls` still shows what you wrote. Delete
that file for an empty namespace. **`Ctrl-A X` quits QEMU.**

Try all three CPU targets. It is the same `kernel-core` dispatcher on each.

---

## 2. Let the model drive the console

This is the part that makes Aletheia what it claims to be: the model does not get an API around the
OS, it gets **the operator's surface with the operator's authority**, and it cannot type a line you
could not have typed.

### Get the weights and serve them

```bash
./aletheia/target/release/aletheiad model status     # what is selected, and whether it is present
./aletheia/target/release/aletheiad model pull       # ~1.6 GB into the local HF cache
```

Then serve it. **`--jinja` is not optional** — without it `llama-server` never parses a tool call,
and a correct answer reads as no answer at all:

```bash
llama-server -m "$(./aletheia/target/release/aletheiad model status | sed -n 's/^weights: *present — //p')" \
             -c 8192 --port 8099 --host 127.0.0.1 --jinja
```

### Ask it for one command

```bash
export MODEL_ENDPOINT=http://127.0.0.1:8099
printf '  objects:\n    30 manifesto\n    12 poem\n' > /tmp/brief.txt

./aletheia/target/release/aletheiad console plan --interpreter model \
  --context-file /tmp/brief.txt "show me the poem"
```

It prints **one console line** and nothing else. That line is validated against the kernel's own
command table, so it is a line you could have typed. Then type it at the prompt from section 1.

### Ask it for a whole session

```bash
T=/tmp/session.json
./aletheia/target/release/aletheiad console agent --transcript $T --interpreter model \
  --context-file /tmp/brief.txt --approve \
  "make a copy of manifesto called backup, then tell me how big the copy is"
```

Exit `0` with a line on stdout means *type this and come back*. Feed the console's reply straight
back and ask again:

```bash
echo "manifesto -> backup (30 bytes)" > /tmp/obs.txt
./aletheia/target/release/aletheiad console agent --transcript $T --interpreter model \
  --context-file /tmp/brief.txt --approve --observation-file /tmp/obs.txt \
  "make a copy of manifesto called backup, then tell me how big the copy is"
```

Exit `10` means it is done and `answer:` on stderr is the answer. Watch stderr for `corrected:` lines
— those are proposals Aletheia refused and re-asked, which never reached the machine — and for
`turn-ms:` and `model-call:`, which are what the turn cost.

**Try to make it do something it should not.** These are the bounds, and they hold whatever the model
proposes:

```bash
# destructive without --approve: refused, and NOTHING on stdout for a driver to type
./aletheia/target/release/aletheiad console agent --transcript /tmp/t1 --interpreter model \
  --context-file /tmp/brief.txt "delete the poem"; echo "exit=$?"

# stopping the machine: refused EVEN WITH --approve
./aletheia/target/release/aletheiad console agent --transcript /tmp/t2 --interpreter model \
  --context-file /tmp/brief.txt --approve "shut the machine down"; echo "exit=$?"
```

---

## 3. Re-run the gates that produced this repository's numbers

Nothing here needs you to trust a README.

```bash
# The model drives a multi-step session at a live console, on all three CPU targets.
# Without MODEL_ENDPOINT the model arm SKIPs loudly and the deterministic arm still gates everything.
MODEL_ENDPOINT=http://127.0.0.1:8099 ./scripts/console-agent-e2e.sh

# Boot, type, halt, reboot, read it back — three targets.
./scripts/console-e2e.sh

# Every VM boot gate, all targets.
./scripts/e2e-all.sh

# Aletheia against a real Linux kernel on the SAME emulator.
./scripts/comparative-bench.sh
WITH_REDOX=1 ./scripts/comparative-bench.sh     # + Redox OS (~70 MB download)

# The three claim-checking gates: evidence exists, requirements are tracked, gates actually run.
./scripts/check-traceability.sh && ./scripts/check-register.sh && ./scripts/check-ci-parity.sh
```

`scripts/comparative-bench.sh` prints what Aletheia **loses** as well as what it wins, and reports
boot time as a median over repeated runs with the individual samples shown — because a single boot
under TCG is noise, and this script twice read one sample as a result before that was fixed.

---

## 4. What you are looking at, and what you are not

**Real.** A microkernel that boots on three CPU architectures, a capability check on the syscall path
that a user-mode process cannot talk its way past, an interactive console with persistent storage
across a power cycle, and a resident model that reaches that console through the same validation an
operator's keystrokes go through.

**Not real yet.** No inference engine in kernel space — `kernel-core` is `no_std` with no network,
the model runs on the host, and what crosses into the guest is a line of ASCII. No package manager,
no graphics, no general hardware support, one flat namespace on one block device. Approval is a CLI
flag rather than the Core's human-in-the-loop surface (**ALET-P2-046**, open).

`docs/MATURITY.md` is the authority on all of it. If something here disagrees with that file, that
file is right.
