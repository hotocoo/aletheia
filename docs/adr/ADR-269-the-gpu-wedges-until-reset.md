# ADR-269: The GPU wedges until reset

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-DRV-023 (new)
**Builds on:** ADR-267 (`Virtqueue::reset`, the same rule for virtio-net and virtio-rng), REQ-GFX-001
and REQ-GFX-002 (the GPU driver and its resource table), ADR-150's 2026-11-19 addendum (the control
wait is bounded in time).

## Context

virtio-gpu sends every control command as one two-descriptor chain (command buffer, response buffer)
at head 0 of its control queue. When the answer did not come inside the budget, the driver returned
`GpuError::Timeout` and the next command reused both buffers and head 0. The device's late used entry
for the old command would then be harvested as the new command's, with the response buffer holding
whatever the device last wrote: a command that failed could read as accepted.

## Decision

* **Wedged until reset.** `VirtioGpu::command` sets `wedged` on a timeout and refuses every later
  command by name before a descriptor is published (`cmds_sent` does not move).
* **`VirtioGpu::reset`**: status 0, the same negotiation `init` uses (extracted as `negotiate`; a
  device that changes its EDID feature across the reset is refused), the control queue re-published
  on its frame by `Virtqueue::reset`, DRIVER_OK. A device reset forgets every resource, binding and
  backing, so the driver's table follows it: every backing page's DMA registration is revoked and the
  table emptied, and nothing vouches for memory the device no longer knows. Whoever drew on the
  device creates its resources again. Nothing is allocated.
* **A boot invariant, deterministic** (`gpu=13 -> 14`, all three CPUs), at the end of the GPU suite
  where no resource is alive: the wedge is set directly, GET_DISPLAY_INFO is refused with nothing
  published, the device is reset, and GET_DISPLAY_INFO answers with the same scanout table as before.
  The framebuffer console, composition and the live desktop that follow all run on the reset device.

## Evidence

* `[gpu] ALL 14 VIRTIO-GPU INVARIANTS HOLD` on aarch64, riscv64 and x86-64, followed by the
  framebuffer-console and composition suites and, in `vinput-e2e.sh` and `desktop-fuzz-e2e.sh`, the
  live desktop under keyboard and pointer storms: all on the device the suite reset.
* `kernel-core/tests/virtiogpu.rs` pins the count (14) and the new name.
* The GPU's wedge shares its trigger and recovery with ADR-267's (`poll_used_within`,
  `Virtqueue::reset`), which `kernel-core/tests/virtio_wedge.rs` proves on a simulated device with
  mutants; the GPU-specific half (refusal before publishing, the resource table emptied) is proved
  only by the boot invariant, which compares against the refusal and an unchanged `cmds_sent`.
* Gates: `vm-e2e.sh`, `vm-e2e-riscv.sh`, `vm-e2e-x86.sh` (its second boot once stayed in OVMF on a
  loaded host and passed rerun alone; see ADR-200's addendum), `vinput-e2e.sh`,
  `desktop-fuzz-e2e.sh`, quality gate under Rust 1.100's clippy, every `check-*.sh`.

## What this is not

* **The live desktop does not recover a wedged GPU.** Nothing outside the suite calls `reset`; a
  desktop whose device wedges stops drawing and the console stays on serial. Rebuilding the desktop's
  resources after a reset is its own rung.
* **xHCI needs no wedge for correctness**: every completion names the TRB it completes, and the
  driver refuses one that is not its own (`XhciError::Unexpected`), so a late completion is refused,
  never taken. Recovering the ring after one (a controller reset and re-enumeration) is not done.
