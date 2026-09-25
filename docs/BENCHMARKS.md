# Aletheia benchmarks — 2026-09-25

Every number here was measured on one workstation (Apple silicon, macOS, 16 cores), with QEMU
TCG emulating the guest CPU. TCG slows compute-bound code by about 50x (ADR-173), so absolute
times say as much about the emulator as about any kernel. The comparisons are fair only where
the table says the conditions are the same. The machine was also running a llama.cpp server and
other agent jobs (load average about 3 of 16 cores) during the runs.

Reproduce:

```bash
WITH_REDOX=1 WITH_FREEBSD=1 BOOT_SAMPLES=5 WORKLOAD_OPS=50 BOOT_TIMEOUT=400 ./scripts/comparative-bench.sh
./scripts/boot-profile.sh
./scripts/security-surface.sh
./scripts/linux_pipe_bench.sh
ALETHEIA_PROPERTY_SEED=<hex> ALETHEIA_PROPERTY_CASES=1024 cargo test --release --test property_campaign   # in kernel-core
```

## 1. Four operating systems, same emulator (x86-64, `qemu-system-x86_64`, TCG, q35, 4 vCPU)

| | Aletheia | Linux 6.12-lts | Redox OS | FreeBSD 15.1 |
|---|---|---|---|---|
| boot to a prompt, no NIC (median of 3) | 2132-2310 ms | 1834 ms | 4402 ms | 13488 ms |
| boot to a prompt, q35 default NIC (median of 5) | 2306 ms | 1780 ms | 5946 ms | 18254 ms |
| of which firmware | 1452 ms (OVMF) | none (`-kernel`) | OVMF, included | SeaBIOS, included |
| of which the kernel | 854 ms | 1780 ms | - | - |
| idle host CPU at the prompt | 0.0 % | 0.5-0.7 % | 3.4 % | 0.3-0.4 % |
| bootable payload | 2.6 MB | 13.9 MB | 512 MB disk | 6.5 GB disk |
| 50 typed `echo` round-trips | 112 ms (2 ms/op) | 1486 ms (29 ms/op) | login prompt | login prompt |
| syscalls exposed to user space | 11 (8 capability-gated) | 375 | - | - |
| privileged lines of code | ~65-70k Rust (967 `unsafe`) | ~40M C (cited) | - | - |

Samples with the default NIC: Aletheia 2337/2317/2306/2291/2288; Linux 1802/1796/1778/1775/1780;
Redox 6233/6100/5946/5919/4412; FreeBSD 18254/18777/18001/21329/18039. Without a NIC
(`-nic none`, now the bench default on every leg): Aletheia 2168/2132/2111 and, in a second run,
2310 median; Linux 1834/1882/1824; Redox 4402 median; FreeBSD 13488/11999/13875. q35's default
e1000e costs FreeBSD about 4.8 s (it waits for DHCP), Redox about 1.5 s and Aletheia about 0.1 s
(OVMF initializes the NIC's option ROM). Aletheia's two no-NIC runs differ by 180 ms: that is the
host-load noise floor for this machine.

Read with care:

* **Boot.** Linux wins the total. Aletheia's kernel share (854 ms) is half of Linux's, but
  Aletheia boots through OVMF (1452 ms, 63 % of its total) and the Linux leg skips firmware.
  That is a boot-path difference, not a kernel speed result.
* **Typed echo.** Aletheia is about 13x faster, but its line dispatcher runs in kernel space,
  while busybox `sh` runs in user space over syscalls. The column prices a design difference.
* **Idle.** Aletheia waits at 0.0 %. Redox spends 3.4 % of a host core doing nothing.
* **Payload.** The size win is mostly "Aletheia has fewer drivers", not a design victory.
* Redox and FreeBSD boot to a login prompt. The bench guesses no credentials, so they have no
  typed-workload number.

## 2. Where Aletheia's own boot time goes (x86-64, `boot-profile.sh`)

Total 2213 ms = firmware 1443 ms + kernel 770 ms. The largest gaps after ExitBootServices:

| gap | after |
|---|---|
| 174 ms | `mlsched` suite (the 4,096-task commissioning run, ADR-173) |
| 60 ms | ring-3 advisor consultation per user task |
| 40 ms | `reclaim` suite |
| 15 ms | SMP bring-up (INIT-SIPI-SIPI) |
| 11 ms | paging hardening (EFER.NXE, SMEP) |

## 3. In-kernel operation costs (the boot `bench` suite, TCG)

| operation (100,000 each; storage 256) | aarch64 | riscv64 | x86-64 |
|---|---|---|---|
| authority (capability) check | 144 ns | 81 ns | 200 ns |
| in-kernel message delivery round-trip | 336 ns | 174 ns | 400 ns |
| scheduler dispatch | 432 ns | 124 ns | 500 ns |
| console line | 48 ns | <1 ns | 100 ns |
| storage transaction | 83.0 µs | 37.7 µs | 57.3 µs |

aarch64: one `svc` trap and `eret` costs 386 ns, so the capability check Aletheia adds costs
0.87x one syscall trap.

The Linux pipe round-trip is 22.2 µs (`linux_pipe_bench.sh`, 200,000 iterations, 2 processes,
Docker's hardware-virtualized Linux VM). **This is not comparable** with the delivery row above:
Aletheia's loop crosses no address space, the Linux pipe crosses two. No cross-address-space IPC
benchmark exists yet.

## 4. Stress and load

* **Typed load:** 50 back-to-back round-trips per boot, 5 boots per OS. A dropped keystroke fails
  the leg. Aletheia and Linux: no drops.
* **Property campaigns** (`--release`): 8 seeds x 1,024 generated scheduler loads (12 properties
  each) and 8 seeds x 4,096 x 8 hostile documents through the renderer, HTTP reader, URL parser
  and navigator: all pass. The campaign scales linearly (128/256/1024 cases: 15.6/31.7/130.8 s).
* The nightly campaign found a real renderer bug on 2026-09-23 (a stray `<` swallowed a hidden
  element's opener). Fixed in `14cadb1`; nightly green again.

## 5. The resident model (ADR-174)

DavidAU LFM2.5 NEO-MAX Q8_0 on llama.cpp: 5/6 operations planned correctly, median plan latency
583 ms; 7/8 console commands. Stock LFM2.5 Q4_K_M recorded 6/6 and 8/8.

## 6. What to improve, ranked by what the numbers show

1. **Boot path: not a kernel problem.** OVMF is about 1.27 s of Aletheia's boot under TCG, but
   the boot-profile stamps show Aletheia's own code before ExitBootServices costs 16 ms (GOP
   lookup, image bounds). The rest is EDK2 itself. A firmware-free entry (PVH) would shrink only
   QEMU boots, while VMware, VirtualBox and hardware all boot through UEFI, so it would be
   benchmark tuning, not an improvement. The fair comparison is the "of which the kernel" row.
   Done: the bench gives no leg a NIC it does not need (saved 85 ms of firmware ROM init here).
2. **Boot-time commissioning.** The `mlsched` commissioning run is the largest kernel gap
   (174 ms under TCG), but ADR-173 timed it natively at 2.6 ms: a TCG artifact, not a target.
3. **Storage transactions: attributed.** Timed natively (`--release`), one journal commit of two
   blocks costs 11.1 us, nearly all of it the byte-serial FNV-1a checksum over the 8 KiB payload
   (five 4 KiB block copies cost about 1 us). The boot bench also paid for its own workload
   generator: a `% 251` per byte and two extra hashes per step cost more than the commit itself in
   a debug image. That harness cost is gone (2026-09-26): aarch64 83 -> 70 us, x86-64 57 -> 33 us
   per transaction under TCG. What remains is the checksum. A word-at-a-time hash would cut it
   several-fold, but it is the journal's ON-DISK format: changing it needs a versioned record so
   a device written by an older kernel still recovers. A decision for its own ADR, not a bench fix.
4. **A cross-address-space IPC benchmark**, so Aletheia's IPC can be compared with the Linux pipe
   baseline honestly. Without it, section 3's delivery row cannot be set against Linux.
5. **Model accuracy.** The temporary default misses one operation and one console line, and its
   multi-step agent loop stalls on aarch64 and riscv64. Aletheia-LM, or prompt work on the agent
   transcript, is the fix.
6. **Hardware numbers.** Nothing here ran on real silicon. Every absolute time above is a TCG
   time until a hardware run exists.
