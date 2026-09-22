#!/usr/bin/env python3
"""Normalize qemu-img's generated VMDK content identifier for reproducible releases.

qemu-img gives every VMDK a RANDOM 32-bit CID, printed as an UNPADDED hexadecimal integer in the
embedded text descriptor - so one time in sixteen the CID is seven digits long instead of eight.
The CID is replaced here with eight digits derived from the payload, so a rebuild of the same
kernel yields the same bytes.

The replacement is done INSIDE the descriptor region the sparse header declares, and the file
length never changes: the descriptor is text followed by zero padding, so growing the text by one
byte just consumes one byte of padding. An earlier version of this script spliced the whole file
instead, and when the CID happened to be seven digits the file grew by one byte, every grain moved
one byte past where the header said it was, and the disk read as garbage - the packaged selftest
disk then failed to boot on the runner (OVMF fell through to PXE) and two builds of one commit
disagreed about a VMDK's digest (ADR-152).

    normalize-vmdk.py --vmdk FILE --seed RAWIMAGE      rewrite FILE's CID in place
    normalize-vmdk.py --self-test FILE                 prove a 7-digit and an 8-digit CID normalize
                                                       to the same bytes, without touching FILE
"""

from __future__ import annotations

import argparse
import hashlib
import re
import struct
from pathlib import Path

SECTOR = 512
MAGIC = b"KDMV"
# qemu-img emits the CID as an unpadded hexadecimal integer, so leading zeroes may be omitted.
CID = re.compile(rb"(?m)^CID=([0-9a-fA-F]{7,8})$")


def descriptor_region(data: bytes) -> tuple[int, int]:
    """The byte range of the embedded descriptor, from the VMDK4 sparse header."""
    if data[:4] != MAGIC:
        raise SystemExit("error: not a VMDK sparse extent (bad magic)")
    # VMDK4 sparse header: magic(4) version(4) flags(4) capacity(8) grainSize(8)
    # descriptorOffset(8) descriptorSize(8), offsets and sizes in sectors.
    desc_off, desc_size = struct.unpack_from("<QQ", data, 28)
    start, end = desc_off * SECTOR, (desc_off + desc_size) * SECTOR
    if desc_size == 0 or end > len(data):
        raise SystemExit("error: the descriptor region lies outside the file")
    return start, end


def normalized(data: bytes, digest: bytes) -> bytes:
    """`data` with its CID replaced by `digest`, the same length as `data`."""
    start, end = descriptor_region(data)
    region = data[start:end]
    text_end = region.find(b"\x00")
    if text_end < 0:
        raise SystemExit("error: the descriptor has no zero padding to absorb a longer CID")
    text = region[:text_end]
    matches = list(CID.finditer(text))
    if len(matches) != 1:
        raise SystemExit(f"error: expected exactly one VMDK descriptor CID, found {len(matches)}")
    m = matches[0]
    new_text = text[: m.start()] + b"CID=" + digest + text[m.end():]
    if len(new_text) >= len(region):
        raise SystemExit("error: the normalized descriptor does not fit its region")
    new_region = new_text + b"\x00" * (len(region) - len(new_text))
    assert len(new_region) == len(region)
    return data[:start] + new_region + data[end:]


def payload_digest(seed: Path) -> bytes:
    return hashlib.sha256(b"aletheia-vmdk-cid\0" + seed.read_bytes()).hexdigest()[:8].encode("ascii")


def self_test(vmdk: Path) -> int:
    """Take a real VMDK, forge the 7-digit shape qemu-img emits one time in sixteen, and prove both
    shapes normalize to identical bytes of identical length."""
    data = vmdk.read_bytes()
    digest = b"0123abcd"
    eight = normalized(data, digest)
    start, end = descriptor_region(data)
    region = data[start:end]
    text_end = region.find(b"\x00")
    text = region[:text_end]
    m = CID.search(text)
    if m is None:
        raise SystemExit("error: self-test needs a descriptor with a CID")
    # Seven digits: the same descriptor one byte shorter, its padding one byte longer.
    seven_text = text[: m.start()] + b"CID=abcdef1" + text[m.end():]
    seven_region = seven_text + b"\x00" * (len(region) - len(seven_text))
    seven = data[:start] + seven_region + data[end:]
    assert len(seven) == len(data)
    seven_norm = normalized(seven, digest)
    if seven_norm != eight or len(seven_norm) != len(data):
        raise SystemExit("error: a 7-digit and an 8-digit CID did not normalize to the same bytes")
    print(f"normalize-vmdk self-test: PASS (7- and 8-digit CIDs normalize to identical {len(data)}-byte files)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--vmdk", type=Path)
    parser.add_argument("--seed", type=Path)
    parser.add_argument("--self-test", type=Path)
    args = parser.parse_args()
    if args.self_test is not None:
        return self_test(args.self_test)
    if args.vmdk is None or args.seed is None:
        parser.error("--vmdk and --seed are required (or --self-test FILE)")
    digest = payload_digest(args.seed)
    data = args.vmdk.read_bytes()
    out = normalized(data, digest)
    assert len(out) == len(data)
    args.vmdk.write_bytes(out)
    print(f"normalized VMDK CID: {digest.decode()} ({args.vmdk}, {len(out)} bytes, length unchanged)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
