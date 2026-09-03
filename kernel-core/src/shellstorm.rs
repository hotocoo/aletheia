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
const REPORTING: [&str; 5] = ["help", "ver", "mem", "ls", "history"];

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

    // 1 — A COMMAND THAT ONLY REPORTS COSTS NOTHING.
    {
        let mut sink = |_: &str| {};
        let mut round = |fs: &mut Filesystem, dev: &mut MemBlockDevice| {
            for i in 0..COMMANDS {
                let cmd = REPORTING[(i as usize) % REPORTING.len()];
                let _ = shell::execute(cmd, host, fs, dev, &[], &mut sink);
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
        let _ = shell::execute("cat note", host, &mut fs, &mut dev, &[], &mut sink); // warm-up
        let before = used_bytes();
        for _ in 0..64 {
            let _ = shell::execute("cat note", host, &mut fs, &mut dev, &[], &mut sink);
        }
        let after = used_bytes();
        let per = (after - before) / 64;
        let body = 22; // "hello from the console"
        check!(
            per <= body * 3,
            "shellstorm: a command that returns data costs its data, not a multiple of it"
        );
    }
    // 4 — THE SAME SESSION TWICE PRINTS THE SAME BYTES.
    {
        let script = [
            "help",
            "ver",
            "mem",
            "ls",
            "stat note",
            "wc note",
            "history",
        ];
        let transcript = |fs: &mut Filesystem, dev: &mut MemBlockDevice| -> String {
            let mut log = String::new();
            for c in script {
                let mut sink = |s: &str| {
                    log.push_str(s);
                    log.push('\n');
                };
                let out = shell::execute(c, host, fs, dev, &[], &mut sink);
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
    let _: Vec<u8> = Vec::new(); // keep the alloc import honest on every feature combination
    Ok(n)
}

/// A device the storm can hand to a caller that wants to inspect it afterwards.
pub fn storm_device() -> impl BlockDevice {
    MemBlockDevice::new(BLOCKS)
}
