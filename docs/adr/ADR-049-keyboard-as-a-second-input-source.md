# ADR-049 — A keyboard is a second input source, not a second console

**Status:** Accepted
**Date:** 2026-08-09
**Requirements:** REQ-CON-003 (keyboard input on the machine's own hardware)
**Closes:** GAPS4 `ALET-P2-039`
**Extends:** ADR-044 (interactive console), ADR-045 (interrupt-driven console input)

---

## Context

ADR-044 gave Aletheia a console you can sit in front of. ADR-045 made its input interrupt-driven so
the machine is not spinning while it waits. Both were gated on all three CPU targets, and both were
gated **over the serial line**, because that was the only input source the console had.

That is exactly why the gap survived. Under QEMU with `-serial stdio`, and under the VirtualBox
host-pipe recipe in `docs/VIRTUALBOX.md`, the terminal *is* the wire — a kernel with no keyboard
driver is indistinguishable from one with a working keyboard. `console-e2e.sh` types on the wire, so
no amount of running it could have found this.

What found it was a person booting the image on a VirtualBox GUI window: the framebuffer showed the
prompt, the keyboard reached nothing, and a working OS looked hung. An operating system whose
interactive console cannot be typed at from the machine's own keyboard is not an operating system
someone can use, whatever its invariant count.

---

## Decision

### 1. The keyboard produces bytes for the existing ring — it does not get a console

`kernel_core::shell` owns the line editor, the commands and the refusals. `kernel_core::conring`
owns the bounded input ring and its overflow policy. A keyboard arrives as **bytes in that same
ring**, decoded by `kernel_core::keymap`, and nothing downstream of the ring knows a keyboard exists.

The alternative — a keyboard path with its own reader — is the one that rots: two input paths become
two line editors, and the console ends up with one set of refusals for the wire and another for the
keys. The whole of what is keyboard-specific ends at the keymap.

### 2. What a scancode *means* is arch-independent; the controller is not

`kernel_core::keymap` (scancode set 1 → console bytes, held modifiers, caps, `Ctrl-C`) lives in
`kernel-core` and is proved on **all three targets** plus the host. `kernel-x86_64/src/ps2.rs` (the
i8042) is x86-64's alone — the QEMU `virt` machine used by aarch64 and RISC-V exposes no PS/2
controller, an honest architectural difference of the same kind as the RISC-V `PTE_U` case in
ADR-034.

### 3. The decoder's output alphabet is a security boundary

The line editor's contract is written against a byte alphabet: printable ASCII, `\r`, backspace,
`Ctrl-C`. It refuses everything else — `console: a non-printable byte never enters the line`. A
decoder free to emit arbitrary bytes could hand the editor a control character it has no rule for,
**from a device an attacker may be holding**. So `Keymap::feed` emits only bytes in that alphabet,
and the invariant proves it over the entire input space: all 256 scancodes against every reachable
modifier state, not a sample. `Ctrl` with anything other than `C` produces nothing rather than an
arbitrary control code.

### 4. The controller is *enumerated*, not poked

This is the part that separates a driver from a demo, and it is why this ADR exists rather than a
four-line patch:

* **The machine says whether it has one.** On x86-64 you cannot ask the hardware what exists by
  reading it: an absent device's ports return `0xFF` on one chipset, float on another, and on a
  legacy-free platform are not wired at all. The firmware states it in the ACPI FADT
  (`IAPC_BOOT_ARCH` bit 1), so that is consulted **before any port is touched**. Absent field ≠ zero
  field: a missing `IAPC_BOOT_ARCH` means ACPI 1.0, where the controller is universal, so the answer
  is "assume present and let the self-test decide". Collapsing the two would make every older
  machine keyboardless.
* **Every wait is spin-bounded.** There is no unbounded loop anywhere in the driver. A keyboard that
  never answers costs the boot a bounded delay and a printed reason. An OS that a missing device can
  stop forever is not a production OS.
* **The controller self-test and the port test are separate**, because a controller can pass its own
  self-test with a dead keyboard port, and the two failures want different lines in the log.
* **The configuration byte is written again after the self-test**, which resets it on many
  implementations, and then **read back**. That read-back is not ceremony: the translation bit is
  what makes the arch-independent decoder's set-1 assumption true, and a controller that silently
  dropped the write would deliver set 2 into a set-1 decoder — every key wrong, presenting as a
  broken keymap rather than a broken assumption.
* **Every failure is a distinct `Ps2Error`**, because "this machine has no PS/2 controller" and
  "this machine has one and it failed its self-test" are different facts, and a boolean cannot tell
  you which.

The ACPI walk moved out of `smp.rs` into `kernel-x86_64/src/acpi.rs` when the keyboard became its
second consumer (SMP wants the MADT, the keyboard wants the FADT). It now **verifies table
checksums**, which the MADT walk did not: a table that does not sum to zero is one the firmware did
not finish writing, and enumerating hardware from it is worse than finding no table.

### 5. The driver is proved on every boot, not only interactive ones

`ps2::keyboard_suite` runs in the **non-interactive gate build** too, performing the real bring-up
against the real controller and then leaving the machine exactly as it found it — IRQ1 stays masked,
because arming an input source is the console's decision and a boot suite that left an interrupt
live would hand every later suite a machine taking interrupts it was not written for. A driver that
only runs when someone is sitting at the machine is a driver no gate covers.

On a machine that genuinely has no controller the suite reports that as information and passes — a
legacy-free platform is not a defect — but it is *named*, so a log can distinguish "this machine has
no keyboard" from "this kernel cannot find one".

### 6. A gate that types on the keyboard

`scripts/keyboard-e2e.sh` boots the interactive image under QEMU with the serial line pointed at a
**file** — nothing can type at it — and drives the emulated i8042 through QMP `send-key`. Every
keystroke travels the real path: controller output buffer → IRQ1 → PIC → vector 0x21 → decoder →
shared ring → line editor. The test knows nothing about scancodes; it presses keys by name and
asserts that Aletheia's own filesystem changed. Shift is sent as a real held modifier rather than a
synthetic uppercase byte, so the decoder's state machine is what is being tested.

Proved additionally by hand on Oracle VirtualBox via `VBoxManage controlvm keyboardputstring`, which
injects at that hypervisor's own i8042 — the configuration the bug was reported from.

---

## Consequences

**Measured.** `KEYBOARD-E2E: PASS` (11 checks, QEMU+OVMF — 7 at first delivery, extended to 11 by
ADR-050's navigation keys). `[keys] ALL 12 KEYBOARD-DECODE INVARIANTS HOLD` on aarch64, RISC-V and x86-64. `[ps2] ALL 5 KEYBOARD INVARIANTS HOLD` on x86-64 under both
QEMU (`device id AB 41` — a translated MF2 keyboard, which is itself the evidence that translation
is on) and VirtualBox.

**Not claimed — USB HID.** No USB stack, so no USB keyboard driver. On the overwhelming majority of
machines the firmware's legacy USB emulation presents a USB keyboard through this same i8042, which
is why this is the right first driver; with that emulation disabled, a USB keyboard will not work.
Registered, not implied.

**Not claimed — one layout.** US QWERTY, in two tables. A layout is data and the decoder is written
to take another one, but there is exactly one today and no mechanism to select it.

**Not claimed — no key repeat, no LEDs, no keypad-numlock semantics.** The typematic rate is whatever
the controller defaults to; caps lock changes decoding but does not light the lamp. Each is a device
command away and none of them is a correctness property of the input path.

**Not claimed — aarch64 and RISC-V still have serial input only.** Their QEMU `virt` machines expose
no PS/2 controller. The decoder is proved there; there is nothing to decode from.

## Alternatives considered

**Poll port 0x60 in the console loop.** Rejected: it undoes ADR-045. The console would spin, and on
a machine with no controller it would spin on an unclaimed port.

**Give the keyboard its own reader alongside the serial one.** Rejected — see §1. Two input paths
with one editor is the design; two editors is the rot.

**Skip ACPI and probe the ports directly.** Rejected. It is what most hobby kernels do and it is
undefined behavior on a legacy-free platform. The firmware publishes the answer; a driver that
ignores it is guessing about hardware on a machine that already told it.
