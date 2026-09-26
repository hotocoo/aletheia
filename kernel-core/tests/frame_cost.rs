//! What a frame costs (ADR-197): the compositor into the real ComposeSink at 1080p (2x scale).
use kernel_core::compositor::{Compositor, Rect};
use kernel_core::fbcon::{ComposeSink, Surface};

fn pages(bytes: usize) -> (Vec<u8>, Vec<usize>) {
    let v = vec![0u8; bytes + 4096];
    let base = (v.as_ptr() as usize).div_ceil(4096) * 4096;
    let n = bytes.div_ceil(4096);
    (v, (0..n).map(|i| base + i * 4096).collect())
}

#[test]
fn a_cursor_move_sends_a_small_rect_and_a_full_repaint_is_measured() {
    let (fw, fh, scale) = (1920u32, 1080u32, 2u32);
    let (lw, lh) = (fw / scale, fh / scale);
    let (_keep, pg) = pages((fw * fh * 4) as usize);
    let mut comp = Compositor::new(1, lw, lh);
    let sess = comp.open_input_session().unwrap();
    let tok = comp.mint_surface(1, lw, lh).unwrap();
    let _ = comp.fill_rect(
        1,
        tok,
        Rect {
            x: 0,
            y: 0,
            w: lw,
            h: lh,
        },
        false,
    );
    comp.attach(1, tok, 0, 0).unwrap();
    comp.move_cursor(sess, 10, 10).unwrap();

    let mut surf = Surface::new(&pg, fw, fh).unwrap();
    let t = std::time::Instant::now();
    let dirty = {
        let mut sink = ComposeSink::new(&mut surf).with_scale(scale);
        comp.compose_frame(&mut sink);
        sink.dirty_rect()
    };
    let full = t.elapsed();
    assert_eq!(
        dirty,
        Some((0, 0, fw, fh)),
        "the first frame is the whole screen"
    );

    comp.move_cursor(sess, 400, 300).unwrap();
    let t = std::time::Instant::now();
    let sent: u32 = {
        let mut sink = ComposeSink::new(&mut surf).with_scale(scale);
        comp.compose_frame(&mut sink);
        sink.dirty_rects().map(|(_, _, w, h)| w * h).sum()
    };
    let small = t.elapsed();
    assert!(
        sent * 4 < 20_000,
        "a cursor move sends {sent} px, not the frame"
    );
    eprintln!("full 1080p frame {full:?}; cursor move {small:?}; {sent} px sent for a move");
}
