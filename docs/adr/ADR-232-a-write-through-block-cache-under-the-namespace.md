# ADR-232 — A write-through block cache under the namespace

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-STOR-004 (new)
**Builds on:** ADR-024 (journaled block store), ADR-088/089 (namespace without per-call allocation).

## Context

The namespace (`kernel-core/src/fs.rs`) keeps nothing in memory between calls: the journal's
durable state is on the device, and every operation reads the directory block, and every write
also reads the bitmap. A console session therefore moves the same two blocks across a polled
virtio-blk device on every command. The survey for this wave found no block cache, buffer cache
or readahead anywhere in the tree.

## Decision

* `kernel_core::bcache::BlockCache<D>` implements `BlockDevice` over any device. It holds at most
  N blocks; the console uses `CONSOLE_BLOCKS` = 16 (64 KiB of kernel heap, allocated on first use
  and kept).
* **Write-through:** a write goes to the device and returns the device's answer. The device's write
  sequence and content are exactly what they are without the cache, so the journal's crash proofs
  (`kernel-core/tests/storage.rs`, `scripts/crash-e2e.sh`) still describe the system.
* **Read-allocate, write-update:** only reads bring blocks in. A journal commit writes up to 64
  slots that only recovery reads again; admitting them would push out the directory and bitmap.
* **Errors keep nothing:** a failed read caches nothing; a failed write drops the cached copy,
  since the device's content is then unknown.
* **CLOCK eviction** (one reference bit per slot): O(1) per access; a run of one-time reads cannot
  push out a block that is still being read.
* **Placement:** in each CPU's `shellio::session_on`, under `DeviceGuard`. Every request is still
  authorized; only device traffic is saved. Programs' `SYS_FS_READ` goes through the same device.

## Evidence (2026-10-08)

`kernel-core/tests/bcache.rs`:

* 40 seeds × 3000 skewed operations over a device that fails one read in nine and tears and fails
  one write in five: every successful read equals the device's bytes. Removing the stale-copy
  drop makes it fail (checked by mutation).
* A namespace workload (12 files, 400 mixed reads, stats, listings and replacements): the same
  answers, the same device write sequence and the same final device bytes with and without the
  cache; device reads 778 to 49 (94 % saved; 729 hits, 33 evictions).
* Live: `scripts/console-e2e.sh` (write, read back, reboot, read again) and `scripts/crash-e2e.sh`
  (SIGKILL mid-write) pass on all three CPUs with the cache in place.

## Not done, and why

* **No learned policy.** A System-1 eviction or prefetch model is not justified: CLOCK already
  serves 94 % of this workload, and readahead saves nothing while every driver keeps one request
  in flight (virtio-blk, NVMe, AHCI). Batched multi-block requests come first; then measure.
* No write-back: it would trade the journal's durability ordering for latency.
* The cache is per console session; the boot suites use their own devices unchanged.
