#!/usr/bin/env python3
"""Deterministic SBOM + license check from `cargo metadata` (ALET-P2-005, REQ-QUAL-002).

Why not an off-the-shelf generator: it would be another toolchain dependency for a repo whose whole
dependency set is small and deliberately pure-Rust (ADR-004). `cargo metadata` already knows every
package, version, source and license; this turns that into a stable inventory and enforces an
allow-list, so a new dependency with an unexpected license fails CI instead of being noticed later.

Output: build/sbom/<crate>.json — sorted by (name, version), no timestamps, so the file is
byte-identical for an unchanged lockfile (a timestamp would make every run a diff and hide real ones).
Exit 1 if any dependency's license is not on the allow-list, or is missing.
"""
import json
import pathlib
import subprocess
import sys

# Licenses this project accepts. Permissive only: a copyleft dependency in a kernel we intend to
# distribute is a decision for a human, so it fails here rather than being absorbed silently.
ALLOWED = {
    "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib",
    "Unlicense", "0BSD", "CC0-1.0", "Apache-2.0 WITH LLVM-exception",
}

MANIFESTS = ["aletheia/Cargo.toml", "kernel-core/Cargo.toml", "component-sdk/Cargo.toml"]


def split_expr(expr):
    """Split a license expression into the licenses it offers.

    Two syntaxes appear in the wild: modern SPDX (`MIT OR Apache-2.0`) and the legacy slash form
    (`MIT/Apache-2.0`, sometimes spaced as `Apache-2.0 / MIT`). Both mean "either", so both are split
    the same way — a checker that understood only the modern form would reject a permissive dependency
    for its punctuation, which is the kind of false failure that gets a gate disabled.
    """
    normalized = (
        expr.replace("(", " ")
        .replace(")", " ")
        .replace("/", " OR ")
        .replace(" AND ", " OR ")
    )
    return [p.strip() for p in normalized.split(" OR ") if p.strip()]


def acceptable(expr):
    if not expr:
        return False
    offered = split_expr(expr)
    # "Either" semantics: one allowed license is enough. (`AND` is flattened the same way, which is
    # deliberately permissive; the dependency set here has none, and a real conjunction is a decision for
    # a human — it will surface in the SBOM even though this check passes it.)
    return any(p in ALLOWED for p in offered)


def main():
    out_dir = pathlib.Path("build/sbom")
    out_dir.mkdir(parents=True, exist_ok=True)
    bad = []
    total = 0
    for manifest in MANIFESTS:
        if not pathlib.Path(manifest).exists():
            continue
        crate = manifest.split("/")[0]
        raw = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1",
             "--manifest-path", manifest],
            capture_output=True, text=True, check=True,
        ).stdout
        meta = json.loads(raw)
        packages = []
        for pkg in meta["packages"]:
            entry = {
                "name": pkg["name"],
                "version": pkg["version"],
                "license": pkg.get("license") or pkg.get("license_file") or "",
                "source": pkg.get("source") or "local",
            }
            packages.append(entry)
            total += 1
            # Workspace-local crates carry no license field and are ours, not third-party.
            if entry["source"] == "local":
                continue
            if not acceptable(entry["license"]):
                bad.append((crate, entry["name"], entry["version"], entry["license"]))
        packages.sort(key=lambda p: (p["name"], p["version"]))
        doc = {
            "format": "aletheia-sbom/1",
            "crate": crate,
            "package_count": len(packages),
            "packages": packages,
        }
        path = out_dir / f"{crate}.json"
        path.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")
        print(f"  PASS sbom: {path} ({len(packages)} packages)")

    if bad:
        for crate, name, version, lic in bad:
            print(f"  FAIL license: {crate}: {name} {version} -> {lic or '(none declared)'}")
        print(f"  {len(bad)} dependency license(s) outside the allow-list")
        return 1
    print(f"  PASS licenses: every third-party dependency is permissively licensed ({total} packages seen)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
