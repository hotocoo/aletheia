//! `snake` (ADR-222): a game, in its own colour window. Steer with `w`/`a`/`s`/`d` (or click the
//! side of the window to turn towards), eat the red food, and do not run into the wall or
//! yourself. `q` quits. The exit status is 2000 + the score, so a run says how it went. Given
//! `clock` it only checks the machine's clock moves (ADR-222's boot probe).
#![no_std]
#![no_main]

mod mem;
mod sys;

/// The board, in cells, and each cell's size in pixels.
const COLS: usize = 40;
const ROWS: usize = 30;
const CELL: usize = 4;
const W: usize = COLS * CELL;
const H: usize = ROWS * CELL;
/// Time between moves, on the machine's clock (ADR-222): the game's pace is the clock's, not the
/// emulator's.
const STEP_NS: u64 = 120_000_000;
/// The longest the snake can grow: every cell of the board.
const MAX_LEN: usize = COLS * ROWS;

const GRASS: u8 = 0x08; // dark green
const BODY: u8 = 0x1C; // bright green
const HEAD: u8 = 0xFC; // yellow
const FOOD: u8 = 0xE0; // red
const WALL: u8 = 0x92; // grey

/// The frame, one RGB332 byte per pixel (five pages of `.bss`).
static mut FRAME: [u8; W * H] = [0; W * H];
/// The snake's cells, head first, as a ring.
static mut BODY_CELLS: [(u8, u8); MAX_LEN] = [(0, 0); MAX_LEN];

/// A small deterministic generator: the game must not depend on anything but its input.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as usize % n
    }
}

fn fill_cell(frame: &mut [u8; W * H], (x, y): (u8, u8), colour: u8) {
    for dy in 0..CELL {
        let row = (y as usize * CELL + dy) * W + x as usize * CELL;
        frame[row..row + CELL].fill(colour);
    }
}

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    if unsafe { core::slice::from_raw_parts(args, len) } == b"clock" {
        // The clock probe: two readings, some work apart, must move forward.
        let first = sys::clock_ns();
        let mut x = 0u64;
        for i in 0..100_000u64 {
            x = core::hint::black_box(x ^ i);
        }
        sys::exit(if sys::clock_ns() > first && x != u64::MAX { 1 } else { 0 });
    }
    // SAFETY: single-threaded program; nothing else touches these globals.
    let (frame, body) = unsafe {
        (
            &mut *core::ptr::addr_of_mut!(FRAME),
            &mut *core::ptr::addr_of_mut!(BODY_CELLS),
        )
    };
    let mut rng = Lcg(0x5EED);
    let (mut head, mut len) = (0usize, 3usize);
    for (i, cell) in body.iter_mut().take(len).enumerate() {
        *cell = ((COLS / 2 - i) as u8, (ROWS / 2) as u8);
    }
    let (mut dx, mut dy) = (1i8, 0i8);
    let mut food = (5u8, 5u8);
    let mut score = 0u64;
    loop {
        // Input since the last frame: the last turn that does not reverse onto the body wins.
        while let Ok(Some(event)) = sys::poll_input() {
            let turn = match event {
                sys::Input::Key(b'q') => sys::exit(2000 + score),
                sys::Input::Key(b'w') => (0, -1),
                sys::Input::Key(b's') => (0, 1),
                sys::Input::Key(b'a') => (-1, 0),
                sys::Input::Key(b'd') => (1, 0),
                sys::Input::Pointer {
                    x,
                    y,
                    click: true,
                    held: true,
                } => {
                    // Turn towards the click, along the axis the snake is not moving on.
                    let (hx, hy) = body[head];
                    let (cx, cy) = (x as i32 / CELL as i32, y as i32 / CELL as i32);
                    if dx != 0 {
                        (0, if cy < hy as i32 { -1 } else { 1 })
                    } else {
                        (if cx < hx as i32 { -1 } else { 1 }, 0)
                    }
                }
                _ => continue,
            };
            if (turn.0 + dx, turn.1 + dy) != (0, 0) {
                (dx, dy) = turn;
            }
        }
        // Move: the new head, then the wall and the body.
        let (hx, hy) = body[head];
        let (nx, ny) = (hx as i32 + dx as i32, hy as i32 + dy as i32);
        let hit_wall = nx <= 0 || ny <= 0 || nx >= COLS as i32 - 1 || ny >= ROWS as i32 - 1;
        let next = (nx as u8, ny as u8);
        let ate = next == food;
        let tail_moves = if ate { 0 } else { 1 };
        let hit_self =
            (0..len - tail_moves).any(|i| body[(head + MAX_LEN - i) % MAX_LEN] == next);
        if hit_wall || hit_self {
            // Show the crash for a moment, then end with the score.
            let until = sys::clock_ns() + 4 * STEP_NS;
            while sys::clock_ns() < until {}
            sys::exit(2000 + score);
        }
        head = (head + 1) % MAX_LEN;
        body[head] = next;
        if ate {
            len += 1;
            score += 1;
            // New food on a free cell inside the wall.
            loop {
                let f = ((1 + rng.next(COLS - 2)) as u8, (1 + rng.next(ROWS - 2)) as u8);
                if !(0..len).any(|i| body[(head + MAX_LEN - i) % MAX_LEN] == f) {
                    food = f;
                    break;
                }
            }
        }
        // Draw.
        frame.fill(GRASS);
        for x in 0..COLS {
            fill_cell(frame, (x as u8, 0), WALL);
            fill_cell(frame, (x as u8, (ROWS - 1) as u8), WALL);
        }
        for y in 0..ROWS {
            fill_cell(frame, (0, y as u8), WALL);
            fill_cell(frame, ((COLS - 1) as u8, y as u8), WALL);
        }
        fill_cell(frame, food, FOOD);
        for i in 1..len {
            fill_cell(frame, body[(head + MAX_LEN - i) % MAX_LEN], BODY);
        }
        fill_cell(frame, body[head], HEAD);
        if sys::present_rgb332(frame, W as u32, H as u32).is_err() {
            // No desktop, or the operator closed the window.
            sys::exit(2000 + score);
        }
        // Wait out the rest of the step, still taking input as it comes.
        let until = sys::clock_ns() + STEP_NS;
        while sys::clock_ns() < until {}
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
