# ADR-252 — Several block requests in flight

**Status:** Accepted (2026-10-14)
**Requirements:** REQ-STOR-006 (new)
**Builds on:** ADR-248 (a run of blocks in one request), ADR-150 (completion wait bounded on the
clock), ADR-043 (DMA gate), ADR-237 (open row: drivers polled, one request in flight).

## Context

ADR-248 made one virtio-blk request carry up to six blocks, but the driver still posted one request,
rang the doorbell, and waited for it before posting the next. A 48-block read was eight round trips
to the device. ADR-237 names "one request in flight" as the I/O row every later I/O wave waits on:
with one request outstanding there is no queue for an I/O System 1 to order.

## Decision

* **The queue holds four requests.** `QSIZE_WANT` is `IN_FLIGHT * (RUN_BLOCKS + 2)` = 32
  descriptors (was 8). The ring frame is re-laid out so the descriptor table (512 B), avail ring,
  used ring, four 16-byte request headers and four status bytes all fit one 4 KiB frame, each
  region aligned and disjoint. The driver registers `IN_FLIGHT * RUN_BLOCKS` = 24 data frames with
  its DMA gate (was 6); `DMA_REGIONS` is 25 and every IOMMU suite already reads the constant.
* **Request slot `j` owns descriptors `j * 8 ..`, header `j`, status `j` and data frames
  `j * 6 ..`.** No two slots share anything the device writes. The single-block path keeps slot 0
  and descriptors 0..2, and runs only when no batch is in flight.
* **`submit_batch` replaces `submit`.** It posts every head of a batch, rings the doorbell once,
  and drains the used ring. Completions may arrive in any order; each is validated before it counts:
  its `id` must name a head of this batch not yet answered (another descriptor, or the same head
  twice, is refused), its byte count must be exactly that request's (`m` blocks plus the status
  byte, per request), and its status byte must be OK. The clock bound (ADR-150) covers the whole
  batch. One request is the single-request case, so read, write and flush go through the same
  validation as before.
* **`read_run` reads in batches of up to four runs of six.** A batch's bytes reach the caller only
  after every request in it validated, so a refused batch leaves its part of the buffer untouched
  (ADR-248 copied per run). Nothing above the driver changes: the cache, journal and filesystem
  already hand runs down (ADR-248).
* **A small queue degrades to the old behaviour.** `in_flight()` is how many whole requests the
  device's `QueueNumMax` holds, at least one; a device offering 8 entries gets one request in flight
  exactly as before.

## Evidence

* Boot, every CPU: virtio-blk family 22 to 23. New invariant: 31 blocks (one full batch of four
  runs of six, then a batch of a six and a one) read as runs in flight answer byte-for-byte what 31
  block reads answer. All three gates pass, twice each; the SMMU (aarch64) and VT-d (x86-64) suites
  pass with the 25 regions.
* Timed and reported (not gated), 48 blocks, one request per block / runs one at a time
  (ADR-248) / four runs in flight, two boots each, QEMU TCG:

  | CPU | per block | runs, 1 in flight | runs, 4 in flight |
  |---|---|---|---|
  | aarch64 | 984 / 950 us | 214 / 211 us | 127 / 130 us |
  | riscv64 | 1180 / 1072 us | 234 / 172 us | 107 / 108 us |
  | x86-64 | 983 / 961 us | 232 / 224 us | 143 / 138 us |

  Four in flight is 1.6-2.2x over ADR-248's runs and 7-11x over one request per block.
* Host (`kernel-core/tests/virtioblk.rs`): the mock device now serves every avail entry since the
  last doorbell and every data descriptor of a chain (it used to keep only the last one, so runs
  were not modelled on the host at all). New tests: a roomy queue reads 53 blocks with exactly one
  doorbell per batch (3); a device offering 8 entries still reads one request at a time; completions
  published in reverse order are matched to their own requests; and a duplicate completion, one
  short completion in a batch, a completion naming a data descriptor, a bad status, and a silent
  device are each refused with the caller's buffer untouched. The existing fuzz sweep (400 seeded
  malformations of single requests) passes unchanged.

## What this is not

* **Still polled.** The CPU spins on the used ring while the batch is out. Interrupt-driven
  completion is the next rung, and it has a constraint to design around: x86-64 console mode masks
  IRQ0 and quiesces the PIT (ADR-168), so a wait that halts for a block completion has no wake source
  if the device never answers, and the clock bound of ADR-150 could not fire. That wave needs a
  timer alive during the wait (the ADR-131 watchdog pattern, the LAPIC timer, or HPET).
* **Reads only.** Writes are still one request per block; the journal writes single blocks by
  design.
* **Not a scheduler.** Requests in a batch are consecutive runs of one caller's read. Nothing yet
  queues independent requests from different callers, so an I/O System 1 still has nothing to order.
  This ADR builds the queue that ordering will need.
* **After a refused batch** the remaining requests may still complete on the device. The next
  submission reads the used index fresh, so completed entries are skipped; a late completion naming
  a reused head is the same exposure the single-request path has had since ADR-150.
* NVMe and AHCI still use the one-block default.
