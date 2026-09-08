#!/usr/bin/env python3
"""Normalize qemu-img's generated VMDK content identifier for reproducible releases."""

from __future__ import annotations

import argparse
import hashlib
import re
from pathlib import Path


# qemu-img emits the CID as an unpadded hexadecimal integer, so leading zeroes may be omitted.
CID = re.compile(rb"(?m)^CID=[0-9a-fA-F]{7,8}$")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--vmdk", required=True, type=Path)
    parser.add_argument("--seed", required=True, type=Path)
    args = parser.parse_args()

    payload = args.seed.read_bytes()
    digest = hashlib.sha256(b"aletheia-vmdk-cid\0" + payload).hexdigest()[:8].encode("ascii")
    data = args.vmdk.read_bytes()
    matches = list(CID.finditer(data[:64 * 1024]))
    if len(matches) != 1:
        raise SystemExit(f"error: expected exactly one VMDK descriptor CID, found {len(matches)}")

    match = matches[0]
    args.vmdk.write_bytes(data[:match.start()] + b"CID=" + digest + data[match.end():])
    print(f"normalized VMDK CID: {digest.decode()} ({args.vmdk})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
