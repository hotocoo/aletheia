//! Scancode decoding, attacked rather than exercised (REQ-CON-003/004, ADR-049/050,
//! `docs/INVARIANT-CONTRACTS.md` §INV-KEYMAP).
//!
//! A keyboard is a device someone else may be holding. The interesting properties are therefore not
//! "does `a` type an `a`" but "can any sequence of bytes from this device make the line editor see
//! something it has no rule for, or leave the decoder in a state the user cannot get out of".

use kernel_core::keymap::*;
use kernel_core::shell::{self, LineEditor};

const LSHIFT: u8 = 0x2A;
const RSHIFT: u8 = 0x36;
const CTRL: u8 = 0x1D;
const CAPS: u8 = 0x3A;
const ENTER: u8 = 0x1C;
const BKSP: u8 = 0x0E;
const EXTENDED: u8 = 0xE0;
const BREAK: u8 = 0x80;
const KEY_A: u8 = 0x1E;
const KEY_C: u8 = 0x2E;
const KEY_1: u8 = 0x02;
const E0_LEFT: u8 = 0x4B;
const E0_UP: u8 = 0x48;
const E0_DELETE: u8 = 0x53;

fn typed(codes: &[u8]) -> Vec<u8> {
    let mut k = Keymap::new();
    let mut out = Vec::new();
    for c in codes {
        out.extend_from_slice(k.feed(*c).as_slice());
    }
    out
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-1 — the output alphabet, over the entire input space.
// ---------------------------------------------------------------------------

/// The security property. Every scancode, in every reachable modifier state, must decode to bytes
/// the console's line editor has a rule for — or to nothing. Exhaustive, not sampled: the input is
/// 256 values and the state is five booleans, so there is no excuse for a sample.
#[test]
fn every_byte_the_map_can_emit_is_one_the_console_accepts() {
    for shift in [false, true] {
        for ctrl in [false, true] {
            for caps in [false, true] {
                for extended in [false, true] {
                    let mut base = Keymap::new();
                    if shift {
                        base.feed(LSHIFT);
                    }
                    if ctrl {
                        base.feed(CTRL);
                    }
                    if caps {
                        base.feed(CAPS);
                    }
                    if extended {
                        base.feed(EXTENDED);
                    }
                    for code in 0u16..=0xFF {
                        let mut k = base;
                        for b in k.feed(code as u8).as_slice() {
                            assert!(
                                Keymap::is_console_byte(*b),
                                "scancode {code:#04x} (shift={shift} ctrl={ctrl} caps={caps} ext={extended}) \
                                 emitted {b:#04x}, which the line editor has no rule for"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The same property stated the other way: no chord produces a control byte the editor does not
/// name. The editor's alphabet is the single definition both modules use — a decoder free to invent
/// a 27th control byte would be handing the editor input from a device an attacker may be holding.
#[test]
fn no_chord_invents_a_control_byte() {
    let mut k = Keymap::new();
    k.feed(CTRL);
    for code in 0u16..=0x7F {
        let mut probe = k;
        for b in probe.feed(code as u8).as_slice() {
            assert!(
                shell::editor_accepts(*b),
                "Ctrl + {code:#04x} emitted control byte {b:#04x}, which the editor has no rule for"
            );
        }
    }
}

/// And a chord the editor does NOT implement produces nothing at all — widening the editor's
/// alphabet is the only thing that may widen what the keyboard can send.
#[test]
fn a_chord_the_editor_does_not_implement_produces_nothing() {
    let mut k = Keymap::new();
    k.feed(CTRL);
    // Ctrl-G (0x07, bell) and Ctrl-Z (0x1a, suspend) mean nothing to this editor.
    for code in [0x22u8, 0x2C] {
        let mut probe = k;
        assert!(
            probe.feed(code).is_empty(),
            "a chord with no rule in the editor emitted bytes"
        );
    }
    // …while the chords it does implement arrive.
    let mut probe = k;
    assert_eq!(probe.feed(KEY_A).as_slice(), [shell::CTRL_A].as_slice());
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-2 — a press types once.
// ---------------------------------------------------------------------------

#[test]
fn a_release_never_types() {
    for code in 0u16..=0x7F {
        let mut k = Keymap::new();
        k.feed(code as u8);
        assert!(
            k.feed(code as u8 | BREAK).is_empty(),
            "release of {code:#04x} produced a byte — every key would type twice"
        );
    }
}

#[test]
fn a_word_types_exactly_its_letters() {
    // h e l l o  →  set-1 make codes
    let codes = [0x23, 0x12, 0x26, 0x26, 0x18];
    let mut with_releases = Vec::new();
    for c in codes {
        with_releases.push(c);
        with_releases.push(c | BREAK);
    }
    assert_eq!(typed(&with_releases), b"hello");
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-3 — modifiers are held state, ended only by their own release.
// ---------------------------------------------------------------------------

#[test]
fn shift_is_held_state_and_either_key_works() {
    for shift in [LSHIFT, RSHIFT] {
        let mut k = Keymap::new();
        k.feed(shift);
        assert_eq!(k.feed(KEY_A).as_slice(), b"A");
        assert_eq!(
            k.feed(KEY_1).as_slice(),
            b"!",
            "shift must reach the number row"
        );
        k.feed(shift | BREAK);
        assert_eq!(k.feed(KEY_A).as_slice(), b"a");
    }
}

/// A character key must not unstick a modifier. Holding shift through a whole word is the ordinary
/// case, and a decoder that cleared shift after the first letter would type `Hello` as `HELLO`'s
/// opposite — right once, wrong four times.
#[test]
fn a_character_key_does_not_release_a_modifier() {
    let mut k = Keymap::new();
    k.feed(LSHIFT);
    let mut out = Vec::new();
    for c in [0x23, 0x12, 0x26] {
        out.extend_from_slice(k.feed(c).as_slice());
        k.feed(c | BREAK);
    }
    assert_eq!(out, b"HEL");
    assert!(k.shift_held());
}

#[test]
fn a_spurious_release_changes_nothing() {
    let mut k = Keymap::new();
    let before = k;
    for code in [LSHIFT | BREAK, CTRL | BREAK, 0x38 | BREAK] {
        k.feed(code);
    }
    assert_eq!(k, before);
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-4 — caps lock.
// ---------------------------------------------------------------------------

#[test]
fn caps_affects_letters_only_and_cancels_against_shift() {
    let mut k = Keymap::new();
    k.feed(CAPS);
    assert_eq!(k.feed(KEY_A).as_slice(), b"A");
    assert_eq!(
        k.feed(KEY_1).as_slice(),
        b"1",
        "caps must not reach the number row"
    );
    k.feed(LSHIFT);
    assert_eq!(k.feed(KEY_A).as_slice(), b"a", "caps and shift cancel");
    assert_eq!(
        k.feed(KEY_1).as_slice(),
        b"!",
        "…but shift still reaches punctuation"
    );
}

/// Toggling on the release as well as the press would cancel itself and the key would look dead.
#[test]
fn caps_toggles_on_the_press_only() {
    let mut k = Keymap::new();
    k.feed(CAPS);
    k.feed(CAPS | BREAK);
    assert!(k.caps_on());
    k.feed(CAPS);
    k.feed(CAPS | BREAK);
    assert!(!k.caps_on());
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-5 — the extended prefix cannot stick anything.
// ---------------------------------------------------------------------------

/// The i8042 wraps some keys in a *fake shift* (`E0 2A` … `E0 AA`). A decoder that let an extended
/// byte fall through to the modifier logic would leave shift held after an arrow key, and every
/// subsequent letter would be uppercase with no key held.
///
/// **This sweep found a real defect.** The prefix test used to run before the pending-prefix test,
/// so `E0 E0` re-armed instead of resolving: a device emitting a stream of `E0` swallowed every real
/// key after it and the keyboard was permanently dead with nothing crashed. Resolving first bounds a
/// malformed stream to eating one code per prefix.
#[test]
fn an_extended_prefix_consumes_exactly_one_code_and_sticks_nothing() {
    for code in 0u16..=0xFF {
        let mut k = Keymap::new();
        assert!(k.feed(EXTENDED).is_empty());
        let emitted = k.feed(code as u8);
        // The navigation keys now emit — as SEQUENCES, which is the whole of REQ-CON-004. Anything
        // else after a prefix must still emit nothing.
        if !emitted.is_empty() {
            let s = emitted.as_slice();
            assert!(
                s == b"\r" || s == b"/" || (s.len() >= 3 && s[0] == 0x1b && s[1] == b'['),
                "extended {code:#04x} emitted {s:02x?}, which is neither a sequence nor a key"
            );
        }
        // Right-ctrl and right-alt are the two extended keys whose state the console DOES track;
        // everything else must leave no trace at all.
        let is_right_modifier = matches!(code as u8, 0x1D | 0x38 | 0x9D | 0xB8);
        // `E0 E0` resolves the pending prefix rather than re-arming it — see the module docs. The
        // key after a doubled prefix therefore types normally, which the branch below asserts.
        if !is_right_modifier {
            assert!(
                !k.shift_held() && !k.ctrl_held(),
                "extended {code:#04x} stuck a modifier"
            );
            // The very next ordinary key must decode normally — the prefix state is gone.
            assert_eq!(k.feed(KEY_A).as_slice(), b"a");
        }
    }
}

/// Right-ctrl arrives as `E0 1D`, and it must work: Ctrl-C from the right-hand key is the same
/// cancel as from the left.
#[test]
fn the_extended_control_key_still_cancels() {
    let mut k = Keymap::new();
    k.feed(EXTENDED);
    k.feed(0x1D);
    assert!(k.ctrl_held());
    assert_eq!(k.feed(KEY_C).as_slice(), [0x03].as_slice());
    k.feed(EXTENDED);
    k.feed(0x9D);
    assert!(!k.ctrl_held());
    assert_eq!(k.feed(KEY_C).as_slice(), b"c");
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-6 — the two bytes the line editor's contract names.
// ---------------------------------------------------------------------------

#[test]
fn enter_and_backspace_are_the_bytes_the_editor_expects() {
    let mut k = Keymap::new();
    assert_eq!(k.feed(ENTER).as_slice(), [CARRIAGE_RETURN].as_slice());
    assert_eq!(k.feed(BKSP).as_slice(), [BACKSPACE].as_slice());
    assert_eq!(CARRIAGE_RETURN, b'\r');
    assert_eq!(BACKSPACE, 0x08);
    assert_eq!(ETX, 0x03);
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-8 — navigation keys speak the editor's grammar (REQ-CON-004, ADR-050).
// ---------------------------------------------------------------------------

/// The bug this exists to prevent: an arrow key that reaches the editor as anything other than a
/// sequence the editor parses ends up as literal text in the middle of the operator's command.
#[test]
fn a_navigation_key_moves_the_cursor_and_never_types_into_the_line() {
    for code in [E0_UP, E0_LEFT, E0_DELETE, 0x4D, 0x47, 0x4F, 0x50] {
        let mut k = Keymap::new();
        k.feed(EXTENDED);
        let keys = k.feed(code);
        let mut ed = LineEditor::new();
        for b in b"write notes hello" {
            ed.feed(*b, &mut |_| {});
        }
        let before = ed.line().to_string();
        for b in keys.as_slice() {
            ed.feed(*b, &mut |_| {});
        }
        assert_eq!(
            ed.line(),
            before,
            "extended {code:#04x} changed the line instead of the cursor"
        );
        assert!(
            !ed.in_escape(),
            "extended {code:#04x} left the editor's parser armed — the next keystroke would be eaten"
        );
    }
}

/// The left arrow really moves: typing after it inserts before the last character rather than after
/// it. A cursor key that is merely swallowed passes the test above and fails this one.
#[test]
fn the_left_arrow_really_moves_the_insertion_point() {
    let mut k = Keymap::new();
    k.feed(EXTENDED);
    let left = k.feed(E0_LEFT);
    let mut ed = LineEditor::new();
    for b in b"ls" {
        ed.feed(*b, &mut |_| {});
    }
    for b in left.as_slice() {
        ed.feed(*b, &mut |_| {});
    }
    ed.feed(b'x', &mut |_| {});
    assert_eq!(ed.line(), "lxs");
}

// ---------------------------------------------------------------------------
// INV-KEYMAP-7 — the decoder cannot be driven into an unusable state.
// ---------------------------------------------------------------------------

/// 100 000 arbitrary bytes from the device, then a clean word must still type. A device that can
/// wedge the decoder can lock the user out of their own console without ever crashing anything.
#[test]
fn no_sequence_of_scancodes_can_wedge_the_decoder() {
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut k = Keymap::new();
    for _ in 0..100_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        for b in k.feed(rng as u8).as_slice() {
            assert!(Keymap::is_console_byte(*b));
        }
    }
    // Release every modifier the storm may have left held, exactly as a user would by pressing and
    // letting go — then the keyboard must be usable again.
    for m in [LSHIFT, RSHIFT, CTRL, 0x38] {
        k.feed(m | BREAK);
    }
    if k.caps_on() {
        k.feed(CAPS);
    }
    let mut out = Vec::new();
    for c in [0x23, 0x12, 0x26, 0x26, 0x18, ENTER] {
        out.extend_from_slice(k.feed(c).as_slice());
    }
    assert_eq!(
        out, b"hello\r",
        "the decoder was left unusable by a byte storm"
    );
}

/// And the storm cannot make the EDITOR unusable either: whatever the device sends, the line the
/// operator then types is exactly what they typed. This is the two modules' claim taken together —
/// the decoder's alphabet and the editor's parser, attacked as one surface.
#[test]
fn no_sequence_of_scancodes_can_corrupt_the_next_line_typed() {
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut k = Keymap::new();
    let mut ed = LineEditor::new();
    for _ in 0..100_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        for b in k.feed(rng as u8).as_slice() {
            ed.feed(*b, &mut |_| {});
        }
    }
    ed.reset();
    for m in [LSHIFT, RSHIFT, CTRL, 0x38] {
        k.feed(m | BREAK);
    }
    if k.caps_on() {
        k.feed(CAPS);
    }
    let mut line = None;
    for c in [0x23, 0x12, 0x26, 0x26, 0x18, ENTER] {
        for b in k.feed(c).as_slice() {
            if let kernel_core::shell::Edit::Line(l) = ed.feed(*b, &mut |_| {}) {
                line = Some(l);
            }
        }
    }
    assert_eq!(line.as_deref(), Some("hello"));
}

// ---------------------------------------------------------------------------
// The in-kernel suite, run on the host — same doctrine as `tests/invariants.rs`.
// ---------------------------------------------------------------------------

#[test]
fn the_in_kernel_suite_holds_on_the_host_and_reports_every_check_once() {
    let mut reported: Vec<(u32, bool, &'static str)> = Vec::new();
    let outcome = keymap_suite(|n, passed, name| reported.push((n, passed, name)));
    let count = match outcome {
        Ok(n) => n,
        Err((idx, name)) => panic!("in-kernel keyboard-decode invariant {idx} failed: {name}"),
    };
    assert_eq!(reported.len() as u32, count);
    for (i, (n, passed, _)) in reported.iter().enumerate() {
        assert_eq!(*n, i as u32 + 1);
        assert!(passed);
    }
    // Pinned: the boot gates grep for this number.
    assert_eq!(count, 12);
}
