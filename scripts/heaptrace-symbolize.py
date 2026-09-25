#!/usr/bin/env python3
"""Attribute heap allocations from a `heaptrace` kernel's serial log (ADR-182).

Build the traced aarch64 kernel, run anything against it, then symbolize:

    cd kernel && RUSTFLAGS="-C link-arg=-Tlinker.ld -C force-frame-pointers=yes" \\
        cargo build --features interactive,heaptrace --target-dir target/heaptrace
    DFUZZ_ELF=$PWD/target/heaptrace/aarch64-unknown-none-softfloat/debug/aletheia-kernel \\
        DFUZZ_TARGETS=aarch64 ../scripts/desktop-fuzz-e2e.sh
    ../scripts/heaptrace-symbolize.py target/heaptrace/aarch64-unknown-none-softfloat/debug/aletheia-kernel \\
        target/desktop-fuzz-aarch64.log [--after N]

Every `[heaptrace] <size> <ra>...` line is mapped to function names with `llvm-nm` (the nearest
text symbol at or below each return address), allocator plumbing is dropped, and call sites are
ranked by bytes. `--after N` starts at the N-th `heap:` line of `mem` output, so a measured window
can be isolated from boot-time and warm-up allocations.
"""
import bisect
import collections
import glob
import os
import re
import subprocess
import sys


def llvm_nm():
    hits = glob.glob(os.path.expanduser("~/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-nm"))
    return hits[0] if hits else "llvm-nm"


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    elf, log_path = sys.argv[1], sys.argv[2]
    after = int(sys.argv[sys.argv.index("--after") + 1]) if "--after" in sys.argv else 0
    out = subprocess.run([llvm_nm(), "-n", "--demangle", elf], capture_output=True, text=True).stdout
    syms = []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in ("t", "T"):
            syms.append((int(parts[0], 16), parts[2]))
    addrs = [a for a, _ in syms]

    def name(addr):
        i = bisect.bisect_right(addrs, addr) - 1
        return syms[i][1] if i >= 0 else "?"

    log = open(log_path, errors="replace").read()
    if after:
        marks = [m.end() for m in re.finditer(r"heap: \d+ B used", log)]
        log = log[marks[after - 1]:] if len(marks) >= after else ""
    plumbing = re.compile(r"alloc::|__rust|RawVec|raw_vec|heap::|core::ptr|<alloc")
    count, total = collections.Counter(), collections.Counter()
    for m in re.finditer(r"\[heaptrace\] (\d+)([ 0-9a-f]*)", log):
        chain = [name(int(x, 16)) for x in m.group(2).split()]
        keep = [c for c in chain if not plumbing.search(c)]
        site = " <- ".join(c[:70] for c in keep[:3]) or "(allocator only)"
        count[site] += 1
        total[site] += int(m.group(1))
    print(f"{sum(total.values())} bytes in {sum(count.values())} allocations")
    for site, size in total.most_common(20):
        print(f"{size:9d} B {count[site]:5d}x  {site}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
