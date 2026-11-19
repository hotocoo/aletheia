# ADR-266: A command left with the device wedges the driver

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-DRV-020 (new)
**Builds on:** ADR-223 (NVMe), ADR-225 (AHCI), ADR-264 (their interrupt-driven completions),
ADR-150 (completion waits bounded on the clock), and the virtio-blk wedge (`VirtioBlk::reset`,
ADR-263's audit). Closes the NVMe item of ADR-263's "What this is not".

## Context

Both drivers keep one command in flight in one fixed place: NVMe in the next submission slot and
its one data frame, AHCI in command slot 0, its one command table and its one data frame. When a
command's completion did not arrive inside its budget, the driver returned an error and went on. The
controller still owned the command. Its late completion then landed in the completion-queue slot (or
set PxIS) and its data in the frame that the NEXT command was about to use, so the next command
could take the old command's answer, or the old command's bytes, for its own. A completion naming
another command id, or another queue, means the same thing for NVMe: this command is still out.

virtio-blk already refuses every request after such a failure until `reset` (ADR-263's audit). The
same rule was missing here.

## Decision

* **Wedged until reset.** `Nvme::submit` sets `wedged` on a timeout and on `WrongCommandId` /
  `WrongQueue`; `AhciDisk::command` sets it when the budget ran out or PxCI still holds slot 0 after
  the wait. A failed command whose slot the device released (a task-file error, a short transfer, an
  NVMe status code) does not wedge: nothing is left with the device. A wedged driver refuses every
  command before writing anything the device reads and before ringing the doorbell or writing PxCI
  (`NvmeError::Wedged`; `StorageError::Device` through `BlockDevice`).
* **NVMe reset** (`Nvme::reset`): CC.EN = 0, which deletes every queue and ends what they held (NVMe
  §3.7.2), wait for CSTS.RDY = 0 within CAP.TO, zero both queue pairs' frames and restart our
  indices and phase, re-enable with the same admin queue, and recreate the I/O pair with the same
  MSI-X vector. Bring-up shares `enable_admin` and `create_io_queues` with `init`. Same five frames,
  nothing allocated.
* **AHCI reset** (`AhciDisk::reset`, AHCI 1.3.1 §10.4.2): PxCMD.ST = 0, which clears PxCI and ends
  the slot (§3.3.14), wait for PxCMD.CR = 0, clear PxSERR, PxIS and (when the port interrupts) the
  HBA's IS bit, then restart ST once the task file is idle. A disk still busy after its engine
  stopped needs a COMRESET, which this driver does not issue: that is refused by name and the port
  stays wedged.
* **A boot invariant each, in the read-only group** (so it holds on any controller, scratch or
  not, and on VirtualBox's AHCI): a zero budget leaves a read with the device (a controller answers
  after its doorbell, not during it; up to 64 tries bound the attempt), the next command must be
  refused with nothing issued, and `reset` must bring the driver back to a correct read on the same
  frames. The interrupt groups that follow then judge a clean run, which proves NVMe's I/O queue
  came back with its vector and AHCI's port with its interrupt.

## Evidence

* `[nvme] ALL 31 NVME INVARIANTS HOLD` (30 before) and `[ahci] ALL 32 AHCI INVARIANTS HOLD` (31) on
  x86-64 and aarch64 under QEMU, followed by `completions: interrupt-driven (...), 52 interrupts, 50
  sleeps, 0 bound wakes` (NVMe x86-64) and `19 interrupts, 18 sleeps, 0 bound wakes` (AHCI, both
  CPUs): the interrupt path survives the reset.
* The simulated controllers now behave as silicon does where it matters: NVMe CC.EN = 0 deletes the
  I/O queues and drops what was pending (it kept them before, so a reset test would have proved
  nothing); AHCI PxCMD.ST = 0 clears PxCI; a `late` controller answers on the CPU's second look at
  the clock after its doorbell, so a command the driver stops waiting for completes late.
* `kernel-core/tests/nvme.rs` `a_command_left_with_the_controller_is_never_answered_into_the_next`
  and `a_completion_for_another_command_wedges_the_driver`; `kernel-core/tests/ahci.rs`
  `a_command_left_with_the_hba_is_never_finished_into_the_next` and
  `a_port_whose_disk_stays_busy_is_refused_by_name_and_stays_wedged`. Each read after the reset
  returns its own block, and the NVMe controller records no protocol violation.
* Mutation: removing the wedge check from `submit` fails 5 NVMe tests; from `command`, 3 AHCI tests.
* Gates: `vm-e2e.sh`, `vm-e2e-x86.sh`, quality gate, kernel-core suite, every `check-*.sh`.

## What this is not

* **virtio-gpu, virtio-net, HD Audio and xHCI still return a timeout and go on.** virtio-gpu's
  control queue and virtio-net's transmit queue post at head 0 with one fixed response or transmit
  buffer through `virtq::poll_used_bounded`, so a late used entry can be read as the next command's:
  the same hazard, and their fix is a virtio device reset with renegotiation, as virtio-blk has.
  HD Audio's CORB/RIRB and xHCI's command and event rings carry their own indices and are the
  following rung. Deferred, named.
* **No COMRESET.** An AHCI disk that stays busy after its port stops is left wedged.
* **One command in flight.** A queue with several outstanding commands would have to abort them
  individually (NVMe Abort, AHCI per-slot) rather than reset the whole controller or port.
* **Proved on QEMU's models and the simulated controllers**, not on silicon.
