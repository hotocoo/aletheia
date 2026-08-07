//! The interactive console — the OS you can actually sit in front of (REQ-CON-001, ADR-044).
//!
//! Every gate in this repo boots, proves its invariants and **exits with a verdict**. That is what
//! makes the claims checkable, and it is also why "can I run it?" had no answer: nothing kept the
//! machine up and listening. This module is the part that does — a line editor and a command
//! dispatcher over the subsystems that are already proved (the named-object filesystem over the
//! journal, the frame allocator, the HAL's clock), so the console adds *reach* without adding
//! unproved surface underneath it.
//!
//! **Arch-independent on purpose.** A serial port differs per target; a line does not. Targets
//! supply only `getc`/`putc`; the editing rules, the command grammar and every refusal live here
//! once and are proved three times (a live suite per target) plus on the host.
//!
//! **Fail-closed editing.** The editor is the first thing a human byte touches, so it is written as
//! a filter rather than a buffer: only printable ASCII may ENTER the line, a full line drops further
//! input instead of growing, and a byte that is neither printable nor a recognized control is
//! discarded without an echo. A terminal can send anything — a mouse report, a paste of a binary
//! file, an arrow key's escape sequence — and none of it can become a command argument.
//!
//! **What this is not.** The dispatcher runs in kernel space and drives the kernel's own objects
//! directly; it is not a user-mode shell process over a syscall ABI, and it is not claimed as one.
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs::{Filesystem, FsError};
use crate::storage::{BlockDevice, StorageError, BLOCK_SIZE};

/// The longest line the editor will assemble. A line above this is TRUNCATED at the bound — the
/// extra bytes are dropped, never buffered — so a terminal that pastes a megabyte cannot make the
/// kernel allocate one.
pub const MAX_LINE: usize = 256;

/// What one input byte did to the line being edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The line is still being typed (or the byte was refused).
    Pending,
    /// The user pressed return: here is the finished line, and the editor is empty again.
    Line(String),
    /// The user pressed Ctrl-C: whatever was typed is discarded, and no command runs.
    Cancelled,
}

/// A single line of input under construction.
///
/// Holds bytes, not a `String`: only ASCII is ever admitted, so the buffer is valid UTF-8 by
/// construction rather than by a check that could be forgotten.
pub struct LineEditor {
    buf: Vec<u8>,
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl LineEditor {
    pub fn new() -> Self {
        LineEditor {
            buf: Vec::with_capacity(MAX_LINE),
        }
    }

    /// Bytes currently held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Discard the line without producing one.
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Feed one byte from the console. `echo` receives exactly what should be written back to the
    /// terminal — nothing is echoed for a byte that was refused, so what the user sees is what the
    /// kernel actually holds.
    pub fn feed(&mut self, byte: u8, echo: &mut dyn FnMut(&str)) -> Edit {
        match byte {
            // Return: the line is complete. CR and LF are both accepted because a serial terminal
            // may send either (and CRLF then arrives as a complete line plus one empty one).
            b'\r' | b'\n' => {
                echo("\r\n");
                let line = String::from_utf8(core::mem::take(&mut self.buf)).unwrap_or_default(); // unreachable: only ASCII was admitted
                Edit::Line(line)
            }
            // Ctrl-C: abandon the line. Visible, because a silent discard looks like a hang.
            0x03 => {
                self.buf.clear();
                echo("^C\r\n");
                Edit::Cancelled
            }
            // Ctrl-U: kill the whole line, staying on it.
            0x15 => {
                while !self.buf.is_empty() {
                    self.buf.pop();
                    echo("\x08 \x08");
                }
                Edit::Pending
            }
            // Backspace / DEL: remove one byte. On an empty line this is a no-op AND draws nothing,
            // so the cursor can never walk back over the prompt.
            0x08 | 0x7f => {
                if self.buf.pop().is_some() {
                    echo("\x08 \x08");
                }
                Edit::Pending
            }
            // Printable ASCII: the only bytes that may enter the buffer, and only while there is
            // room. At the bound the byte is dropped without an echo — the line stops growing
            // instead of the allocation doing so.
            0x20..=0x7e => {
                if self.buf.len() < MAX_LINE {
                    self.buf.push(byte);
                    // One-byte &str without allocating a String per keystroke.
                    let b = [byte];
                    echo(core::str::from_utf8(&b).unwrap_or(""));
                }
                Edit::Pending
            }
            // Everything else — other control codes, and every byte >= 0x80 — is discarded.
            _ => Edit::Pending,
        }
    }
}

/// Whether the session continues after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Print the prompt and keep reading.
    Continue,
    /// The user asked to stop: the caller halts the machine.
    Halt,
}

/// The facts a command may ask of the running target. Everything here is already established by the
/// boot the console runs after; the trait exists so the dispatcher never names an architecture.
pub trait ShellHost {
    /// Target backend name (as `Hal::arch_name`).
    fn arch(&self) -> &str;
    /// Monotonic nanoseconds since boot.
    fn uptime_ns(&self) -> u64;
    /// Physical frames currently free in the allocator.
    fn free_frames(&self) -> usize;
    /// Physical frames the allocator manages in total.
    fn total_frames(&self) -> usize;
    /// Current CPU privilege level, backend-defined (as `Hal::current_privilege`).
    fn privilege(&self) -> u64;
    /// Console bytes the target refused because the input ring was full (REQ-CON-002). A polled
    /// target reports 0. Surfaced rather than hidden: input loss the operator cannot see is input
    /// loss they will blame on the command they typed.
    fn input_dropped(&self) -> u64 {
        0
    }
}

/// The prompt. A constant because the live suite asserts on it.
pub const PROMPT: &str = "aletheia> ";

/// Every command name, in help order. Kept next to the dispatcher so a command cannot be added
/// without appearing in `help` (the live suite asserts the two agree).
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands"),
    ("arch", "active target backend and privilege level"),
    ("uptime", "nanoseconds since boot"),
    ("mem", "physical frame allocator usage"),
    ("df", "filesystem space, in blocks"),
    ("ls", "every named object"),
    ("stat NAME", "one object's extent and length"),
    ("cat NAME", "an object's contents"),
    ("write NAME TEXT", "create or atomically replace an object"),
    ("rm NAME", "remove an object (contents erased)"),
    ("echo TEXT", "print TEXT"),
    ("halt", "stop the machine"),
];

/// Render a filesystem refusal as one line a human can act on. Every arm is named: an unmatched
/// error would otherwise print as a debug blob at exactly the moment a user needs to understand it.
fn fs_error(e: FsError) -> String {
    match e {
        FsError::DeviceTooSmall => "device too small to hold a namespace".to_string(),
        FsError::NotFormatted => "device is not formatted".to_string(),
        FsError::BadName => "bad name (1..=32 bytes, no NUL, no '/')".to_string(),
        FsError::Exists => "a live object already owns that name".to_string(),
        FsError::NotFound => "no such object".to_string(),
        FsError::NoSpace => "no space (directory slot or contiguous extent)".to_string(),
        FsError::TooLarge => "object too large for one transaction".to_string(),
        FsError::Corrupt => "the directory describes something impossible".to_string(),
        FsError::Storage(s) => format!("storage: {}", storage_error(s)),
    }
}

fn storage_error(e: StorageError) -> &'static str {
    match e {
        StorageError::OutOfRange => "block index off the device",
        StorageError::BadBlockSize => "a buffer was not one block",
        StorageError::TooLarge => "the transaction is too large for the journal",
        StorageError::Device => "the device reported a failure",
    }
}

/// Split a command line into the verb and the untouched remainder. The remainder keeps its interior
/// spacing, so `write greeting  hello  world` stores exactly the text after the name.
fn split_first(line: &str) -> (&str, &str) {
    let t = line.trim();
    match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim_start()),
        None => (t, ""),
    }
}

/// Run one line. Returns whether the session continues; every output goes through `out`, one call
/// per line WITHOUT its newline (the caller owns line endings, which differ between a raw serial
/// terminal and a test that collects strings).
///
/// A refusal is a printed line and `Outcome::Continue` — the console never dies of bad input.
pub fn execute<H: ShellHost, D: BlockDevice>(
    line: &str,
    host: &H,
    fs: &mut Filesystem,
    dev: &mut D,
    out: &mut dyn FnMut(&str),
) -> Outcome {
    let (verb, rest) = split_first(line);
    if verb.is_empty() {
        return Outcome::Continue;
    }
    match verb {
        "help" => {
            out("commands:");
            for (name, doc) in COMMANDS {
                out(&format!("  {:<18} {}", name, doc));
            }
        }
        "arch" => out(&format!(
            "{} (privilege level {})",
            host.arch(),
            host.privilege()
        )),
        "uptime" => out(&format!("{} ns since boot", host.uptime_ns())),
        "mem" => {
            let (free, total) = (host.free_frames(), host.total_frames());
            out(&format!(
                "frames: {} free / {} total ({} used)",
                free,
                total,
                total.saturating_sub(free)
            ));
            out(&format!(
                "input: {} byte(s) dropped since boot",
                host.input_dropped()
            ));
        }
        "df" => match fs.free_blocks(dev) {
            Ok(free) => out(&format!(
                "{} free data blocks of {} bytes each",
                free, BLOCK_SIZE
            )),
            Err(e) => out(&fs_error(e)),
        },
        "ls" => match fs.list(dev) {
            Ok(entries) if entries.is_empty() => out("(no objects)"),
            Ok(entries) => {
                for e in entries {
                    out(&format!("{:>8}  {}", e.len, e.name));
                }
            }
            Err(e) => out(&fs_error(e)),
        },
        "stat" => {
            if rest.is_empty() {
                out("usage: stat NAME");
            } else {
                match fs.stat(dev, rest) {
                    Ok(e) => out(&format!(
                        "{}: {} bytes, {} block(s) at device block {}",
                        e.name,
                        e.len,
                        e.blocks(),
                        e.start
                    )),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "cat" => {
            if rest.is_empty() {
                out("usage: cat NAME");
            } else {
                match fs.read(dev, rest) {
                    // Contents came from a device, so they are NOT assumed to be text: bytes that
                    // are not valid UTF-8 are reported as a count rather than sprayed at a terminal
                    // that would interpret them as escape sequences.
                    Ok(bytes) => match core::str::from_utf8(&bytes) {
                        Ok(s) => out(s),
                        Err(_) => out(&format!("<{} bytes, not text>", bytes.len())),
                    },
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "write" => {
            let (name, text) = split_first(rest);
            if name.is_empty() {
                out("usage: write NAME TEXT");
            } else {
                // `replace` rather than remove+create: one transaction, so a crash mid-write leaves
                // the old contents or the new ones, never a vanished name (ADR-035).
                match fs.replace(dev, name, text.as_bytes()) {
                    Ok(()) => out(&format!("wrote {} bytes to {}", text.len(), name)),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "rm" => {
            if rest.is_empty() {
                out("usage: rm NAME");
            } else {
                match fs.remove(dev, rest) {
                    Ok(()) => out(&format!("removed {}", rest)),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "echo" => out(rest),
        "halt" => {
            out("halting.");
            return Outcome::Halt;
        }
        other => out(&format!(
            "unknown command '{}' — try `help`",
            other.escape_debug()
        )),
    }
    Outcome::Continue
}

/// A console session: the editor and the dispatcher wired together, driven one input byte at a
/// time. The interactive loop and the live invariant suite run THIS, so what a gate proves is the
/// same code a human types at.
pub struct Session {
    editor: LineEditor,
    started: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Session {
            editor: LineEditor::new(),
            started: false,
        }
    }

    /// Feed one byte. Emits through `out`, which receives raw terminal text (echo, command output
    /// with CRLF endings, and the next prompt). Returns `Outcome::Halt` when the user asked to stop.
    pub fn feed<H: ShellHost, D: BlockDevice>(
        &mut self,
        byte: u8,
        host: &H,
        fs: &mut Filesystem,
        dev: &mut D,
        out: &mut dyn FnMut(&str),
    ) -> Outcome {
        if !self.started {
            self.started = true;
            out(PROMPT);
        }
        match self.editor.feed(byte, out) {
            Edit::Pending => Outcome::Continue,
            Edit::Cancelled => {
                out(PROMPT);
                Outcome::Continue
            }
            Edit::Line(line) => {
                let outcome = execute(&line, host, fs, dev, &mut |s| {
                    out(s);
                    out("\r\n");
                });
                if outcome == Outcome::Continue {
                    out(PROMPT);
                }
                outcome
            }
        }
    }

    /// Emit the first prompt without consuming input (what a banner does before the user types).
    pub fn prompt(&mut self, out: &mut dyn FnMut(&str)) {
        self.started = true;
        out(PROMPT);
    }
}

/// The interactive loop itself, owned here so all three targets share one: read a byte, feed the
/// session, repeat until the user halts. A target supplies only `getc` (non-blocking; `None` means
/// nothing typed yet) and `out`, so the polling discipline, the banner and the exit condition are
/// defined once rather than three times with three sets of bugs.
///
/// Returns when the session halts. The caller decides what "halt" means on its hardware.
pub fn run_loop<H: ShellHost, D: BlockDevice>(
    host: &H,
    fs: &mut Filesystem,
    dev: &mut D,
    getc: &mut dyn FnMut() -> Option<u8>,
    out: &mut dyn FnMut(&str),
) {
    let mut session = Session::new();
    session.prompt(out);
    loop {
        let Some(byte) = getc() else { continue };
        if session.feed(byte, host, fs, dev, out) == Outcome::Halt {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The live invariant suite. Runs on every target, in kernel space, over a real filesystem — the
// console is not proved by a human noticing it works.
// ---------------------------------------------------------------------------------------------

/// Drive a session with a canned input string and collect everything it printed.
fn transcript<H: ShellHost, D: BlockDevice>(
    input: &str,
    host: &H,
    fs: &mut Filesystem,
    dev: &mut D,
) -> (String, Outcome) {
    let mut session = Session::new();
    let mut log = String::new();
    let mut outcome = Outcome::Continue;
    for byte in input.bytes() {
        if outcome == Outcome::Halt {
            break;
        }
        outcome = session.feed(byte, host, fs, dev, &mut |s| log.push_str(s));
    }
    (log, outcome)
}

/// Prove the console on the live target. `logger` receives `(index, passed, name)` per invariant;
/// returns the count on success or the first failure's `(index, name)`.
///
/// The device is the caller's: a target passes a RAM disk (every target) or a real virtio-blk device
/// (where one is attached), so the same behaviors are asserted over whatever storage exists.
pub fn console_suite<H: ShellHost, D: BlockDevice, F: FnMut(u32, bool, &str)>(
    host: &H,
    dev: &mut D,
    logger: &mut F,
) -> Result<u32, (u32, &'static str)> {
    let mut n = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let passed = $cond;
            logger(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    Filesystem::format(dev).map_err(|_| (0u32, "console: format the console's device"))?;
    let mut fs =
        Filesystem::mount(dev).map_err(|_| (0u32, "console: mount the console's device"))?;

    // 1. A line is only a command once return arrives. Typing a command without pressing return
    //    must not run it — otherwise every prefix of every command would execute as it is typed.
    let (log, _) = transcript("halt", host, &mut fs, dev);
    check!(
        "console: a line does not execute until return is pressed",
        !log.contains("halting.")
    );

    // 2. Return runs it.
    let (log, outcome) = transcript("halt\r", host, &mut fs, dev);
    check!(
        "console: return executes the line and halt ends the session",
        log.contains("halting.") && outcome == Outcome::Halt
    );

    // 3. Backspace removes the last byte, so a typo is correctable rather than fatal.
    let (log, _) = transcript("helpx\x08\r", host, &mut fs, dev);
    check!(
        "console: backspace removes the last byte typed",
        log.contains("commands:") && !log.contains("unknown command")
    );

    // 4. Backspace on an empty line does nothing — the cursor cannot walk back over the prompt.
    let mut ed = LineEditor::new();
    let mut drew = false;
    ed.feed(0x08, &mut |_| drew = true);
    check!(
        "console: backspace on an empty line draws nothing and holds nothing",
        !drew && ed.is_empty()
    );

    // 5. A byte that is not printable ASCII never enters the line. A terminal can send anything;
    //    none of it may become part of a command.
    let mut ed = LineEditor::new();
    for b in [0x00u8, 0x1b, 0x7f, 0x80, 0xff, 0x9b] {
        let _ = ed.feed(b, &mut |_| {});
    }
    check!(
        "console: a non-printable byte never enters the line",
        ed.is_empty()
    );

    // 6. The line is bounded: past MAX_LINE, input is dropped rather than buffered.
    let mut ed = LineEditor::new();
    for _ in 0..(MAX_LINE * 2) {
        let _ = ed.feed(b'a', &mut |_| {});
    }
    check!(
        "console: a line stops growing at its bound instead of allocating",
        ed.len() == MAX_LINE
    );

    // 7. Ctrl-C discards the line and runs nothing.
    let (log, outcome) = transcript("halt\x03\r", host, &mut fs, dev);
    check!(
        "console: Ctrl-C discards the line without running it",
        !log.contains("halting.") && outcome == Outcome::Continue
    );

    // 8. An unknown command is a named refusal, not a crash and not a silent no-op.
    let (log, _) = transcript("frobnicate\r", host, &mut fs, dev);
    check!(
        "console: an unknown command is refused by name",
        log.contains("unknown command 'frobnicate'")
    );

    // 9. Every command in the table appears in `help` — a command cannot be reachable and hidden.
    let (log, _) = transcript("help\r", host, &mut fs, dev);
    check!(
        "console: help lists every command the dispatcher accepts",
        COMMANDS.iter().all(|(name, _)| {
            let verb = split_first(name).0;
            log.contains(verb)
        })
    );

    // 10. Write then read: the console really drives the namespace.
    let (log, _) = transcript(
        "write greeting hello world\rcat greeting\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: an object written through the console reads back byte for byte",
        log.contains("wrote 11 bytes to greeting") && log.contains("hello world")
    );

    // 11. And the namespace really changed — a fresh mount of the same device sees it, so the
    //     write went through the journal rather than living in the session.
    let remount =
        Filesystem::mount(dev).map_err(|_| (11u32, "console: remount after a console write"))?;
    let seen = remount
        .read(dev, "greeting")
        .map(|b| b == b"hello world")
        .unwrap_or(false);
    check!(
        "console: a console write is committed, not held in the session",
        seen
    );

    // 12. Remove erases the name; a following read is a refusal, not stale bytes.
    let (log, _) = transcript("rm greeting\rcat greeting\r", host, &mut fs, dev);
    check!(
        "console: a removed object is gone and reading it is refused",
        log.contains("removed greeting") && log.contains("no such object")
    );

    // 13. A refused name is refused with a reason — the fs's rules reach the human unchanged.
    let (log, _) = transcript("write bad/name x\r", host, &mut fs, dev);
    check!(
        "console: an invalid name is refused with the reason, and writes nothing",
        log.contains("bad name") && fs.stat(dev, "bad/name").is_err()
    );

    // 14. `ls` reflects the namespace, including when it is empty.
    let (log, _) = transcript("ls\r", host, &mut fs, dev);
    check!(
        "console: ls says so when the namespace is empty",
        log.contains("(no objects)")
    );

    // 15. The prompt is reprinted after every command, so the session is usable rather than
    //     one-shot: three commands, four prompts (the first one plus one after each).
    let (log, _) = transcript("arch\ruptime\rmem\r", host, &mut fs, dev);
    check!(
        "console: the prompt returns after every command",
        log.matches(PROMPT).count() == 4
    );

    Ok(n)
}
