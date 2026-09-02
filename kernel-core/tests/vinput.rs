//! Host proofs for the input-HARDWARE rung (ALET-P2-021's device rung, ADR-080).
//!
//! The driver's device leg (config-space identity, DMA-gated queues, armed silence) runs
//! against the REAL virtio-input devices in the boot gate on all three targets. What the
//! host can prove EXHAUSTIVELY is everything upstream of the wire: the keyboard decoder's
//! alphabet property over the ENTIRE keycode space in every reachable modifier state, the
//! US-layout expectations, the pointer mapping's exactness and clamps, and the routing path
//! — decode→post→drain, click→focus, refusals by name — against the real compositor model.

use kernel_core::compositor::{CompFault, Compositor, EventKind, MAX_INPUT_EVENTS};
use kernel_core::keymap::Keymap;
use kernel_core::vinput::{
    route_key, route_pointer, Button, KeyDecoder, PointerBatch, PointerDecoder, RawEvent, ABS_X,
    ABS_Y, BTN_LEFT, BTN_RIGHT, EV_ABS, EV_KEY, EV_SYN,
};

fn ev(ty: u16, code: u16, value: u32) -> RawEvent {
    RawEvent { ty, code, value }
}

const W: u32 = 640;
const H: u32 = 240;
const AXIS: u32 = 4095; // the PINNED measured QEMU tablet range (see vinput_suite)

/// A machine path like the desktop's: compositor, ONE session, panel + window, focused.
struct Machine {
    c: Compositor,
    s: u64,
    tok_panel: u64,
    tok_win: u64,
}

impl Machine {
    fn new() -> Self {
        let mut c = Compositor::new(0x0A80_10CA, W, H);
        let s = c.open_input_session().unwrap();
        let tok_panel = c.mint_surface(1, 400, 200).unwrap();
        let tok_win = c.mint_surface(2, 200, 80).unwrap();
        c.attach(1, tok_panel, 0, 0).unwrap();
        c.attach(2, tok_win, 300, 60).unwrap();
        c.set_focus(s, 2).unwrap();
        Machine {
            c,
            s,
            tok_panel,
            tok_win,
        }
    }
}

// ---------------------------------------------------------------------------
// The keyboard decoder.
// ---------------------------------------------------------------------------

/// The security property, over the WHOLE u16 keycode space (not a sample): no record a
/// virtio keyboard can deliver — press, release or autorepeat, in any reachable modifier
/// state — emits a byte the console's line editor has no rule for. This is the cross-module
/// claim ADR-049 proved for the PS/2 wire, re-proved on this one.
#[test]
fn the_full_keycode_space_never_leaves_the_editors_alphabet() {
    for shift in [false, true] {
        for ctrl in [false, true] {
            for caps in [false, true] {
                let seed = {
                    let mut d = KeyDecoder::new();
                    if shift {
                        d.feed(ev(EV_KEY, 42, 1));
                    }
                    if ctrl {
                        d.feed(ev(EV_KEY, 29, 1));
                    }
                    if caps {
                        d.feed(ev(EV_KEY, 58, 1));
                    }
                    d
                };
                for code in 0u32..=0xFFFF {
                    for value in [1u32, 2, 0] {
                        let mut probe = seed;
                        let keys = probe.feed(ev(EV_KEY, code as u16, value));
                        for b in keys.as_slice() {
                            assert!(
                                Keymap::is_console_byte(*b),
                                "keycode {} value {} in (shift={}, ctrl={}, caps={}) emitted {:?}",
                                code,
                                value,
                                shift,
                                ctrl,
                                caps,
                                b
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The US layout lands on the SAME characters the PS/2 decoder types: the table is a
/// different index space (Linux keycodes, not set-1 scancodes) with the same answers.
#[test]
fn the_us_layout_lands_on_the_same_keys_the_ps2_decoder_types() {
    let mut d = KeyDecoder::new();
    let type_one = |d: &mut KeyDecoder, code: u16| -> Vec<u8> {
        let down = d.feed(ev(EV_KEY, code, 1));
        let up = d.feed(ev(EV_KEY, code, 0));
        assert!(up.is_empty(), "a release must type nothing");
        down.as_slice().to_vec()
    };
    assert_eq!(type_one(&mut d, 30), b"a");
    assert_eq!(type_one(&mut d, 44), b"z");
    assert_eq!(type_one(&mut d, 2), b"1");
    assert_eq!(type_one(&mut d, 11), b"0");
    assert_eq!(type_one(&mut d, 12), b"-");
    assert_eq!(type_one(&mut d, 39), b";");
    assert_eq!(type_one(&mut d, 43), b"\\");
    assert_eq!(type_one(&mut d, 57), b" ");
    assert_eq!(type_one(&mut d, 55), b"*");
    assert_eq!(type_one(&mut d, 71), b"7"); // keypad, unshifted
    assert_eq!(type_one(&mut d, 98), b"/"); // keypad slash, by name

    // Shift reaches the punctuation row, which an uppercase transform would not.
    d.feed(ev(EV_KEY, 42, 1));
    assert_eq!(type_one(&mut d, 2), b"!");
    assert_eq!(type_one(&mut d, 12), b"_");
    assert_eq!(type_one(&mut d, 41), b"~");
    assert_eq!(type_one(&mut d, 30), b"A");
    d.feed(ev(EV_KEY, 42, 0));
    assert_eq!(type_one(&mut d, 30), b"a");

    // The editor's grammar for the non-character keys, identical to the PS/2 wire's.
    assert_eq!(type_one(&mut d, 28), b"\r");
    assert_eq!(type_one(&mut d, 96), b"\r"); // keypad enter
    assert_eq!(type_one(&mut d, 14), [0x08]);
    assert_eq!(type_one(&mut d, 15), [b'\t']);
    assert_eq!(type_one(&mut d, 103), b"\x1b[A");
    assert_eq!(type_one(&mut d, 108), b"\x1b[B");
    assert_eq!(type_one(&mut d, 106), b"\x1b[C");
    assert_eq!(type_one(&mut d, 105), b"\x1b[D");
    assert_eq!(type_one(&mut d, 102), b"\x1b[H");
    assert_eq!(type_one(&mut d, 107), b"\x1b[F");
    assert_eq!(type_one(&mut d, 111), b"\x1b[3~");

    // Dropped on purpose, exactly as the PS/2 decoder drops its ESC: a lone ESC would leave
    // the editor's parser armed for a byte a person pressing Escape never sends.
    assert!(type_one(&mut d, 1).is_empty());
    // Unmapped keys contribute nothing.
    assert!(type_one(&mut d, 59).is_empty()); // F1
    assert!(type_one(&mut d, 125).is_empty()); // left meta
    assert!(type_one(&mut d, 0xFFFF).is_empty()); // not a real keycode at all
}

/// Modifiers are HELD state a device controls, cleared only by their own release; caps
/// toggles on the press only; and a Ctrl chord is delivered only when the editor has a rule
/// for the resulting byte.
#[test]
fn modifiers_are_held_state_and_ctrl_chords_obey_the_editor() {
    let mut d = KeyDecoder::new();
    // Shift ends at its own release (both shift keys drive the one held bit, exactly as the
    // PS/2 decoder rules), and an unrelated key's release does not unstick it.
    d.feed(ev(EV_KEY, 42, 1));
    d.feed(ev(EV_KEY, 30, 1)); // shift+a -> A
    d.feed(ev(EV_KEY, 30, 0)); // 'a' release: not a modifier, shift stays
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), b"A");
    d.feed(ev(EV_KEY, 42, 0));
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), b"a");

    // Caps toggles on the press only; shift cancels it on letters but not on digits.
    d.feed(ev(EV_KEY, 58, 1));
    d.feed(ev(EV_KEY, 58, 0));
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), b"A");
    assert_eq!(d.feed(ev(EV_KEY, 2, 1)).as_slice(), b"1");
    d.feed(ev(EV_KEY, 42, 1));
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), b"a");
    d.feed(ev(EV_KEY, 42, 0));
    // Caps was toggled ON above and stays on until ITS key toggles it off — state a device
    // controls is not cleared by anything else.
    d.feed(ev(EV_KEY, 58, 1));
    d.feed(ev(EV_KEY, 58, 0));

    // Ctrl chords: the editing set is delivered, everything else invents nothing.
    d.feed(ev(EV_KEY, 29, 1));
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), [0x01]); // Ctrl-A, a rule the editor has
    d.feed(ev(EV_KEY, 30, 0));
    assert!(d.feed(ev(EV_KEY, 34, 1)).is_empty()); // Ctrl-G: no rule, no byte
    d.feed(ev(EV_KEY, 34, 0));
    d.feed(ev(EV_KEY, 29, 0));
    assert_eq!(d.feed(ev(EV_KEY, 30, 1)).as_slice(), b"a");
}

// ---------------------------------------------------------------------------
// The pointer decoder.
// ---------------------------------------------------------------------------

/// The axis mapping is exact at its edges, clamps out-of-range samples INSIDE, is monotone
/// across the whole axis, and fails closed for a device that never declared its range.
#[test]
fn pointer_mapping_is_exact_monotone_and_fail_closed() {
    let mut d = PointerDecoder::new(W, H);
    d.set_axis(AXIS, AXIS);
    assert_eq!(d.map(0, AXIS, W), 0);
    assert_eq!(d.map(AXIS, AXIS, W), W - 1);
    assert_eq!(d.map(AXIS + 7, AXIS, H), H - 1); // out of range clamps inside
    let mut prev = 0u32;
    for v in 0..=AXIS {
        let m = d.map(v, AXIS, W);
        assert!(
            m >= prev,
            "mapping must be monotone: {} -> {} < {}",
            v,
            m,
            prev
        );
        assert!(m < W);
        prev = m;
    }
    // An undeclared range maps everything to the edge, never somewhere undefined.
    let undeclared = PointerDecoder::new(W, H);
    assert_eq!(undeclared.map(1234, 0, W), 0);
}

/// Batches commit on EV_SYN: a half batch (one axis without the other) is refused by name
/// and contributes no position, an unknown axis is counted, and a button autorepeat never
/// reaches the routing as a click.
#[test]
fn pointer_batches_commit_on_syn_and_refuse_half_batches() {
    let mut d = PointerDecoder::new(W, H);
    d.set_axis(AXIS, AXIS);
    assert_eq!(d.feed(ev(EV_ABS, ABS_X, 1000)), PointerBatch::default());
    assert_eq!(
        d.feed(ev(EV_SYN, 0, 0)),
        PointerBatch {
            move_to: None,
            button: None
        },
        "nothing was committed — but the half batch is counted"
    );
    assert_eq!(d.unknown_refusals(), 1);
    assert_eq!(d.feed(ev(EV_ABS, 55, 7)), PointerBatch::default());
    assert_eq!(d.unknown_refusals(), 2);

    // A real batch: both axes plus a click, committed together as ONE decision.
    let _ = d.feed(ev(EV_ABS, ABS_X, 2560));
    let _ = d.feed(ev(EV_ABS, ABS_Y, 1707));
    let _ = d.feed(ev(EV_KEY, BTN_LEFT, 1));
    let batch = d.feed(ev(EV_SYN, 0, 0));
    assert_eq!(
        batch,
        PointerBatch {
            move_to: Some((d.map(2560, AXIS, W), d.map(1707, AXIS, H))),
            button: Some((Button::Left, true)),
        }
    );
    // The pending state is CONSUMED by the commit: a second SYN commits nothing.
    assert_eq!(d.feed(ev(EV_SYN, 0, 0)), PointerBatch::default());
    // A button autorepeat is named and ignored.
    let _ = d.feed(ev(EV_KEY, BTN_LEFT, 2));
    let _ = d.feed(ev(EV_KEY, BTN_RIGHT, 2));
    let batch = d.feed(ev(EV_SYN, 0, 0));
    assert_eq!(batch, PointerBatch::default());
    assert_eq!(d.unknown_refusals(), 4);
}

// ---------------------------------------------------------------------------
// The routing path: decode -> post -> drain, click -> focus.
// ---------------------------------------------------------------------------

/// Keystrokes route to the focused surface and nowhere else; a keystroke with nothing
/// focused is refused by name and exists nowhere; a backlogged surface refuses and counts.
#[test]
fn route_key_delivers_to_the_focus_and_propagates_its_refusals() {
    let m = Machine::new();
    let Machine {
        mut c,
        s,
        tok_win,
        tok_panel,
    } = m;
    let mut d = KeyDecoder::new();
    // Focused: 'h','i' land in the window's queue in order.
    assert_eq!(route_key(&mut d, &mut c, s, ev(EV_KEY, 35, 1)), Ok(1)); // h
    assert_eq!(route_key(&mut d, &mut c, s, ev(EV_KEY, 23, 1)), Ok(1)); // i
    let drained = c.drain_input(2, tok_win).unwrap();
    assert_eq!(
        drained
            .iter()
            .map(|e| match e.kind {
                EventKind::Key(b) => b,
                _ => b'?',
            })
            .collect::<Vec<u8>>(),
        b"hi"
    );
    // Not the panel's: the input path decides WHERE events go.
    assert!(c.drain_input(1, tok_panel).unwrap().is_empty());

    // Nothing focused: refused NoFocus, the event exists NOWHERE. The clear itself is the
    // only thing the window's queue holds — the FocusLost notice, never the keystroke.
    c.clear_focus(s).unwrap();
    assert_eq!(
        route_key(&mut d, &mut c, s, ev(EV_KEY, 35, 1)),
        Err(CompFault::NoFocus)
    );
    let told = c.drain_input(2, tok_win).unwrap();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].kind, EventKind::FocusLost);

    // Backlogged: the bounded queue refuses and counts, never evicts.
    c.set_focus(s, 2).unwrap();
    for _ in 0..MAX_INPUT_EVENTS {
        assert_eq!(route_key(&mut d, &mut c, s, ev(EV_KEY, 35, 1)), Ok(1));
    }
    assert_eq!(
        route_key(&mut d, &mut c, s, ev(EV_KEY, 35, 1)),
        Err(CompFault::Backlogged { surface: 2 })
    );
    let (dropped, _) = c.input_counters();
    assert_eq!(dropped, 1);
    assert_eq!(c.drain_input(2, tok_win).unwrap().len(), MAX_INPUT_EVENTS);
}

/// Clicks route through the pointer's own batches: a left press at the cursor focuses the
/// topmost surface under the point, a click on empty space clears focus (the loser told),
/// a right press routes nothing (there is no context menu to open), and autorepeat never
/// machine-guns the focus.
#[test]
fn route_pointer_moves_the_cursor_and_clicks_decide_focus() {
    let m = Machine::new();
    let Machine {
        mut c,
        s,
        tok_win,
        tok_panel,
    } = m;
    let mut d = PointerDecoder::new(W, H);
    d.set_axis(AXIS, AXIS);
    // Move over the window (300..500 x 60..140): sample (2560, 1707).
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_X, 2560)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_Y, 1707)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(
        c.cursor(),
        Some((d.map(2560, AXIS, W), d.map(1707, AXIS, H)))
    );
    // The window already holds focus: the click is idempotent, nothing queued.
    route_pointer(&mut d, &mut c, s, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(c.focus(), Some(2));
    assert!(c.drain_input(2, tok_win).unwrap().is_empty());

    // A click over the GAP (600, 199) clears focus, and the window is told.
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_X, 3840)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_Y, 3413)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(c.focus(), None);
    assert!(c
        .drain_input(2, tok_win)
        .unwrap()
        .iter()
        .any(|e| e.kind == EventKind::FocusLost));

    // Clicking the PANEL now focuses it, and a keystroke follows the click: the input path
    // decided where the event goes, and the panel's OWNER reads it.
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_X, 500)).unwrap(); // (78, ..)
    route_pointer(&mut d, &mut c, s, ev(EV_ABS, ABS_Y, 300)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(c.focus(), Some(1));
    let mut k = KeyDecoder::new();
    route_key(&mut k, &mut c, s, ev(EV_KEY, 36, 1)).unwrap(); // 'j'
    let told = c.drain_input(1, tok_panel).unwrap();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].kind, EventKind::Key(b'j'));
}

/// The owner/reader split holds through the click path: the click ROUTES, the OWNER reads,
/// and a wrong owner token is the same refusal a forged draw token is.
#[test]
fn clicks_route_but_only_the_owner_reads() {
    let mut c = Compositor::new(0x0A80_10CB, W, H);
    let s = c.open_input_session().unwrap();
    let t = c.mint_surface(5, 100, 100).unwrap();
    c.attach(5, t, 0, 0).unwrap();
    let mut d = PointerDecoder::new(W, H);
    d.set_axis(AXIS, AXIS);
    let _ = d.feed(ev(EV_ABS, ABS_X, 100));
    let _ = d.feed(ev(EV_ABS, ABS_Y, 100));
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
    route_pointer(&mut d, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(c.focus(), Some(5));
    let mut k = KeyDecoder::new();
    route_key(&mut k, &mut c, s, ev(EV_KEY, 38, 1)).unwrap(); // 'l'
                                                              // The session token cannot drain; the owner token can.
    assert!(matches!(
        c.drain_input(5, s),
        Err(CompFault::NotOwner { surface: 5 })
    ));
    let drained = c.drain_input(5, t).unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind, EventKind::Key(b'l'));
}

/// Unknown records are counted by the decoders and change nothing about the session.
#[test]
fn unknown_records_are_counted_and_change_nothing() {
    let mut c = Compositor::new(0x0A80_10CC, W, H);
    let s = c.open_input_session().unwrap();
    let t = c.mint_surface(5, 100, 100).unwrap();
    c.attach(5, t, 0, 0).unwrap();
    c.set_focus(s, 5).unwrap();
    let mut k = KeyDecoder::new();
    let mut p = PointerDecoder::new(W, H);
    p.set_axis(AXIS, AXIS);
    let (focus0, cursor0) = (c.focus(), c.cursor());
    let _ = route_key(&mut k, &mut c, s, ev(99, 0, 1)); // unknown type
    let _ = route_key(&mut k, &mut c, s, ev(EV_KEY, 30, 9)); // impossible value
    let _ = route_pointer(&mut p, &mut c, s, ev(EV_ABS, 77, 1)); // unknown axis
    let _ = route_pointer(&mut p, &mut c, s, ev(55, 0, 0)); // unknown type
    assert_eq!(k.unknown_refusals(), 2);
    assert_eq!(p.unknown_refusals(), 2);
    assert_eq!(c.focus(), focus0);
    assert_eq!(c.cursor(), cursor0);
    assert!(c.drain_input(5, t).unwrap().is_empty());
}

/// Identical event streams land bit-identical: the whole decode→route→compose path is a
/// deterministic function of its inputs.
#[test]
fn identical_event_sequences_land_bit_identical() {
    let run = || {
        let m = Machine::new();
        let Machine {
            mut c,
            s,
            tok_win: _,
            tok_panel: _,
        } = m;
        let mut k = KeyDecoder::new();
        let mut p = PointerDecoder::new(W, H);
        p.set_axis(AXIS, AXIS);
        for (x, y) in [(100u32, 200u32), (2560, 1707), (3840, 3413), (2560, 1707)] {
            route_pointer(&mut p, &mut c, s, ev(EV_ABS, ABS_X, x)).unwrap();
            route_pointer(&mut p, &mut c, s, ev(EV_ABS, ABS_Y, y)).unwrap();
            route_pointer(&mut p, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
        }
        route_pointer(&mut p, &mut c, s, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
        route_pointer(&mut p, &mut c, s, ev(EV_SYN, 0, 0)).unwrap();
        for code in [30u16, 31, 32, 28] {
            route_key(&mut k, &mut c, s, ev(EV_KEY, code, 1)).unwrap();
            route_key(&mut k, &mut c, s, ev(EV_KEY, code, 0)).unwrap();
        }
        let mut bits = vec![false; (W * H) as usize];
        struct Shadow<'a>(&'a mut Vec<bool>);
        impl kernel_core::compositor::Raster for Shadow<'_> {
            fn put(&mut self, x: u32, y: u32, ink: bool) {
                self.0[(y as usize) * (W as usize) + x as usize] = ink;
            }
        }
        let st = c.compose_frame(&mut Shadow(&mut bits));
        (
            bits,
            st,
            c.focus(),
            c.cursor(),
            c.input_counters(),
            k.unknown_refusals(),
            p.unknown_refusals(),
        )
    };
    let a = run();
    let b = run();
    assert_eq!(a, b);
}
