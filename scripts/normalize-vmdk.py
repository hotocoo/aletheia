#!/usr/bin/env python3
"""Normalize qemu-img's generated VMDK content identifier for reproducible releases.

qemu-img gives every VMDK a RANDOM 32-bit CID, printed as an UNPADDED hexadecimal integer in the
embedded text descriptor - so one time in sixteen the CID is seven digits long instead of eight.
The CID is replaced here with eight digits derived from the payload, so a rebuild of the same
kernel yields the same bytes.

The replacement is done INSIDE the descriptor's own bytes, and the file length never changes: the
descriptor is text followed by zero padding, so growing the text by one byte just consumes one byte
of padding. An earlier version of this script spliced the whole file instead, and when the CID
happened to be seven digits the file grew by one byte, every grain moved one byte past where the
header said it was, and the disk read as garbage - the packaged selftest disk then failed to boot
on the runner (OVMF fell through to PXE) and two builds of one commit disagreed about a VMDK's
digest (ADR-152).

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
# Any length 1..8: a random 32-bit CID printed unpadded is seven digits one time in sixteen, but
# SIX or fewer one time in 256 - which the CI runner drew (2026-09-25: "normalize selftest vmdk"
# failed once and passed on rerun, the flake memory had written off as the runner's).
CID = re.compile(rb"(?m)^CID=([0-9a-fA-F]{1,8})$")


def descriptor_region(data: bytes) -> tuple[int, int]:
    """The byte range holding the embedded descriptor TEXT and its zero padding.

    First by the VMDK4 sparse header (`descriptorOffset`, `descriptorSize`, in sectors). If the
    region the header names does not hold exactly one CID line - one qemu-img on one runner
    produced a file where it did not, and this script must not guess why - fall back to finding the
    CID in the first 64 KiB and taking the text around it: from the byte after the previous NUL (or
    the file start) to the first NUL after the match, plus the run of NULs that pads it. Either way
    the caller rewrites only inside that range and never changes the file's length.
    """
    if data[:4] != MAGIC:
        raise SystemExit("error: not a VMDK sparse extent (bad magic)")
    desc_off, desc_size = struct.unpack_from("<QQ", data, 28)
    start, end = desc_off * SECTOR, (desc_off + desc_size) * SECTOR
    if desc_size and end <= len(data):
        text = data[start:end].split(b"\x00", 1)[0]
        found = len(CID.findall(text))
        if found == 1:
            return start, end
        print(
            f"normalize-vmdk: the header names descriptor sectors {desc_off}+{desc_size} but that "
            f"region holds {found} CID lines (first bytes {data[start:start + 24]!r}); "
            "locating the descriptor by its CID line instead"
        )
    head = data[: 64 * 1024]
    matches = list(CID.finditer(head))
    if len(matches) != 1:
        raise SystemExit(f"error: expected exactly one VMDK descriptor CID, found {len(matches)}")
    m = matches[0]
    text_start = head.rfind(b"\x00", 0, m.start()) + 1
    text_end = head.find(b"\x00", m.end())
    if text_end < 0:
        raise SystemExit("error: the descriptor has no zero padding to absorb a longer CID")
    pad_end = text_end
    while pad_end < len(data) and data[pad_end] == 0:
        pad_end += 1
    return text_start, pad_end


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
    shapes normalize to identical bytes of identical length - by the header route and, with the
    header's descriptor fields deliberately broken, by the CID-locating route."""
    data = vmdk.read_bytes()
    digest = b"0123abcd"
    start, end = descriptor_region(data)
    region = data[start:end]
    text = region[: region.find(b"\x00")]
    m = CID.search(text)
    if m is None:
        raise SystemExit("error: self-test needs a descriptor with a CID")
    # Every shorter shape, down to one digit: the same descriptor N bytes shorter, its padding N
    # bytes longer, must normalize to the 8-digit result exactly.
    eight = normalized(data, digest)
    for short in (b"abcdef1", b"abcde1", b"ab1", b"1"):
        short_text = text[: m.start()] + b"CID=" + short + text[m.end():]
        short_region = short_text + b"\x00" * (len(region) - len(short_text))
        shorter = data[:start] + short_region + data[end:]
        assert len(shorter) == len(data)
        short_norm = normalized(shorter, digest)
        if short_norm != eight or len(short_norm) != len(data):
            raise SystemExit(f"error: a {len(short)}-digit and an 8-digit CID did not normalize to the same bytes")
    # The fallback route must agree byte for byte with the header route.
    broken = bytearray(data)
    struct.pack_into("<QQ", broken, 28, 0, 0)
    via_fallback = normalized(bytes(broken), digest)
    struct.pack_into("<QQ", broken, 28, *struct.unpack_from("<QQ", data, 28))
    if via_fallback[SECTOR:] != eight[SECTOR:]:
        raise SystemExit("error: the CID-locating route did not agree with the header route")
    print(
        f"normalize-vmdk self-test: PASS (1- to 8-digit CIDs normalize to identical "
        f"{len(data)}-byte files, by the header and by the CID line)"
    )
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
