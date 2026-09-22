#!/usr/bin/env python3
"""Regenerate docs/BOOT-COST.md from three boot-gate logs (ADR-162).

    python3 scripts/boot-cost-harvest.py AARCH64.log RISCV64.log X86.log > docs/BOOT-COST.md

Each log is what `scripts/vm-e2e.sh`, `scripts/vm-e2e-riscv.sh` and `scripts/vm-e2e-x86.sh` printed;
the harvest reads the `[boot] FAMILY suite: N ms` / `[boot] NAME phase: N ms` laps and the
`[boot] suites:` summary and writes the page. Numbers are one boot each on one machine: relative,
not a promise about hardware.
"""
import re, sys, datetime

LAP = re.compile(r"\[boot\] (\S+) (suite|phase): (\d+) ms")
SUMMARY = re.compile(r"\[boot\] suites: (\d+) timed, (\d+) ms total, slowest (\S+) at (\d+) ms")
NAMES = ["aarch64 (QEMU virt, cortex-a72, TCG)", "riscv64 (QEMU virt, rv64, TCG)", "x86-64 (QEMU q35, OVMF, TCG)"]

def main(paths):
    today = datetime.date.today().isoformat()
    out = ["# Boot cost, measured (ADR-162)", "",
           "Every target proves its contracts at boot, suite after suite, before the console or the desktop is",
           "offered. This page is what that COSTS, read from the machine's own monotonic counter and printed on",
           "every gate's boot log: one `[boot] FAMILY suite: N ms` line under each family's marker, a",
           "`[boot] NAME phase: N ms` line after each timed phase that is not a suite, and one",
           "`[boot] suites: N timed, T ms total, slowest F at S ms` line before the console.", "",
           f"**How to read it.** Numbers are QEMU TCG on one development machine (Apple silicon, {today}),",
           "uncontended, one boot each; they are RELATIVE - which suite is heavy on which CPU - not a promise",
           "about hardware. `total` runs from the first suite to the summary. `unattributed` is `total` minus the",
           "sum of the laps: time no lap claims (device bring-up between suites, printing).", "",
           "Regenerate: `python3 scripts/boot-cost-harvest.py A64.log RV.log X86.log > docs/BOOT-COST.md` from the",
           "three boot gates' output.", ""]
    for name, path in zip(NAMES, paths):
        log = open(path, errors="replace").read()
        m = SUMMARY.search(log)
        laps = [(int(ms), fam, kind) for fam, kind, ms in LAP.findall(log)]
        total = int(m.group(2)); summed = sum(ms for ms, _, _ in laps)
        laps.sort(reverse=True)
        out += [f"## {name}", "", "| measure | value |", "|---|---|", f"| laps timed | {len(laps)} ({m.group(1)} suites) |",
                f"| total, first suite to summary | {total} ms |", f"| sum of laps | {summed} ms |",
                f"| unattributed (between laps) | {total - summed} ms |", f"| slowest suite | {m.group(3)} at {m.group(4)} ms |",
                "", "| rank | lap | kind | ms | share of total |", "|---|---|---|---|---|"]
        for i, (ms, fam, kind) in enumerate(laps[:12], 1):
            out.append(f"| {i} | `{fam}` | {kind} | {ms} | {100 * ms / total:.0f}% |")
        out.append("")
    out += ["## What the numbers say", "",
            "* **The storms and the bench are the cost, not the contracts.** On every CPU the heavy laps are",
            "  `bench`, `fsstorm`, `mlrisk-stress`, `reclaim`, `schedstorm`, `conring` and `compose`: suites that",
            "  run thousands of iterations to prove a bound holds under load. The contract suites (a TLS",
            "  handshake, the HTTP reader, the renderer, the policy) cost tens of milliseconds each.",
            "* **What the first harvest left unattributed is now named.** On x86-64 it was the VT-d suite",
            "  (`dmar`, a real remapping unit programmed and probed under TCG); on aarch64 the",
            "  performance-validation pass after the suites (`perf-report`); on all three the compositor's",
            "  marker had a shape the stopwatch missed. What remains unattributed is bring-up and printing.",
            "* **The interactive image pays all of this before its prompt.** `scripts/comparative-bench.sh`",
            "  measures boot-to-prompt on x86-64 against Linux; this page is the part of that number the",
            "  kernel controls. What to do about it (defer the storms to an opt-in `selftest` command, keep",
            "  the contracts at boot) is a decision for its own ADR, with this page as its evidence.", ""]
    sys.stdout.write("\n".join(out))

if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(sys.argv[1:])
