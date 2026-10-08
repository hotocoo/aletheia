# ADR-248 — A run of blocks in one request

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-STOR-005 (new)
**Builds on:** ADR-232/243 (block cache), ADR-043 (DMA gate), ADR-237 (open row: one request in flight, nothing for an I/O System 1 to order).

## Context

Every block the namespace read was one virtio-blk request: header, one 4 KiB data descriptor,
status, a notify, a poll. Reading an object of N blocks cost N round trips to the device even
though its extent is contiguous on disk. ADR-237 named "one request in flight" as the I/O gap that
every other I/O row waits on.

## Decision

* **`BlockDevice::read_run(start, out)`**, a provided method: read a run of whole blocks. The
  default reads them one at a time, so every one of the ten device types answers the same bytes
  without change; a device that can do better overrides it.
* **virtio-blk does better.** It registers five more data frames with its DMA gate at init (six
  in all) and serves a run in requests of up to six blocks: header, up to six device-writable
  data descriptors, status, within the existing 8-entry queue. Each request is validated as a
  single-block one is: every descriptor address passes the gate, status must be OK, and the
  device's byte count must be exactly the blocks asked for plus the status byte.
* **The layers above pass runs through.** `DeviceGuard`/`AuthorizedDevice` check read authority
  once per run; the block cache answers cached blocks from memory and sends each maximal stretch
  of misses to the device as one run, keeping them as it keeps single misses; the journal bounds
  the run to home blocks; `Filesystem::read` reads an object's whole blocks as one run (and its
  last partial block through a stack buffer, so the vector is still exactly the object's size;
  the console storm's "a command costs its data" invariant caught the first version, which
  allocated whole blocks).
* Still polled, still one request in flight: this changes how much each request carries, not how
  many are outstanding.

## Evidence

* Boot, every CPU: virtio-blk family 21 to 22. New invariant: a 13-block run read (three requests
  of 6, 6, 1) answers byte-for-byte what 13 block reads answer, and a run that leaves the device is
  refused `OutOfRange`. Timed and reported (not gated), 48 blocks one request per block vs in runs
  of 6, two boots each: aarch64 936/984 vs 219/175 us, riscv64 1784/887 vs 381/235 us, x86-64
  1162/910 vs 194/210 us: 3-5x, QEMU TCG.
* Host: `run_reads_answer_what_block_reads_answer` (default path, and the cache with hits and
  misses interleaved: exactly the misses reach the device); the host copy of the virtio-blk suite
  checks the new invariant's place; the console storm and the whole kernel-core suite pass.
* The DMA gate assertions on every CPU and in the VT-d and SMMU suites now expect the six data
  frames plus the ring (`virtioblk::DMA_REGIONS`).

## What this is not

Writes are still one request per block (the journal writes single blocks by design). NVMe and
AHCI use the one-block default. There is still no interrupt-driven completion and no queue of
independent requests, so an I/O scheduler still has nothing to order.
