#!/usr/bin/env python3
"""Host-independent bootable ESP image writer for the Aletheia x86-64 kernel (ADR-046).

    mkesp.py --efi <BOOTX64.EFI> --out <disk.img> [--size-mb 64] [--label ALETHEIA] [--no-gpt]

Writes a GPT-partitioned disk whose single partition is an EFI System Partition holding a FAT32
filesystem containing ``\\EFI\\BOOT\\BOOTX64.EFI``. Pure Python 3, standard library only.

WHY THIS EXISTS. The repository had two image builders and neither is portable:
``build-image.sh`` uses macOS ``hdiutil``/``diskutil``, and ``build-image-linux.sh`` needs ``mtools``.
Neither runs on a Windows host, so the ADR-046 VirtualBox gate — whose whole point is that
qualification stops being host-shaped — could not have used either. This produces the same artifact
on macOS, Linux and Windows with no tooling beyond the Python that ``scripts/sbom.py`` already
requires.

WHY GPT, WHEN ``build-image-linux.sh`` GETS AWAY WITHOUT IT. UEFI firmware treats a partition-less
FAT volume as removable media and boots the fallback path, which is why the mtools image works under
OVMF. A VirtualBox SATA disk is *fixed* media, and its firmware is a different implementation of the
same spec; a real GPT with a real ESP type GUID is what both firmwares agree on. ``--no-gpt`` writes
the bare-FAT form for parity with the mtools builder.

DETERMINISM. Every field that would otherwise carry a timestamp or random GUID is derived from the
payload instead, so the same ``.efi`` yields a byte-identical image. This is the same reason
``scripts/sbom.py`` is timestamp-free: a file that changes on every run hides the runs where
something really changed.
"""

from __future__ import annotations

import argparse
import hashlib
import struct
import sys
import zlib
from pathlib import Path

SECTOR = 512
ESP_TYPE_GUID = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
GPT_ENTRIES = 128
GPT_ENTRY_SIZE = 128
# Where the ESP starts. 1 MiB is the universal alignment: it is what every partitioner uses, and it
# leaves room for the GPT header + the 128-entry array without special-casing.
PART_START_LBA = 2048
RESERVED_SECTORS = 32  # FAT32 minimum layout: boot(0), FSInfo(1), backup boot(6)
NUM_FATS = 2
DIR_ENTRY = 32


# --- GUIDs -------------------------------------------------------------------------------------


def guid_to_bytes(text: str) -> bytes:
    """GUID text -> the mixed-endian on-disk form (first three fields little-endian)."""
    a, b, c, d, e = text.split("-")
    return (
        struct.pack("<IHH", int(a, 16), int(b, 16), int(c, 16))
        + bytes.fromhex(d)
        + bytes.fromhex(e)
    )


def derived_guid(seed: bytes, purpose: bytes) -> bytes:
    """A stable RFC-4122 v4-shaped GUID derived from the payload — not random, so the image is
    reproducible, and not constant, so two different kernels do not claim the same disk identity."""
    digest = hashlib.sha256(purpose + seed).digest()[:16]
    raw = bytearray(digest)
    raw[7] = (raw[7] & 0x0F) | 0x40  # version 4
    raw[8] = (raw[8] & 0x3F) | 0x80  # RFC 4122 variant
    return bytes(raw)


# --- FAT32 -------------------------------------------------------------------------------------


def fat32_geometry(part_sectors: int) -> tuple[int, int]:
    """Return (sectors_per_cluster, fat_size_sectors) for a volume of `part_sectors`.

    FAT32 is only *valid* above 65 524 clusters — below that the driver is required to read the
    volume as FAT16 regardless of what the BPB claims, and firmware that does so will not find the
    ESP. So the cluster size is chosen as the largest that still clears the threshold, which for a
    64 MiB volume means 512-byte clusters.
    """
    for spc in (1, 2, 4, 8, 16, 32, 64):
        usable = part_sectors - RESERVED_SECTORS
        # clusters * spc + NUM_FATS * ceil((clusters + 2) * 4 / SECTOR) <= usable
        clusters = usable // spc
        while clusters > 0:
            fat_sz = ((clusters + 2) * 4 + SECTOR - 1) // SECTOR
            if clusters * spc + NUM_FATS * fat_sz <= usable:
                break
            clusters -= 1
        if clusters > 65524:
            return spc, ((clusters + 2) * 4 + SECTOR - 1) // SECTOR
    raise SystemExit(
        f"error: {part_sectors * SECTOR // (1024 * 1024)} MiB is too small for a valid FAT32 "
        f"volume (needs > 65 524 clusters); use --size-mb 64 or larger"
    )


def short_name(name: str, ext: str = "") -> bytes:
    return name.ljust(8)[:8].upper().encode("ascii") + ext.ljust(3)[:3].upper().encode("ascii")


def dir_entry(name: bytes, attr: int, cluster: int, size: int) -> bytes:
    """One 8.3 directory entry. Every timestamp is 0 — the FAT spec permits it, and a real one would
    make the image non-reproducible for no functional gain."""
    return (
        name
        + bytes([attr])
        + b"\x00"  # NT reserved
        + b"\x00"  # creation tenth-of-second
        + struct.pack("<HH", 0, 0)  # creation time, date
        + struct.pack("<H", 0)  # last access date
        + struct.pack("<H", (cluster >> 16) & 0xFFFF)
        + struct.pack("<HH", 0, 0)  # write time, date
        + struct.pack("<H", cluster & 0xFFFF)
        + struct.pack("<I", size)
    )


def build_fat32(payload: bytes, part_sectors: int, part_start_lba: int, label: str) -> bytearray:
    spc, fat_sz = fat32_geometry(part_sectors)
    cluster_bytes = spc * SECTOR
    vol = bytearray(part_sectors * SECTOR)

    data_start = RESERVED_SECTORS + NUM_FATS * fat_sz
    total_clusters = (part_sectors - data_start) // spc

    # Cluster 2 = root. 3 = \EFI. 4 = \EFI\BOOT. 5.. = the payload.
    root_cl, efi_cl, boot_cl, file_cl = 2, 3, 4, 5
    file_clusters = max(1, (len(payload) + cluster_bytes - 1) // cluster_bytes)
    if file_cl + file_clusters - 2 > total_clusters:
        raise SystemExit(
            f"error: {len(payload)} bytes of kernel do not fit in a "
            f"{part_sectors * SECTOR // (1024 * 1024)} MiB volume — raise --size-mb"
        )

    # --- FAT ---
    fat = bytearray(fat_sz * SECTOR)

    def set_fat(idx: int, value: int) -> None:
        struct.pack_into("<I", fat, idx * 4, value & 0x0FFFFFFF)

    set_fat(0, 0x0FFFFFF8)  # media descriptor copy
    set_fat(1, 0x0FFFFFFF)
    set_fat(root_cl, 0x0FFFFFFF)
    set_fat(efi_cl, 0x0FFFFFFF)
    set_fat(boot_cl, 0x0FFFFFFF)
    for i in range(file_clusters):
        cl = file_cl + i
        set_fat(cl, 0x0FFFFFFF if i == file_clusters - 1 else cl + 1)

    # --- directories ---
    root = bytearray()
    root += dir_entry(short_name(label[:8], label[8:11]), 0x08, 0, 0)  # volume label
    root += dir_entry(short_name("EFI"), 0x10, efi_cl, 0)

    efi = bytearray()
    efi += dir_entry(short_name("."), 0x10, efi_cl, 0)
    # ".." of a first-level directory names the ROOT as cluster 0, not as cluster 2. Writing 2 here
    # is the classic FAT bug: most drivers tolerate it, some firmware does not.
    efi += dir_entry(short_name(".."), 0x10, 0, 0)
    efi += dir_entry(short_name("BOOT"), 0x10, boot_cl, 0)

    boot = bytearray()
    boot += dir_entry(short_name("."), 0x10, boot_cl, 0)
    boot += dir_entry(short_name(".."), 0x10, efi_cl, 0)
    boot += dir_entry(short_name("BOOTX64", "EFI"), 0x20, file_cl, len(payload))

    def write_cluster(cl: int, blob: bytes) -> None:
        off = (data_start + (cl - 2) * spc) * SECTOR
        vol[off : off + len(blob)] = blob

    write_cluster(root_cl, bytes(root))
    write_cluster(efi_cl, bytes(efi))
    write_cluster(boot_cl, bytes(boot))
    write_cluster(file_cl, payload)

    # --- boot sector (BPB) ---
    bs = bytearray(SECTOR)
    bs[0:3] = b"\xeb\x58\x90"
    bs[3:11] = b"MSWIN4.1"  # the string firmware and chkdsk expect; not a Windows dependency
    struct.pack_into("<H", bs, 11, SECTOR)
    bs[13] = spc
    struct.pack_into("<H", bs, 14, RESERVED_SECTORS)
    bs[16] = NUM_FATS
    struct.pack_into("<H", bs, 17, 0)  # root entries: 0 on FAT32
    struct.pack_into("<H", bs, 19, 0)  # total sectors 16: 0 => use the 32-bit field
    bs[21] = 0xF8  # fixed media
    struct.pack_into("<H", bs, 22, 0)  # FAT size 16: 0 on FAT32
    struct.pack_into("<H", bs, 24, 63)
    struct.pack_into("<H", bs, 26, 255)
    struct.pack_into("<I", bs, 28, part_start_lba)
    struct.pack_into("<I", bs, 32, part_sectors)
    struct.pack_into("<I", bs, 36, fat_sz)
    struct.pack_into("<H", bs, 40, 0)  # ext flags: both FATs mirrored
    struct.pack_into("<H", bs, 42, 0)  # FS version
    struct.pack_into("<I", bs, 44, root_cl)
    struct.pack_into("<H", bs, 48, 1)  # FSInfo sector
    struct.pack_into("<H", bs, 50, 6)  # backup boot sector
    bs[64] = 0x80
    bs[66] = 0x29  # extended boot signature
    struct.pack_into("<I", bs, 67, struct.unpack("<I", hashlib.sha256(payload).digest()[:4])[0])
    bs[71:82] = label.ljust(11)[:11].upper().encode("ascii")
    bs[82:90] = b"FAT32   "
    bs[510:512] = b"\x55\xaa"

    # --- FSInfo ---
    fsi = bytearray(SECTOR)
    struct.pack_into("<I", fsi, 0, 0x41615252)
    struct.pack_into("<I", fsi, 484, 0x61417272)
    used = 3 + file_clusters
    struct.pack_into("<I", fsi, 488, total_clusters - used)
    struct.pack_into("<I", fsi, 492, file_cl + file_clusters)
    struct.pack_into("<I", fsi, 508, 0xAA550000)

    vol[0:SECTOR] = bs
    vol[SECTOR : 2 * SECTOR] = fsi
    vol[6 * SECTOR : 7 * SECTOR] = bs  # backup boot sector
    vol[7 * SECTOR : 8 * SECTOR] = fsi  # backup FSInfo
    for i in range(NUM_FATS):
        off = (RESERVED_SECTORS + i * fat_sz) * SECTOR
        vol[off : off + len(fat)] = fat

    return vol


# --- GPT ---------------------------------------------------------------------------------------


def write_gpt(disk: bytearray, part_start: int, part_end: int, seed: bytes, label: str) -> None:
    total = len(disk) // SECTOR
    last_lba = total - 1

    # Protective MBR: one 0xEE partition spanning the disk, so a legacy tool sees the disk as fully
    # allocated rather than as empty space it may safely repartition.
    mbr = bytearray(SECTOR)
    mbr[446] = 0x00
    mbr[447:450] = b"\x00\x02\x00"
    mbr[450] = 0xEE
    mbr[451:454] = b"\xff\xff\xff"
    struct.pack_into("<I", mbr, 454, 1)
    struct.pack_into("<I", mbr, 458, min(total - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xaa"
    disk[0:SECTOR] = mbr

    entries = bytearray(GPT_ENTRIES * GPT_ENTRY_SIZE)
    entry = (
        guid_to_bytes(ESP_TYPE_GUID)
        + derived_guid(seed, b"aletheia-partition")
        + struct.pack("<QQQ", part_start, part_end, 0)
        + label[:36].encode("utf-16-le").ljust(72, b"\x00")
    )
    entries[0:GPT_ENTRY_SIZE] = entry
    entries_crc = zlib.crc32(entries) & 0xFFFFFFFF
    disk_guid = derived_guid(seed, b"aletheia-disk")

    array_lbas = (GPT_ENTRIES * GPT_ENTRY_SIZE + SECTOR - 1) // SECTOR
    primary_array_lba = 2
    backup_array_lba = last_lba - array_lbas
    first_usable = primary_array_lba + array_lbas
    last_usable = backup_array_lba - 1

    def header(current: int, backup: int, array_lba: int) -> bytes:
        h = bytearray(92)
        h[0:8] = b"EFI PART"
        struct.pack_into("<I", h, 8, 0x00010000)
        struct.pack_into("<I", h, 12, 92)
        struct.pack_into("<I", h, 16, 0)  # CRC placeholder
        struct.pack_into("<I", h, 20, 0)
        struct.pack_into("<Q", h, 24, current)
        struct.pack_into("<Q", h, 32, backup)
        struct.pack_into("<Q", h, 40, first_usable)
        struct.pack_into("<Q", h, 48, last_usable)
        h[56:72] = disk_guid
        struct.pack_into("<Q", h, 72, array_lba)
        struct.pack_into("<I", h, 80, GPT_ENTRIES)
        struct.pack_into("<I", h, 84, GPT_ENTRY_SIZE)
        struct.pack_into("<I", h, 88, entries_crc)
        struct.pack_into("<I", h, 16, zlib.crc32(bytes(h)) & 0xFFFFFFFF)
        return bytes(h)

    disk[SECTOR : SECTOR + 92] = header(1, last_lba, primary_array_lba)
    off = primary_array_lba * SECTOR
    disk[off : off + len(entries)] = entries

    off = backup_array_lba * SECTOR
    disk[off : off + len(entries)] = entries
    disk[last_lba * SECTOR : last_lba * SECTOR + 92] = header(last_lba, 1, backup_array_lba)


# --- driver ------------------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description="Build a bootable ESP disk image (no external tools).")
    ap.add_argument("--efi", required=True, type=Path, help="the BOOTX64.EFI payload")
    ap.add_argument("--out", required=True, type=Path, help="output raw disk image")
    ap.add_argument("--size-mb", type=int, default=64, help="total disk size in MiB (default 64)")
    ap.add_argument("--label", default="ALETHEIA", help="FAT volume label (default ALETHEIA)")
    ap.add_argument(
        "--no-gpt",
        action="store_true",
        help="write a bare FAT volume with no partition table (removable-media form, matching "
        "build-image-linux.sh)",
    )
    args = ap.parse_args()

    if not args.efi.is_file():
        print(f"error: missing payload {args.efi}", file=sys.stderr)
        return 1
    payload = args.efi.read_bytes()
    if payload[:2] != b"MZ":
        print(
            f"error: {args.efi} is not a PE image (no MZ header) — the UEFI target produces a PE, "
            "and firmware will refuse anything else",
            file=sys.stderr,
        )
        return 1

    total_sectors = args.size_mb * 1024 * 1024 // SECTOR

    if args.no_gpt:
        vol = build_fat32(payload, total_sectors, 0, args.label)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_bytes(bytes(vol))
        print(f"built: {args.out} ({args.size_mb} MiB, bare FAT32, no partition table)")
        return 0

    array_lbas = (GPT_ENTRIES * GPT_ENTRY_SIZE + SECTOR - 1) // SECTOR
    part_end = total_sectors - 1 - array_lbas - 1
    part_sectors = part_end - PART_START_LBA + 1
    if part_sectors <= 0:
        print("error: --size-mb too small to hold a GPT", file=sys.stderr)
        return 1

    disk = bytearray(total_sectors * SECTOR)
    vol = build_fat32(payload, part_sectors, PART_START_LBA, args.label)
    disk[PART_START_LBA * SECTOR : PART_START_LBA * SECTOR + len(vol)] = vol
    write_gpt(disk, PART_START_LBA, part_end, hashlib.sha256(payload).digest(), args.label)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes(bytes(disk))
    spc, fat_sz = fat32_geometry(part_sectors)
    print(
        f"built: {args.out}\n"
        f"  disk       {args.size_mb} MiB, GPT, 1 partition (EFI System)\n"
        f"  ESP        LBA {PART_START_LBA}..{part_end} "
        f"({part_sectors * SECTOR // (1024 * 1024)} MiB FAT32, {spc * SECTOR} B clusters, "
        f"FAT {fat_sz} sectors)\n"
        f"  payload    \\EFI\\BOOT\\BOOTX64.EFI ({len(payload)} bytes)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
