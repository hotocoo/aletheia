#!/usr/bin/env bash
# Cross-architecture conformance gate (GAPS2 Issue #2).
#
# The biggest systemic risk once a feature exists on all three CPU targets is *silent behavioral
# divergence* — the security boundary differing by architecture. This gate asserts that every target
# proves the SAME core SEMANTIC contract, booting each and checking that each named behavior appears
# in that target's live invariant log.
#
# It is spec'd on NAMED BEHAVIORS, not identical invariant COUNTS: architectures legitimately differ
# (e.g. x86-64 proves 13 virtual-memory invariants where aarch64/RISC-V prove 21, because long mode
# cannot do the MMU-off→on flip — an honest arch difference, not a regression). Such per-arch invariants
# are EXTENSIONS, reported informationally, never conformance failures. Only the core contract below is
# required of all three.
#
# Exit 0 iff every core behavior is proved by every target.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The CORE CONTRACT: arch-neutral substrings (no el0/u-mode/ring3 privilege term, no TTBR0/satp/PML4
# address-space term, no svc/ecall/int-0x80 trap term) that each target's boot log MUST contain. These
# are the capability-secure user-mode + IPC semantics every Aletheia backend must reproduce identically.
CONTRACT=(
  "capability-secure IPC — message delivered kernel-mediated across distinct address spaces"
  "shared-memory grant is capability-gated"
  "grant-table maps one frame into two distinct"
  "a successful grant revoke gates the unmap"
  "recv on an empty endpoint BLOCKS the receiver"
  "a send WAKES the blocked receiver"
  "the woken receiver RESUMES past its"
  "scheduler dispatches boosted LOW over Ready MEDIUM"
  # Mapping-API admission check (ALET-P1-001, REQ-MM-001). Worded identically on every target on
  # purpose: these are the refusals that must NOT differ by architecture, since a target that
  # accepts an address the others refuse is a security boundary that varies by CPU.
  "mapping an unaligned VA is refused"
  "mapping an unaligned PA is refused"
  "mapping a PA outside the frame-allocator window is refused"
  "mapping the null page is refused"
  # Frame-ownership model (ALET-P1-003, REQ-MM-002, ADR-030). The physical-memory twin of the
  # rules above: a double free hands ONE page to two owners, and freeing a frame you do not hold
  # takes a live page from whoever does. Every target must refuse both, in the same words — an
  # architecture where a double free is accepted is a different memory-safety boundary.
  "double free is refused (would hand one page to two owners)"
  "freeing another owner's frame is refused (fail-closed)"
  "freeing a never-allocated frame is refused"
  "a freed frame has no owner"
  "ownership table and free list agree on the free count"
  # Erase on free (ALET-P2-026, REQ-MM-005, ADR-033). Ownership stops two owners holding one frame
  # at the same TIME; this is what stops the next owner READING the last one's bytes. A target that
  # hands back a dirty frame is a cross-task information leak, whatever its CPU.
  "a reused frame carries NO bytes of its previous owner (erased on free)"
  "the next allocation reuses the just-freed frame (LIFO)"
  # Page-table reclamation (ALET-P1-002, REQ-MM-003, ADR-031). The level COUNT differs honestly by
  # architecture (3-level aarch64/Sv39 vs 4-level x86-64), so the contract names the behavior, not
  # the number: a sibling mapping must protect the tables, an emptied chain must come back to the
  # allocator, the root must survive, and the address space must still work afterwards.
  "unmapping one of two pages in a leaf table reclaims NO table (sibling still mapped)"
  "no table frame was returned while the leaf table was still in use"
  "the reclaimed table frames came back to the allocator"
  "neither VA resolves after reclamation"
  "the address space rebuilds the reclaimed chain (root intact, frames reusable)"
  # Address-space destruction (ALET-P1-004, REQ-MM-004, ADR-032). A dying space must give back
  # everything it owned and nothing else, and no target may allow the running kernel to destroy the
  # space it is executing in.
  "destroying the ACTIVE address space is refused (the kernel is running in it)"
  "teardown freed exactly the pages the space owned"
  "every table in the tree was one this space owned"
  "destroying the space returned every frame it held, including its root"
  "the surviving address space is intact after the teardown"
  # W^X and attribute validation (ALET-P1-007/008, REQ-MM-006, ADR-034). Every target must refuse a
  # writable+executable mapping and must be able to audit a live tree and find none among the pages
  # its own APIs created. The third rule (a user page that is kernel-executable) is expressible on
  # aarch64 (PXN) and x86-64 (SMEP) but NOT on RISC-V, whose single X bit is qualified by PTE_U — an
  # honest architectural difference, so it is not part of the shared contract.
  "wx: mapping a writable+executable page is refused (W^X)"
  "wx: a legal non-executable writable mapping still succeeds"
  "wx: the attribute audit actually walked the live address space"
  "wx: NO dynamically mapped page in the live tree is writable+executable"
  # The kernel image is not remappable through the mapping APIs (REQ-MM-006, ALET-P2-032). Every
  # target that splits its image into 4 KiB pages to make text read-only also removes the block/huge
  # descriptor that had made those addresses undescendable — so all three must refuse the span
  # explicitly, and since the refusal now lives once in `kernel_core::vmaddr` rather than per target,
  # this pair is what proves each one declared its span rather than merely owning the code.
  "wx: mapping over the split kernel image is refused (text still maps to itself)"
  "wx: unmapping the split kernel image is refused (text still read-only + executable)"
  # The filesystem namespace (REQ-FS-001, ADR-035). The namespace is arch-independent code, so a
  # divergence here would mean a target's heap/alloc or storage path behaves differently — and the
  # behaviors that matter are refusals and crash outcomes, which must not vary by CPU. On aarch64 the
  # same twelve are additionally proved over the real virtio-blk device (an arch extension).
  "fs: a formatted device mounts and is empty"
  "fs: a created object reads back byte for byte"
  "fs: creating a duplicate name is refused (names are unique)"
  "fs: a malformed name is refused (empty, over-long, or reserved byte)"
  "fs: reading an absent name is refused"
  "fs: two objects never share a data block"
  "fs: removing an object returns exactly its blocks to the free map"
  "fs: a deleted object's blocks carry no bytes of it (erased on delete)"
  "fs: an object too large for one transaction is refused"
  "fs: the namespace survives a remount (all durable state is on the device)"
  "fs: a create that dies before its commit record leaves the namespace unchanged"
  "fs: a create that dies after its commit record is completed by the next mount"
  # Atomic UPDATE (REQ-FS-001, ADR-035 + ADR-038's prerequisite). "remove then create" is two
  # transactions, so a crash between them loses the NAME — the one outcome an update must never have.
  "fs: replacing an object's contents updates it in one transaction"
  "fs: a replace that dies before its commit record keeps the OLD contents (never nothing)"
  "fs: a replace that dies after its commit record is completed by the next mount"
  # The OS remembers (REQ-STOR-003, ADR-038). A store that reloads is not enough: loading must
  # RE-VERIFY each entity's content address, so a damaged medium is a refusal rather than accepted
  # state. Every target must agree, because "your data is wrong" must not depend on the CPU.
  "persist: a blank medium yields an empty store, not a failure"
  "persist: saving the store writes one object atomically"
  "persist: a reloaded store holds the same entities, byte for byte"
  "persist: the id sequence continues across a reload (no id is ever reissued)"
  "persist: a single flipped content byte is REFUSED, not restored (content address re-verified)"
  "persist: a flipped metadata byte is REFUSED (the whole record is checksummed)"
  "persist: an unknown record format is refused"
  "persist: a truncated record is refused"
  "persist: the witness survives a remount and counts the boot"
  # The cross-reboot claim itself (REQ-STOR-003, ADR-038). Each target's gate boots the SAME image
  # TWICE against the SAME persistent disk; this line is boot 1 creating the store on real hardware,
  # and the gate additionally requires boot 2 to find and verify it. "The OS remembers" must not be a
  # property of one CPU.
  "PERSISTENT MEDIUM: boot #1, 0 entities verified"
  # Address-space layout (REQ-MM-007/008, ALET-P1-006/012, ADR-040). Two properties that must not vary
  # by CPU: a kernel stack overflow FAULTS instead of walking into .bss, and VA 0 has no translation at
  # all — the second was a real hole this wave found, since the boot identity maps covered page 0.
  "guard: the guard page has no leaf"
  "layout: VA 0 has NO translation in the live map (a kernel null dereference faults)"
  # Networking (REQ-NET-001/002, ADR-041). The whole point is that something ANSWERS: a transmit-only
  # driver is indistinguishable from a frame that vanished. Every target must resolve the gateway by ARP
  # and get a verified ICMP echo reply back — "the network works" must not vary by CPU or by bus (two of
  # the three run virtio-mmio, one runs virtio-pci).
  "net: the device reported a unicast MAC address from its config space"
  "net: the DMA gate denies an unregistered descriptor address (rings and buffers are registered)"
  "net: an ARP request for the gateway is answered with its hardware address"
  "net: an ICMP echo request is answered with a matching reply (both checksums verified)"
  "net: a second echo is matched on its own sequence (replies are read, not assumed)"
  # The task supervisor's POLICY (REQ-REL-002, ADR-042) — every target compiles it in and routes its
  # unexpected-user-fault path through it. The end-to-end kill-and-continue proof (take an undeclared fault,
  # then run another task) is an x86-64 arch extension for now; this is the part that must not vary.
  "supervisor: the policy is live in this kernel"
  # The DMA boundary (REQ-DRV-006, ADR-043). What the KERNEL may tell a device about is policy, not a
  # hardware property, so it must be identical everywhere — and "deny by default" is the rule that matters.
  "virtio-blk: the DMA gate denies an unregistered descriptor address (ring and data are registered)"
  "dma: an address nobody registered is never device-visible (deny by default)"
  "dma: a range overlapping the kernel image is refused"
  "dma: revoking ends visibility, and revoking twice is refused"
  "the boosted LOW runs and services the endpoint"
  "HIGH resumes as highest-priority and receives"
)

hr() { printf '========================================================================\n'; }

hr; echo "==> Booting all three targets to capture their live invariant logs"; hr
echo "--> aarch64 …";  AOUT="$(bash "$ROOT/scripts/vm-e2e.sh" 2>&1)"
echo "--> RISC-V …";   ROUT="$(bash "$ROOT/scripts/vm-e2e-riscv.sh" 2>&1)"
echo "--> x86-64 …"
# Drive the portable leg (scripts/vm-e2e-x86.sh: mtools ESP, no hdiutil/root) so the x86-64 column
# is really compared on Linux/CI too, not only on a macOS workstation. It builds the .efi from HEAD
# and boots under QEMU+OVMF. The leg SKIPs — never silently passes — when the host lacks the tools.
x86_ran=0
have_ovmf=0
for c in /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/OVMF/OVMF_CODE_4M.fd \
         /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -f "$c" ] && { have_ovmf=1; break; }
done
if command -v qemu-system-x86_64 >/dev/null 2>&1 && command -v mformat >/dev/null 2>&1 && [ "$have_ovmf" = "1" ]; then
  XOUT="$(bash "$ROOT/scripts/vm-e2e-x86.sh" 2>&1)"
  x86_ran=1
else
  XOUT=""
  echo "    x86-64 boot unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — its column is SKIPPED (never a silent pass)."
fi

fail=0
report_target() {
  # $1 = label, $2 = the captured log
  local label="$1" log="$2" b missing=0
  for b in "${CONTRACT[@]}"; do
    if ! grep -qF "$b" <<<"$log"; then
      echo "  FAIL [$label] missing core behavior: $b"
      missing=1; fail=1
    fi
  done
  if [ "$missing" -eq 0 ]; then
    # Count arch-specific invariants proved (informational — the per-arch EXTENSIONS).
    local n
    n="$(grep -cE '\[pass +[0-9]+\]' <<<"$log")"
    echo "  PASS [$label] proves all ${#CONTRACT[@]} core behaviors (${n} total invariants incl. arch extensions)"
  fi
}

hr; echo "CONFORMANCE — core contract (${#CONTRACT[@]} named behaviors) across targets"; hr
report_target "aarch64" "$AOUT"
report_target "riscv64" "$ROUT"
if [ "$x86_ran" -eq 1 ]; then
  report_target "x86-64 " "$XOUT"
else
  echo "  SKIP [x86-64 ] not booted on this host"
fi

hr
if [ "$fail" -eq 0 ]; then
  echo "CONFORMANCE: PASS — every booted target proves the same core semantic contract"
  exit 0
else
  echo "CONFORMANCE: FAIL — a target diverged (missing a core behavior another target proves)"
  exit 1
fi
