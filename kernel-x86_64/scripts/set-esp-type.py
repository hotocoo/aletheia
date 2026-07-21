#!/usr/bin/env python3
"""Patch partition entry 0 of a GPT disk image to the EFI System Partition type GUID.

macOS `diskutil partitionDisk ... "MS-DOS FAT32"` creates the FAT partition with the *Microsoft
Basic Data* type GUID. OVMF boots the `\\EFI\\BOOT\\BOOTX64.EFI` fallback off any FAT partition, but
VMware expects a real ESP (type C12A7328-F81F-11D2-BA4B-00A0C93EC93B). This flips the type GUID in
BOTH the primary and backup GPT partition arrays and recomputes the entries-array CRC32 and header
CRC32 for each — a valid, portable ESP with no external tooling.
"""
import struct
import sys
import zlib

LBA = 512
# ESP type GUID on disk (mixed-endian: first three fields little-endian, last 8 bytes as-is).
ESP = bytes([0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
             0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B])


def patch_header(d: bytearray, hdr_off: int) -> None:
    assert d[hdr_off:hdr_off + 8] == b"EFI PART", f"no GPT header at offset {hdr_off}"
    hdr_size = struct.unpack_from("<I", d, hdr_off + 12)[0]
    part_lba = struct.unpack_from("<Q", d, hdr_off + 72)[0]
    num = struct.unpack_from("<I", d, hdr_off + 80)[0]
    esz = struct.unpack_from("<I", d, hdr_off + 84)[0]
    arr_off = part_lba * LBA
    d[arr_off:arr_off + 16] = ESP  # entry[0] type GUID -> ESP
    arr_crc = zlib.crc32(bytes(d[arr_off:arr_off + num * esz])) & 0xFFFFFFFF
    struct.pack_into("<I", d, hdr_off + 88, arr_crc)
    struct.pack_into("<I", d, hdr_off + 16, 0)  # zero header CRC before computing
    hdr_crc = zlib.crc32(bytes(d[hdr_off:hdr_off + hdr_size])) & 0xFFFFFFFF
    struct.pack_into("<I", d, hdr_off + 16, hdr_crc)


def main() -> None:
    path = sys.argv[1]
    with open(path, "rb") as f:
        d = bytearray(f.read())
    patch_header(d, 1 * LBA)  # primary GPT header @ LBA 1
    alt = struct.unpack_from("<Q", d, 1 * LBA + 32)[0]  # AlternateLBA -> backup header
    patch_header(d, alt * LBA)
    with open(path, "wb") as f:
        f.write(d)
    print("patched: entry0 type -> EFI System Partition; primary+backup CRCs recomputed")


if __name__ == "__main__":
    main()
