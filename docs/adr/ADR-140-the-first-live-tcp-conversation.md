# ADR-140 — The first live TCP conversation: a person types, and a real peer answers

- **Status:** accepted
- **Date:** 2026-09-17
- **Requirement:** REQ-NET-006, ALET-P2-020 (the network rung), Lethe integration stage N1
- **Supersedes:** nothing. Extends ADR-138 (TCP as a bounded state machine), ADR-139 (where TCP
  meets the link) and ADR-044/051 (the interactive console).

## Context

ADR-138 proved the transport and ADR-139 proved the join, and after both of them this kernel had
still never opened a socket. Everything held over a scripted link; nothing had spoken to a peer
that was not written by this repository.

There was also a plain defect in the way the network was brought up: `net_suite` **consumed** the
device. The kernel proved its network and then had none — the only NIC was dropped at the end of
the suite, so nothing after boot could use it even in principle.

## Decision

Give the device back, keep it, and let a person use it.

1. **`net_suite` hands the device back** (`Result<(usize, VirtioNet<..>), ..>`). A suite that eats
   the only NIC is a proof that costs the machine the thing it proved.
2. **Each target keeps it** in `netstatic.rs` — one static, written once during boot, read only by
   the console's own thread between keystrokes. The same posture the desktop's singleton already
   has, and for the same reason: no interrupt context touches it.
3. **The console gains one command**, `tcp ADDR PORT TEXT`, dispatched through a new defaulted
   `ShellHost::tcp_fetch`. A machine with no NIC keeps the default, which is a **named refusal**
   rather than a zero-length answer that would read as a silent peer.
4. **Every bound belongs to this machine, not the peer**: the local port (walked upward per
   connection), the initial sequence number (drawn from the machine's clock, because it must be
   unpredictable on a real network), the retransmission timeout (a fifth of a second), the poll
   budget, and the 512-byte reply buffer. A peer can make this slow; it cannot make it unbounded.
5. **`tcp` is classified DESTRUCTIVE** for hosted approval (`aletheia/src/console_ops.rs`). Nothing
   on the medium changes, and the classification is still right: the command is **outward facing**.
   It announces this machine to a peer that did not ask and sends it bytes the operator typed.

Bytes that come back are printed the way the file panel prints a file: a non-text answer is named
rather than executed, because a peer's answer is the least trustworthy input this machine has.

## The proof (`scripts/tcp-e2e.sh`, in CI)

A gate that mocks the peer proves nothing new after ADR-139, so this one does not. The script
starts a **real socket server on the host loopback**, and QEMU's user network maps `10.0.2.2` to
that host. A scripted operator then types at the console:

1. `tcp 10.0.2.2 1 nobody-listens-here` — a port nobody listens on must be refused **by name**,
   not by a hang and not by a pretend success.
2. `tcp 10.0.2.2 <port> hello-from-aletheia` — the live peer.

Three things are asserted, and they are deliberately not the same thing: the console printed the
peer's answer, the answer carries a prefix **the peer added** (so a console echoing its own input
cannot pass), and the peer's own transcript shows it received the request. Serial in, TCP out, TCP
in, serial out.

The gate covers both device-tree targets (aarch64 and RISC-V), which is where `-netdev user` is
already part of the interactive boot. The x86-64 console runs the identical code path; extending
the gate to it is named as open rather than implied.

## Alternatives considered

**Prove the conversation in the boot suite instead.** Rejected: every boot would then depend on a
server running on whatever machine boots this kernel, and a boot invariant that is skipped when
nobody is listening is not an invariant.

**Re-open the device for the console.** Rejected: on x86-64 the NIC is brought up before the VT-d
gate closes, and constructing a second instance afterwards publishes queues under an active
remapping unit — the failure ADR-073 already records for the block device.

**A socket API with its own buffers.** Still rejected, as in ADR-139: one bounded buffer per
connection is enough until more than one connection exists at a time.

## Consequences

This kernel has now held a TCP conversation with a program it did not write, driven by a person at
a console. Stage N1 of `docs/LETHE-INTEGRATION.md` is delivered in the full sense: the contract,
the join, and a live socket.

What is still absent is everything above the transport: no TLS, no HTTP, no name resolution (the
operator types an address, because there is no DNS client), one connection at a time, and no
listening socket. The `tcp` command is a proof and a tool, not a network stack's public interface.
