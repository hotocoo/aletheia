//! A keyboard is a second input SOURCE, not a second console (REQ-CON-003, ADR-049).
//!
//! `REQ-CON-001/002` gave Aletheia a console you can sit in front of, and then could not be typed
//! at from the machine's own screen: its only input source was the UART. Under QEMU with
//! `-serial stdio`, and under the VirtualBox host-pipe recipe, that is invisible — the terminal IS
//! the serial line. On a VirtualBox GUI window the framebuffer shows the prompt and the keyboard
//! reaches nothing, so a working OS is indistinguishable from a hung one. That was ALET-P2-039, and
//! it was reported by the first person to boot the image who was not its author.
//!
//! The fix is deliberately NOT a second console. `kernel_core::shell` owns the line editor, the
//! commands and the refusals; `kernel_core::conring` owns the bounded input ring and its overflow
//! policy. A keyboard has to arrive as *bytes in that same ring*, or the two input paths get two
//! line editors and drift — the console would have one set of refusals for the wire and another for
//! the keys. So this module does exactly one thing: turn scancodes into the bytes the UART would
//! have delivered.
//!
//! # Scancode set 1, because that is what the hardware hands over
//!
//! The i8042 controller ships with translation ENABLED, so a PS/2 keyboard's set-2 codes arrive as
//! set 1 (the old XT set) regardless of what the keyboard itself speaks. Set 1 is: a **make** code
//! with bit 7 clear, the matching **break** code with bit 7 set, and `0xE0` as a prefix for the keys
//! the original XT layout had no room for.
//!
//! # The one rule that makes this a security boundary and not a lookup table
//!
//! The line editor's contract is written against a byte alphabet — [`shell::editor_accepts`], the
//! single definition both sides use. It refuses anything else (`console: a non-printable byte never
//! enters the line`). A decoder free to emit arbitrary bytes could hand the editor a control
//! character it has no rule for, from a device an attacker may be holding. [`Keymap::feed`]
//! therefore emits **only** bytes in that alphabet, and
//! `every_byte_the_map_can_emit_is_one_the_console_accepts` proves it over the entire input space —
//! all 256 scancodes against every reachable modifier state, prefixed and not, not a sample.
//!
//! # Arrows, Home/End and Delete are sequences, not characters (REQ-CON-004, ADR-050)
//!
//! The keys the XT layout had no room for arrive as `E0`-prefixed codes, and the console's editor
//! reads cursor movement in the language a serial terminal already speaks: ANSI control sequences.
//! So the decoder emits `ESC [ D` for the left arrow rather than inventing a private byte — one
//! editor, one grammar, whichever wire the keystroke came in on. Emitting a byte the editor has no
//! rule for was the old failure; emitting a *sequence* it parses is the fix.
use crate::shell;

/// Byte the line editor reads as "erase the character before the cursor".
pub const BACKSPACE: u8 = shell::BACKSPACE;
/// Byte the line editor reads as "run this line". A keyboard's Enter is a carriage return, the same
/// thing a serial terminal sends, so the editor needs no second rule.
pub const CARRIAGE_RETURN: u8 = b'\r';
/// `Ctrl-C` — the editor discards the line without running it.
pub const ETX: u8 = shell::CTRL_C;

/// The bytes one scancode produced.
///
/// A fixed four-byte buffer rather than a slice or an allocation: this is filled inside an interrupt
/// handler, where nothing may allocate, and the longest sequence the layout can emit (`ESC [ 3 ~`)
/// is four bytes. A key that produces nothing returns [`Keys::EMPTY`], which is the common answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Keys {
    buf: [u8; 4],
    len: u8,
}

impl Keys {
    /// The answer for a release, a modifier, and every key this layout does not map.
    pub const EMPTY: Keys = Keys {
        buf: [0; 4],
        len: 0,
    };

    /// One byte.
    pub const fn one(b: u8) -> Keys {
        Keys {
            buf: [b, 0, 0, 0],
            len: 1,
        }
    }

    /// A sequence of up to four bytes. Anything longer is a programming error in this module, and
    /// truncating is the fail-closed answer: a short sequence is ignored by the editor's parser,
    /// where a wrapped one would leak its tail into the line.
    const fn seq(bytes: [u8; 4], len: u8) -> Keys {
        Keys { buf: bytes, len }
    }

    /// The bytes, in order.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }
}

/// `ESC [ A` and friends: the cursor keys, in the language the editor parses. Public since
/// the input-hardware rung (ADR-080): the virtio keyboard's decoder speaks the SAME editor
/// grammar from a different wire, and two decoders must not grow two grammars.
pub const fn csi(final_byte: u8) -> Keys {
    Keys::seq([0x1b, b'[', final_byte, 0], 3)
}

/// `ESC [ 3 ~` — the one navigation sequence that carries a parameter (Delete). Same reason
/// as [`csi`]: one grammar, whichever wire the keystroke came in on.
pub const fn csi_delete() -> Keys {
    Keys::seq([0x1b, b'[', b'3', b'~'], 4)
}

// Set-1 make codes for the keys that are not characters.
const SC_ESCAPE: u8 = 0x01;
const SC_BACKSPACE: u8 = 0x0E;
const SC_TAB: u8 = 0x0F;
const SC_ENTER: u8 = 0x1C;
const SC_CTRL: u8 = 0x1D;
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_KEYPAD_STAR: u8 = 0x37;
const SC_ALT: u8 = 0x38;
const SC_SPACE: u8 = 0x39;
const SC_CAPS: u8 = 0x3A;
/// Prefix introducing a key the XT layout had no code for (arrows, right ctrl/alt, keypad enter…).
const SC_EXTENDED: u8 = 0xE0;

// The `E0`-prefixed keys the console has an answer for. Set-1 gives them the same low byte as the
// keypad key in the same position, which is why they are only meaningful after the prefix.
const SC_E0_HOME: u8 = 0x47;
const SC_E0_UP: u8 = 0x48;
const SC_E0_LEFT: u8 = 0x4B;
const SC_E0_RIGHT: u8 = 0x4D;
const SC_E0_END: u8 = 0x4F;
const SC_E0_DOWN: u8 = 0x50;
const SC_E0_DELETE: u8 = 0x53;
/// Keypad Enter and keypad `/`, which share their low byte with the main Enter and with `.`.
const SC_E0_KP_ENTER: u8 = 0x1C;
const SC_E0_KP_SLASH: u8 = 0x35;
/// Bit 7 distinguishes a release from a press.
const BREAK_BIT: u8 = 0x80;

/// Unshifted characters for set-1 make codes `0x00..=0x39`. `0` means "not a character key" — those
/// are handled by name above, so the table stays a table.
const UNSHIFTED: [u8; 0x3A] = [
    0, 0, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0, 0, b'q', b'w',
    b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', 0, 0, b'a', b's', b'd', b'f', b'g',
    b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v', b'b', b'n', b'm',
    b',', b'.', b'/', 0, 0, 0, 0,
];

/// Shifted characters, same indexing. A separate table rather than a transform: the punctuation row
/// is a US-layout convention, not a function of the character, and a `to_ascii_uppercase` shortcut
/// would silently produce nothing for `1` → `!`.
const SHIFTED: [u8; 0x3A] = [
    0, 0, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0, 0, b'Q', b'W',
    b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', 0, 0, b'A', b'S', b'D', b'F', b'G',
    b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V', b'B', b'N', b'M',
    b'<', b'>', b'?', 0, 0, 0, 0,
];

/// Decoder state: which modifiers are held, and whether the previous byte was an extended prefix.
///
/// Held modifiers are *state a device controls*, which is why every one of them is cleared by its
/// own break code and by nothing else. A decoder that inferred "shift is up" from a character key
/// would unstick a modifier the user is still holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keymap {
    shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
    extended: bool,
}

impl Keymap {
    pub const fn new() -> Self {
        Keymap {
            shift: false,
            ctrl: false,
            alt: false,
            caps: false,
            extended: false,
        }
    }

    /// True while a shift key is held (either one).
    pub fn shift_held(&self) -> bool {
        self.shift
    }
    /// True while a control key is held.
    pub fn ctrl_held(&self) -> bool {
        self.ctrl
    }
    /// True while caps lock is engaged.
    pub fn caps_on(&self) -> bool {
        self.caps
    }

    /// Feed one scancode; return the bytes the console should see, which is usually none.
    ///
    /// [`Keys::EMPTY`] is the common answer and never an error: releases, modifiers, and every key
    /// this layout does not map all produce nothing. Fail-closed in the literal sense — a code the
    /// decoder does not understand contributes no input rather than a guess.
    pub fn feed(&mut self, code: u8) -> Keys {
        // An extended prefix qualifies exactly the NEXT code, whatever that code is. Consuming it
        // here is what stops `E0 2A` — the fake shift the controller sends around some keys — from
        // toggling the real shift state.
        //
        // The pending-prefix test comes FIRST, before the prefix test, and that order is the whole
        // property: written the other way, `E0 E0` re-arms instead of resolving, so a device
        // emitting a stream of `E0` swallows every real key after it and the keyboard is
        // permanently dead with nothing crashed. Resolving first bounds a malformed stream to
        // eating one code per prefix. The exhaustive sweep found this.
        if self.extended {
            self.extended = false;
            // Right ctrl / right alt are the two extended keys whose STATE matters to the console.
            match code {
                SC_CTRL => self.ctrl = true,
                SC_ALT => self.alt = true,
                c if c == SC_CTRL | BREAK_BIT => self.ctrl = false,
                c if c == SC_ALT | BREAK_BIT => self.alt = false,
                // The navigation keys. Emitted as the ANSI sequences the editor's parser reads, so
                // the machine's own keyboard and a terminal on the serial line move the cursor
                // through exactly the same code path.
                SC_E0_UP => return csi(b'A'),
                SC_E0_DOWN => return csi(b'B'),
                SC_E0_RIGHT => return csi(b'C'),
                SC_E0_LEFT => return csi(b'D'),
                SC_E0_HOME => return csi(b'H'),
                SC_E0_END => return csi(b'F'),
                // Delete is the one navigation key whose sequence carries a parameter.
                SC_E0_DELETE => return Keys::seq([0x1b, b'[', b'3', b'~'], 4),
                SC_E0_KP_ENTER => return Keys::one(CARRIAGE_RETURN),
                SC_E0_KP_SLASH => return Keys::one(b'/'),
                _ => {}
            }
            return Keys::EMPTY;
        }
        if code == SC_EXTENDED {
            self.extended = true;
            return Keys::EMPTY;
        }

        if code & BREAK_BIT != 0 {
            match code & !BREAK_BIT {
                SC_LSHIFT | SC_RSHIFT => self.shift = false,
                SC_CTRL => self.ctrl = false,
                SC_ALT => self.alt = false,
                _ => {}
            }
            return Keys::EMPTY;
        }

        match code {
            SC_LSHIFT | SC_RSHIFT => {
                self.shift = true;
                return Keys::EMPTY;
            }
            SC_CTRL => {
                self.ctrl = true;
                return Keys::EMPTY;
            }
            SC_ALT => {
                self.alt = true;
                return Keys::EMPTY;
            }
            // Caps toggles on the PRESS only. Toggling on the release too would cancel itself.
            SC_CAPS => {
                self.caps = !self.caps;
                return Keys::EMPTY;
            }
            SC_ENTER => return Keys::one(CARRIAGE_RETURN),
            SC_BACKSPACE => return Keys::one(BACKSPACE),
            SC_SPACE => return Keys::one(b' '),
            // Tab completes a name — the editor has a rule for it now, so the key is delivered.
            SC_TAB => return Keys::one(shell::TAB),
            // Escape is still dropped, and deliberately. Delivering a lone `ESC` would leave the
            // editor's parser waiting for a final byte that a person pressing Escape is never going
            // to send, and the next key they typed would be swallowed as the sequence's body. The
            // key that means "cancel" to a human is `Ctrl-C`, and that one is delivered.
            SC_ESCAPE => return Keys::EMPTY,
            SC_KEYPAD_STAR => return Keys::one(b'*'),
            _ => {}
        }

        let i = code as usize;
        if i >= UNSHIFTED.len() {
            return Keys::EMPTY;
        }
        let base = if self.shift { SHIFTED[i] } else { UNSHIFTED[i] };
        if base == 0 {
            return Keys::EMPTY;
        }

        // Ctrl maps a letter to its control code. The chord is delivered only when the editor has a
        // rule for the resulting byte — `Ctrl-A`, `Ctrl-E`, `Ctrl-W` and the rest of the editing
        // set, plus `Ctrl-C`. Every other chord produces nothing rather than an arbitrary control
        // byte, so widening the editor's alphabet widens what the keyboard can send and nothing
        // else can.
        if self.ctrl {
            let ctl = base.to_ascii_lowercase();
            if ctl.is_ascii_lowercase() {
                let byte = ctl & 0x1f;
                if shell::editor_accepts(byte) {
                    return Keys::one(byte);
                }
            }
            return Keys::EMPTY;
        }

        // Caps lock affects letters only — a shifted `1` is `!` whether caps is on or not, and a
        // decoder that ran caps over the punctuation row would make the number row unusable.
        if self.caps && base.is_ascii_alphabetic() {
            return Keys::one(if self.shift {
                base.to_ascii_lowercase()
            } else {
                base.to_ascii_uppercase()
            });
        }
        Keys::one(base)
    }

    /// Is `b` a byte the console's line editor has a rule for?
    ///
    /// Delegates to [`shell::editor_accepts`] rather than restating the list: the decoder's security
    /// property is "everything I emit, the editor understands", and a property stated against a
    /// private copy of the editor's alphabet proves only that the copy agrees with itself.
    pub fn is_console_byte(b: u8) -> bool {
        shell::editor_accepts(b)
    }
}

/// The keyboard-decoding invariants, in the shape every kernel suite uses. Arch-independent: the
/// DEVICE is x86-64's (`kernel-x86_64/src/ps2.rs`), but what a scancode means is not, so every
/// target proves the decoder and the one with the hardware also proves the wiring.
pub fn keymap_suite(
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

    // 1 — a press produces its character and a release produces nothing. Without the second half
    // every key would type twice.
    {
        let mut k = Keymap::new();
        let press = k.feed(0x1E); // 'a'
        let release = k.feed(0x1E | BREAK_BIT);
        check!(
            press.as_slice() == b"a" && release.is_empty(),
            "keymap: a press types once and its release types nothing"
        );
    }

    // 2 — shift is held state, and it ends at its own break code.
    {
        let mut k = Keymap::new();
        k.feed(SC_LSHIFT);
        let upper = k.feed(0x1E);
        k.feed(SC_LSHIFT | BREAK_BIT);
        let lower = k.feed(0x1E);
        check!(
            upper.as_slice() == b"A" && lower.as_slice() == b"a",
            "keymap: shift is held state and ends at its own release"
        );
    }

    // 3 — shift reaches the punctuation row, which an uppercase transform would not.
    {
        let mut k = Keymap::new();
        k.feed(SC_LSHIFT);
        check!(
            k.feed(0x02).as_slice() == b"!" && k.feed(0x0C).as_slice() == b"_",
            "keymap: shift reaches the punctuation row, not only letters"
        );
    }

    // 4 — caps toggles letters, leaves digits alone, and cancels against shift.
    {
        let mut k = Keymap::new();
        k.feed(SC_CAPS);
        let letter = k.feed(0x1E);
        let digit = k.feed(0x02);
        k.feed(SC_LSHIFT);
        let both = k.feed(0x1E);
        check!(
            letter.as_slice() == b"A" && digit.as_slice() == b"1" && both.as_slice() == b"a",
            "keymap: caps affects letters only, and shift cancels it"
        );
    }

    // 5 — caps toggles on the press only. Toggling on the release as well cancels itself, and the
    // key appears dead.
    {
        let mut k = Keymap::new();
        k.feed(SC_CAPS);
        k.feed(SC_CAPS | BREAK_BIT);
        check!(
            k.caps_on() && k.feed(0x1E).as_slice() == b"A",
            "keymap: caps toggles on the press only"
        );
    }

    // 6 — the Ctrl chords the editor implements are delivered, and no chord it does not implement
    // invents a control byte. `Ctrl-A` moves to the start of the line; `Ctrl-G` has no meaning here
    // and therefore produces nothing at all.
    {
        let mut k = Keymap::new();
        k.feed(SC_CTRL);
        let c = k.feed(0x2E); // 'c' — cancel
        let a = k.feed(0x1E); // 'a' — start of line
        let g = k.feed(0x22); // 'g' — no rule in the editor
        check!(
            c.as_slice() == [ETX].as_slice()
                && a.as_slice() == [shell::CTRL_A].as_slice()
                && g.is_empty()
                && a.as_slice().iter().all(|b| Keymap::is_console_byte(*b)),
            "keymap: a Ctrl chord is delivered only when the editor has a rule for it"
        );
    }

    // 7 — an extended prefix consumes exactly one following code, and a repeated prefix RESOLVES
    // rather than re-arming. The first half stops the controller's fake `E0 2A` shift from sticking
    // the real shift down; the second stops a device that emits a stream of `E0` from swallowing
    // every real key after it — a keyboard permanently dead with nothing crashed.
    {
        let mut k = Keymap::new();
        let prefix = k.feed(SC_EXTENDED);
        let arrow = k.feed(SC_E0_LEFT);
        let after = k.feed(0x1E);
        let mut k2 = Keymap::new();
        k2.feed(SC_EXTENDED);
        k2.feed(SC_EXTENDED);
        let recovered = k2.feed(0x1E);
        // The fake shift the controller wraps the navigation keys in on real hardware: `E0 2A` must
        // not stick the real shift down, so the character after an arrow is still lowercase.
        let mut k3 = Keymap::new();
        k3.feed(SC_EXTENDED);
        k3.feed(SC_LSHIFT);
        let after_fake = k3.feed(0x1E);
        check!(
            prefix.is_empty()
                && arrow.as_slice() == b"\x1b[D"
                && !k.shift_held()
                && after.as_slice() == b"a"
                && recovered.as_slice() == b"a"
                && !k3.shift_held()
                && after_fake.as_slice() == b"a",
            "keymap: an extended prefix consumes exactly one code and cannot be re-armed forever"
        );
    }

    // 8 — Enter and Backspace are the two bytes the line editor's contract is written against.
    {
        let mut k = Keymap::new();
        check!(
            k.feed(SC_ENTER).as_slice() == [CARRIAGE_RETURN].as_slice()
                && k.feed(SC_BACKSPACE).as_slice() == [BACKSPACE].as_slice(),
            "keymap: Enter is a carriage return and Backspace is 0x08"
        );
    }

    // 9 — the security property, over the WHOLE input space: no scancode, in any reachable modifier
    // state, prefixed or not, can hand the line editor a byte it has no rule for. The prefixed half
    // is what makes this cover the navigation keys, whose sequences are the widest thing the decoder
    // can emit.
    {
        let mut ok = true;
        for shift in [false, true] {
            for ctrl in [false, true] {
                for caps in [false, true] {
                    for prefixed in [false, true] {
                        let mut k = Keymap::new();
                        if shift {
                            k.feed(SC_LSHIFT);
                        }
                        if ctrl {
                            k.feed(SC_CTRL);
                        }
                        if caps {
                            k.feed(SC_CAPS);
                        }
                        for code in 0u16..=0xFF {
                            let mut probe = k;
                            if prefixed {
                                probe.feed(SC_EXTENDED);
                            }
                            for b in probe.feed(code as u8).as_slice() {
                                if !Keymap::is_console_byte(*b) {
                                    ok = false;
                                }
                            }
                        }
                    }
                }
            }
        }
        check!(
            ok,
            "keymap: no scancode in any modifier state emits a byte the console refuses"
        );
    }

    // 10 — the navigation keys speak the editor's grammar. A sequence that is not exactly what the
    // editor parses is a sequence whose tail lands in the line as text, which is the failure this
    // whole path exists to prevent.
    {
        let mut k = Keymap::new();
        let mut seq = |code: u8| {
            k.feed(SC_EXTENDED);
            k.feed(code)
        };
        check!(
            seq(SC_E0_UP).as_slice() == b"\x1b[A"
                && seq(SC_E0_DOWN).as_slice() == b"\x1b[B"
                && seq(SC_E0_RIGHT).as_slice() == b"\x1b[C"
                && seq(SC_E0_HOME).as_slice() == b"\x1b[H"
                && seq(SC_E0_END).as_slice() == b"\x1b[F"
                && seq(SC_E0_DELETE).as_slice() == b"\x1b[3~"
                && seq(SC_E0_KP_ENTER).as_slice() == [CARRIAGE_RETURN].as_slice(),
            "keymap: the navigation keys emit the sequences the editor parses"
        );
    }

    // 11 — and every sequence the decoder can emit, fed to the editor, leaves the LINE untouched
    // while moving the cursor. This is the cross-module claim: the two modules agree about the
    // grammar, not merely about the alphabet.
    {
        let mut ok = true;
        for code in [
            SC_E0_UP,
            SC_E0_DOWN,
            SC_E0_LEFT,
            SC_E0_RIGHT,
            SC_E0_HOME,
            SC_E0_END,
            SC_E0_DELETE,
        ] {
            let mut k = Keymap::new();
            k.feed(SC_EXTENDED);
            let keys = k.feed(code);
            let mut ed = shell::LineEditor::new();
            for b in b"ls" {
                ed.feed(*b, &mut |_| {});
            }
            for b in keys.as_slice() {
                ed.feed(*b, &mut |_| {});
            }
            // The line still reads `ls`, and the parser is not left mid-sequence waiting for a byte
            // the keyboard will never send.
            if ed.line() != "ls" || ed.in_escape() {
                ok = false;
            }
        }
        check!(
            ok,
            "keymap: a navigation key moves the cursor and never types into the line"
        );
    }

    // 10 — releasing a modifier that was never pressed changes nothing, and an unmapped code
    // disturbs no state. A device can send anything; the decoder's state must not be steerable by
    // codes that mean nothing.
    {
        let mut k = Keymap::new();
        let before = k;
        k.feed(SC_LSHIFT | BREAK_BIT);
        k.feed(0x59); // unassigned in this layout
        k.feed(0x7E);
        check!(
            k == before,
            "keymap: a spurious release or unmapped code changes no state"
        );
    }

    Ok(n)
}
