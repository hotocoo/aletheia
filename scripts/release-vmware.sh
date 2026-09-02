#!/usr/bin/env bash
# The VMware release package (REQ-REL-001): every stable version ships a build a person can open
# in VMware Fusion/Workstation, and the package is VERIFIED by booting it before it is published.
#
#   .efi (release, two feature sets)  ->  GPT/ESP raw images (mkesp.py, deterministic)
#   ->  VMware VMDK (qemu-img)  ->  .vmx configs + README + SHA256SUMS  ->  one zip + its .sha256
#   ->  the packaged SELFTEST disk is BOOTED under QEMU+OVMF from the VMDK itself and must print
#       `[e2e] PASS` and exit 33; the packaged INTERACTIVE disk must reach its console prompt.
#
# Two disks, on purpose. `aletheia-x86_64-selftest.vmdk` boots, proves every invariant suite on
# the boot log (serial0 -> aletheia-serial.log) and halts: it is the machine's own proof that the
# artifact you downloaded is the artifact the gates passed. `aletheia-x86_64.vmdk` is the OS you
# sit in front of: the same kernel with the interactive console (and, with virtio-input devices,
# the live desktop) — it stays up.
#
# Usage: scripts/release-vmware.sh [--version vX.Y.Z|dev] [--out DIR] [--no-verify]
#   --version   defaults to the exact tag on HEAD, else `dev` (a package that is not a release).
#   --out       defaults to dist/
#   --no-verify skips the QEMU boot of the packaged disks (CI never passes this; a host without
#               QEMU/OVMF gets a loud SKIP of the verify step, never a silent one).
#
# Portable: python3 (mkesp.py is stdlib-only), qemu-img, zip, the Rust nightly via the rustup shim.
# No hdiutil, no mtools, no root. Exit 0 = the package exists AND (unless skipped) booted.
set -euo pipefail

# Honor the per-crate nightly toolchain via the rustup shim (a Homebrew/system cargo earlier in
# PATH ignores rust-toolchain.toml and fails cross-compilation with E0463).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
VERSION=""
OUT="$ROOT/dist"
VERIFY=1
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --no-verify) VERIFY=0; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1"; exit 2 ;;
  esac
done
if [ -z "$VERSION" ]; then
  VERSION="$(git -C "$ROOT" describe --tags --exact-match 2>/dev/null || echo dev)"
fi
case "$VERSION" in
  dev|v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "FAIL: --version must be dev or vX.Y.Z (got '$VERSION')"; echo "RELEASE-VMWARE: FAIL"; exit 2 ;;
esac

hr() { printf '========================================================================\n'; }
fail() { echo "FAIL: $*"; echo "RELEASE-VMWARE: FAIL"; exit 1; }

PY="$(command -v python3 || command -v python || true)"
[ -n "$PY" ] || fail "python3 is required (mkesp.py)"
command -v qemu-img >/dev/null 2>&1 || fail "qemu-img is required (raw -> VMDK)"
command -v zip >/dev/null 2>&1 || fail "zip is required"
SHA() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

NAME="aletheia-$VERSION-x86_64-vmware"
STAGE="$OUT/$NAME"
rm -rf "$STAGE" "$OUT/$NAME.zip" "$OUT/$NAME.zip.sha256"
mkdir -p "$STAGE" "$BUILD"

hr; echo "==> [1/6] build the release kernels (x86_64-unknown-uefi): selftest + interactive"; hr
EFI_OUT="$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"
( cd "$X86" && cargo build --release ) || fail "selftest kernel build"
cp "$EFI_OUT" "$BUILD/release-selftest.efi"
( cd "$X86" && cargo build --release --features interactive ) || fail "interactive kernel build"
cp "$EFI_OUT" "$BUILD/release-interactive.efi"

hr; echo "==> [2/6] write the GPT/ESP disk images (deterministic, mkesp.py)"; hr
"$PY" "$X86/scripts/mkesp.py" --efi "$BUILD/release-selftest.efi" --out "$BUILD/release-selftest.img" >/dev/null \
  || fail "selftest image"
"$PY" "$X86/scripts/mkesp.py" --efi "$BUILD/release-interactive.efi" --out "$BUILD/release-interactive.img" >/dev/null \
  || fail "interactive image"

hr; echo "==> [3/6] convert raw -> VMware VMDK"; hr
qemu-img convert -f raw -O vmdk "$BUILD/release-selftest.img" "$STAGE/aletheia-x86_64-selftest.vmdk" \
  || fail "selftest vmdk"
qemu-img convert -f raw -O vmdk "$BUILD/release-interactive.img" "$STAGE/aletheia-x86_64.vmdk" \
  || fail "interactive vmdk"

hr; echo "==> [4/6] VMware configs, README, checksums"; hr
vmx() { # $1 = vmdk file name, $2 = display name, $3 = serial log name, $4 = out file
  sed -e "s#^sata0:0.fileName = .*#sata0:0.fileName = \"$1\"#" \
      -e "s#^displayName = .*#displayName = \"$2\"#" \
      -e "s#^serial0.fileName = .*#serial0.fileName = \"$3\"#" \
      "$X86/aletheia-x86_64.vmx" > "$4"
}
vmx "aletheia-x86_64.vmdk" "Aletheia $VERSION x86-64 (interactive console)" "aletheia-serial.log" \
    "$STAGE/aletheia-x86_64.vmx"
vmx "aletheia-x86_64-selftest.vmdk" "Aletheia $VERSION x86-64 (selftest: boots, proves, halts)" \
    "aletheia-selftest-serial.log" "$STAGE/aletheia-x86_64-selftest.vmx"
GIT_SHA="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
cat > "$STAGE/README.txt" <<EOF
Aletheia $VERSION — x86-64 VMware package (built from $GIT_SHA)

Two UEFI disks for x86-64 hosts, ready for VMware Workstation / Fusion (firmware = EFI, never
legacy BIOS). x86-64 ONLY: there is no arm64 VMware image — VMware Fusion on Apple Silicon runs
arm64 guests and cannot boot these disks; Aletheia's aarch64 kernel targets QEMU's virt machine
(docs/BOOT.md).

  aletheia-x86_64.vmx / aletheia-x86_64.vmdk
      The OS you sit in front of: Aletheia boots as its own kernel, runs its boot invariants,
      then opens the interactive console (type \`help\`). Serial port 1 is written to
      aletheia-serial.log next to the .vmx — that file is the machine-checkable boot log.

  aletheia-x86_64-selftest.vmx / aletheia-x86_64-selftest.vmdk
      The proof disk: boots, proves every invariant suite on the boot log, prints
      \`[e2e] PASS\`, halts. Open it once to see the artifact prove itself on YOUR machine.

Open the .vmx in VMware (File > Open). Both VMs are configured with 256 MiB, 1 vCPU, UEFI
firmware, a SATA disk and a serial port to a file; no network. To capture more, add devices
in VMware — a virtio input keyboard/tablet is what the live desktop rung (ADR-080) drives
under QEMU; VMware exposes PS/2, which reaches the console.

Integrity: SHA256SUMS lists every file here; the zip's own digest is beside it on the
release page. The images are DETERMINISTIC for a given kernel (mkesp.py derives every GUID
and timestamp from the payload), so a rebuild of the same commit yields the same bytes.

What this is and is not: see docs/MATURITY.md in the repository — it grades every subsystem
and states plainly what is proved, implemented, or only architected. A stable version tag
means the gates in .github/workflows/ci.yml passed on this commit, and that this package
booted under QEMU+OVMF from these very VMDKs before it was published.

https://github.com/hotocoo/aletheia
EOF

hr; echo "==> [5/6] verify the PACKAGED disks boot (QEMU + OVMF, from the VMDKs themselves)"; hr
verify_skip=0
OVMF_CODE_PATH=""
for c in "${OVMF_CODE:-}" /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
         /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -n "$c" ] && [ -f "$c" ] && { OVMF_CODE_PATH="$c"; break; }
done
OVMF_VARS_PATH=""
for v in "${OVMF_VARS:-}" /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
         /opt/homebrew/share/qemu/edk2-i386-vars.fd /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
  [ -n "$v" ] && [ -f "$v" ] && { OVMF_VARS_PATH="$v"; break; }
done
if [ "$VERIFY" -eq 0 ]; then
  echo "SKIP: verify step disabled by --no-verify (the package is UNVERIFIED)"; verify_skip=1
elif ! command -v qemu-system-x86_64 >/dev/null 2>&1 || [ -z "$OVMF_CODE_PATH" ] || [ -z "$OVMF_VARS_PATH" ]; then
  echo "SKIP: verify needs qemu-system-x86_64 + OVMF (the package is UNVERIFIED on this host)"; verify_skip=1
else
  VARS="$BUILD/release-verify-vars.fd"
  # Launch one packaged disk in the background; the caller decides how long to let it run. The
  # serial log is removed HERE, in the foreground, before QEMU starts: a stale log from an earlier
  # run once satisfied the prompt poll below before the fresh boot had written a byte, and the
  # poll then killed a machine that was three seconds from its prompt. QPID is the QEMU pid — the
  # only thing ever killed, by pid, never by pattern.
  launch_vmdk() { # $1 = vmdk, $2 = serial log
    cp "$OVMF_VARS_PATH" "$VARS"
    rm -f "$2"
    qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep \
      -drive if=pflash,format=raw,unit=0,file="$OVMF_CODE_PATH",readonly=on \
      -drive if=pflash,format=raw,unit=1,file="$VARS" \
      -drive format=vmdk,file="$1" \
      -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
      -serial file:"$2" -display none -no-reboot &
    QPID=$!
  }
  SLOG="$BUILD/release-verify-selftest.log"
  launch_vmdk "$STAGE/aletheia-x86_64-selftest.vmdk" "$SLOG"
  ( sleep 240; kill -9 "$QPID" 2>/dev/null ) & WPID=$!
  set +e; wait "$QPID" 2>/dev/null; RC=$?; set -e
  kill "$WPID" 2>/dev/null; wait "$WPID" 2>/dev/null || true
  echo "selftest disk: QEMU exit code $RC (expect 33)"
  [ "$RC" -eq 33 ] || { tail -20 "$SLOG" 2>/dev/null; fail "the packaged selftest disk did not exit 33"; }
  grep -q '\[e2e\] PASS' "$SLOG" || fail "the packaged selftest disk did not print [e2e] PASS"
  echo "  PASS: the packaged selftest VMDK boots and proves its suites ([e2e] PASS, exit 33)"
  ILOG="$BUILD/release-verify-interactive.log"
  # The interactive disk stays up: poll its serial log for the prompt, then stop the machine.
  launch_vmdk "$STAGE/aletheia-x86_64.vmdk" "$ILOG"
  waited=0
  while ! grep -q 'aletheia> ' "$ILOG" 2>/dev/null; do
    sleep 1; waited=$((waited + 1))
    [ "$waited" -ge 240 ] && break
    kill -0 "$QPID" 2>/dev/null || break
  done
  kill -9 "$QPID" 2>/dev/null; wait "$QPID" 2>/dev/null || true
  grep -q 'Aletheia interactive console' "$ILOG" || { tail -20 "$ILOG" 2>/dev/null; fail "the packaged interactive disk never opened its console"; }
  grep -q 'aletheia> ' "$ILOG" || fail "the packaged interactive disk never printed a prompt"
  echo "  PASS: the packaged interactive VMDK boots to its console prompt (${waited}s)"
  cp "$SLOG" "$STAGE/verify-selftest-boot.log"
fi

hr; echo "==> [6/6] checksums (over EVERY shipped file, the boot log included), zip, digest, release notes"; hr
( cd "$STAGE" && rm -f SHA256SUMS && SHA $(ls | grep -v '^SHA256SUMS$' | sort) > SHA256SUMS )
( cd "$OUT" && zip -q -r -X "$NAME.zip" "$NAME" )
( cd "$OUT" && SHA "$NAME.zip" > "$NAME.zip.sha256" )
{
  echo "# Aletheia $VERSION — x86-64 VMware package"
  echo
  echo "Built from \`$GIT_SHA\`. Open \`aletheia-x86_64.vmx\` (interactive console) or"
  echo "\`aletheia-x86_64-selftest.vmx\` (boots, proves every suite, halts) in VMware Fusion/Workstation;"
  echo "firmware is UEFI. See README.txt inside the zip."
  echo
  if [ "$verify_skip" -eq 0 ]; then
    echo "Verified before publishing: the packaged selftest VMDK booted under QEMU+OVMF, printed"
    echo "\`[e2e] PASS\` and exited 33; the packaged interactive VMDK reached its console prompt."
  else
    echo "**UNVERIFIED on the build host** (no QEMU/OVMF): the package was not booted before publishing."
  fi
  echo
  echo '```'
  cat "$OUT/$NAME.zip.sha256"
  echo '```'
  echo
  echo "Maturity: nothing here is production-ready — read \`docs/MATURITY.md\` before quoting a claim."
} > "$OUT/RELEASE-NOTES.md"
cp "$STAGE/SHA256SUMS" "$OUT/SHA256SUMS"
ls -la "$OUT/$NAME.zip" "$OUT/$NAME.zip.sha256"
hr
if [ "$verify_skip" -eq 1 ]; then echo "RELEASE-VMWARE: PASS (package built; verify SKIPPED)"; else echo "RELEASE-VMWARE: PASS (package built AND booted)"; fi
