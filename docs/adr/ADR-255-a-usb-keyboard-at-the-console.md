# ADR-255 — A USB keyboard at the console (x86-64)

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-014 (new)
**Builds on:** ADR-253 (xHCI driver, boot keyboard), ADR-049 (PS/2 keyboard into the console ring),
ADR-080 (no device draining in hard-IRQ time), ADR-168 (a waiting console costs nothing),
ADR-180 (check-then-sleep), ADR-075/235 (VT-d grants for devices kept past enforcement).

## Context

ADR-253 enumerated and configured a USB boot keyboard, then halted the controller: the driver was
proved, but nobody could type on it. On a machine whose only keyboard is USB the console was still
unreachable except over the serial line.

## Decision

* **An interactive x86-64 machine keeps the keyboard.** After the boot suite halts the controller,
  `pci::xhci_keep` re-opens it (`Xhci::init`, then `Xhci::bind_keyboard`, which the suite now uses
  too) and `usbkbd::keep` holds the result. The gate build does not keep it, so the boot family
  stays `xhci` = 7.
* **`kernel_core::xhci::KeyboardInput`** turns a configured keyboard into a byte source: when the
  console asks for a byte and none is queued, it drains up to eight reports, decodes them through
  `BootKeyboard` (the virtio keyboard's decoder, ADR-080), and queues the bytes, so an arrow's
  three-byte sequence is handed out whole and in order. A controller that answers wrongly once is
  halted and the input goes quiet, keeping the error.
* **Polled from the console's foreground, woken by the tick.** `getc` asks the serial/PS-2 ring
  first, then the USB keyboard, then the desktop terminal. Nothing touches the controller in an
  interrupt handler (ADR-080). The console sleeps in `sti; hlt`; with no USB keyboard the PIT is
  quiesced as before (ADR-168), and with one held `conirq::init` runs it at 100 Hz, so a keystroke
  waits at most 10 ms to be read.
* **VT-d**: the kept controller keeps fetching its rings, so its grants join the DMAR table next to
  the kept e1000's before enforcement turns on.

## Evidence

* `scripts/usbkbd-e2e.sh` (new CI job): an interactive x86-64 image boots with `qemu-xhci` and a
  `usb-kbd` bound to display `vga0`; every key is sent with QMP `input-send-event` naming `vga0`, so
  QEMU delivers it to the USB keyboard only, and the serial line is a file. `echo Usb` (shift held)
  is echoed and runs; `write usbtest hello`, then `cat usbtst`, left, left, `e`, End, Enter reads
  `hello` back with no escape bytes in the line. 5/5 pass. **Negative control**: the same image with
  the console's USB poll removed fails all three typing checks, so the keys were not reaching the
  console through the PS/2 path.
* Host: `keyboard_input_hands_out_bytes_in_order_and_stops_on_a_lie` (Up then h arrive as
  `ESC [ A h`; a transfer event for another slot halts the controller and the input reports nothing
  afterwards). 15 xHCI host tests.
* **Idle cost, measured** (QEMU CPU time over 20 s at the prompt, two runs each, this host, TCG):
  0.00 % without a USB keyboard, 0.75 % with one held. The 100 Hz tick and QEMU's xHCI model are
  paid only on a machine that holds a USB keyboard; BENCHMARKS section 000's 0.0 % idle figure is
  for a console with none.

## What this is not

* **x86-64 console only.** aarch64 keeps its keyboard out (its timer interrupt is enabled only with
  a live desktop; arming it for a USB keyboard is its own change). The live desktop's pump does not
  read the USB keyboard yet, so desktop shortcuts and the terminal window still need the virtio
  keyboard.
* Not interrupt-driven: the controller's interrupter stays off, and the tick is the wake.
* One keyboard, no hubs, no hot-plug: a keyboard plugged in after boot is not seen.
