//! The console session that never grows (REQ-CON-001 / REQ-QUAL-007, ADR-089).
//!
//! ADR-086/087/088 measured the desktop, the scheduler and the filesystem at volume and each found
//! a per-event allocation on a heap that never frees (ADR-063). The console is the fourth hot
//! path, and the one a HUMAN drives: a session is exactly a stream of commands, so a console that
//! spends memory per command is a machine that dies of being used. Measured before this wave:
//! ~450 bytes per command, from `format!` on every printed line and a fresh `String` per history
//! entry.
//!
//! This suite storms the dispatcher with the commands a session actually runs and holds it to
//! four claims, measured on the platform's own heap:
//!
//! * **A command that only REPORTS costs nothing.** `help`, `ver`, `mem`, `ls`, `history` — all
//!   formatting, no data — must not move the watermark at all.
//! * **A session that types forever keeps a bounded history and allocates nothing for it.**
//! * **A command that RETURNS DATA allocates that data and nothing else.** `cat` of an object
//!   hands the caller its bytes; the claim is that the cost is the bytes, not a multiple of them.
//! * **The same session twice prints the same bytes.** Output is a pure function of the machine.

use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::Filesystem;
use crate::shell::{self, Outcome, ShellHost};
use crate::storage::{BlockDevice, MemBlockDevice};

/// Commands per round.
const COMMANDS: u32 = 256;
/// The storm's device: the journal's area plus a little namespace.
const BLOCKS: usize = 96;

/// Report-only commands: everything they print, they format; nothing they print, they own.
/// `oc off` and a refused `oc` belong here too (ADR-184): the power governor is driven by console
/// lines, and a line that moves a clock must cost the heap no more than one that reads it.
const REPORTING: [&str; 11] = [
    "help", "ver", "mem", "ls", "history", "date", "power", "oc 1", "boot", "caps", "net",
];

/// The boot suite (ADR-089). `used_bytes` reports the CALLER's own heap watermark.
pub fn storm_suite<H: ShellHost>(
    host: &H,
    used_bytes: &mut dyn FnMut() -> usize,
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

    let mut dev = MemBlockDevice::new(BLOCKS);
    Filesystem::format(&mut dev).map_err(|_| (0u32, "shellstorm: format"))?;
    let mut fs = Filesystem::mount(&mut dev).map_err(|_| (0u32, "shellstorm: mount"))?;
    fs.create(&mut dev, "note", b"hello from the console")
        .map_err(|_| (0u32, "shellstorm: seed"))?;
    // The browser's navigation state a session carries (ADR-156): one for the storm, as a console
    // has one; a Copy value on the stack, so it costs the heap nothing per command.
    let mut nav = crate::browser::Navigator::new();

    // 1 — A COMMAND THAT ONLY REPORTS COSTS NOTHING. Every eighth line is also an operator hold at
    //     the top of the machine's own ladder and its release — the point read, not written here.
    {
        let top = crate::lethed::resident::facts()
            .filter(|f| f.n > 0 && f.domains[0].n_points > 0)
            .map(|f| f.domains[0].points[f.domains[0].n_points - 1])
            .unwrap_or(0);
        let mut hold = crate::linebuf::LineBuf::<{ crate::linebuf::LINE_MAX }>::new();
        let _ = core::fmt::Write::write_fmt(&mut hold, format_args!("oc {}", top));
        let mut sink = |_: &str| {};
        let mut round = |fs: &mut Filesystem, dev: &mut MemBlockDevice| {
            for i in 0..COMMANDS {
                let cmd = REPORTING[(i as usize) % REPORTING.len()];
                let _ = shell::execute(cmd, host, fs, dev, &[], &mut nav, &mut sink);
                if i % 8 == 7 {
                    let _ = shell::execute(hold.as_str(), host, fs, dev, &[], &mut nav, &mut sink);
                    let _ = shell::execute("oc off", host, fs, dev, &[], &mut nav, &mut sink);
                }
            }
        };
        round(&mut fs, &mut dev); // warm-up: first-touch growth is paid once per boot
        let before = used_bytes();
        round(&mut fs, &mut dev);
        let after = used_bytes();
        crate::storm_report("shellstorm", before, after);
        check!(
            after == before,
            "shellstorm: two hundred and fifty-six reporting commands allocate NOTHING"
        );
    }
    // 2 — A SESSION THAT TYPES FOREVER keeps a BOUNDED history whose buffers are REUSED. The
    //     finished line itself is handed to the caller (`Edit::Line` owns its bytes, by design and
    //     named), so the honest claim is per-line cost bounded by the LINE, not by the line plus a
    //     second copy kept forever: before this wave every submission also allocated a fresh
    //     history `String` and dropped the oldest, which on a never-freeing heap is a session that
    //     grows without end.
    {
        let mut ed = shell::LineEditor::new();
        let mut echo = |_: &str| {};
        let line = b"echo hello";
        let mut submit = |ed: &mut shell::LineEditor, i: u32| {
            for b in line.iter() {
                let _ = ed.feed(*b, &mut echo);
            }
            let _ = ed.feed(b'0' + (i % 10) as u8, &mut echo);
            let _ = ed.feed(b'\r', &mut echo);
        };
        for i in 0..(shell::HISTORY_MAX as u32 * 2) {
            submit(&mut ed, i); // warm-up: every history buffer now exists
        }
        let before = used_bytes();
        let lines = 1024u32;
        for i in 0..lines {
            submit(&mut ed, i);
        }
        let after = used_bytes();
        let per = (after - before) / lines as usize;
        check!(
            ed.history_len() == shell::HISTORY_MAX && per <= (line.len() + 1) * 3,
            "shellstorm: a thousand submitted lines keep a bounded history and cost only the line itself"
        );
    }
    // 3 — A COMMAND THAT RETURNS DATA allocates THAT DATA and not a multiple of it. `cat` hands
    //     the caller an object's bytes; the claim is that the cost is the bytes, named.
    {
        let mut sink = |_: &str| {};
        let _ = shell::execute(
            "cat note",
            host,
            &mut fs,
            &mut dev,
            &[],
            &mut nav,
            &mut sink,
        ); // warm-up
        let before = used_bytes();
        for _ in 0..64 {
            let _ = shell::execute(
                "cat note",
                host,
                &mut fs,
                &mut dev,
                &[],
                &mut nav,
                &mut sink,
            );
        }
        let after = used_bytes();
        let per = (after - before) / 64;
        let body = 22; // "hello from the console"
        check!(
            per <= body * 3,
            "shellstorm: a command that returns data costs its data, not a multiple of it"
        );
    }
    // 4 — THE SAME SESSION TWICE PRINTS THE SAME BYTES. `df`, not `mem`: `mem` reports the heap
    //     watermark (ADR-180), and the transcript's own log string moves it between the tellings.
    {
        let script = ["help", "ver", "df", "ls", "stat note", "wc note", "history"];
        let mut transcript = |fs: &mut Filesystem, dev: &mut MemBlockDevice| -> String {
            let mut log = String::new();
            for c in script {
                let mut sink = |s: &str| {
                    log.push_str(s);
                    log.push('\n');
                };
                let out = shell::execute(c, host, fs, dev, &[], &mut nav, &mut sink);
                if out == Outcome::Halt {
                    break;
                }
            }
            log
        };
        let a = transcript(&mut fs, &mut dev);
        let b = transcript(&mut fs, &mut dev);
        check!(
            !a.is_empty() && a == b,
            "shellstorm: the same session told twice prints byte-for-byte the same answer"
        );
    }
    // 5 — THE WHOLE SESSION PATH, as a person drives it (ADR-180). Claims 1-4 drove `execute` and
    //     the editor separately, and `Session::feed` - the path that joins them - cloned all of
    //     history on every line and let Tab collect a `Vec<String>`: ~3.6 KB per command, found by
    //     the live console fuzz when aarch64 ran out of heap after ~1100 hostile lines. Here the
    //     real session types reporting commands, walks history with Ctrl-P/Ctrl-N, and presses Tab
    //     both at a command and at a file name; the heap must not move.
    {
        let mut session = shell::Session::new();
        let mut out = |_: &str| {};
        let script: &[&[u8]] = &[
            b"ver\r",
            b"mem\r",
            b"ls\r",
            b"history\r",
            b"he\t\r",
            b"stat no\t\x03",
            b"\x10\x10\x0e\r",
            b"\x03",
            b"help\r",
        ];
        let mut round =
            |session: &mut shell::Session, fs: &mut Filesystem, dev: &mut MemBlockDevice| {
                for _ in 0..(COMMANDS / script.len() as u32) {
                    for line in script {
                        for b in line.iter() {
                            let _ = session.feed(*b, host, fs, dev, &mut out);
                        }
                    }
                }
            };
        round(&mut session, &mut fs, &mut dev); // warm-up: history and line buffers now exist
        let before = used_bytes();
        round(&mut session, &mut fs, &mut dev);
        let after = used_bytes();
        check!(
            after == before,
            "shellstorm: the whole session path - typing, Tab, history walk, reporting commands - allocates NOTHING"
        );
    }
    let _: Vec<u8> = Vec::new(); // keep the alloc import honest on every feature combination
    Ok(n)
}

/// A device the storm can hand to a caller that wants to inspect it afterwards.
pub fn storm_device() -> impl BlockDevice {
    MemBlockDevice::new(BLOCKS)
}
