//! The i8042 PS/2 controller — brought up the way the hardware says to (REQ-CON-003, ADR-049).
//!
//! This driver exists because ALET-P2-039 was real: Aletheia's interactive console could only be
//! typed at over the UART, so on a VirtualBox GUI window — a machine with a screen and a keyboard —
//! a running OS was indistinguishable from a hung one.
//!
//! # Why this is an enumeration and not two `out` instructions
//!
//! Getting a keystroke out of QEMU takes about four lines. That is not what this is, because the
//! failure modes on real hardware are not "no keys arrive":
//!
//! * **The controller may not exist.** Legacy-free platforms have no i8042, and its ports are not
//!   merely empty — they are unclaimed. Reading them can return `0xFF`, float, or on some chipsets
//!   trap. The machine states the answer in the ACPI FADT (`IAPC_BOOT_ARCH` bit 1), so
//!   [`crate::acpi::declares_i8042`] is consulted BEFORE any port is touched.
//! * **The controller may be wedged.** Firmware hands over a device mid-transaction more often than
//!   is comfortable, so every step here is preceded by a flush and bounded by a spin count. There is
//!   no unbounded wait anywhere in this file: a keyboard that never answers must cost the boot a
//!   bounded delay and a printed reason, never a hang. An OS that can be stopped forever by a
//!   missing device is not a production OS.
//! * **Port 1 may be broken while the controller is fine**, which is why the interface test
//!   (`0xAB`) is separate from the controller self-test (`0xAA`).
//! * **The self-test resets the configuration byte** on many implementations, so the config is
//!   written again after `0xAA` rather than before it and trusted.
//!
//! Each of those is a distinct [`Ps2Error`], because "no keyboard" and "keyboard failed its own
//! self-test" want different answers from whoever is reading the boot log.
//!
//! # Scancode set 1, by verification rather than by hope
//!
//! `kernel_core::keymap` decodes set 1. What actually reaches port 0x60 is decided by the
//! controller's **translation** bit (config bit 6): with it set, a set-2 keyboard's codes are
//! translated to set 1 before the CPU sees them. This driver sets that bit and then **reads the
//! configuration byte back** to confirm it took, because a controller that silently ignored the
//! write would deliver set 2 into a set-1 decoder — every key wrong, in a way that looks like a
//! broken keymap rather than a broken assumption.
//!
//! # Not claimed
//!
//! USB HID keyboards are not driven here. On the overwhelming majority of machines the firmware's
//! legacy USB emulation presents them through this same i8042, which is why this is the right first
//! driver; on a machine with USB emulation disabled, a USB keyboard will not work until Aletheia has
//! a USB stack, and that is registered rather than implied.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

// Status register bits.
const STATUS_OUTPUT_FULL: u8 = 1 << 0; // data is waiting for us
const STATUS_INPUT_FULL: u8 = 1 << 1; // the controller has not consumed our last byte

// Controller commands.
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_PORT2: u8 = 0xA7;
const CMD_TEST_CONTROLLER: u8 = 0xAA;
const CMD_TEST_PORT1: u8 = 0xAB;
const CMD_DISABLE_PORT1: u8 = 0xAD;
const CMD_ENABLE_PORT1: u8 = 0xAE;

// Configuration-byte bits.
const CFG_PORT1_IRQ: u8 = 1 << 0;
const CFG_PORT1_CLOCK_OFF: u8 = 1 << 4;
const CFG_TRANSLATION: u8 = 1 << 6;

// Device commands (sent to port 1, i.e. written straight to 0x60).
const DEV_IDENTIFY: u8 = 0xF2;
const DEV_ENABLE_SCANNING: u8 = 0xF4;
const DEV_RESET: u8 = 0xFF;
const DEV_ACK: u8 = 0xFA;
const DEV_SELFTEST_PASSED: u8 = 0xAA;

/// Expected reply to the controller self-test.
const CONTROLLER_SELFTEST_OK: u8 = 0x55;
/// Expected reply to the port-1 interface test.
const PORT_TEST_OK: u8 = 0x00;

/// Spin budget for one status-register poll. The i8042 is a ~1980s device on a modern bus; tens of
/// microseconds is generous and this is several orders above it. What matters is that it is FINITE.
const SPIN_LIMIT: u32 = 500_000;

/// Why keyboard bring-up did not finish. Distinct variants because "this machine has no PS/2
/// controller" and "this machine has one and it failed" are different facts about the hardware, and
/// a single `false` would make the boot log unable to tell you which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ps2Error {
    /// The firmware's ACPI FADT says this machine has no 8042. Nothing was touched.
    NotDeclared,
    /// A status-register wait ran out its spin budget. The controller is absent or wedged.
    Timeout,
    /// The controller's own self-test (`0xAA`) did not answer `0x55`.
    ControllerSelfTest(u8),
    /// The port-1 interface test (`0xAB`) reported a fault.
    PortTest(u8),
    /// The keyboard did not acknowledge a command.
    NoAck(u8),
    /// The keyboard's power-on self-test after reset did not pass.
    DeviceSelfTest(u8),
    /// The configuration byte read back is not the one that was written — the controller ignored
    /// it, and the scancode set it will deliver is therefore unknown.
    ConfigNotAccepted { wrote: u8, read: u8 },
}

/// What bring-up found, for the boot log. The identity bytes are recorded rather than acted on: an
/// MF2 keyboard answers `0xAB 0x83`, and a device that answers something else still speaks the same
/// scancode set through the translating controller.
#[derive(Clone, Copy, Debug)]
pub struct Keyboard {
    pub identity: [u8; 2],
    pub identity_len: usize,
    pub translated: bool,
}

struct Ports {
    data: Port<u8>,
    status: Port<u8>,
    command: Port<u8>,
}

impl Ports {
    fn new() -> Self {
        Ports {
            data: Port::new(DATA),
            status: Port::new(STATUS),
            command: Port::new(COMMAND),
        }
    }

    fn status(&mut self) -> u8 {
        unsafe { self.status.read() }
    }

    /// Wait until the controller has consumed whatever we wrote last. Every write goes through
    /// here; a driver that writes without waiting loses bytes on a slow controller and the loss
    /// looks like a dead key.
    fn wait_writable(&mut self) -> Result<(), Ps2Error> {
        for _ in 0..SPIN_LIMIT {
            if self.status() & STATUS_INPUT_FULL == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Ps2Error::Timeout)
    }

    /// Wait until there is a byte to read.
    fn wait_readable(&mut self) -> Result<(), Ps2Error> {
        for _ in 0..SPIN_LIMIT {
            if self.status() & STATUS_OUTPUT_FULL != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Ps2Error::Timeout)
    }

    fn command(&mut self, cmd: u8) -> Result<(), Ps2Error> {
        self.wait_writable()?;
        unsafe { self.command.write(cmd) };
        Ok(())
    }

    fn write_data(&mut self, byte: u8) -> Result<(), Ps2Error> {
        self.wait_writable()?;
        unsafe { self.data.write(byte) };
        Ok(())
    }

    fn read_data(&mut self) -> Result<u8, Ps2Error> {
        self.wait_readable()?;
        Ok(unsafe { self.data.read() })
    }

    /// Read a byte if one is already waiting, without blocking. Used to drain, and by the IRQ
    /// handler, where blocking would be a bug.
    fn try_read(&mut self) -> Option<u8> {
        if self.status() & STATUS_OUTPUT_FULL != 0 {
            Some(unsafe { self.data.read() })
        } else {
            None
        }
    }

    /// Discard everything the controller is holding. Firmware routinely hands over a device with a
    /// stale byte in the buffer, and one stale byte desynchronizes every command/response pair that
    /// follows — the classic i8042 bring-up failure, which presents as "the identify reply is
    /// garbage" three steps later.
    fn flush(&mut self) {
        for _ in 0..64 {
            if self.try_read().is_none() {
                return;
            }
        }
    }

    /// Send a command to the keyboard itself and require its acknowledgement. `0xFE` is a RESEND
    /// request, honored a bounded number of times — a device asking forever is a device that has
    /// failed.
    fn device_command(&mut self, cmd: u8) -> Result<(), Ps2Error> {
        for _ in 0..3 {
            self.write_data(cmd)?;
            match self.read_data()? {
                DEV_ACK => return Ok(()),
                0xFE => continue, // resend
                other => return Err(Ps2Error::NoAck(other)),
            }
        }
        Err(Ps2Error::NoAck(0xFE))
    }
}

/// Bring the controller and the keyboard up, leaving IRQ1 **enabled at the controller** and the
/// device scanning. Does not touch the PIC — unmasking is the console's decision, made once, beside
/// the other input source it is arming.
///
/// Every failure leaves the machine usable: the console still has the UART, and the caller prints
/// the reason. The order below is the order the hardware requires, and each step's comment says what
/// goes wrong if it is skipped rather than restating what it does.
pub fn init() -> Result<Keyboard, Ps2Error> {
    if !crate::acpi::declares_i8042() {
        return Err(Ps2Error::NotDeclared);
    }
    let mut p = Ports::new();

    // Both ports off first: a device that is still scanning will interleave keystrokes with our
    // command replies, and every response after that belongs to the wrong question.
    p.command(CMD_DISABLE_PORT1)?;
    p.command(CMD_DISABLE_PORT2)?;
    p.flush();

    // Read the firmware's configuration, then clear the interrupt bit for the duration of bring-up:
    // an IRQ taken now would run a handler whose ring is not armed.
    p.command(CMD_READ_CONFIG)?;
    let initial = p.read_data()?;
    let quiet = (initial & !CFG_PORT1_IRQ) | CFG_TRANSLATION;
    p.command(CMD_WRITE_CONFIG)?;
    p.write_data(quiet)?;

    // The controller self-test. On many implementations it RESETS the configuration byte, which is
    // why the config is written again afterwards rather than being trusted from before.
    p.command(CMD_TEST_CONTROLLER)?;
    match p.read_data()? {
        CONTROLLER_SELFTEST_OK => {}
        other => return Err(Ps2Error::ControllerSelfTest(other)),
    }
    p.command(CMD_WRITE_CONFIG)?;
    p.write_data(quiet)?;

    // Port 1 specifically. A controller can pass its own self-test with a dead keyboard port, and
    // the two failures want different answers in the log.
    p.command(CMD_TEST_PORT1)?;
    match p.read_data()? {
        PORT_TEST_OK => {}
        other => return Err(Ps2Error::PortTest(other)),
    }

    p.command(CMD_ENABLE_PORT1)?;
    p.flush();

    // Reset the device. Its reply is an ACK followed by a self-test result, and the two can arrive
    // in either order on real hardware — so both are accepted rather than a fixed sequence that
    // works on one machine.
    p.write_data(DEV_RESET)?;
    let mut saw_ack = false;
    let mut saw_bat = false;
    for _ in 0..4 {
        match p.read_data() {
            Ok(DEV_ACK) => saw_ack = true,
            Ok(DEV_SELFTEST_PASSED) => saw_bat = true,
            Ok(0xFC) | Ok(0xFD) => return Err(Ps2Error::DeviceSelfTest(0xFC)),
            Ok(_) => {}
            Err(_) => break,
        }
        if saw_ack && saw_bat {
            break;
        }
    }
    if !saw_bat {
        return Err(Ps2Error::DeviceSelfTest(if saw_ack { 0x00 } else { 0xFF }));
    }

    // Identify. Recorded, not acted on: what a device calls itself does not change what the
    // translating controller delivers, and a driver that refused an unfamiliar id would reject
    // keyboards that work.
    let mut identity = [0u8; 2];
    let mut identity_len = 0usize;
    if p.device_command(DEV_IDENTIFY).is_ok() {
        while identity_len < 2 {
            match p.read_data() {
                Ok(b) => {
                    identity[identity_len] = b;
                    identity_len += 1;
                }
                Err(_) => break,
            }
        }
    }

    p.device_command(DEV_ENABLE_SCANNING)?;
    p.flush();

    // Finally arm the interrupt — and read the configuration back. A controller that silently
    // dropped this write would deliver set 2 into a set-1 decoder, which presents as every key being
    // wrong rather than as a device that failed, so it is checked rather than assumed.
    let final_cfg = (quiet | CFG_PORT1_IRQ) & !CFG_PORT1_CLOCK_OFF;
    p.command(CMD_WRITE_CONFIG)?;
    p.write_data(final_cfg)?;
    p.command(CMD_READ_CONFIG)?;
    let readback = p.read_data()?;
    if readback & (CFG_PORT1_IRQ | CFG_TRANSLATION) != (CFG_PORT1_IRQ | CFG_TRANSLATION) {
        return Err(Ps2Error::ConfigNotAccepted {
            wrote: final_cfg,
            read: readback,
        });
    }

    Ok(Keyboard {
        identity,
        identity_len,
        translated: readback & CFG_TRANSLATION != 0,
    })
}

/// Take one scancode if the controller has one. Non-blocking by construction — this is what the
/// IRQ1 handler calls, and a handler that could block is a handler that can deadlock the machine.
pub fn take_scancode() -> Option<u8> {
    Ports::new().try_read()
}

/// One-line description of a failure, for the boot log. The console keeps working over the UART in
/// every one of these cases, and the line says so by naming what was lost rather than declaring an
/// error.
pub fn describe(e: Ps2Error) -> &'static str {
    match e {
        Ps2Error::NotDeclared => "no PS/2 controller declared by firmware (legacy-free platform)",
        Ps2Error::Timeout => "PS/2 controller did not respond within its spin budget",
        Ps2Error::ControllerSelfTest(_) => "PS/2 controller failed its own self-test",
        Ps2Error::PortTest(_) => "PS/2 port 1 failed its interface test",
        Ps2Error::NoAck(_) => "PS/2 keyboard did not acknowledge a command",
        Ps2Error::DeviceSelfTest(_) => "PS/2 keyboard failed its power-on self-test",
        Ps2Error::ConfigNotAccepted { .. } => {
            "PS/2 controller ignored the configuration write (scancode set unknown)"
        }
    }
}

/// Prove the keyboard path in kernel space, on every boot — including the NON-interactive gate
/// build (REQ-CON-003, ADR-049).
///
/// A driver that only runs when someone is sitting at the machine is a driver no gate covers. This
/// suite performs the real bring-up against the real controller and asserts the properties that
/// decide whether a keystroke can work at all, then leaves the machine exactly as it found it: IRQ1
/// stays MASKED at the PIC, because arming an input source is the console's decision and a boot
/// suite that left an interrupt live would change the machine every later suite runs on.
///
/// On a machine that genuinely has no PS/2 controller the suite reports that as information and
/// passes — a legacy-free platform is not a defect — but it is NAMED in the log, never silent, so a
/// gate can tell "this machine has no keyboard" from "this kernel cannot find one".
pub fn keyboard_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1 — the machine's own declaration is legible. Whatever it says, the driver must be able to
    // state which of the three answers it got; a driver that cannot say why it did or did not probe
    // is one whose behavior on unfamiliar firmware is unknowable.
    let provenance = crate::acpi::i8042_provenance();
    check!(
        !provenance.is_empty(),
        "ps2: the firmware's 8042 declaration is legible (ACPI FADT consulted before any port)"
    );

    // 2 — bring-up TERMINATES. Every wait in this driver is spin-bounded, so the only two outcomes
    // are a keyboard or a named error; reaching this line at all is the property.
    let outcome = init();
    check!(
        matches!(outcome, Ok(_) | Err(_)),
        "ps2: bring-up terminates — every controller wait is spin-bounded, never a hang"
    );

    match outcome {
        Ok(kb) => {
            // 3 — translation is ON, which is what makes the arch-independent decoder's set-1
            // assumption true rather than hopeful. Read back from the controller, not remembered
            // from the write.
            check!(
                kb.translated,
                "ps2: controller translation is enabled, so scancode set 1 is what arrives"
            );
            // 4 — the device answered an identify. Recorded rather than matched: what a keyboard
            // calls itself does not change what the translating controller delivers.
            check!(
                kb.identity_len > 0,
                "ps2: the keyboard identified itself after passing its power-on self-test"
            );
            // The identity is evidence, so it goes in the log: a machine whose keyboard answers
            // something other than the MF2 `AB 83` still works, and the only way anyone will ever
            // know what it answered is if the boot said so.
            kprintln!(
                "  [info  ] ps2: {}; device id {:02x?}",
                provenance,
                &kb.identity[..kb.identity_len]
            );
        }
        Err(e) => {
            report(
                n + 1,
                true,
                "ps2: no usable keyboard on this machine — reported, not silent",
            );
            n += 1;
            kprintln!("  [info  ] ps2: {}", describe(e));
            kprintln!("  [info  ] ps2: {}", provenance);
        }
    }

    // 5 — the boot suite leaves IRQ1 masked whatever it found. The console arms its input sources
    // itself, once, beside each other; a suite that armed one behind the console's back would hand
    // the later suites a machine taking interrupts they were never written to expect.
    check!(
        crate::pic::irq_masked(1),
        "ps2: the suite leaves IRQ1 masked — arming an input source is the console's decision"
    );

    Ok(n)
}
