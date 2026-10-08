# ADR-235 — The console speaks over the NIC the machine has

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-NET-003, REQ-NET-007, REQ-DRV-010 (tightened)
**Builds on:** ADR-140 (the console keeps the proved NIC), ADR-224/227/228 (e1000), ADR-234 (DHCP).

## Context

The console's network commands (`fetch`, `tls`, `go`, `resolve`, `net`) used only the virtio-net
device the boot kept. The e1000 suite proved its NIC and then dropped it. A VMware guest (the
released package) and most real machines have an Intel NIC and no virtio-net, so on the artifact
Aletheia ships the console had no network at all. The e1000 also still spoke from the QEMU
constant address, and the DNS client existed only as a virtio-net method.

## Decision

* **One DHCP exchange for every NIC.** `virtionet::take_lease(mac, xid, old, round)` runs
  DISCOVER, REQUEST and ACK over any device given a `round(frame, accept)` closure;
  `dhcp_frame` and `dhcp_payload` build and filter the frames. `VirtioNet::dhcp_lease` and the
  new `E1000::dhcp_lease` are both a few lines over it. The e1000 holds the same `Addressing`
  cell, speaks from it, and its suite takes the lease before ARPing the gateway the lease names;
  a network with no DHCP server keeps the defaults (the ADR-228 posture).
* **DNS over any link.** `tcpnet::udp_query` sends one UDP question over any `Ipv4Link` and takes
  only the answer from the server's address and port to ours (checksums verified, everything
  else skipped, a bounded number of waits); `dns::resolve_over` is the resolver on top of it.
* **The console keeps the e1000.** `pci::e1000_selftest` hands the device back when its suite
  holds, and the boot keeps it (`netstatic::keep_e1000`) on x86-64 and aarch64. `netstatic`
  picks the NIC with a two-arm enum (virtio-net first, else the e1000) and a delegating
  `Ipv4Link`, so `fetch`, `tls`, `resolve` and `net` run unchanged over either. RISC-V has no
  PCI host here and keeps its virtio-only path.
* **A kept NIC is quiet across the VT-d enable.** Keeping the e1000 alive means it goes on
  receiving after its suite. The x86-64 VT-d suite runs last precisely because QEMU mis-resolves
  DMA that is live across the moment translation turns on (ADR-073), and the first boot with the
  kept e1000 failed VT-d invariant 11: first a context fault (its function had no window), then,
  with its seven DMA frames granted, a write fault at a granted receive buffer. The boot now adds
  the e1000's grants to the window set and stops its receiver (`E1000::set_receiving(false)`)
  for the suite, restarting it after.
* `NetLink::resolve` and the e1000 link ARP for the peer's next hop (the peer on the leased
  subnet, else the gateway), so an off-subnet peer is reached through the router.
* `tcpnet::exchange` and `tlsclient::exchange` accept an unsized link (`?Sized`), so a `&dyn`
  link works where a concrete one did.

## Evidence (2026-10-08)

* `kernel-core` host test `a_udp_query_takes_only_the_answer_from_its_server_and_port` (an
  impostor source, a wrong port and a reply to another query are skipped; an empty wire ends by
  budget); dropping the source check makes it fail.
* `scripts/dns-e2e.sh` gains an x86-64 leg whose machine has only an e1000: all eight console
  questions (two A records, a CNAME chain, NXDOMAIN, a spoofed id, a truncated answer, a looping
  name, a malformed name, and a real name through QEMU's resolver) give the same answers as over
  virtio-net. Its first run failed one question: the generic query waited 1 000 000 polls where
  the virtio resolver waited 20 000 000, and a cold first question needs the longer wait. The
  wait now matches.
* Boot gates on all three CPUs and the conformance gate pass with the e1000 leasing on QEMU.

## The package (release preparation, same day)

* The shipped `.vmx` template now enables an Intel e1000 on VMware NAT (it carried no NIC), and
  `scripts/release-vmware.sh` boots the packaged disks with `-nic user,model=e1000` to match, so
  the package proves the e1000 path before upload: the selftest disk's e1000 suite holds (5
  invariants, the lease taken before the gateway ARP) and the interactive disk reaches its prompt.
* That boot found a regression the other gates could not: with only an e1000 kept, the console's
  `net` called `dma_grants()`, which builds a list, and the console-storm suite (reporting
  commands allocate nothing) failed. `E1000::dma_regions` counts without allocating.

## Not done

* Not booted in VMware itself here (no VMware on the build host); the package is boot-verified
  under QEMU with the same NIC model. VirtualBox's e1000 gate (`vm-e2e-vbox.sh`) is the other
  hypervisor evidence.
* `tls` still refuses on VMware: it takes key material only from an entropy device, and VMware
  offers no virtio-rng. RDRAND (x86-64) as a second entropy source is the next step.
* No interrupt-driven receive and no DHCP renewal on either NIC.
