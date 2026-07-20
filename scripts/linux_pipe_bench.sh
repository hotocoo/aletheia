#!/usr/bin/env bash
# Real-Linux IPC baseline for the performance-validation comparison.
#
# SUBSTRATE LABEL: this runs REAL Linux syscalls inside a Linux container (near-native under
# the host VM), whereas the kernel's in-VM numbers are QEMU TCG emulation. The two are NOT
# directly comparable in wall-clock — see kernel/README.md "Performance honesty". What IS
# comparable is the STRUCTURE: this pipe round-trip crosses the user/kernel boundary twice
# and forces a context switch between two processes every round-trip; the Aletheia IPC
# fast-path does neither. This script provides the real Linux number for that discussion.
set -uo pipefail

if ! command -v docker >/dev/null 2>&1; then
  echo "[skip] docker not available; cannot run the real-Linux baseline here."
  exit 0
fi

IMG="${LINUX_BENCH_IMAGE:-gcc:13-slim}"
echo "==> real-Linux pipe round-trip baseline (image: $IMG)"

docker run --rm -i "$IMG" bash -s <<'EOF'
set -e
cat > /tmp/pb.c <<'CEOF'
#include <stdio.h>
#include <unistd.h>
#include <time.h>
int main(void) {
    int a2b[2], b2a[2];
    if (pipe(a2b) || pipe(b2a)) { perror("pipe"); return 1; }
    const long N = 200000;
    char c = 0;
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 1; }
    if (pid == 0) {                    /* child: echo server */
        for (long i = 0; i < N; i++) {
            if (read(a2b[0], &c, 1) != 1) _exit(1);
            if (write(b2a[1], &c, 1) != 1) _exit(1);
        }
        _exit(0);
    }
    struct timespec t0, t1;            /* parent: client, timed */
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (long i = 0; i < N; i++) {
        if (write(a2b[1], &c, 1) != 1) return 1;
        if (read(b2a[0], &c, 1) != 1) return 1;
    }
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double ns = (t1.tv_sec - t0.tv_sec) * 1e9 + (t1.tv_nsec - t0.tv_nsec);
    printf("[linux] pipe round-trip: %.1f ns/op over %ld iters (real Linux, 2 procs, 2 syscalls + ctx switch)\n", ns / N, N);
    return 0;
}
CEOF
gcc -O2 /tmp/pb.c -o /tmp/pb
/tmp/pb
EOF
