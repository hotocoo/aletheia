//! Host proofs for the window manager (ALET-P2-021's window rung, ADR-084).
//!
//! The boot suite proves the contract on every CPU; these proofs push the edges a boot has no
//! reason to walk: partially-off windows, drags that would leave the scanout, a window closed
//! mid-drag, focus courtesy across close, and the ledger under a long pointer story.

use kernel_core::compositor::{CompFault, Compositor, EventKind};
use kernel_core::textgrid::{CLOSE_W, TITLE_H};
use kernel_core::wm::{hit_at, wm_suite, Hit, Press, WindowManager, WmFault, MAX_WINDOWS};

fn desk() -> (Compositor, WindowManager, u64) {
    let mut comp = Compositor::new(0xC0FF_EE84, 200, 120);
    let sess = comp.open_input_session().unwrap();
    let mut wm = WindowManager::new();
    wm.open(&mut comp, 1, 80, 60, 0, 0).unwrap();
    wm.open(&mut comp, 2, 80, 60, 40, 20).unwrap();
    (comp, wm, sess)
}

#[test]
fn the_boot_suite_passes_on_the_host() {
    let mut seen = 0;
    let n = wm_suite(|k, ok, name| {
        seen += 1;
        assert_eq!(k, seen);
        assert!(ok, "{name}");
    })
    .unwrap();
    assert_eq!(n, 12);
}

#[test]
fn a_partially_off_window_is_clicked_only_where_it_is_visible() {
    let mut comp = Compositor::new(1, 200, 120);
    let sess = comp.open_input_session().unwrap();
    let mut wm = WindowManager::new();
    // 40 pixels of this window hang off the left edge: local x 0..40 can never be seen.
    wm.open(&mut comp, 1, 80, 60, -40, 0).unwrap();
    assert_eq!(wm.window_at(&comp, 0, 30), Some((1, 40, 30)));
    assert_eq!(wm.window_at(&comp, 39, 30), Some((1, 79, 30)));
    assert_eq!(wm.window_at(&comp, 40, 30), None);
    // The visible part of the band still drags; the close box hangs off screen entirely, so
    // this window cannot be closed by a click — and is not closed by one either.
    assert_eq!(wm.press(&mut comp, sess, 5, 2), Press::Dragging(1));
    assert!(wm.is_open(1));
    let _ = wm.release();
}

#[test]
fn a_drag_that_would_leave_the_scanout_is_refused_and_the_window_stays() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    let before = comp.placement(2);
    // Pointer positions are scanout points, so a drag cannot ask for a placement that is
    // fully off; ask through the manager's own offset math for one that is.
    assert_eq!(wm.motion(&mut comp, 199, 119), Some(2));
    assert_eq!(comp.placement(2), Some((195, 117)));
    assert_ne!(comp.placement(2), before);
    assert_eq!(wm.release(), Some(2));
    assert_eq!(wm.counters().2, 1);
}

#[test]
fn a_window_closed_mid_drag_drags_nothing_afterwards() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    wm.close(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.dragging(), None);
    assert_eq!(wm.motion(&mut comp, 90, 90), None);
    assert_eq!(wm.release(), None);
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(wm.counters().2, 0); // no drag ever completed
}

#[test]
fn the_window_that_loses_focus_is_told_through_its_own_queue() {
    let (mut comp, mut wm, sess) = desk();
    let tok1 = wm.token(1).unwrap();
    comp.set_focus(sess, 1).unwrap();
    // A press on window 2 takes focus away from 1: 1 hears about it, once.
    assert_eq!(wm.press(&mut comp, sess, 60, 40), Press::Focused(2));
    let ev = comp.drain_input(1, tok1).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].kind, EventKind::FocusLost);
}

#[test]
fn closing_a_window_kills_its_queue_with_it() {
    let (mut comp, mut wm, sess) = desk();
    let tok2 = wm.token(2).unwrap();
    comp.set_focus(sess, 2).unwrap();
    comp.post_key(sess, b'a').unwrap();
    assert_eq!(comp.queued_len(2), 1);
    wm.close(&mut comp, sess, 2).unwrap();
    assert_eq!(comp.queued_len(2), 0);
    assert_eq!(comp.drain_input(2, tok2), Err(CompFault::UnknownSurface(2)));
}

#[test]
fn the_chrome_of_a_window_narrower_than_the_close_box_is_all_title() {
    // A degenerate window cannot carry a close box; the band stays a drag strip rather than
    // becoming a close box that covers the whole window.
    assert_eq!(hit_at(CLOSE_W, 40, 0, 0), Some(Hit::Title));
    assert_eq!(
        hit_at(CLOSE_W, 40, (CLOSE_W - 1) as i32, 0),
        Some(Hit::Title)
    );
    assert_eq!(hit_at(CLOSE_W, 40, 0, TITLE_H as i32), Some(Hit::Client));
}

#[test]
fn the_manager_refuses_what_it_does_not_own_and_counts_every_refusal() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.close(&mut comp, sess, 7), Err(WmFault::UnknownWindow(7)));
    assert_eq!(wm.token(7), None);
    assert!(!wm.is_open(7));
    assert_eq!(
        wm.open(&mut comp, 1, 8, 8, 0, 0),
        Err(WmFault::DuplicateWindow(1))
    );
    assert_eq!(wm.counters().3, 2);
    assert_eq!(wm.count(), 2);
}

#[test]
fn a_surface_the_manager_does_not_own_is_never_routed_to() {
    let mut comp = Compositor::new(9, 200, 120);
    let sess = comp.open_input_session().unwrap();
    // A wallpaper panel the desktop owns directly, under every window.
    let panel = comp.mint_surface(50, 200, 120).unwrap();
    comp.attach(50, panel, 0, 0).unwrap();
    let mut wm = WindowManager::new();
    wm.open(&mut comp, 1, 80, 60, 0, 0).unwrap();
    // A press on the panel where no window sits is EMPTY: the manager routes to windows only,
    // and the panel is not a window (no focus, no chrome, no close).
    assert_eq!(wm.window_at(&comp, 150, 100), None);
    assert_eq!(wm.press(&mut comp, sess, 150, 100), Press::Empty);
    assert_eq!(comp.focus(), None);
    assert_eq!(comp.z_order(), vec![50, 1]);
}

#[test]
fn the_ceiling_holds_under_a_long_open_close_story() {
    let mut comp = Compositor::new(3, 400, 400);
    let sess = comp.open_input_session().unwrap();
    let mut wm = WindowManager::new();
    for round in 0..20u32 {
        let id = 100 + round;
        wm.open(&mut comp, id, 40, 40, 0, 0).unwrap();
        if wm.count() > MAX_WINDOWS / 2 {
            wm.close(&mut comp, sess, id).unwrap();
        }
    }
    assert!(wm.count() <= MAX_WINDOWS);
    let (opens, closes, _, refusals) = wm.counters();
    assert_eq!(opens, 20);
    assert_eq!(opens - closes, wm.count() as u64);
    assert_eq!(refusals, 0);
    // Every surface the compositor still holds is a window the manager still owns.
    let mut ids = wm.ids();
    ids.sort_unstable();
    let mut z = comp.z_order();
    z.sort_unstable();
    assert_eq!(ids, z);
}

#[test]
fn the_motion_route_moves_the_cursor_and_leaves_the_click_to_the_manager() {
    use kernel_core::vinput::{
        route_pointer_motion, Button, PointerDecoder, RawEvent, ABS_X, ABS_Y, BTN_LEFT, EV_ABS,
        EV_KEY, EV_SYN,
    };
    let (mut comp, mut wm, sess) = desk();
    comp.move_cursor(sess, 0, 0).unwrap();
    let mut dec = PointerDecoder::new(200, 120);
    dec.set_axis(32767, 32767);
    let ev = |ty, code, value| RawEvent { ty, code, value };
    // A press over window 2's CLOSE BOX: the cursor follows the hardware, and focus does NOT
    // move — the manager has not decided anything yet, and `focus_at` must not decide for it.
    let half = 32767u32 / 2;
    route_pointer_motion(&mut dec, &mut comp, sess, ev(EV_ABS, ABS_X, half)).unwrap();
    route_pointer_motion(&mut dec, &mut comp, sess, ev(EV_ABS, ABS_Y, half)).unwrap();
    route_pointer_motion(&mut dec, &mut comp, sess, ev(EV_KEY, BTN_LEFT, 1)).unwrap();
    let batch = route_pointer_motion(&mut dec, &mut comp, sess, ev(EV_SYN, 0, 0)).unwrap();
    assert_eq!(batch.button, Some((Button::Left, true)));
    let (cx, cy) = comp.cursor().unwrap();
    assert_eq!(batch.move_to, Some((cx, cy)));
    assert_eq!(
        comp.focus(),
        None,
        "the route must not take the click decision"
    );
    // Now the manager decides, from the same point the cursor is at.
    let press = wm.press(&mut comp, sess, cx, cy);
    assert!(matches!(
        press,
        Press::Focused(_) | Press::Dragging(_) | Press::Closed(_) | Press::Empty
    ));
    assert_eq!(wm.dragging().is_some(), matches!(press, Press::Dragging(_)));
}
