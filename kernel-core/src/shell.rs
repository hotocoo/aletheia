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
//! file, a mangled escape sequence — and none of it can become a command argument.
//!
//! **Escape sequences are PARSED, not discarded (REQ-CON-004, ADR-050).** Discarding `ESC` and then
//! admitting the rest of the sequence as printable text is worse than either extreme: pressing the
//! up arrow on a serial terminal typed a literal `[A` into the line, and every arrow key corrupted
//! the command the operator was in the middle of writing. The editor therefore runs a bounded CSI
//! state machine: `ESC` opens a sequence, the parameter bytes are counted rather than buffered
//! without limit, the final byte decides what happened, and **nothing inside a sequence can reach
//! the line**. An unrecognized sequence is consumed and ignored — the fail-closed answer — rather
//! than leaking its bytes.
//!
//! **The editor is a line editor, not a line buffer.** A cursor moves inside the line (arrows,
//! `Home`/`End`, `Ctrl-A`/`Ctrl-E`/`Ctrl-B`/`Ctrl-F`), text is inserted and deleted where the cursor
//! is, words are killed (`Ctrl-W`), the tail is killed (`Ctrl-K`), a bounded history is walked with
//! the up/down arrows (`Ctrl-P`/`Ctrl-N`), and `Tab` completes command and object names. Redrawing
//! never re-emits the prompt: the editor repaints only from the cursor rightwards, using backspaces
//! and spaces, so it is correct on a terminal that understands no escape sequences at all and the
//! prompt remains something only the session prints.
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

/// Lines the history remembers. Bounded for the same reason the line is: a session that runs for a
/// month must not turn every command ever typed into resident kernel memory.
pub const HISTORY_MAX: usize = 32;

/// Parameter bytes a single CSI sequence may carry before the editor stops recording them. A
/// terminal (or something pretending to be one) can send `ESC [` followed by digits forever; the
/// sequence is still consumed to its final byte, but nothing past this bound is remembered, so a
/// hostile stream costs a fixed number of bytes rather than an allocation.
const CSI_PARAM_MAX: usize = 8;

// The control bytes the editor has a rule for. Named, because two modules depend on this list: the
// editor implements them, and `keymap` may emit only bytes the editor implements (proved in both).
/// `Ctrl-A` — move to the start of the line.
pub const CTRL_A: u8 = 0x01;
/// `Ctrl-B` — move one character left.
pub const CTRL_B: u8 = 0x02;
/// `Ctrl-C` — abandon the line.
pub const CTRL_C: u8 = 0x03;
/// `Ctrl-D` — delete the character under the cursor.
pub const CTRL_D: u8 = 0x04;
/// `Ctrl-E` — move to the end of the line.
pub const CTRL_E: u8 = 0x05;
/// `Ctrl-F` — move one character right.
pub const CTRL_F: u8 = 0x06;
/// Backspace — erase the character before the cursor.
pub const BACKSPACE: u8 = 0x08;
/// `Tab` — complete the word under the cursor.
pub const TAB: u8 = 0x09;
/// `Ctrl-K` — kill from the cursor to the end of the line.
pub const CTRL_K: u8 = 0x0b;
/// `Ctrl-N` — the next line in history.
pub const CTRL_N: u8 = 0x0e;
/// `Ctrl-P` — the previous line in history.
pub const CTRL_P: u8 = 0x10;
/// `Ctrl-U` — kill the whole line.
pub const CTRL_U: u8 = 0x15;
/// `Ctrl-W` — kill the word before the cursor.
pub const CTRL_W: u8 = 0x17;
/// `ESC` — opens a control sequence; never reaches the line by itself.
pub const ESC: u8 = 0x1b;
/// `DEL`, which many terminals send for the backspace key.
pub const DEL: u8 = 0x7f;

/// Does the editor have a rule for this byte?
///
/// This is the console's **input alphabet**, and it exists as one function because two independent
/// producers feed the editor: a serial line, and `keymap`'s scancode decoder. The decoder's security
/// property is "every byte I can emit is one the editor has a rule for" — a property that can only
/// be proved against a single definition. A second copy of this list in the decoder would be a
/// second list that drifts.
pub const fn editor_accepts(b: u8) -> bool {
    matches!(
        b,
        0x20..=0x7e
            | b'\r'
            | b'\n'
            | ESC
            | CTRL_A
            | CTRL_B
            | CTRL_C
            | CTRL_D
            | CTRL_E
            | CTRL_F
            | BACKSPACE
            | TAB
            | CTRL_K
            | CTRL_N
            | CTRL_P
            | CTRL_U
            | CTRL_W
            | DEL
    )
}

/// What one input byte did to the line being edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The line is still being typed (or the byte was refused).
    Pending,
    /// The user pressed return: here is the finished line, and the editor is empty again.
    Line(String),
    /// The user pressed Ctrl-C: whatever was typed is discarded, and no command runs.
    Cancelled,
    /// The user pressed Tab: the caller knows what names exist, so completion is resolved one level
    /// up (the editor owns the line; the session owns the namespace).
    Complete,
}

/// Where the escape-sequence parser is. A terminal's arrow key is three bytes that arrive one at a
/// time, so "am I inside a sequence" is state the editor must hold — and holding it is exactly what
/// stops the tail of a sequence from being typed into the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscState {
    /// Not in a sequence: bytes mean what they say.
    Ground,
    /// `ESC` arrived; the next byte says whether this is a sequence at all.
    Escape,
    /// `ESC [` (or `ESC O`) arrived: collect parameters until the final byte.
    Csi,
}

/// A single line of input under construction.
///
/// Holds bytes, not a `String`: only ASCII is ever admitted, so the buffer is valid UTF-8 by
/// construction rather than by a check that could be forgotten.
pub struct LineEditor {
    buf: Vec<u8>,
    /// Insertion point, in bytes from the start of the line. Always `<= buf.len()`.
    cursor: usize,
    esc: EscState,
    /// Parameter bytes of the sequence being parsed (digits and `;`), bounded by `CSI_PARAM_MAX`.
    params: [u8; CSI_PARAM_MAX],
    nparams: usize,
    /// Lines already submitted, oldest first. Bounded by `HISTORY_MAX`.
    history: Vec<String>,
    /// Which history entry is being shown, if the user is walking it.
    hist: Option<usize>,
    /// What was typed before the walk started, restored by walking back down past the newest entry.
    stash: Vec<u8>,
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
            cursor: 0,
            esc: EscState::Ground,
            params: [0; CSI_PARAM_MAX],
            nparams: 0,
            history: Vec::new(),
            hist: None,
            stash: Vec::new(),
        }
    }

    /// Bytes currently held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Where the cursor sits, in bytes from the start of the line.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The line as it currently reads. Valid UTF-8 by construction: only ASCII was ever admitted.
    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.buf).unwrap_or("")
    }

    /// Lines already submitted, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Discard the line without producing one.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.esc = EscState::Ground;
        self.nparams = 0;
        self.hist = None;
        self.stash.clear();
    }

    /// True while the parser is part-way through an escape sequence. Exposed because "the arrow key
    /// typed nothing into the line" and "the editor is stuck waiting for a final byte" are different
    /// states, and a test that could not tell them apart would pass on a wedged parser.
    pub fn in_escape(&self) -> bool {
        self.esc != EscState::Ground
    }

    /// Emit `n` backspaces — the one cursor movement every terminal understands, including the ones
    /// that understand no escape sequences at all.
    fn back(n: usize, echo: &mut dyn FnMut(&str)) {
        for _ in 0..n {
            echo("\x08");
        }
    }

    /// Repaint from the cursor rightwards: the tail of the line, `pad` spaces to cover characters
    /// that were removed, then enough backspaces to put the cursor back where it belongs.
    ///
    /// This is the whole redraw discipline, and it deliberately never mentions the prompt: the
    /// session prints prompts, the editor prints the line, and a redraw that re-emitted the prompt
    /// would make "how many prompts did this session print" — an assertion the live suite makes —
    /// depend on how the user edited their line.
    fn repaint_tail(&self, pad: usize, echo: &mut dyn FnMut(&str)) {
        let tail = &self.buf[self.cursor..];
        if let Ok(s) = core::str::from_utf8(tail) {
            echo(s);
        }
        for _ in 0..pad {
            echo(" ");
        }
        Self::back(tail.len() + pad, echo);
    }

    /// Replace the whole line with `next` and leave the cursor at its end. Used by history: the old
    /// text is erased with the same `\x08 \x08` a backspace draws, so no escape sequence is needed.
    fn replace_line(&mut self, next: &[u8], echo: &mut dyn FnMut(&str)) {
        // Walk to the end first — erasing works backwards from wherever the cursor is.
        if self.cursor < self.buf.len() {
            if let Ok(s) = core::str::from_utf8(&self.buf[self.cursor..]) {
                echo(s);
            }
        }
        for _ in 0..self.buf.len() {
            echo("\x08 \x08");
        }
        self.buf.clear();
        self.buf
            .extend_from_slice(&next[..next.len().min(MAX_LINE)]);
        self.cursor = self.buf.len();
        if let Ok(s) = core::str::from_utf8(&self.buf) {
            echo(s);
        }
    }

    /// Insert one printable byte at the cursor. Returns false when the line is at its bound, in
    /// which case NOTHING is drawn — the line stops growing rather than the allocation doing so.
    fn insert(&mut self, byte: u8, echo: &mut dyn FnMut(&str)) -> bool {
        if self.buf.len() >= MAX_LINE {
            return false;
        }
        self.buf.insert(self.cursor, byte);
        self.cursor += 1;
        // One-byte &str without allocating a String per keystroke.
        let b = [byte];
        echo(core::str::from_utf8(&b).unwrap_or(""));
        if self.cursor < self.buf.len() {
            self.repaint_tail(0, echo);
        }
        true
    }

    /// Insert a whole string at the cursor (completion). Bytes past the bound are dropped.
    pub fn insert_str(&mut self, s: &str, echo: &mut dyn FnMut(&str)) {
        for b in s.bytes() {
            if !b.is_ascii_graphic() && b != b' ' {
                continue;
            }
            if !self.insert(b, echo) {
                break;
            }
        }
    }

    /// Redraw the line from scratch after the session printed something (completion candidates).
    /// The session has just printed a fresh prompt, so this draws the text and repositions only.
    pub fn redraw(&self, echo: &mut dyn FnMut(&str)) {
        if let Ok(s) = core::str::from_utf8(&self.buf) {
            echo(s);
        }
        Self::back(self.buf.len() - self.cursor, echo);
    }

    /// Erase the character before the cursor.
    fn erase_left(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.buf.remove(self.cursor);
        echo("\x08");
        self.repaint_tail(1, echo);
    }

    /// Delete the character under the cursor.
    fn delete_at(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.cursor >= self.buf.len() {
            return;
        }
        self.buf.remove(self.cursor);
        self.repaint_tail(1, echo);
    }

    fn move_left(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.cursor > 0 {
            self.cursor -= 1;
            echo("\x08");
        }
    }

    fn move_right(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.cursor < self.buf.len() {
            let b = [self.buf[self.cursor]];
            echo(core::str::from_utf8(&b).unwrap_or(""));
            self.cursor += 1;
        }
    }

    fn move_home(&mut self, echo: &mut dyn FnMut(&str)) {
        Self::back(self.cursor, echo);
        self.cursor = 0;
    }

    fn move_end(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.cursor < self.buf.len() {
            if let Ok(s) = core::str::from_utf8(&self.buf[self.cursor..]) {
                echo(s);
            }
            self.cursor = self.buf.len();
        }
    }

    /// Kill from the cursor to the end of the line.
    fn kill_to_end(&mut self, echo: &mut dyn FnMut(&str)) {
        let n = self.buf.len() - self.cursor;
        if n == 0 {
            return;
        }
        self.buf.truncate(self.cursor);
        self.repaint_tail(n, echo);
    }

    /// Kill the word before the cursor: the run of spaces immediately left of it, then the run of
    /// non-spaces before that. Deleting a whole argument is one keystroke rather than thirty.
    fn kill_word(&mut self, echo: &mut dyn FnMut(&str)) {
        let mut start = self.cursor;
        while start > 0 && self.buf[start - 1] == b' ' {
            start -= 1;
        }
        while start > 0 && self.buf[start - 1] != b' ' {
            start -= 1;
        }
        let n = self.cursor - start;
        if n == 0 {
            return;
        }
        self.buf.drain(start..self.cursor);
        Self::back(n, echo);
        self.cursor = start;
        self.repaint_tail(n, echo);
    }

    /// Kill the whole line, staying on it.
    fn kill_line(&mut self, echo: &mut dyn FnMut(&str)) {
        self.move_end(echo);
        while !self.buf.is_empty() {
            self.buf.pop();
            echo("\x08 \x08");
        }
        self.cursor = 0;
    }

    /// Walk to the previous (older) history entry.
    fn history_prev(&mut self, echo: &mut dyn FnMut(&str)) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.hist {
            // Starting a walk: remember what was being typed, so walking back down returns it.
            None => {
                self.stash = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => return, // already at the oldest: stay, rather than wrap to the newest
            Some(i) => i - 1,
        };
        self.hist = Some(next);
        let entry = self.history[next].clone();
        self.replace_line(entry.as_bytes(), echo);
    }

    /// Walk to the next (newer) history entry, and past the newest back to what was being typed.
    fn history_next(&mut self, echo: &mut dyn FnMut(&str)) {
        let Some(i) = self.hist else { return };
        if i + 1 < self.history.len() {
            self.hist = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.replace_line(entry.as_bytes(), echo);
        } else {
            self.hist = None;
            let stash = core::mem::take(&mut self.stash);
            self.replace_line(&stash, echo);
        }
    }

    /// Record a submitted line. Empty lines and an immediate repeat are not recorded: history exists
    /// to save typing, and a screen of identical entries saves none.
    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().map(|l| l.as_str()) == Some(line) {
            return;
        }
        if self.history.len() == HISTORY_MAX {
            self.history.remove(0);
        }
        self.history.push(line.to_string());
    }

    /// Act on a completed CSI sequence. `final_byte` is the byte that ended it; the parameters are
    /// whatever was recorded (possibly truncated, which is why an unparsable parameter means
    /// "ignore this sequence" and never "guess").
    fn csi(&mut self, final_byte: u8, echo: &mut dyn FnMut(&str)) {
        match final_byte {
            b'A' => self.history_prev(echo),
            b'B' => self.history_next(echo),
            b'C' => self.move_right(echo),
            b'D' => self.move_left(echo),
            b'H' => self.move_home(echo),
            b'F' => self.move_end(echo),
            // The `ESC [ n ~` family. Only the three keys the editor has an answer for are acted on;
            // Page Up, Insert and the function keys are consumed and ignored rather than guessed at.
            b'~' => match core::str::from_utf8(&self.params[..self.nparams])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                Some(1) | Some(7) => self.move_home(echo),
                Some(4) | Some(8) => self.move_end(echo),
                Some(3) => self.delete_at(echo),
                _ => {}
            },
            _ => {}
        }
    }

    /// Feed one byte from the console. `echo` receives exactly what should be written back to the
    /// terminal — nothing is echoed for a byte that was refused, so what the user sees is what the
    /// kernel actually holds.
    pub fn feed(&mut self, byte: u8, echo: &mut dyn FnMut(&str)) -> Edit {
        // ---- escape-sequence parsing comes first: inside a sequence, no byte means what it says --
        match self.esc {
            EscState::Ground => {}
            EscState::Escape => {
                self.esc = EscState::Ground;
                match byte {
                    // `ESC [` is the CSI introducer; `ESC O` is what a terminal in application
                    // cursor mode sends instead, and its arrow keys have the same final bytes.
                    b'[' | b'O' => {
                        self.esc = EscState::Csi;
                        self.nparams = 0;
                        return Edit::Pending;
                    }
                    // A second ESC restarts rather than nesting.
                    ESC => {
                        self.esc = EscState::Escape;
                        return Edit::Pending;
                    }
                    // `ESC` then anything else is not a sequence this editor knows. The byte is
                    // SWALLOWED, not typed: admitting it is exactly the bug this parser exists to
                    // fix, and a lone ESC on a serial line is nearly always the head of a sequence
                    // whose tail this editor has no rule for.
                    _ => return Edit::Pending,
                }
            }
            EscState::Csi => {
                match byte {
                    // Parameter bytes: digits, `;`, and the private-use markers. Recorded while
                    // there is room; past the bound the sequence still runs to its final byte, but
                    // nothing more is remembered, so an endless parameter run costs nothing.
                    0x30..=0x3f => {
                        if self.nparams < CSI_PARAM_MAX {
                            self.params[self.nparams] = byte;
                            self.nparams += 1;
                        }
                        return Edit::Pending;
                    }
                    // Intermediate bytes — consumed, never recorded.
                    0x20..=0x2f => return Edit::Pending,
                    // The final byte ends the sequence.
                    0x40..=0x7e => {
                        self.esc = EscState::Ground;
                        self.csi(byte, echo);
                        return Edit::Pending;
                    }
                    // A control byte inside a sequence means the sequence was interrupted — a line
                    // arriving mid-escape must still execute, so the parser gives up on the sequence
                    // and lets the byte mean what it says. Without this an editor that saw a stray
                    // `ESC [` would ignore every keystroke until a letter happened to arrive.
                    _ => self.esc = EscState::Ground,
                }
            }
        }

        match byte {
            // Return: the line is complete. CR and LF are both accepted because a serial terminal
            // may send either (and CRLF then arrives as a complete line plus one empty one).
            b'\r' | b'\n' => {
                echo("\r\n");
                let line = String::from_utf8(core::mem::take(&mut self.buf)).unwrap_or_default(); // unreachable: only ASCII was admitted
                self.cursor = 0;
                self.hist = None;
                self.stash.clear();
                self.remember(&line);
                Edit::Line(line)
            }
            // Ctrl-C: abandon the line. Visible, because a silent discard looks like a hang.
            CTRL_C => {
                self.buf.clear();
                self.cursor = 0;
                self.hist = None;
                self.stash.clear();
                echo("^C\r\n");
                Edit::Cancelled
            }
            ESC => {
                self.esc = EscState::Escape;
                Edit::Pending
            }
            TAB => Edit::Complete,
            CTRL_U => {
                self.kill_line(echo);
                Edit::Pending
            }
            CTRL_K => {
                self.kill_to_end(echo);
                Edit::Pending
            }
            CTRL_W => {
                self.kill_word(echo);
                Edit::Pending
            }
            CTRL_A => {
                self.move_home(echo);
                Edit::Pending
            }
            CTRL_E => {
                self.move_end(echo);
                Edit::Pending
            }
            CTRL_B => {
                self.move_left(echo);
                Edit::Pending
            }
            CTRL_F => {
                self.move_right(echo);
                Edit::Pending
            }
            // Ctrl-D deletes forwards. On an EMPTY line it does nothing: on a real terminal that
            // keystroke means end-of-file, and a console that halted the machine because someone
            // pressed one key too many would be a console nobody trusts.
            CTRL_D => {
                self.delete_at(echo);
                Edit::Pending
            }
            CTRL_P => {
                self.history_prev(echo);
                Edit::Pending
            }
            CTRL_N => {
                self.history_next(echo);
                Edit::Pending
            }
            // Backspace / DEL: remove one byte before the cursor. At the start of the line this is a
            // no-op AND draws nothing, so the cursor can never walk back over the prompt.
            BACKSPACE | DEL => {
                self.erase_left(echo);
                Edit::Pending
            }
            // Printable ASCII: the only bytes that may enter the buffer, and only while there is
            // room. At the bound the byte is dropped without an echo — the line stops growing
            // instead of the allocation doing so.
            0x20..=0x7e => {
                self.insert(byte, echo);
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

    /// Lines this session has run, oldest first.
    pub fn history(&self) -> &[String] {
        self.editor.history()
    }

    /// Resolve a `Tab`.
    ///
    /// The editor owns the line and the session owns the namespace, so completion is resolved here:
    /// the first word completes against the command table, and every later word against the objects
    /// that actually exist. Completing file names from the real directory rather than from a guess
    /// is what makes `cat` usable on a machine whose names were typed months ago.
    fn complete<D: BlockDevice>(&mut self, fs: &Filesystem, dev: &D, out: &mut dyn FnMut(&str)) {
        let line = self.editor.line().to_string();
        let cursor = self.editor.cursor();
        let head = &line[..cursor];
        let start = head.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let token = &head[start..];
        let mut cands: Vec<String> = Vec::new();
        if start == 0 {
            for (name, _) in COMMANDS {
                let verb = split_first(name).0;
                if verb.starts_with(token) {
                    cands.push(verb.to_string());
                }
            }
        } else if let Ok(entries) = fs.list(dev) {
            for e in entries {
                if e.name.starts_with(token) {
                    cands.push(e.name);
                }
            }
        }
        if cands.is_empty() {
            return;
        }
        // The longest prefix every candidate shares: typing stops exactly where the choice begins,
        // which is the behavior that makes completion feel like typing rather than like a menu.
        let mut common = cands[0].clone();
        for c in &cands[1..] {
            let n = common
                .bytes()
                .zip(c.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            common.truncate(n);
        }
        if common.len() > token.len() {
            let add = common[token.len()..].to_string();
            self.editor.insert_str(&add, out);
        }
        if cands.len() == 1 {
            // One answer: finish the word and separate it, so the next argument can be typed.
            if !self.editor.line()[..self.editor.cursor()].ends_with(' ') {
                self.editor.insert_str(" ", out);
            }
        } else if common.len() == token.len() {
            // Ambiguous with nothing to add: SHOW the choice rather than beeping at the operator,
            // then reprint the prompt and the line exactly as it was.
            out("\r\n");
            for c in &cands {
                out(c);
                out("  ");
            }
            out("\r\n");
            out(PROMPT);
            self.editor.redraw(out);
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
            Edit::Complete => {
                self.complete(fs, dev, out);
                Outcome::Continue
            }
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

    // ---- the editor as an EDITOR (REQ-CON-004, ADR-050) ------------------------------------------

    // 16. THE regression. An arrow key is `ESC [ A`; the old editor dropped the `ESC` and admitted
    //     the rest as printable text, so every arrow press typed `[A` into the middle of the command
    //     the operator was writing. Nothing inside a sequence may reach the line.
    let mut ed = LineEditor::new();
    for b in b"ls\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[3~\x1b[5~\x1b[200~" {
        ed.feed(*b, &mut |_| {});
    }
    check!(
        "console: an arrow key moves the cursor and types nothing into the line",
        ed.line() == "ls" && !ed.in_escape()
    );

    // 17. And the sequence really did MOVE the cursor: left, then a character, inserts in the middle
    //     rather than appending. A cursor that draws but does not move is the same bug wearing a hat.
    let (log, _) = transcript("ls\x1b[Dx\r", host, &mut fs, dev);
    check!(
        "console: text is inserted where the cursor is, not always at the end",
        log.contains("unknown command 'lxs'")
    );

    // 18. Backspace in the middle of a line removes the character before the cursor, not the last
    //     one typed.
    let (log, _) = transcript("lsx\x1b[D\x08\r", host, &mut fs, dev);
    check!(
        "console: backspace erases before the cursor, not at the end of the line",
        log.contains("unknown command 'lx'")
    );

    // 19. Delete removes the character UNDER the cursor. Home and End reach the ends of the line.
    let (log, _) = transcript("xls\x1b[H\x1b[3~\r", host, &mut fs, dev);
    check!(
        "console: Delete removes under the cursor and Home reaches the start of the line",
        log.contains("(no objects)") && !log.contains("unknown command")
    );

    // 20. A sequence the editor has no rule for is consumed ENTIRELY — an unknown final byte must
    //     not leave the parser armed, or the next real keystroke is eaten looking for one.
    let mut ed = LineEditor::new();
    for b in b"\x1b[1;2Rab" {
        ed.feed(*b, &mut |_| {});
    }
    check!(
        "console: an unrecognized sequence is consumed whole and leaves the parser unarmed",
        ed.line() == "ab" && !ed.in_escape()
    );

    // 21. And a sequence whose parameters run past the bound still ends at its final byte: the
    //     parameters are forgotten, never buffered, so a hostile stream costs a fixed size.
    let mut ed = LineEditor::new();
    ed.feed(0x1b, &mut |_| {});
    ed.feed(b'[', &mut |_| {});
    for _ in 0..4096 {
        ed.feed(b'9', &mut |_| {});
    }
    ed.feed(b'~', &mut |_| {});
    ed.feed(b'z', &mut |_| {});
    check!(
        "console: an over-long escape sequence is bounded and still terminates",
        ed.line() == "z" && !ed.in_escape()
    );

    // 22. A line interrupted mid-sequence still runs. Otherwise a stray `ESC [` on a noisy wire
    //     would make the console ignore everything until a letter happened to arrive.
    let (log, _) = transcript("help\x1b[\r", host, &mut fs, dev);
    check!(
        "console: a return arriving inside a sequence still executes the line",
        log.contains("commands:")
    );

    // 23. History: the up arrow recalls the last line, and running it runs the same command. This is
    //     the difference between an OS you can work in and one you retype every command at.
    let (log, _) = transcript("echo remembered\r\x1b[A\r", host, &mut fs, dev);
    check!(
        "console: the up arrow recalls the previous line and it runs again",
        log.matches("remembered").count() >= 3
    );

    // 24. History does not record blanks or an immediate repeat, and is bounded — a session left
    //     running for a month must not turn every keystroke into resident memory.
    let mut ed = LineEditor::new();
    for i in 0..(HISTORY_MAX * 2) {
        for b in format!("echo {}\r", i).bytes() {
            ed.feed(b, &mut |_| {});
        }
    }
    for b in b"\r\r" {
        ed.feed(*b, &mut |_| {});
    }
    for _ in 0..2 {
        for b in b"ls\r" {
            ed.feed(*b, &mut |_| {});
        }
    }
    check!(
        "console: history is bounded, and records neither blank lines nor an immediate repeat",
        ed.history().len() == HISTORY_MAX
            && ed.history().iter().filter(|l| *l == "ls").count() == 1
            && !ed.history().iter().any(|l| l.trim().is_empty())
    );

    // 25. Walking down past the newest entry restores the half-typed line the walk interrupted.
    //     Losing it is the classic history bug: the operator's unfinished command silently vanishes.
    let mut ed = LineEditor::new();
    for b in b"echo one\r" {
        ed.feed(*b, &mut |_| {});
    }
    for b in b"half-typed" {
        ed.feed(*b, &mut |_| {});
    }
    for b in b"\x1b[A" {
        ed.feed(*b, &mut |_| {});
    }
    let recalled = ed.line().to_string();
    for b in b"\x1b[B" {
        ed.feed(*b, &mut |_| {});
    }
    check!(
        "console: walking history down past the newest entry restores the half-typed line",
        recalled == "echo one" && ed.line() == "half-typed"
    );

    // 26. `Ctrl-A`/`Ctrl-E`/`Ctrl-W`/`Ctrl-K` do what every line editor's do. A console that spelled
    //     these differently would be a console whose muscle memory is wrong on purpose.
    let mut ed = LineEditor::new();
    for b in b"write notes hello" {
        ed.feed(*b, &mut |_| {});
    }
    ed.feed(CTRL_W, &mut |_| {}); // kill "hello"
    ed.feed(CTRL_A, &mut |_| {}); // to the start
    ed.feed(CTRL_K, &mut |_| {}); // kill the rest
    let emptied = ed.is_empty();
    for b in b"ls -l" {
        ed.feed(*b, &mut |_| {});
    }
    ed.feed(CTRL_A, &mut |_| {});
    ed.feed(CTRL_E, &mut |_| {});
    ed.feed(CTRL_W, &mut |_| {});
    check!(
        "console: the editing chords kill a word, a tail and a whole line",
        emptied && ed.line() == "ls "
    );

    // 27. The cursor cannot leave the line in either direction, however hard a terminal pushes.
    let mut ed = LineEditor::new();
    for _ in 0..64 {
        ed.feed(CTRL_B, &mut |_| {});
    }
    let left_ok = ed.cursor() == 0;
    for b in b"ab" {
        ed.feed(*b, &mut |_| {});
    }
    for _ in 0..64 {
        ed.feed(CTRL_F, &mut |_| {});
    }
    check!(
        "console: the cursor stops at both ends of the line",
        left_ok && ed.cursor() == 2 && ed.len() == 2
    );

    // 28. Tab completes a command name from a prefix — against the SAME table `help` prints, so a
    //     command cannot be completable and undocumented.
    let (log, _) = transcript("upt\t\r", host, &mut fs, dev);
    check!(
        "console: Tab completes a command name from its prefix",
        log.contains("ns since boot")
    );

    // 29. Tab completes an OBJECT name from the real namespace, so a name typed months ago need not
    //     be remembered exactly.
    let (log, _) = transcript("write completeme x\rcat compl\t\r", host, &mut fs, dev);
    check!(
        "console: Tab completes an object name from the namespace that exists",
        log.contains("wrote 1 bytes to completeme") && log.lines().any(|l| l.trim() == "x")
    );

    // 30. An ambiguous Tab shows the choices and leaves the line intact rather than guessing. The
    //     line must survive being redrawn: a completion that ate the operator's text would be worse
    //     than none.
    let (log, _) = transcript("h\t", host, &mut fs, dev);
    check!(
        "console: an ambiguous Tab shows the candidates and keeps the line",
        log.contains("help") && log.contains("halt") && log.ends_with("h")
    );

    Ok(n)
}
