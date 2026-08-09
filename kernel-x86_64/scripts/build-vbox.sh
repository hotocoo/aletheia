#!/usr/bin/env bash
# SUPERSEDED by scripts/vm-e2e-vbox.sh (ADR-046). Kept as a thin delegator, not deleted, because it
# is referenced from STATUS.md and from external notes.
#
# This script used to provision its own VirtualBox VM, and that was the problem: two provisioners
# for the same hypervisor drift, and this one was written against the macOS-only image builder and
# the VirtualBox 6.x CLI (`--uartmode1`, `--audio-enabled`, both renamed in 7.x). The gate is now the
# single source of truth for how an Aletheia VirtualBox VM is configured — it builds the image with
# the dependency-free `mkesp.py`, provisions the VM, boots it, asserts marker parity with the QEMU
# gate, names what VirtualBox cannot cover, and tears the VM down.
#
# To keep a VM around for interactive use rather than have the gate delete it, run the gate once and
# read the provisioning block in scripts/vm-e2e-vbox.sh — every VBoxManage call is there verbatim.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
echo "note: build-vbox.sh is superseded by scripts/vm-e2e-vbox.sh (ADR-046) — delegating."
exec bash "$HERE/scripts/vm-e2e-vbox.sh" "$@"
