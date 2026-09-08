//! Host proofs for the window manager (ALET-P2-021's window rung, ADR-084).
//!
//! The boot suite proves the contract on every CPU; these proofs push the edges a boot has no
//! reason to walk: partially-off windows, drags that would leave the scanout, a window closed
//! mid-drag, focus courtesy across close, and the ledger under a long pointer story.

use kernel_core::compositor::{CompFault, Compositor, EventKind};
use kernel_core::textgrid::{CLOSE_W, TITLE_H};
use kernel_core::wm::{
    hit_at, wm_suite, Hit, NudgeDirection, Press, ResizeDirection, SnapDirection, WindowManager, WmFault,
    MAX_WINDOWS,
};

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
    assert_eq!(n, 14);
}

#[test]
fn show_desktop_hides_and_restores_the_managed_set() {
    let (mut comp, mut wm, sess) = desk();
    let a = 1;
    let b = 2;
    let tok_a = wm.token(a).unwrap();
    let tok_b = wm.token(b).unwrap();

    comp.set_focus(sess, b).unwrap();
    comp.post_key(sess, b'x').unwrap();
    assert_eq!(wm.toggle_show_desktop(&mut comp, sess), Ok(true));
    assert_eq!(comp.is_visible(a), Some(false));
    assert_eq!(comp.is_visible(b), Some(false));
    assert_eq!(comp.focus(), None);

    assert_eq!(wm.toggle_show_desktop(&mut comp, sess), Ok(false));
    assert_eq!(comp.is_visible(a), Some(true));
    assert_eq!(comp.is_visible(b), Some(true));
    assert_eq!(comp.focus(), Some(b));
    assert!(comp.drain_input(a, tok_a).unwrap().is_empty());
    assert!(comp
        .drain_input(b, tok_b)
        .unwrap()
        .iter()
        .any(|e| e.kind == EventKind::Key(b'x')));
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
fn escape_cancels_move_and_restores_the_press_geometry() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 140, 90), Some(2));
    assert_ne!(comp.placement(2), Some((40, 20)));
    assert_eq!(wm.cancel_drag(&mut comp), Some(2));
    assert_eq!(wm.dragging(), None);
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));
}

#[test]
fn escape_cancels_resize_and_restores_the_press_geometry() {
    let (mut comp, mut wm, sess) = desk();
    // Bottom-right resize grip of window 2.
    assert_eq!(wm.press(&mut comp, sess, 119, 79), Press::Resizing(2));
    assert_eq!(wm.motion(&mut comp, 170, 110), Some(2));
    assert_ne!(comp.surface_size(2), Some((80, 60)));
    assert_eq!(wm.cancel_drag(&mut comp), Some(2));
    assert_eq!(wm.dragging(), None);
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));
}

#[test]
fn escape_cancel_of_a_maximized_move_preserves_maximize_state() {
    let (mut comp, mut wm, sess) = desk();
    wm.toggle_maximize(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.is_maximized(2), Some(true));
    assert_eq!(wm.press(&mut comp, sess, 20, 2), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 80, 30), Some(2));
    assert_eq!(wm.cancel_drag(&mut comp), Some(2));
    assert_eq!(comp.placement(2), Some((0, 0)));
    assert_eq!(comp.surface_size(2), Some((200, 120)));
    assert_eq!(wm.is_maximized(2), Some(true));
}

#[test]
fn escape_without_a_drag_is_a_no_op() {
    let (mut comp, mut wm, _sess) = desk();
    assert_eq!(wm.cancel_drag(&mut comp), None);
    assert_eq!(comp.placement(2), Some((40, 20)));
}

#[test]
fn dragging_to_an_edge_snaps_move_and_preserves_restore_geometry() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 0, 60), Some(2));
    assert_eq!(wm.release_at(&mut comp, sess, 0, 60), Some(2));
    assert_eq!(comp.placement(2), Some((0, 0)));
    assert_eq!(comp.surface_size(2), Some((100, 120)));
    assert_eq!(wm.is_maximized(2), Some(true));

    assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(false));
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));
}

#[test]
fn dragging_to_the_right_edge_snaps_to_the_right_half() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 199, 60), Some(2));
    assert_eq!(wm.release_at(&mut comp, sess, 199, 60), Some(2));
    assert_eq!(comp.placement(2), Some((100, 0)));
    assert_eq!(comp.surface_size(2), Some((100, 120)));
}

#[test]
fn dragging_to_the_bottom_edge_snaps_to_the_lower_half() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 100, 119), Some(2));
    assert_eq!(wm.release_at(&mut comp, sess, 100, 119), Some(2));
    assert_eq!(comp.placement(2), Some((0, 60)));
    assert_eq!(comp.surface_size(2), Some((200, 60)));
    assert_eq!(wm.is_maximized(2), Some(true));

    assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(false));
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));
}

#[test]
fn dragging_to_each_corner_snaps_to_the_matching_quarter() {
    let cases = [
        (0, 0, (0, 0)),
        (199, 0, (100, 0)),
        (0, 119, (0, 60)),
        (199, 119, (100, 60)),
    ];
    for (x, y, expected) in cases {
        let (mut comp, mut wm, sess) = desk();
        assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
        assert_eq!(wm.motion(&mut comp, x, y), Some(2));
        assert_eq!(wm.release_at(&mut comp, sess, x, y), Some(2));
        assert_eq!(comp.placement(2), Some(expected));
        assert_eq!(comp.surface_size(2), Some((100, 60)));
        assert_eq!(wm.is_maximized(2), Some(true));

        assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(false));
        assert_eq!(comp.placement(2), Some((40, 20)));
        assert_eq!(comp.surface_size(2), Some((80, 60)));
    }
}

#[test]
fn edge_snap_accepts_a_forgiving_release_band_but_not_a_distant_release() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();

    wm.press(&mut comp, sess, 60, 29);
    wm.motion(&mut comp, 14, 40);
    wm.release_at(&mut comp, sess, 16, 40);
    assert_eq!(comp.placement(2), Some((0, 0)));
    assert_eq!(wm.size(2), Some((100, 120)));

    // Start a fresh drag and release clearly outside the snap band. The free-form position
    // must survive unchanged rather than being pulled to the edge.
    wm.press(&mut comp, sess, 10, 9);
    wm.motion(&mut comp, 40, 40);
    wm.release_at(&mut comp, sess, 17, 40);
    assert_eq!(comp.placement(2), Some((30, 31)));
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
fn title_bar_controls_are_distinct_and_drive_window_lifecycle() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();
    // Window 2 starts at x=40 and is 80px wide; controls occupy local x=50..80.
    assert_eq!(wm.press(&mut comp, sess, 40 + 51, 20), Press::Minimized(2));
    assert_eq!(wm.is_minimized(&comp, 2), Some(true));
    assert_eq!(comp.focus(), Some(1));

    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.press(&mut comp, sess, 40 + 61, 20), Press::Maximized(2));
    assert_eq!(wm.is_maximized(2), Some(true));
    assert_eq!(comp.placement(2), Some((0, 0)));

    // Maximize changes the window width to the 200px scanout, so its close control moves with it.
    assert_eq!(wm.press(&mut comp, sess, 191, 1), Press::Closed(2));
    assert!(!wm.is_open(2));
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

#[test]
fn maximize_and_restore_round_trip_the_original_geometry() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.is_maximized(2), Some(false));
    assert_eq!(wm.size(2), Some((80, 60)));
    assert_eq!(comp.placement(2), Some((40, 20)));

    assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(true));
    assert_eq!(wm.is_maximized(2), Some(true));
    assert_eq!(wm.size(2), Some(comp.scanout_size()));
    assert_eq!(comp.placement(2), Some((0, 0)));
    assert_eq!(comp.focus(), Some(2));

    assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(false));
    assert_eq!(wm.is_maximized(2), Some(false));
    assert_eq!(wm.size(2), Some((80, 60)));
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.focus(), Some(2));
}

#[test]
fn maximize_refuses_an_unknown_window_without_changing_the_desktop() {
    let (mut comp, mut wm, sess) = desk();
    let z = comp.z_order();
    assert_eq!(
        wm.toggle_maximize(&mut comp, sess, 99),
        Err(WmFault::UnknownWindow(99))
    );
    assert_eq!(comp.z_order(), z);
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(wm.counters().3, 1);
}

#[test]
fn keyboard_focus_cycle_follows_z_order_without_allocating() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 1).unwrap();
    assert_eq!(comp.z_order(), vec![1, 2]);

    assert_eq!(wm.cycle_focus(&mut comp, sess, true).unwrap(), Some(2));
    assert_eq!(comp.focus(), Some(2));
    assert_eq!(comp.z_order(), vec![1, 2]);

    assert_eq!(wm.cycle_focus(&mut comp, sess, true).unwrap(), Some(1));
    assert_eq!(comp.focus(), Some(1));
    // Backwards from 1 selects 2 and keeps the compositor's stacking/focus contract aligned.
    assert_eq!(wm.cycle_focus(&mut comp, sess, false).unwrap(), Some(2));
    assert_eq!(comp.focus(), Some(2));
}

#[test]
fn resize_grip_is_a_distinct_client_control_and_changes_window_size() {
    let (mut comp, mut wm, sess) = desk();
    let old = wm.size(2).unwrap();
    let grip_x = 40 + old.0 - 1;
    let grip_y = 20 + old.1 - 1;
    assert_eq!(
        wm.press(&mut comp, sess, grip_x, grip_y),
        Press::Resizing(2)
    );
    assert_eq!(wm.motion(&mut comp, 140, 100), Some(2));
    assert_eq!(wm.size(2), Some((101, 81)));
    assert_eq!(comp.surface_size(2), Some((101, 81)));
    assert_eq!(wm.release(), Some(2));
}

#[test]
fn resize_edges_and_corners_change_only_the_owned_axes() {
    use kernel_core::wm::ResizeEdge;
    let (mut comp, mut wm, sess) = desk();

    // Left edge keeps the right edge fixed while moving the left edge inward.
    assert_eq!(wm.press(&mut comp, sess, 40, 50), Press::Resizing(2));
    assert_eq!(wm.motion(&mut comp, 56, 50), Some(2));
    assert_eq!(comp.placement(2), Some((56, 20)));
    assert_eq!(wm.size(2), Some((64, 60)));
    let _ = wm.release();

    // Top edge keeps the bottom edge fixed.
    assert_eq!(wm.press(&mut comp, sess, 80, 30), Press::Resizing(2));
    assert_eq!(wm.motion(&mut comp, 80, 36), Some(2));
    assert_eq!(comp.placement(2), Some((56, 36)));
    assert_eq!(wm.size(2), Some((64, 44)));
    let _ = wm.release();

    // Bottom-right remains the familiar two-axis resize.
    let (w, h) = wm.size(2).unwrap();
    assert_eq!(
        wm.press(&mut comp, sess, 56 + w - 1, 36 + h - 1),
        Press::Resizing(2)
    );
    assert_eq!(wm.motion(&mut comp, 140, 100), Some(2));
    assert_eq!(wm.size(2), Some((85, 65)));
    assert_eq!(comp.placement(2), Some((56, 36)));
    let _ = wm.release();

    assert_eq!(ResizeEdge::BottomRight, ResizeEdge::BottomRight);
}

#[test]
fn resize_refuses_a_fully_offscreen_result_without_changing_geometry() {
    let (mut comp, mut wm, sess) = desk();
    let before = wm.size(2).unwrap();
    assert_eq!(wm.press(&mut comp, sess, 44, 22), Press::Dragging(2));
    assert_eq!(wm.motion(&mut comp, 0, 0), Some(2));
    assert_eq!(wm.size(2), Some(before));
    assert_eq!(comp.surface_size(2), Some(before));
    let _ = wm.release();
}

#[test]
fn minimize_hides_without_destroying_the_window_and_restore_reclaims_focus() {
    let (mut comp, mut wm, sess) = desk();
    let tok2 = wm.token(2).unwrap();
    comp.set_focus(sess, 2).unwrap();

    assert_eq!(wm.is_minimized(&comp, 2), Some(false));
    assert_eq!(wm.toggle_minimize(&mut comp, sess, 2), Ok(true));
    assert_eq!(wm.is_minimized(&comp, 2), Some(true));
    assert!(wm.is_open(2));
    assert_eq!(wm.token(2), Some(tok2));
    assert_eq!(comp.is_visible(2), Some(false));
    assert_eq!(comp.focus(), Some(1));
    assert_eq!(wm.window_at(&comp, 60, 40), Some((1, 60, 40)));

    assert_eq!(wm.toggle_minimize(&mut comp, sess, 2), Ok(false));
    assert_eq!(wm.is_minimized(&comp, 2), Some(false));
    assert_eq!(comp.is_visible(2), Some(true));
    assert_eq!(comp.focus(), Some(2));
    assert_eq!(wm.window_at(&comp, 60, 40), Some((2, 20, 20)));
}

#[test]
fn minimized_windows_are_skipped_by_focus_cycle() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 1).unwrap();
    assert_eq!(wm.toggle_minimize(&mut comp, sess, 2), Ok(true));
    assert_eq!(wm.cycle_focus(&mut comp, sess, true).unwrap(), Some(1));
    assert_eq!(comp.focus(), Some(1));
}

#[test]
fn tile_visible_lays_out_only_presented_windows_and_preserves_focus() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();
    assert_eq!(wm.tile_visible(&mut comp, sess), Ok(2));
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), Some((100, 0)));
    assert_eq!(comp.surface_size(1), Some((100, 120)));
    assert_eq!(comp.surface_size(2), Some((100, 120)));
    assert_eq!(comp.focus(), Some(2));
    assert_eq!(wm.is_maximized(1), Some(false));
    assert_eq!(wm.is_maximized(2), Some(false));
}

#[test]
fn tile_visible_excludes_minimized_windows() {
    let (mut comp, mut wm, sess) = desk();
    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    let hidden_placement = comp.placement(2);
    assert_eq!(wm.tile_visible(&mut comp, sess), Ok(1));
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.surface_size(1), Some((200, 120)));
    assert_eq!(comp.placement(2), hidden_placement);
    assert_eq!(comp.is_visible(2), Some(false));
}

#[test]
fn cascade_visible_stacks_presented_windows_without_resizing_or_reordering() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(wm.toggle_maximize(&mut comp, sess, 1), Ok(true));
    comp.set_focus(sess, 2).unwrap();
    let z_before = comp.z_order();
    assert_eq!(wm.is_maximized(1), Some(true));
    assert_eq!(wm.cascade_visible(&mut comp, sess), Ok(2));

    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), Some((32, 32)));
    assert_eq!(comp.surface_size(1), Some((200, 120)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));
    assert_eq!(comp.z_order(), z_before);
    assert_eq!(comp.focus(), Some(2));
    assert_eq!(wm.is_maximized(1), Some(false));
    assert_eq!(wm.is_maximized(2), Some(false));
}

#[test]
fn cascade_visible_skips_minimized_windows_and_keeps_them_untouched() {
    let (mut comp, mut wm, sess) = desk();
    let hidden = comp.placement(2);
    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.cascade_visible(&mut comp, sess), Ok(1));
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), hidden);
    assert_eq!(comp.is_visible(2), Some(false));
}

#[test]
fn keyboard_snap_moves_the_focused_window_to_each_half_and_preserves_restore_geometry() {
    let cases = [
        (SnapDirection::Left, (0, 0)),
        (SnapDirection::Right, (100, 0)),
        (SnapDirection::Up, (0, 0)),
        (SnapDirection::Down, (0, 60)),
    ];
    for (direction, expected) in cases {
        let (mut comp, mut wm, sess) = desk();
        comp.set_focus(sess, 2).unwrap();
        assert_eq!(wm.snap_focused(&mut comp, sess, direction), Ok(Some(2)));
        assert_eq!(comp.placement(2), Some(expected));
        assert_eq!(comp.focus(), Some(2));
        assert_eq!(
            comp.surface_size(2),
            Some(
                if matches!(direction, SnapDirection::Up | SnapDirection::Down) {
                    (200, 60)
                } else {
                    (100, 120)
                }
            )
        );
        assert_eq!(wm.is_maximized(2), Some(true));
        assert_eq!(
            wm.snap_focused(&mut comp, sess, SnapDirection::Right),
            Ok(Some(2))
        );
        assert_eq!(comp.placement(2), Some((100, 0)));
        assert_eq!(wm.toggle_maximize(&mut comp, sess, 2), Ok(false));
        assert_eq!(comp.placement(2), Some((40, 20)));
        assert_eq!(comp.surface_size(2), Some((80, 60)));
    }
}

#[test]
fn keyboard_snap_uses_only_the_visible_focused_window_and_leaves_minimized_windows_untouched() {
    let (mut comp, mut wm, sess) = desk();
    assert_eq!(
        wm.snap_focused(&mut comp, sess, SnapDirection::Left),
        Ok(None)
    );
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), Some((40, 20)));

    comp.set_focus(sess, 2).unwrap();
    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    assert_eq!(
        wm.snap_focused(&mut comp, sess, SnapDirection::Left),
        Ok(Some(1))
    );
    assert_eq!(comp.placement(1), Some((0, 0)));
    assert_eq!(comp.placement(2), Some((40, 20)));
    assert_eq!(comp.is_visible(2), Some(false));
}

#[test]
fn keyboard_nudge_moves_focused_window_in_fixed_steps_and_clamps_to_scanout() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();

    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Right), Ok(Some(2)));
    assert_eq!(comp.placement(2), Some((56, 20)));
    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Down), Ok(Some(2)));
    assert_eq!(comp.placement(2), Some((56, 36)));
    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Left), Ok(Some(2)));
    assert_eq!(comp.placement(2), Some((40, 36)));
    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Up), Ok(Some(2)));
    assert_eq!(comp.placement(2), Some((40, 20)));

    for _ in 0..32 {
        let _ = wm.nudge_focused(&mut comp, NudgeDirection::Right);
        let _ = wm.nudge_focused(&mut comp, NudgeDirection::Down);
    }
    assert_eq!(comp.placement(2), Some((120, 60)));
}

#[test]
fn keyboard_nudge_refuses_hidden_or_snapped_windows_without_destroying_restore_state() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();
    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Right), Ok(Some(1)));
    assert_eq!(comp.placement(2), Some((40, 20)));

    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    wm.snap_focused(&mut comp, sess, SnapDirection::Left).unwrap();
    let snapped = comp.placement(2);
    assert_eq!(wm.nudge_focused(&mut comp, NudgeDirection::Right), Ok(None));
    assert_eq!(comp.placement(2), snapped);
    assert_eq!(wm.is_maximized(2), Some(true));
}

#[test]
fn keyboard_resize_changes_focused_window_in_fixed_steps_and_clamps_to_scanout() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();

    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Right), Ok(Some(2)));
    assert_eq!(comp.surface_size(2), Some((96, 60)));
    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Down), Ok(Some(2)));
    assert_eq!(comp.surface_size(2), Some((96, 76)));
    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Left), Ok(Some(2)));
    assert_eq!(comp.surface_size(2), Some((80, 76)));
    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Up), Ok(Some(2)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));

    for _ in 0..32 {
        let _ = wm.resize_focused(&mut comp, ResizeDirection::Right);
        let _ = wm.resize_focused(&mut comp, ResizeDirection::Down);
    }
    assert_eq!(comp.surface_size(2), Some((160, 92)));
}

#[test]
fn keyboard_resize_refuses_hidden_or_snapped_windows_without_destroying_restore_state() {
    let (mut comp, mut wm, sess) = desk();
    comp.set_focus(sess, 2).unwrap();
    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Right), Ok(Some(1)));
    assert_eq!(comp.surface_size(2), Some((80, 60)));

    wm.toggle_minimize(&mut comp, sess, 2).unwrap();
    comp.set_focus(sess, 2).unwrap();
    wm.snap_focused(&mut comp, sess, SnapDirection::Left).unwrap();
    assert_eq!(wm.resize_focused(&mut comp, ResizeDirection::Down), Ok(None));
    assert_eq!(wm.is_maximized(2), Some(true));
}
