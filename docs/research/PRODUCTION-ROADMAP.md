# From here to "production": what each remaining goal actually requires (2026-09-27)

The operator's stated goal for Aletheia: an AI-native OS with System 1 and System 2 built in, max
performance and security at every load, lighter than every other OS, production-ready, able to run
AAA games and workstation loads with native overclocking, fully customisable, with drivers and a
GUI. This page states, for each clause, what exists (with the ADR that proves it), what does not,
and what the missing part is made of. Nothing below is a promise of a date. Where a statement is
about the outside world (what games or drivers need) it is general engineering knowledge, not a
measurement made here.

## Where each clause stands

| Clause | Today | Evidence |
|---|---|---|
| System 2 built in | Hosted language model plans console commands; untrusted output validated and approved | ADR-053, ADR-174, ADR-186 |
| System 1 built in (console) | Laya-derived v2 checkpoint shipped as a release asset; v3/v3b tried and not shipped | ADR-187, ADR-193, ADR-209 |
| System 1 in the scheduler | Resident integer forest advises every admission, live via `tasks` and `run` | ADR-056, ADR-199, ADR-201 |
| Lethe (power governor) | Resident governor on real timer interrupts, overclock band behind a grant | ADR-165..173, ADR-184 |
| Security | Capability engine, W^X, guard pages, VT-d on x86, fault containment, fuzz and crash gates | MATURITY.md, ADR-202, ADR-180..183 |
| Lightweight | 0.0 % idle CPU vs Linux 2.0 %; 3 MB payload vs 14 MB; ~76k privileged lines | ADR-208 |
| Performance under load | Storms and bench on every boot; typed load 3 vs 29 ms/op against Linux | ADR-086..089, ADR-208 |
| User programs | ELF from the namespace, Rust userland, args, console output, file read, contained faults, preemption, multi-page, writable data, several at once | ADR-201..212 |
| GUI | Compositor, window manager, desktop, file panel, browser window, runtime resolution | ADR-078..085, ADR-194..197 |
| Drivers | virtio (blk, net, gpu 2D, input, rng), i8042, PL011/16550, VT-d, HPET | MATURITY.md |
| Real hardware | None. Every number is QEMU TCG (plus VMware/VirtualBox boot) | BENCHMARKS.md |
| AAA games | Not started | - |
| GPU 3D | Not started | - |
| Production-ready (MATURITY level X) | No subsystem is X | MATURITY.md |

## What "runs AAA games" is made of

A modern AAA title on a PC needs, at minimum: a GPU driver for the actual card (kernel-mode memory
management, command submission, display, power), a user-mode 3D API implementation (Vulkan, or
Direct3D translated onto Vulkan), a shader compiler, a windowing and input stack with low-latency
frame pacing, audio, a Windows-compatible user space for titles that ship only for Windows, and
often a kernel-level anti-cheat that only supports specific operating systems. On Linux this stack
is the product of many years of work by GPU vendors, Mesa, Valve and others.

For Aletheia each layer is a program of work:

1. **Userland that can host large programs** - many pages, writable data, heap, threads, dynamic
   loading. Today: up to 16 code pages, a stack page and a data page, up to four programs at once (ADR-201..212); no heap, threads or dynamic loading yet.
2. **A paravirtual 3D path first** - virtio-gpu with virgl or Venus is the one 3D device QEMU gives
   a guest; it is the honest first rung and still needs a Mesa-class user-mode driver.
3. **Real GPUs** - vendor hardware documentation or porting an existing open driver; each vendor
   family is its own multi-year effort.
4. **Compatibility layers** - running Windows games means a Win32/NT user space (the Wine/Proton
   approach) on top of all the above.

Until rung 1 exists, "AAA games" cannot be tested at all, so no claim about it can be true.

## What "production-ready" is made of

MATURITY.md's level X lists it: real hardware variety, interrupt-driven I/O everywhere, DMA
isolation on every target, update and rollback, key lifecycle, long soak at scale, a process
lifecycle with supervision. Each is a named open item there.

## "Lighter and faster than every other OS in every scenario"

This can only ever be claimed per measured scenario. ADR-208 measures four scenarios against
Linux under emulation, and Aletheia wins three and loses boot time (because of UEFI firmware).
"Every scenario" includes workloads Aletheia cannot run yet (anything needing the rungs above),
so the honest statement is the table in BENCHMARKS.md, extended one scenario at a time.

## The next rungs, in order

1. Multi-page programs with a writable data segment and a heap (widen the ELF judge and the window). Done: ADR-210, ADR-211 (heap still open).
2. More than one program at a time, preempted and scheduled together, with the advisor ranking them. Done: ADR-212; left running in the background: ADR-213.
3. Interrupt-driven virtio on every target (today most drivers poll).
4. virtio-gpu 3D (virgl/Venus) research spike and ADR.
5. First real-hardware boot (one x86-64 machine) with a measured, named driver list.
6. System 1 v4 with a better paraphrase source (ADR-209's measured bottleneck).
