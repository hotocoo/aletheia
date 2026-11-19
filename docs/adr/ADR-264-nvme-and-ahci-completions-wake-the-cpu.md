# ADR-264: NVMe and AHCI completions wake the CPU

**Status:** Accepted (2026-10-23)
**Requirements:** REQ-DRV-019 (new)
**Builds on:** ADR-260 and ADR-261 (the `CompletionWait` seam; their "What this is not" named
NVMe and AHCI as still polling), ADR-223 (NVMe), ADR-225 (AHCI), ADR-226 (both on aarch64),
ADR-131 (x86-64 MSI-X), ADR-150 (completion waits bounded on the clock), ADR-180 (check-then-sleep).

## Context

The NVMe and AHCI drivers ran on two CPUs: x86-64 (ADR-223, ADR-225) and aarch64 (ADR-226). RISC-V
has no PCI host bridge in this kernel, so neither runs there and no RISC-V gate changes. AHCI also
runs on VirtualBox's own controller (ADR-225); NVMe does not (the Oracle Extension Pack).

Both drivers spun on their completion: `Nvme::submit` on the phase bit of the completion queue
slot, `AhciDisk::command` on `PxCI`. ADR-260 and ADR-261 made the virtio-blk completion sleep in
`hlt` / `wfi` until the device's interrupt, through one shared seam
(`virtioblk::CompletionWait`). This wave puts the two real-hardware storage classes on that seam.

## Decision

* **The seam.** `Nvme` and `AhciDisk` carry `set_completion_wait(Option<CompletionWait>)` and the
  same two counters virtio-blk has (`completion_counts`: commands completed through the wait, and by
  spinning). A command is credited to the wait only when it really CALLED the platform's wait (a
  local `entered` flag set at the call), never because a wait is configured: the wait is entered once
  before the first look, and its own check (interrupts off) finds a completion that is already there
  and returns without sleeping, so an early completion is still proved to have gone through the
  platform and a driver that has a wait set but spins anyway counts every command as polled and fails
  `judge_run` (`SilentlyPolled`). Both drivers still check the clock on every look, so a controller
  that never answers ends at the same 20 s / 10 s bounds as before. `set_completion_wait` REFUSES BY NAME a controller whose
  interrupt was never enabled: a wait on it could only ever end on the clock.
* **Message-signalled interrupts, shared.** `virtiopci` gains `enable_msi` and `enable_msix_entry`
  (one copy of the capability programming for both CPUs). The capability list is the device's own
  claim, so `find_cap` refuses by name a pointer below the 0x40 standard header or whose entry leaves
  the 256-byte configuration space, a capability whose body (MSI 20 bytes, MSI-X 12) does not fit
  below 0x100 (a pointer near 0xFC would wrap the byte offsets), and a list that does not end within
  48 hops (a loop): the message address and data, the vector
  or entry unmasked, MSI / MSI-X enabled, the legacy INTx pin silenced (command bit 10, status half
  untouched because it is write-1-to-clear). A function with no MSI / MSI-X capability, an entry
  past its table, or an unmappable table is refused by name. A 32-bit MSI capability refuses a
  doorbell above 4 GiB. The MSI-X table's BIR and offset are the function's own claim and the table
  entry writes land wherever they point, so `enable_msix_entry` also refuses a BIR that names the
  upper half of a 64-bit BAR and a table that does not lie inside the BAR it names (the BAR is sized
  with memory decode off, `offset + entries * 16` is checked arithmetic, and the base address add is
  `checked_add`), before it maps or writes anything.
* **NVMe.** `Nvme::init_with_io_vector(regs, Some(entry))` creates the I/O completion queue with
  `IEN = 1` and `IV = entry` (`init` keeps `IEN = 0`). The admin queue stays polled and its vector,
  MSI-X entry 0, stays masked at the table, so an interrupt is exactly one I/O completion and admin
  traffic cannot pollute the count. The acknowledgement is the CQ head doorbell (already written
  per completion) with the phase flip.
* **AHCI.** `enable_interrupts` clears whatever polling left (`PxIS`, then the HBA's `IS` bit,
  write-1-to-clear), sets `PxIE` to the completion bit (D2H register FIS) plus the four fatal
  conditions, then `GHC.IE`. In interrupt mode a command no longer wipes `PxIS` before it issues: a
  status already set is a missed acknowledgement and the command is refused, once: the refusal
  acknowledges what it saw (`PxIS`, then `IS`), so one late or extra D2H FIS fails one command and
  does not wedge the disk. After each completion
  the driver clears `PxIS`, then `IS`, in the order AHCI 1.3.1 gives. `Drop` writes `PxIE = 0`.
* **One judge.** `kernel_core::devirq` holds the per-source counters (`IrqStats`: interrupts, sleeps,
  bound wakes) and `judge_run`, the verdict both drivers' boot invariants use, with a named fault for
  each way to fail: silently polled, no interrupt, lost interrupt, extra interrupts, bound wake,
  never slept. Equality is exact: `counted interrupts == completed commands`.
* **Interrupt group** (`nvme::interrupt_suite` 7 invariants, `ahci::interrupt_suite` 7), reads only
  so it holds on any controller, run after the unchanged polled suite: the driver waits through the
  platform; a run of `3 * depth + 1` NVMe reads (every phase flip and wrap) or 8 sector reads per
  AHCI disk raises exactly one interrupt per command with none polled; the CPU slept and no sleep
  needed the bounding timer; AHCI's `PxIS` then `IS` read clear and the NEXT completion still
  interrupts (a stale `IS` would swallow it on a message-signalled HBA); and a completion that races
  the wait is not lost. NVMe's last invariant is that every completion of a run longer than the
  queue was consumed (`3 * depth + 1` reads all succeed): a controller never posts into a full
  completion queue (tail + 1 == head), and the CQ head doorbell is the only way it learns a slot is
  free, so a driver that skipped the doorbell stalls the ring at its depth. The simulated controller
  models that rule (a completion owed to a full queue is recorded as a violation) and
  `a_skipped_cq_head_doorbell_fails_the_group` withholds the doorbell from it. An earlier "a quiet
  window takes no interrupt" check was removed: messages are edges, one per completion entry, so a
  skipped doorbell never produces a later interrupt and the check could not fail. AHCI's quiet
  window stays: its status is level-held in `PxIS` / `IS`, which is what that window observes.
* **Fallback, named, for both.** A controller whose completions cannot wake the CPU is not a
  failed boot (ADR-228: missing environment means a shorter suite, never a failed boot). It runs
  polled and says why, ONLY with `[nvme|ahci] completions: POLLED, fallback 1 (<reason>)` on the boot
  log, and one invariant (`nvme::fallback_suite`, `ahci::fallback_suite`) that passes only if the
  reason is non-empty, the commands completed by polling and no wait is armed (NVMe also: the I/O
  queue was created without a vector). The reasons are the platform's: no MSI / MSI-X capability
  (VirtualBox's default AHCI chipset), a LAPIC left in x2APIC mode by firmware (x86-64), no GICv2m
  frame in the device tree (GICv3 and ITS boards, aarch64). NVMe then has 24 invariants and AHCI 25.
  Only a vector that was armed and never fires is refused (by the run verdict). The gates pin the
  `interrupt-driven` line, so a fallback is never silent on QEMU.
* **x86-64.** MSI-X entry 1 of the NVMe function to vector 0x54, the AHCI function's MSI to vector
  0x55, both to the BSP's xAPIC (`enable_msi_target`, `lapic_oneshot` and the `smp` constants lose
  their `input-msix` gate: the boot suites use them too). The handlers count and EOI. The bound wake is per source and judged by the waiter, as ADR-260's
  addendum of 2026-10-23 does for virtio-blk: after the `hlt` the wait disables interrupts, disarms
  the one-shot, and books a bound wake on THAT controller's own `IrqStats` only when the one-shot
  fired during the sleep (the shared vector's expiry count is read as an edge across this one sleep,
  never added to, and is not what the suites report) AND the controller's own interrupt count did not
  move. A one-shot that expires in the instant after the completion interrupt woke the CPU is thus
  not a dead interrupt, and the console's block wait never sees these sleeps. The wait is
  ADR-260's `cli; check; arm one-shot; sti; hlt`, with a bound of 1 << 24 ticks (about 270 ms under
  QEMU), 16 times the console's, because a boot suite's commands finish in microseconds and a sleep
  the bound had to end is a lost interrupt, not a slow device. The ring-3 suite leaves interrupts
  off for every later suite, so the interrupt group runs inside `with_interrupts` (IF set, restored
  after): with IF clear a message waits in the LAPIC's single pending bit per vector, where two
  completions merge into one and the count reads short. The first boot of this wave found exactly
  that (49 commands, 1 interrupt, 0 sleeps) before the window existed.
* **aarch64.** QEMU `virt` declares a GICv2m MSI frame; `dtb.rs` now reads its base from the device
  tree (`Discovery::v2m_base`) instead of a constant, and `MSI_TYPER` names the first SPI the frame
  owns (INTID 80). NVMe takes the first, AHCI the second; the message is the frame's `SETSPI_NS`
  doorbell with the INTID as data. A tree with no v2m frame refuses by name, and so does a frame whose page (4 KiB from the base) does
  not lie wholly inside the Device-mapped peripheral GiB (`vm::GIB`), checked before the frame is
  read, the way an assigned BAR must (`map_region`): the base is the device tree's claim. The boot suites run
  with IRQs masked and the distributor off (the user-mode suite tears the GIC down, the SMP suite
  expects it so), so `devirq::with_window` opens the GIC for the interrupt group only (priority,
  target CPU 0, edge trigger, enable, priority mask, CPU interface, unmask), then puts every
  register back as found and drains anything left active. `el1_irq` asks `devirq::on_irq` first.
  The wait is ADR-261's: mask, check, one-shot on the EL1 physical timer, `wfi`; afterwards the
  one-shot is dropped and a bound wake is counted whenever the timer condition is met after the
  `wfi` (`CNTP_CTL.ISTATUS`) on this source's own `IrqStats` (no counter is shared with the
  console's block wait; IRQs are masked across the sleep, so the device's own count cannot be read
  there and the timer condition alone is the rule, ADR-261's), whether or not the completion is
  visible by then: a wake lost by `wfi`
  (completion posted, SPI pending) ends on the 250 ms timer and must be counted, as the x86-64 and
  virtio-blk handlers count every fire.

## Evidence

* Boot logs, the same machine and image as the gates (x86-64 / aarch64):
  * `[nvme] completions: interrupt-driven (MSI-X vector 0x54, table entry 1), 52 interrupts, 51
    sleeps, 0 bound wakes` (the sleep count varies with host load: a completion already posted
    when the wait looks is not slept on; 49, 50 and 51 all measured) and `(GICv2m MSI-X, table entry 1), 51 interrupts, 50 sleeps, 0 bound
    wakes`. The verdict is on deltas over the run of 49 reads (`3 * 16 + 1`): 49 interrupts, 49
    completions through the wait, 0 polled (`judge_run` is exact equality). The cumulative 51 / 52
    also count the first read and the raced read (49 + 1 + 1 = 51). The extra x86-64 interrupt is
    measured: a counter read after the interrupt window opened and before the group started read 1
    interrupt, 0 sleeps (the window settles for a millisecond before it samples), and 52 after the
    group: 1 + 49 + 1 + 1. The aarch64 count has no such prefix (51 = 49 + 1 + 1). Which earlier
    command that first interrupt belongs to was not measured.
  * `[ahci] completions: interrupt-driven (MSI vector 0x55), 19 interrupts, 18 sleeps, 0 bound wakes`
    and `(GICv2m MSI), 19 interrupts, 18 sleeps, 0 bound wakes` on both CPUs: 16 run reads (two
    disks, 8 each), 2 for the acknowledgement check, 1 raced.
  * `ALL 30 NVME INVARIANTS HOLD` (23 polled + 7) and `ALL 31 AHCI INVARIANTS HOLD` (24 + 7) on both
    CPUs. Suite times on a host at load 11: nvme 32 / 37 ms, ahci 33 / 39 ms (aarch64 / x86-64).
* A real mutation, run on the booted x86-64 image: removing the `PxIE` write from
  `enable_interrupts` leaves every command completing (the status bit still sets and the wait
  returns) and the boot fails at `[ahci] FAILED at ahci invariant 26: completions arrived but no
  interrupt did (source not armed)`. The shared judge names each other fault in
  `kernel-core/src/devirq.rs`; the simulated controllers in `kernel-core/tests/nvme.rs` and
  `ahci.rs` produce each one (no interrupt, a storm, an acknowledgement that does not take, a
  driver that polls, a silent device that still times out through the wait, a completion that races
  the wait: the controller answers between the driver's first look and the wait's check, the check
  is made to report the stale "not ready", and the command still completes through that one wait).
* Review wave: `kernel-core/tests/nvme.rs` `the_fallback_group_passes_only_for_a_named_polled_reason`,
  `ahci.rs` `a_status_left_set_before_a_command_is_refused_not_wiped` (the next command runs), and
  `msi.rs` `a_table_that_leaves_its_bar_or_names_an_upper_half_is_refused_by_name` and
  `a_malformed_capability_list_is_refused_by_name_and_nothing_is_written`; second review wave:
  `a_command_is_credited_to_the_wait_only_when_the_wait_was_called` (nvme.rs, ahci.rs: a controller
  that has already answered still sees the wait called once per command, and a wait that never
  slept fails the group as `NeverSlept`; the test fails if a command is credited without the call)
  and `a_skipped_cq_head_doorbell_fails_the_group` (nvme.rs).
* `kernel-core/tests/msi.rs`: the MSI and MSI-X programming against a simulated configuration
  space (64-bit and 32-bit layouts, entry 0 masked, pin silenced, status half untouched, every
  refusal by name).
* The polled invariants run first and unchanged; they are the baseline. Gates: `vm-e2e.sh`,
  `vm-e2e-x86.sh` (3 boots), `vm-e2e-riscv.sh` (unchanged), the quality gate, the kernel-core suite
  and every `check-*.sh` (STATUS, this date).

## What this is not

* Not proved on VirtualBox from here: `vm-e2e-vbox.sh` accepts `ALL 25 AHCI` (named polled fallback)
  or `ALL 31 AHCI` (MSI worked) and requires the `completions:` line, so CI says which; if
  VirtualBox's AHCI advertises MSI but never delivers it the family fails by name rather than
  falling back.
* AHCI's `IS` acknowledgement order is proved against the hosted simulated HBA, whose `IS` is sticky
  like silicon's; QEMU recomputes `IS` from `PxIS & PxIE`, so the boot gate cannot see a skipped
  `IS` write.
* Message interrupts only. Neither driver uses the legacy pin (x86-64 has no IOAPIC routing built;
  aarch64's INTx would need the `interrupt-map` swizzle and a level-triggered mask-in-handler). A
  board without MSI polls by name (both drivers).
* The interrupt group runs in the boot suites, not in the console: no command stream from the shell
  reaches these drivers, and NVMe and AHCI are still one command in flight, one queue pair, no NCQ.
* The vectors, INTIDs and the 270 ms / 250 ms bounds are this boot's choices, not read from the
  controllers; the GICv2m frame base and SPI range ARE read from the device tree and the frame.
* QEMU's models, not silicon: a real NVMe may coalesce interrupts (the driver never enables
  coalescing) and a real AHCI may raise more than one status per command, which the exact count
  would call extra interrupts.
