# Aletheia qualification — 2026-09-09

This record captures a fresh end-to-end qualification run from the current `main` HEAD and a
same-host comparative benchmark. It is evidence, not a production-readiness claim.

## End-to-end result

Command:

```text
REQUIRE_X86=1 REQUIRE_DESKTOP=1 bash scripts/e2e-all.sh
```

Result: `E2E-ALL: PASS`.

The run proved:

- aarch64 full VM E2E: PASS
- RISC-V/RV64GC full VM E2E: PASS
- x86-64 UEFI VM E2E: PASS
- x86-64 persistent-disk reboot proof: PASS
- x86-64 no-firmware-root fail-closed boot: PASS
- live GUI desktop E2E on aarch64 and RISC-V: PASS
- VirtualBox x86-64 rung: SKIP because this qualification host is arm64 and cannot virtualize x86-64

The x86-64 run additionally proved the live kernel address map has zero W^X violations, real
virtio-input routing, real virtio-GPU scanout/composition, live VT-d enforcement, and the 23-item
SMP invariant set. The interactive input qualification separately proved MSI-X wake delivery for
both live input functions and a 100 Hz PIT watchdog fallback.

## Same-host comparative benchmark

Command:

```text
WORKLOAD_OPS=40 BOOT_SAMPLES=3 bash scripts/comparative-bench.sh
```

Both guests ran under the same `qemu-system-x86_64`, TCG mode, `-m`, `-smp`, and CPU model.

| Metric | Aletheia x86-64 | Linux 6.12-lts |
|---|---:|---:|
| Boot to prompt, median of 3 | 3044 ms | 2037 ms |
| Idle host CPU at prompt | 0.9% | 0.3% |
| Bootable payload | 1,822,208 B | 13,895,206 B |
| 40 typed echo round-trips | 88 ms | 1,200 ms |
| End-to-end typed echo | 2 ms/op | 29 ms/op |

The workload result is a measured result under this emulator and workload, not a claim that the
whole OS is faster than Linux. The boot comparison has a structural asymmetry: Aletheia boots
through OVMF while the Linux leg uses QEMU's direct kernel loading path. The payload comparison is
also not a kernel-size equivalence because Linux's image includes substantially broader hardware
support.

## Hardware-performance qualification

The x86-64 kernel now reaches its architectural performance-control probe during boot. In the
current QEMU qualification CPU, the probe correctly reported:

```text
[hwpm] no architectural HWP performance actuator; no unsafe MSR probing
```

This is intentionally a non-result rather than fabricated overclocking support. The existing
hardware-performance backend programs Intel HWP only when the CPU advertises HWP and keeps the
request inside `IA32_HWP_CAPABILITIES`; unlocked-ratio/voltage overclocking remains hardware- and
platform-specific and is not claimed by the QEMU run.

## Security / GUI qualification

The hosted GUI qualification remains green with per-response CSP nonces, no `unsafe-inline`,
same-origin browser enforcement, bounded request framing, capability-gated Core operations, and
no bearer-token rendering in the capability surface. The real desktop path separately proves
DMA-gated input/GPU access, compositor ownership, focus isolation, and zero-work repaint behavior.

## Next engineering target

The next high-value gap is hardware-qualified performance control and measurement on a real x86-64
platform: enumerate the CPU's actual DVFS/HWP/CPPC capabilities, bind them to the existing
capability/thermal policy, and compare latency/throughput before and after under repeatable load.
QEMU must remain a contract and safety qualification environment, not a substitute for that
hardware evidence.
