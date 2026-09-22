#!/usr/bin/env python3
"""Regenerate the TLS trust fixtures this tree embeds (ADR-147, ADR-149).

Everything here is produced by an INDEPENDENT implementation - Python's `cryptography`, over
OpenSSL - from deterministic keys, so the fixtures are reproducible from this file alone and a
kernel that agrees with them agrees with something it did not write.

  python3 scripts/tls-fixtures.py chain
      Print the root-issued leaf, the root's key, the leaf's key and the root certificate as Rust
      constants (the ones in kernel-core/src/trust.rs).

  python3 scripts/tls-fixtures.py pem <dir>
      Write the leaf certificate and its private key as PEM files into <dir> (for a test server on
      the host, e.g. scripts/tls-e2e.sh) and print the root's public key as 64 hex digits - the pin
      an operator types at the console.

  python3 scripts/tls-fixtures.py certificate-verify <transcript-hash-hex>
      Print the server CertificateVerify signature over that transcript hash, made with the leaf's
      private key (kernel-core/src/trust.rs::FIXTURE_CERTIFICATE_VERIFY). The hash is the one the
      kernel's own handshake computes for the suite's deterministic flight; the host test
      kernel-core/tests/tlshandshake.rs prints it when it changes.

Keys: root private = 0x11 * 32, leaf private = 0x22 * 32. Validity 2026-01-01 to 2036-01-01.
"""
import datetime as dt
import sys

from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.x509.oid import NameOID

ROOT_KEY = ed25519.Ed25519PrivateKey.from_private_bytes(bytes([0x11] * 32))
LEAF_KEY = ed25519.Ed25519PrivateKey.from_private_bytes(bytes([0x22] * 32))
NOT_BEFORE = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
NOT_AFTER = dt.datetime(2036, 1, 1, tzinfo=dt.timezone.utc)


def rust_array(name, data, doc=""):
    vals = [f"0x{b:02x}" for b in data]
    lines = [", ".join(vals[i:i + 12]) + "," for i in range(0, len(vals), 12)]
    body = "\n".join("    " + line for line in lines)
    return f"{doc}pub const {name}: [u8; {len(data)}] = [\n{body}\n];\n"


def build_chain():
    """The root and the leaf as certificate objects."""
    root_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Aletheia Test Root")])
    root = (
        x509.CertificateBuilder()
        .subject_name(root_name)
        .issuer_name(root_name)
        .public_key(ROOT_KEY.public_key())
        .serial_number(0x1001)
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(NOT_AFTER)
        .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
        .sign(ROOT_KEY, None)
    )
    leaf_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "aletheia.test")])
    leaf = (
        x509.CertificateBuilder()
        .subject_name(leaf_name)
        .issuer_name(root_name)
        .public_key(LEAF_KEY.public_key())
        .serial_number(0x2002)
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(NOT_AFTER)
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("aletheia.test")]), critical=False)
        .sign(ROOT_KEY, None)
    )
    ROOT_KEY.public_key().verify(leaf.signature, leaf.tbs_certificate_bytes)
    return root, leaf


def chain():
    root, leaf = build_chain()
    raw = serialization.Encoding.Raw, serialization.PublicFormat.Raw
    print(rust_array("LEAF_FIXTURE", leaf.public_bytes(serialization.Encoding.DER)))
    print(rust_array("ROOT_KEY_FIXTURE", ROOT_KEY.public_key().public_bytes(*raw)))
    print(rust_array("LEAF_KEY_FIXTURE", LEAF_KEY.public_key().public_bytes(*raw)))
    print(rust_array("ROOT_CERTIFICATE_FIXTURE", root.public_bytes(serialization.Encoding.DER)))


def pem(directory):
    import pathlib
    root, leaf = build_chain()
    out = pathlib.Path(directory)
    out.mkdir(parents=True, exist_ok=True)
    (out / "leaf.pem").write_bytes(leaf.public_bytes(serialization.Encoding.PEM))
    (out / "leaf-key.pem").write_bytes(
        LEAF_KEY.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    (out / "root.pem").write_bytes(root.public_bytes(serialization.Encoding.PEM))
    raw = serialization.Encoding.Raw, serialization.PublicFormat.Raw
    print(ROOT_KEY.public_key().public_bytes(*raw).hex())


def certificate_verify(transcript_hash_hex):
    transcript_hash = bytes.fromhex(transcript_hash_hex)
    if len(transcript_hash) != 32:
        sys.exit("the transcript hash is 32 bytes of SHA-256")
    # RFC 8446 section 4.4.3: 64 spaces, the context string, one zero byte, the transcript hash.
    content = b" " * 64 + b"TLS 1.3, server CertificateVerify" + b"\x00" + transcript_hash
    signature = LEAF_KEY.sign(content)
    LEAF_KEY.public_key().verify(signature, content)
    print(rust_array("FIXTURE_TRANSCRIPT_HASH", transcript_hash))
    print(rust_array("FIXTURE_CERTIFICATE_VERIFY", signature))


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "chain":
        chain()
    elif len(sys.argv) == 3 and sys.argv[1] == "pem":
        pem(sys.argv[2])
    elif len(sys.argv) == 3 and sys.argv[1] == "certificate-verify":
        certificate_verify(sys.argv[2])
    else:
        sys.exit(__doc__)
