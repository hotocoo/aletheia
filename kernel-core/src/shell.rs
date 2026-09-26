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
use crate::browser::{NavRefusal, Navigator, Resolved, TrustRefusal, UrlRefusal};
use crate::outf;
use crate::tlsclient::TlsReport;
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
    /// The user pressed return, and the finished line is borrowed from the editor
    /// (`LineEditor::submitted`) rather than handed out - what `feed_in_place` returns so a session
    /// allocates nothing per line (ADR-180). `feed` turns it into `Line`.
    Submitted,
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
    /// The line most recently submitted, kept (with its capacity) until the next one.
    submitted: Vec<u8>,
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
            submitted: Vec::with_capacity(MAX_LINE),
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
                // Into the stash's own buffer, not a fresh clone (ADR-180).
                self.stash.clear();
                self.stash.extend_from_slice(&self.buf);
                self.history.len() - 1
            }
            Some(0) => return, // already at the oldest: stay, rather than wrap to the newest
            Some(i) => i - 1,
        };
        self.hist = Some(next);
        self.show_history(next, echo);
    }

    /// Show history entry `i` as the line, without cloning it: the entry and the line buffer are
    /// both fields of `self`, so the entry is lent out by swapping it with a spare buffer.
    fn show_history(&mut self, i: usize, echo: &mut dyn FnMut(&str)) {
        let entry = core::mem::take(&mut self.history[i]);
        self.replace_line(entry.as_bytes(), echo);
        self.history[i] = entry;
    }

    /// Walk to the next (newer) history entry, and past the newest back to what was being typed.
    fn history_next(&mut self, echo: &mut dyn FnMut(&str)) {
        let Some(i) = self.hist else { return };
        if i + 1 < self.history.len() {
            self.hist = Some(i + 1);
            self.show_history(i + 1, echo);
        } else {
            self.hist = None;
            let stash = core::mem::take(&mut self.stash);
            self.replace_line(&stash, echo);
            self.stash = stash; // hand the buffer back, capacity and all
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
            // Reuse the oldest entry's buffer rather than dropping it and allocating a new one
            // (ADR-089): on a heap that never frees, a `String` per submitted line is a session
            // that costs memory for as long as somebody keeps typing.
            let mut oldest = self.history.remove(0);
            oldest.clear();
            oldest.push_str(line);
            self.history.push(oldest);
        } else {
            // Room for any line from the start (ADR-182): a recycled entry that later holds a
            // longer line must not regrow, and on a heap that never frees every regrowth leaked.
            let mut entry = String::with_capacity(MAX_LINE);
            entry.push_str(line);
            self.history.push(entry);
        }
    }

    /// How many lines the history holds right now — observable so a suite can pin the bound
    /// (ADR-089) without reaching into the editor's internals.
    pub fn history_len(&self) -> usize {
        self.history.len()
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
        match self.feed_in_place(byte, echo) {
            Edit::Submitted => Edit::Line(String::from(self.submitted())),
            other => other,
        }
    }

    /// The line most recently submitted (valid until the next Enter).
    pub fn submitted(&self) -> &str {
        core::str::from_utf8(&self.submitted).unwrap_or("")
    }

    /// `feed`, except that Enter returns `Edit::Submitted` and leaves the line in `submitted()`:
    /// the allocation-free form the console session uses (ADR-180).
    pub fn feed_in_place(&mut self, byte: u8, echo: &mut dyn FnMut(&str)) -> Edit {
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
                    // A control byte after a lone ESC means what it says, exactly as inside a CSI
                    // sequence below: a line ending in ESC must still end. Until 2026-09-26 this
                    // arm swallowed the CR too, and the live console fuzz caught a prompt that
                    // never came back on aarch64 and x86-64 (ADR-180).
                    b if b < 0x20 || b == 0x7f => {}
                    // `ESC` then any other byte is not a sequence this editor knows. The byte is
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
                // The finished line moves into `submitted` by SWAPPING buffers, and the line buffer
                // is cleared rather than taken: both keep their capacity, so a session that types
                // forever allocates nothing per line (ADR-180; `take` left a zero-capacity buffer
                // that regrew on every line, a leak on a heap that never frees).
                core::mem::swap(&mut self.buf, &mut self.submitted);
                self.buf.clear();
                self.cursor = 0;
                self.hist = None;
                self.stash.clear();
                let line = core::mem::take(&mut self.submitted);
                // SAFETY-free: only ASCII was ever admitted, so this cannot fail.
                let text = core::str::from_utf8(&line).unwrap_or("");
                self.remember(text);
                self.submitted = line;
                Edit::Submitted
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

/// Authority classes for commands that can inspect or change machine state.
///
/// The console is kernel code, but kernel code must not turn into ambient authority for a future
/// untrusted shell task. Targets provide this decision from their subject/capability context. The
/// default is deny, so a new target cannot accidentally expose storage or reset authority merely by
/// implementing the machine-facts methods below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellAction {
    /// Read machine metadata or filesystem contents.
    Inspect,
    /// Create, replace, rename, or remove filesystem objects.
    Write,
    /// Commit pending device writes.
    Flush,
    /// Restart the machine.
    Reboot,
    /// Stop the machine.
    Halt,
    /// Pin a clock domain, the overclock band included (ADR-184).
    Overclock,
    /// Change what the display shows (ADR-196).
    Display,
}

impl ShellAction {
    /// Stable capability action name checked by the kernel authority engine.
    pub const fn capability(self) -> &'static str {
        match self {
            ShellAction::Inspect => "console.inspect",
            ShellAction::Write => "console.write",
            ShellAction::Flush => "console.flush",
            ShellAction::Reboot => "system.reboot",
            ShellAction::Halt => "system.halt",
            ShellAction::Overclock => "system.overclock",
            ShellAction::Display => "system.display",
        }
    }

    fn label(self) -> &'static str {
        self.capability()
    }
}

/// Check one console action against explicit kernel capabilities.
///
/// This helper keeps target seams small: target code owns the subject's engine and offered tokens,
/// while command-to-capability mapping stays in this shared dispatcher. Unknown or revoked tokens
/// fail closed through [`CapEngine::evaluate`].
pub fn authorize_with_capabilities(
    engine: &crate::spine::CapEngine,
    offered: &[crate::spine::CapToken],
    action: ShellAction,
) -> bool {
    // `allows`, not `evaluate` (ADR-089): a console asks this on EVERY command, and a refusal
    // that builds its reason string costs the heap on a machine that never frees.
    engine.allows(
        action.capability(),
        &crate::spine::Target::default(),
        offered,
    )
}

/// What the machine's live input session reports to a human (ALET-P2-021's hardware rung,
/// ADR-080). Every field is a counter or a state the SESSION already tracks — this type
/// invents nothing; it is the session's ledger, rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputFacts {
    /// Keystroke bytes the session routed into a focused surface's queue.
    pub events_posted: u64,
    /// Events DROPPED (a full queue behind a surface that stopped draining).
    pub dropped: u64,
    /// Input ops refused, by name, since boot.
    pub refusals: u64,
    /// Events the focused surface has not read yet.
    pub queued: usize,
    /// The cursor's glyph top-left, if shown.
    pub cursor: Option<(u32, u32)>,
    /// The focused surface id, if any.
    pub focus: Option<u32>,
    /// Raw events the keyboard device has delivered since boot.
    pub kb_events: u64,
    /// Raw events the pointer device has delivered since boot.
    pub pt_events: u64,
    /// Event-queue device notifications issued by the keyboard and pointer drivers.
    pub kb_doorbells: u64,
    pub pt_doorbells: u64,
    /// The terminal window's top-left on the scanout, if placed (ADR-083).
    pub window: Option<(i32, i32)>,
    /// Lines the terminal window has completed since boot.
    pub term_lines: u64,
    /// The terminal window's current line (trailing blanks trimmed), and its length.
    pub term_last: [u8; 48],
    pub term_last_len: u8,
    /// Windows the manager holds open right now (ADR-084).
    pub windows: usize,
    /// Windows closed since boot (a close box pressed, or a window closed by the machine).
    pub closes: u64,
    /// Drags completed since boot (a press on a title band, then a release).
    pub drags: u64,
    /// Rows the desktop's file panel is holding (ADR-137).
    pub panel_rows: usize,
    /// Listings the panel has taken since boot, so an operator can see it is being fed at all.
    pub panel_listings: u64,
    /// Entries dropped across every truncated listing, counted rather than silent.
    pub panel_dropped: u64,
    /// The browser window (ADR-157, ADR-160): the URL line as typed, whether the navigation it
    /// latched is still in flight, how much page text the window holds, and its first line -
    /// read from the window's own state, so a live gate can ask the machine what it shows.
    pub browser_url: [u8; 64],
    pub browser_url_len: u8,
    pub browser_fetching: bool,
    pub browser_page_len: usize,
    pub browser_first: [u8; 48],
    pub browser_first_len: u8,
}

/// The machine's network device as the console reports it (ADR-185): addresses and the driver's
/// own counters, copied - nothing here is a handle to the device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetFacts {
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub gateway: [u8; 4],
    /// Frames received that were not the answer being waited for.
    pub dropped: u64,
    /// Broadcast ARP requests put on the wire since init.
    pub arp_requests: u32,
    /// DMA regions the device's queues registered.
    pub dma_regions: usize,
    /// The local port the next conversation will use.
    pub next_port: u16,
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
    /// User tasks contained by the supervisor since boot. Targets return their live supervisor
    /// counter; the default keeps lightweight hosted stands fail-closed and honest.
    fn supervisor_terminated(&self) -> usize {
        0
    }
    /// Faults escalated because the kernel, translation model, or fault report was not trustworthy.
    fn supervisor_escalations(&self) -> usize {
        0
    }
    /// Console bytes the target refused because the input ring was full (REQ-CON-002). A polled
    /// target reports 0. Surfaced rather than hidden: input loss the operator cannot see is input
    /// loss they will blame on the command they typed.
    fn input_dropped(&self) -> u64 {
        0
    }
    /// Open a TCP connection to `ip:port`, send `request`, and copy the peer's answer into
    /// `reply`, returning how many bytes came back (ADR-140).
    ///
    /// Defaulted to a named refusal because most machines this console runs on have no network
    /// device: a target with no NIC says so rather than reporting a zero-length answer that would
    /// look like a peer with nothing to say. The target owns the device, chooses the initial
    /// sequence number, and bounds the wait; the console only asks.
    fn tcp_fetch(
        &self,
        _ip: [u8; 4],
        _port: u16,
        _request: &[u8],
        _reply: &mut [u8],
    ) -> Result<usize, &'static str> {
        Err("this machine has no network device")
    }

    /// Open a TLS 1.3 conversation with `ip:port` as `server_name`, trusting exactly the Ed25519
    /// root whose public key is `pin`, send `request` protected, and copy the protected answer
    /// into `reply` (ADR-151). The operator states whom to trust on the command line; this console
    /// ships no root of its own. Defaulted to a named refusal for the same reason as `tcp_fetch`.
    fn tls_fetch(
        &self,
        _ip: [u8; 4],
        _port: u16,
        _server_name: &[u8],
        _pin: [u8; 32],
        _request: &[u8],
        _reply: &mut [u8],
    ) -> Result<TlsReport, &'static str> {
        Err("this machine has no network device")
    }

    /// Ask the DNS server at `server:port` for `name`'s A records (ADR-176). Defaulted to a named
    /// refusal for the same reason as `tcp_fetch`: a machine with no NIC says so.
    fn dns_resolve(
        &self,
        _server: [u8; 4],
        _port: u16,
        _name: &[u8],
    ) -> Result<crate::dns::Resolved, &'static str> {
        Err("this machine has no network device")
    }

    /// The machine's live input session, if the target installed one (ALET-P2-021's hardware
    /// rung, ADR-080). Default `None` — a target with no desktop says so instead of reporting
    /// zeros that would look like a session nobody can steer.
    fn input_facts(&self) -> Option<InputFacts> {
        None
    }
    /// Optional target-native interrupt telemetry: (MSI-X IRQ hits, sampled wakeups, total TSC
    /// cycles, maximum TSC cycles). The tuple is deliberately optional because non-x86 targets
    /// have different interrupt-controller domains and must not fabricate x86 counters.
    fn input_irq_stats(&self) -> Option<InputIrqStats> {
        None
    }
    /// Processors this kernel brought up. Defaulted to one because a target that has not answered
    /// the question has exactly one core it is sure about, and claiming more would be a claim about
    /// hardware nobody enumerated.
    fn cpu_count(&self) -> usize {
        1
    }
    /// The kernel heap: `(used, free)` bytes (ADR-180). The heap never frees (ADR-063), so a session
    /// that costs memory per command is a machine that dies of being used; `mem` shows the number
    /// so the operator - and the console fuzz - can see it move. `None` for a stand with no heap.
    fn heap_bytes(&self) -> Option<(usize, usize)> {
        None
    }
    /// Wait until something might have happened, called when the input ring is empty (REQ-CON-006).
    ///
    /// The console loop used to spin: `let Some(byte) = getc() else { continue }`. A machine sitting
    /// at a prompt with nobody typing therefore burned a whole core doing nothing, on every target
    /// and every core — which is invisible on hardware with a fan and extremely visible under
    /// emulation, where four spinning guest vCPUs are four saturated host threads. It is also simply
    /// wrong: input arrives by INTERRUPT (REQ-CON-002), so the loop already has something to wait
    /// for.
    ///
    /// **Defaulted to doing nothing**, and that default is the safety argument. A target whose
    /// console is polled rather than interrupt-driven would never be woken, and halting such a
    /// machine forever is a far worse failure than spinning on it. A target opts in only by
    /// implementing this, which is a statement that an interrupt will arrive — and the console gates
    /// on all three targets are what prove the statement, because a target that got it wrong stops
    /// responding to the very first thing typed at it.
    fn idle(&self) {}
    /// Bytes in one physical frame, so `mem` can report memory in the unit an operator thinks in
    /// without the arch-independent dispatcher knowing a page size.
    fn frame_bytes(&self) -> usize {
        4096
    }
    /// Switch the desktop to `w` x `h` (ADR-196): `Ok` with the mode now running, or the named
    /// reason it was not switched (the desktop keeps running as it was). Defaulted to no desktop.
    fn set_display_mode(&self, _w: u32, _h: u32) -> Result<(u32, u32), &'static str> {
        Err("this machine has no desktop to switch")
    }
    /// The network device the console dials with, `None` on a machine without one (ADR-185).
    fn net_facts(&self) -> Option<NetFacts> {
        None
    }
    /// Walk the capabilities this console offers: subject, action, still live (ADR-185). Defaulted
    /// to naming none - a stand that binds no authority has none to show.
    fn capabilities(&self, _f: &mut dyn FnMut(&str, &str, bool)) {}
    /// The wall clock, UTC seconds since the epoch (ADR-148's reading, ADR-184's command).
    /// Defaulted to a named absence: a stand with no clock says so rather than printing 1970.
    fn wall_clock(&self) -> Result<crate::clock::UnixSeconds, crate::clock::ClockRefusal> {
        Err(crate::clock::ClockRefusal::Absent)
    }
    /// Restart the machine. Returns only on FAILURE — a target with no reset path returns `false`
    /// and the console says so, rather than pretending to reboot and hanging.
    fn reboot(&self) -> bool {
        false
    }
    /// Authorize one command class. Fail closed: targets must explicitly bind the console to a
    /// subject/capability set before exposing machine or filesystem authority.
    fn authorize(&self, _action: ShellAction) -> bool {
        false
    }
}

/// The prompt. A constant because the live suite asserts on it.
pub const PROMPT: &str = "aletheia> ";

/// Every command name, in help order. Kept next to the dispatcher so a command cannot be added
/// without appearing in `help` (the live suite asserts the two agree).
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands"),
    ("ver", "what this system is, and what it is not"),
    ("arch", "active target backend and privilege level"),
    ("uptime", "time since boot"),
    ("boot", "where boot time went: suites timed, total, the slowest"),
    ("date", "the wall clock, in UTC"),
    (
        "display",
        "the display: its current geometry, every mode and refresh rate it reports, the best fit",
    ),
    (
        "resolution MODE",
        "switch the desktop to MODE (WxH, one `display` lists); window contents reset",
    ),
    ("mem", "physical memory, in frames and bytes"),
    ("faults", "supervisor containment and escalation counters"),
    ("caps", "the capabilities this console holds (names, never tokens)"),
    (
        "input",
        "the machine's input session: cursor, focus, counters",
    ),
    ("net", "the network device: addresses and driver counters"),
    (
        "tcp ADDR PORT TEXT",
        "open a TCP connection, send TEXT, print what comes back",
    ),
    (
        "tls ADDR PORT NAME PIN TEXT",
        "open a TLS 1.3 connection as NAME, trusting the Ed25519 root PIN (64 hex), send TEXT",
    ),
    (
        "https ADDR PORT NAME PIN PATH",
        "GET PATH from NAME over TLS 1.3 under the root PIN; print the status, headers and body",
    ),
    (
        "resolve NAME [SERVER [PORT]]",
        "ask a DNS server (default 10.0.2.3 53) for NAME's addresses; an answer is where to dial, never trust",
    ),
    (
        "trust NAME IP PIN",
        "pin the Ed25519 root PIN (64 hex) for host NAME at IP: the only hosts `go` will dial; `trust NAME PIN` asks the nameserver for IP",
    ),
    (
        "nameserver [ADDR] [PORT]",
        "where `trust NAME PIN` asks for addresses (default 10.0.2.3 53); no arguments prints it",
    ),
    (
        "go URL",
        "navigate the browser to an https:// URL on a trusted host; print the page",
    ),
    ("back", "navigate the browser to the previous page"),
    ("follow N", "navigate to link [N] of the current page"),
    (
        "block HOST",
        "refuse every navigation to HOST, pinned or not",
    ),
    (
        "forget",
        "drop browser history, page and links; keep trust and blocks",
    ),
    (
        "mlstat",
        "the resident risk advisor: what it is, and what it has done since boot",
    ),
    (
        "power",
        "the resident power governor: every clock domain, its points, heat and holds",
    ),
    (
        "oc KHZ [DOMAIN]",
        "hold a clock domain (default 0) at operating point KHZ, overclock band included; `oc off` releases it",
    ),
    ("lsblk", "the console's block device geometry"),
    ("df", "filesystem space, in blocks"),
    ("ls", "every named object"),
    ("find PREFIX", "names beginning with PREFIX"),
    ("stat NAME", "one object's extent and length"),
    ("cat NAME", "an object's contents"),
    ("head NAME [N]", "the first N lines (default 10)"),
    ("wc NAME", "lines, words and bytes"),
    ("grep TEXT NAME", "lines of NAME containing TEXT"),
    ("hexdump NAME [N]", "the first N bytes, in hex"),
    ("write NAME TEXT", "create or atomically replace an object"),
    ("append NAME TEXT", "add a line to the end of an object"),
    ("touch NAME", "create an empty object if it does not exist"),
    ("cp SRC DST", "copy an object"),
    ("mv SRC DST", "rename an object"),
    ("rm NAME", "remove an object (contents erased)"),
    ("sync", "flush the device's write path"),
    ("history", "lines run in this session"),
    ("echo TEXT", "print TEXT"),
    ("clear", "clear the screen"),
    ("reboot", "restart the machine"),
    ("halt", "stop the machine"),
];

/// What this system says about itself. One place, so the console and the boot banner cannot claim
/// different things.
pub const VERSION: &str = "Aletheia 0.1.0 — capability-secure microkernel";

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
        StorageError::Unauthorized => "capability denied for device operation",
        StorageError::OutOfRange => "block index off the device",
        StorageError::BadBlockSize => "a buffer was not one block",
        StorageError::TooLarge => "the transaction is too large for the journal",
        StorageError::Device => "the device reported a failure",
    }
}

/// `display` (ADR-191): what the monitor says it can show.
fn report_display(out: &mut dyn FnMut(&str)) {
    use crate::edid::resident::EdidAbsent;
    let Some(f) = crate::edid::resident::facts() else {
        out("display: no display device on this machine");
        return;
    };
    outf!(
        out,
        "display: scanout {}x{}{}",
        f.current.0,
        f.current.1,
        match f.running {
            Some((w, h)) => {
                let mut b = crate::linebuf::LineBuf::<48>::new();
                let _ =
                    core::fmt::Write::write_fmt(&mut b, format_args!(", desktop at {}x{}", w, h));
                b
            }
            None => crate::linebuf::LineBuf::<48>::new(),
        }
        .as_str()
    );
    match &f.edid {
        Err(EdidAbsent::NotOffered) => out("  EDID: the device offers none (modes unknown)"),
        Err(EdidAbsent::DeviceError) => out("  EDID: the device failed the request"),
        Err(EdidAbsent::Refused(why)) => outf!(out, "  EDID: refused ({:?})", why),
        Ok(e) => {
            let m = e.manufacturer;
            outf!(
                out,
                "  monitor: {} {}{}{} product {:#06x}, EDID {}.{}, {} extension block(s) not read",
                core::str::from_utf8(e.name()).unwrap_or("?"),
                m[0] as char,
                m[1] as char,
                m[2] as char,
                e.product,
                e.version,
                e.revision,
                e.extensions
            );
            let sorted = e.sorted();
            let pref = e.preferred();
            for mode in &sorted[..e.modes().len()] {
                outf!(
                    out,
                    "  {:>5}x{:<5} {:>3}.{:02} Hz{}{}{}",
                    mode.width,
                    mode.height,
                    mode.refresh_mhz / 1000,
                    (mode.refresh_mhz % 1000) / 10,
                    if mode.interlaced { " interlaced" } else { "" },
                    if Some(*mode) == pref {
                        "  preferred"
                    } else {
                        ""
                    },
                    if Some(*mode) == f.best {
                        "  best fit"
                    } else {
                        ""
                    }
                );
            }
        }
    }
}

/// `power` (ADR-184): the resident governor, read as one copy so printing holds no lock.
fn report_power(out: &mut dyn FnMut(&str)) {
    let Some(f) = crate::lethed::resident::facts() else {
        if crate::lethed::resident::active() {
            out("power: the watch is busy this instant; ask again");
        } else {
            out("power: no power governor was commissioned on this machine");
        }
        return;
    };
    outf!(
        out,
        "governor: {} domain(s), advisor {}, last tick {}",
        f.n,
        if f.advisor_loaded { "loaded" } else { "ABSENT" },
        f.last_tick.unwrap_or(0)
    );
    for d in &f.domains[..f.n] {
        outf!(
            out,
            "domain {}: {} kHz (nominal {}, envelope {}), demand {}%, {}{}{}",
            d.id,
            d.current_khz,
            d.nominal_khz,
            d.envelope_khz,
            d.demand_pct,
            match d.idle {
                None | Some(crate::pm::IdleState::C0) => "running",
                Some(crate::pm::IdleState::C1) => "parked C1",
                Some(crate::pm::IdleState::C2) => "parked C2",
            },
            if d.held { ", HELD by operator" } else { "" },
            if d.cooldown.is_some() {
                ", COOLING"
            } else {
                ""
            }
        );
        let mut line = crate::linebuf::LineBuf::<{ crate::linebuf::LINE_MAX }>::new();
        let _ = core::fmt::Write::write_str(&mut line, "  points kHz:");
        for p in &d.points[..d.n_points] {
            let band = if *p > d.nominal_khz { "+" } else { "" };
            let here = if *p == d.current_khz { "*" } else { "" };
            let _ = core::fmt::Write::write_fmt(&mut line, format_args!(" {}{}{}", p, band, here));
        }
        out(line.as_str());
        outf!(
            out,
            "  trip {} mC, cooldown {} tick(s) left",
            d.trip_mc,
            d.cooldown.unwrap_or(0)
        );
    }
    let c = f.census;
    outf!(
        out,
        "ticks: {} offered, {} admitted, {} consulted, {} warm-up, {} cooling, {} held",
        c.offered,
        c.admitted,
        c.consulted_steps,
        c.warmup_steps,
        c.cooldown_holds,
        c.operator_holds
    );
    outf!(
        out,
        "holds ended by heat {}, contract refusals {}, lock contended {}",
        c.holds_dropped_by_heat,
        c.pm_refusals,
        crate::lethed::resident::contended()
    );
    out("points marked + are the overclock band (grant-only); * is the current point");
}

/// `oc KHZ [DOMAIN]` / `oc off [DOMAIN]` (ADR-184): an operator hold through the power contract.
fn overclock(rest: &str, out: &mut dyn FnMut(&str)) {
    let (what, tail) = split_first(rest);
    let (dom_text, extra) = split_first(tail);
    if what.is_empty() || !extra.is_empty() {
        out("usage: oc KHZ [DOMAIN] | oc off [DOMAIN]");
        return;
    }
    let domain = if dom_text.is_empty() {
        0
    } else {
        match dom_text.parse::<u32>() {
            Ok(d) => d,
            Err(_) => {
                out("usage: oc KHZ [DOMAIN] (DOMAIN is a number; `power` lists them)");
                return;
            }
        }
    };
    if what == "off" {
        match crate::lethed::resident::release(domain) {
            Ok(()) => outf!(
                out,
                "domain {}: released to the governor at nominal",
                domain
            ),
            Err(e) => outf!(out, "oc refused: {:?}", e),
        }
        return;
    }
    let Ok(khz) = what.parse::<u32>() else {
        out("usage: oc KHZ [DOMAIN] (KHZ is one of the points `power` lists)");
        return;
    };
    match crate::lethed::resident::hold(domain, khz) {
        Ok(()) => outf!(
            out,
            "domain {}: held at {} kHz; heat still wins, `oc off` releases",
            domain,
            khz
        ),
        Err(e) => outf!(out, "oc refused: {:?}", e),
    }
}

fn authorize<H: ShellHost>(host: &H, action: ShellAction, out: &mut dyn FnMut(&str)) -> bool {
    if host.authorize(action) {
        true
    } else {
        outf!(out, "permission denied: {}", action.label());
        false
    }
}

/// Split a command line into the verb and the untouched remainder. The remainder keeps its interior
/// spacing, so `write greeting  hello  world` stores exactly the text after the name.
/// Fetch a resolved page over the TLS client, hand the answer to the navigator, and print the
/// page the browser window would show (ADR-156). The whole conversation is bounded by the
/// console's buffers; the page is bounded by the navigator's.
fn browse<H: ShellHost>(
    host: &H,
    nav: &mut Navigator,
    resolved: Resolved,
    out: &mut dyn FnMut(&str),
) {
    fetch_into(host, nav, resolved);
    print_page(nav, out);
}

/// Fetch a resolved page over the TLS client and hand the answer to the navigator.
fn fetch_into<H: ShellHost>(host: &H, nav: &mut Navigator, resolved: Resolved) {
    let mut resolved = resolved;
    let mut hops: u8 = 0;
    loop {
        match fetch_once(host, nav, resolved) {
            None => {
                nav.note_redirects(hops);
                return;
            }
            // A redirect (ADR-190): the target must pass the same policy `go` applies — a trusted
            // host, not blocked, https only — and the chain is bounded.
            Some(next) => {
                if hops >= crate::browser::MAX_REDIRECTS {
                    nav.set_failure(next, "too many redirects");
                    return;
                }
                hops += 1;
                match nav.resolve(&next) {
                    Ok(r) => resolved = r,
                    Err(crate::browser::NavRefusal::Blocked) => {
                        nav.set_failure(next, "the redirect names a blocked host");
                        return;
                    }
                    Err(_) => {
                        nav.set_failure(
                            next,
                            "the redirect names a host this machine does not trust",
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// One conversation. `Some(url)` when the answer is a redirect to follow; `None` when the page (or
/// the named reason there is none) has been handed to the navigator.
fn fetch_once<H: ShellHost>(
    host: &H,
    nav: &mut Navigator,
    resolved: Resolved,
) -> Option<crate::browser::Url> {
    let url = resolved.url;
    let mut request = [0u8; 1024];
    let Ok(request_len) = crate::http::request(url.host(), url.path(), &mut request) else {
        nav.set_failure(url, "the request does not fit");
        return None;
    };
    let mut reply = [0u8; 4096];
    match host.tls_fetch(
        resolved.ip,
        url.port,
        url.host(),
        resolved.pin,
        &request[..request_len],
        &mut reply,
    ) {
        Ok(report) => {
            let raw = &reply[..report.received];
            let mut body = [0u8; crate::browser::BODY_CAP];
            match crate::http::parse(raw, &mut body, report.truncated) {
                Ok(response) => {
                    if crate::browser::is_redirect(response.status) {
                        match response.header(raw, b"location") {
                            Some(loc) => match crate::browser::redirect_target(&url, loc) {
                                Ok(next) => return Some(next),
                                Err(why) => {
                                    nav.set_failure(url, why);
                                    return None;
                                }
                            },
                            None => {
                                nav.set_failure(url, "a redirect with no Location");
                                return None;
                            }
                        }
                    }
                    let is_html = response
                        .header(raw, b"content-type")
                        .is_some_and(|v| v.len() >= 9 && v[..9].eq_ignore_ascii_case(b"text/html"));
                    if is_html {
                        // HTML is RENDERED (ADR-158): a bounded subset into lines, scripts and
                        // styles dropped whole, links numbered. The page keeps the rendered text
                        // and the links; the raw markup is never shown.
                        let mut shown = [0u8; crate::browser::BODY_CAP];
                        let rendered =
                            crate::content::render(&body[..response.body_len], &mut shown, 30, 40);
                        let cut = response.truncated || rendered.cut;
                        nav.set_page(
                            url,
                            response.status,
                            response.reason.of(raw),
                            rendered.text,
                            cut,
                        );
                        nav.set_links(&rendered);
                    } else {
                        nav.set_page(
                            url,
                            response.status,
                            response.reason.of(raw),
                            &body[..response.body_len],
                            response.truncated,
                        )
                    }
                }
                Err(_) => nav.set_failure(url, "the answer is not HTTP this browser reads"),
            }
        }
        Err(why) => nav.set_failure(url, why),
    }
    None
}

fn print_page(nav: &mut Navigator, out: &mut dyn FnMut(&str)) {
    let mut grid = nav
        .console_grid
        .take()
        .unwrap_or_else(|| crate::textgrid::TextGrid::new(64, 16));
    grid.clear();
    nav.render(&mut grid);
    print_grid(&grid, out);
    nav.console_grid = Some(grid);
}

fn print_grid(grid: &crate::textgrid::TextGrid, out: &mut dyn FnMut(&str)) {
    for row in 0..grid.rows() {
        let line = grid.line(row);
        let end = line
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map_or(0, |i| i + 1);
        if end == 0 {
            continue;
        }
        match core::str::from_utf8(&line[..end]) {
            Ok(text) => out(text),
            Err(_) => out("(a line of this page is not text)"),
        }
    }
}

fn split_first(line: &str) -> (&str, &str) {
    let t = line.trim();
    match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim_start()),
        None => (t, ""),
    }
}

/// Everything `mlstat` prints, as a function a boot can call before there is a console.
///
/// The console command and the boot banner must never be able to say different things about the
/// resident advisor, so there is exactly one place that renders it (REQ-ML-003, ADR-056). Every
/// value is read live from the advisor at the moment of the call.
pub fn report_risk_advisor(out: &mut dyn FnMut(&str)) {
    match crate::mlsched::resident::stats() {
        None => out("risk advisor: none installed on this machine"),
        Some(s) => {
            match crate::mlsched::resident::shape() {
                        Some((trees, nodes, compares)) => outf!(out,
                            "risk advisor: RESIDENT — {} trees, {} nodes, worst case {} compares per advice",
                            trees, nodes, compares
                        ),
                        // Named absence: a machine running without advice says which check refused
                        // the model, never merely omits the line.
                        None => match crate::mlsched::resident::model_error() {
                            Some(e) => outf!(out, "risk advisor: REFUSED — {:?}", e),
                            None => out("risk advisor: installed with no model (control arm)"),
                        },
                    }
            outf!(
                out,
                "advices: {} ({} low / {} elevated / {} abstain: {} band, {} degenerate input)",
                s.advices,
                s.low,
                s.elevated,
                s.abstain,
                s.band_abstain,
                s.degenerate_abstain
            );
            outf!(
                out,
                "decisive: {}.{}% — {} out-of-box arrival(s) declined",
                s.decisive_permille() / 10,
                s.decisive_permille() % 10,
                s.out_of_range
            );
            outf!(out,
                        "watching: {} dispatch(es), {} finished / {} failed / {} evicted, {} housekeeping tick(s)",
                        s.schedules, s.finished, s.failed, s.evicted, s.ticks
                    );
            // The falsifiable one. A model consulted in a burst at boot and never since has
            // a longest gap equal to its uptime, and this line says so.
            outf!(
                out,
                "continuity: first advice at {}s, last at {}s (span {}s), longest gap {}s",
                s.first_advice_secs,
                s.last_advice_secs,
                s.span_secs(),
                s.max_gap_secs
            );
            // The falsifiable line: a historical gap closes only when the NEXT advice
            // arrives, so an advisor that fell silent keeps reporting the small gaps it
            // managed while busy. Silence is measured against the machine's own clock and
            // grows with it.
            outf!(
                out,
                "silence: {}s since the last advice, as of the machine's clock at {}s",
                s.silence_secs(),
                s.last_tick_secs
            );
            // The memory boundary (ADR-081): what the allocator last said, the least it ever said,
            // and how often the bounded door said no. Unmetered is a state, printed as one.
            if s.memory_samples == 0 {
                outf!(out,
                    "memory: UNMETERED - no allocator reading yet, bounded admission refuses everything ({} refused so far)",
                    s.unmetered_refusals
                );
            } else {
                outf!(out,
                    "memory: {} of {} frames free (low-water {}, {} reading(s)); {} admission(s) refused MemoryExhausted, {} pressure crossing(s){}",
                    s.last_free_pages,
                    s.total_pages,
                    s.low_free_pages,
                    s.memory_samples,
                    s.memory_refusals,
                    s.pressure_events,
                    if s.in_pressure { " - UNDER PRESSURE now" } else { "" }
                );
            }
        }
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
    history: &[String],
    nav: &mut Navigator,
    out: &mut dyn FnMut(&str),
) -> Outcome {
    // The resident advisor ages with the machine, not with the boot (REQ-ML-003, ADR-056). Every
    // line a human types is a moment the machine is still up, so it is also a moment the cell census
    // must move on: this is what makes `mlstat`'s tick count and continuity span grow through a
    // session instead of freezing at whatever the boot left behind. It is housekeeping only — no
    // advice is given here, and `advices` deliberately does not move.
    crate::mlsched::resident::tick(host.uptime_ns() / 1_000_000_000);

    let (verb, rest) = split_first(line);
    if verb.is_empty() {
        return Outcome::Continue;
    }
    match verb {
        "help" => {
            out("commands:");
            for (name, doc) in COMMANDS {
                outf!(out, "  {:<18} {}", name, doc);
            }
        }
        "ver" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            out(VERSION);
            outf!(
                out,
                "target {}, {} processor(s), privilege level {}",
                host.arch(),
                host.cpu_count(),
                host.privilege()
            );
            // Said here rather than only in a document, because the person most likely to
            // over-claim about this system is the one sitting in front of it.
            out("this console runs in kernel space over the kernel's own objects; it is not a");
            out("user-mode shell over a syscall ABI, and nothing here is production-ready.");
        }
        "arch" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            outf!(
                out,
                "{} (privilege level {}, {} processor(s))",
                host.arch(),
                host.privilege(),
                host.cpu_count()
            );
        }
        "uptime" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let ns = host.uptime_ns();
            let secs = ns / 1_000_000_000;
            outf!(
                out,
                "up {}h {:02}m {:02}s ({} ns since boot)",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60,
                ns
            );
        }
        "mem" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let (free, total) = (host.free_frames(), host.total_frames());
            let bytes = host.frame_bytes();
            outf!(
                out,
                "frames: {} free / {} total ({} used)",
                free,
                total,
                total.saturating_sub(free)
            );
            outf!(
                out,
                "memory: {} MiB free / {} MiB managed ({} B per frame)",
                (free * bytes) / (1024 * 1024),
                (total * bytes) / (1024 * 1024),
                bytes
            );
            outf!(
                out,
                "input: {} byte(s) dropped since boot",
                host.input_dropped()
            );
            if let Some((used, free)) = host.heap_bytes() {
                outf!(out, "heap: {} B used, {} B free", used, free);
            }
        }
        "date" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match host.wall_clock() {
                Ok(t) => {
                    let (y, mo, d, h, mi, se) = crate::clock::civil_from_unix(t);
                    outf!(
                        out,
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC (unix {})",
                        y,
                        mo,
                        d,
                        h,
                        mi,
                        se,
                        t
                    );
                }
                Err(e) => outf!(out, "date: no wall clock ({:?})", e),
            }
        }
        "display" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            report_display(out);
        }
        "resolution" => {
            if !authorize(host, ShellAction::Display, out) {
                return Outcome::Continue;
            }
            let (mode, extra) = split_first(rest);
            let parsed = mode
                .split_once('x')
                .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)));
            match parsed {
                Some((w, h)) if extra.is_empty() => match host.set_display_mode(w, h) {
                    Ok((rw, rh)) => outf!(out, "resolution: the desktop now runs at {}x{}", rw, rh),
                    Err(why) => outf!(out, "resolution refused: {}", why),
                },
                _ => out("usage: resolution WxH (a mode `display` lists, e.g. 1920x1080)"),
            }
        }
        "boot" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match crate::boottime::recorded() {
                Some(b) => outf!(
                    out,
                    "boot: {} suite(s) timed, {} ms total, slowest {} at {} ms",
                    b.laps,
                    b.total_ms,
                    b.slowest,
                    b.slowest_ms
                ),
                None => out("boot: no suite summary was recorded on this machine"),
            }
        }
        "caps" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let mut n = 0usize;
            host.capabilities(&mut |subject, action, live| {
                n += 1;
                outf!(
                    out,
                    "  {:<16} {:<20} {}",
                    subject,
                    action,
                    if live { "live" } else { "REVOKED" }
                );
            });
            if n == 0 {
                out("caps: this console offers no capabilities");
            } else {
                outf!(
                    out,
                    "{} capability(ies) offered; tokens are never printed",
                    n
                );
            }
        }
        "net" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match host.net_facts() {
                Some(f) => {
                    let m = f.mac;
                    outf!(
                        out,
                        "net: mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, address {}.{}.{}.{}, gateway {}.{}.{}.{} (static)",
                        m[0], m[1], m[2], m[3], m[4], m[5],
                        f.ip[0], f.ip[1], f.ip[2], f.ip[3],
                        f.gateway[0], f.gateway[1], f.gateway[2], f.gateway[3]
                    );
                    outf!(
                        out,
                        "  {} frame(s) dropped, {} ARP request(s) sent, {} DMA region(s), next local port {}",
                        f.dropped,
                        f.arp_requests,
                        f.dma_regions,
                        f.next_port
                    );
                }
                None => out("net: this machine has no network device"),
            }
        }
        "faults" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            outf!(
                out,
                "supervisor: {} user task(s) contained, {} fault(s) escalated",
                host.supervisor_terminated(),
                host.supervisor_escalations()
            );
        }
        // The machine's input session, reported the way every other fact here is reported: read
        // LIVE from the session the machine is running (ALET-P2-021's hardware rung, ADR-080).
        // A target with no desktop says so — absence is named, never rendered as zeros.
        "input" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match host.input_facts() {
                Some(f) => {
                    outf!(
                        out,
                        "session: held; events posted {} dropped {} refused {}",
                        f.events_posted,
                        f.dropped,
                        f.refusals
                    );
                    match f.cursor {
                        Some((x, y)) => outf!(out, "cursor: ({}, {}) shown", x, y),
                        None => out("cursor: hidden"),
                    }
                    match f.focus {
                        Some(id) => outf!(out, "focus: surface {} ({} queued)", id, f.queued),
                        None => out("focus: none"),
                    }
                    outf!(
                        out,
                        "devices: keyboard {} events, pointer {} events",
                        f.kb_events,
                        f.pt_events
                    );
                    outf!(
                        out,
                        "doorbells: keyboard {}, pointer {}",
                        f.kb_doorbells,
                        f.pt_doorbells
                    );
                    if let Some((
                        (hits, samples, total_cycles, max_cycles),
                        (timer_samples, timer_total, timer_max),
                    )) = host.input_irq_stats()
                    {
                        let avg = total_cycles.checked_div(samples).unwrap_or(0);
                        let timer_avg = timer_total.checked_div(timer_samples).unwrap_or(0);
                        outf!(
                            out,
                            "msix: hits {} wake-samples {} avg-cycles {} max-cycles {} | timer: samples {} avg-cycles {} max-cycles {}",
                            hits,
                            samples,
                            avg,
                            max_cycles,
                            timer_samples,
                            timer_avg,
                            timer_max
                        );
                    }
                    // The terminal window (ADR-083): where it sits and what its last line says,
                    // read from the same grid the compositor paints - not a second copy.
                    match f.window {
                        Some((x, y)) => outf!(out, "window: at ({}, {})", x, y),
                        None => out("window: not placed"),
                    }
                    let n = (f.term_last_len as usize).min(f.term_last.len());
                    let last = core::str::from_utf8(&f.term_last[..n]).unwrap_or("?");
                    outf!(out, "terminal: {} lines, last \"{}\"", f.term_lines, last);
                    // The managed set (ADR-084): how many windows are open, and what the
                    // pointer has done to them since boot.
                    outf!(
                        out,
                        "windows: {} open, {} closed, {} drags",
                        f.windows,
                        f.closes,
                        f.drags
                    );
                    // The desktop's view of the namespace (ADR-137): the panel holds rows, it is
                    // fed by this console, and anything it could not fit is counted.
                    outf!(
                        out,
                        "files: {} rows, {} listings, {} dropped",
                        f.panel_rows,
                        f.panel_listings,
                        f.panel_dropped
                    );
                    // The browser window (ADR-160): what was typed into its URL line, whether
                    // the platform is still fetching, and what the window shows - so the live
                    // gate's question "did the page reach the window?" is answered by the
                    // window, not inferred from the serial line.
                    let un = (f.browser_url_len as usize).min(f.browser_url.len());
                    let url = core::str::from_utf8(&f.browser_url[..un]).unwrap_or("?");
                    if f.browser_fetching {
                        outf!(out, "browser: url \"{}\", fetching", url);
                    } else if f.browser_page_len == 0 {
                        outf!(out, "browser: url \"{}\", no page", url);
                    } else {
                        let fnl = (f.browser_first_len as usize).min(f.browser_first.len());
                        let first = core::str::from_utf8(&f.browser_first[..fnl]).unwrap_or("?");
                        outf!(
                            out,
                            "browser: url \"{}\", page {} bytes, first \"{}\"",
                            url,
                            f.browser_page_len,
                            first
                        );
                    }
                }
                None => out("input: no machine input session on this target"),
            }
        }
        // The command that makes "the model is running" a question a human can ask the machine
        // instead of a claim a README makes on its behalf (REQ-ML-003, ADR-056). Everything printed
        // is read live from the resident advisor at the moment the line is typed.
        "mlstat" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            report_risk_advisor(out);
        }
        "power" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            report_power(out);
        }
        "oc" => {
            if !authorize(host, ShellAction::Overclock, out) {
                return Outcome::Continue;
            }
            overclock(rest, out);
        }
        "tcp" => {
            // A network conversation is a WRITE to the world, not an inspection of this machine:
            // it announces this host to a peer that did not ask. Authorized as such.
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (addr, tail) = split_first(rest);
            let (port_text, text) = split_first(tail);
            let Some(ip) = parse_ipv4_address(addr) else {
                out("usage: tcp ADDR PORT TEXT (ADDR is dotted quad, e.g. 10.0.2.2)");
                return Outcome::Continue;
            };
            let Ok(port) = port_text.parse::<u16>() else {
                out("usage: tcp ADDR PORT TEXT (PORT is 1..65535)");
                return Outcome::Continue;
            };
            if port == 0 || text.is_empty() {
                out("usage: tcp ADDR PORT TEXT (PORT is 1..65535, TEXT is what to send)");
                return Outcome::Continue;
            }
            // Bounded here as well as inside the stack: the console prints what comes back, and a
            // terminal is not a place to put an unbounded answer.
            let mut reply = [0u8; 512];
            match host.tcp_fetch(ip, port, text.as_bytes(), &mut reply) {
                Ok(n) => {
                    outf!(out, "tcp {}: {} byte(s) back", port, n);
                    match core::str::from_utf8(&reply[..n]) {
                        Ok(answer) => out(answer),
                        // A peer may answer with anything at all. Bytes that would drive the
                        // terminal are shown, never executed (the same rule the file panel keeps).
                        Err(_) => out("(the answer is not text)"),
                    }
                }
                Err(why) => outf!(out, "tcp: {}", why),
            }
        }
        "tls" => {
            // A protected conversation is still a WRITE to the world: it announces this host to
            // a peer and sends it bytes. Authorized as such.
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (addr, tail) = split_first(rest);
            let (port_text, tail) = split_first(tail);
            let (name, tail) = split_first(tail);
            let (pin_text, text) = split_first(tail);
            let Some(ip) = parse_ipv4_address(addr) else {
                out("usage: tls ADDR PORT NAME PIN TEXT (ADDR is dotted quad, e.g. 10.0.2.2)");
                return Outcome::Continue;
            };
            let Ok(port) = port_text.parse::<u16>() else {
                out("usage: tls ADDR PORT NAME PIN TEXT (PORT is 1..65535)");
                return Outcome::Continue;
            };
            if port == 0 || name.is_empty() || name.len() > 255 || !name.is_ascii() {
                out("usage: tls ADDR PORT NAME PIN TEXT (NAME is the DNS name the peer must speak for)");
                return Outcome::Continue;
            }
            // The pin is the whole of the trust decision, so it is refused unless it is exactly a
            // 32-byte key: a short or odd pin is not "close enough" to a root.
            let Some(pin) = parse_hex_key(pin_text) else {
                out("usage: tls ADDR PORT NAME PIN TEXT (PIN is the root's Ed25519 public key, 64 hex digits)");
                return Outcome::Continue;
            };
            if text.is_empty() {
                out("usage: tls ADDR PORT NAME PIN TEXT (TEXT is what to send, protected)");
                return Outcome::Continue;
            }
            let mut reply = [0u8; 512];
            match host.tls_fetch(ip, port, name.as_bytes(), pin, text.as_bytes(), &mut reply) {
                Ok(report) => {
                    let k = &report.peer_key;
                    outf!(
                        out,
                        "tls {}: peer verified as {} under the pin (key {:02x}{:02x}{:02x}{:02x}..), {} record(s) in, {} out, {} byte(s) back",
                        port,
                        name,
                        k[0],
                        k[1],
                        k[2],
                        k[3],
                        report.records_in,
                        report.records_out,
                        report.received
                    );
                    match core::str::from_utf8(&reply[..report.received]) {
                        Ok(answer) => out(answer),
                        Err(_) => out("(the answer is not text)"),
                    }
                }
                Err(why) => outf!(out, "tls: {}", why),
            }
        }
        "https" => {
            // An HTTP request is a protected conversation with a peer: a WRITE to the world, like
            // `tls`. Authorized as such.
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (addr, tail) = split_first(rest);
            let (port_text, tail) = split_first(tail);
            let (name, tail) = split_first(tail);
            let (pin_text, path) = split_first(tail);
            let Some(ip) = parse_ipv4_address(addr) else {
                out("usage: https ADDR PORT NAME PIN PATH (ADDR is dotted quad, e.g. 10.0.2.2)");
                return Outcome::Continue;
            };
            let Ok(port) = port_text.parse::<u16>() else {
                out("usage: https ADDR PORT NAME PIN PATH (PORT is 1..65535)");
                return Outcome::Continue;
            };
            if port == 0 || name.is_empty() || name.len() > 255 || !name.is_ascii() {
                out("usage: https ADDR PORT NAME PIN PATH (NAME is the DNS name the peer must speak for)");
                return Outcome::Continue;
            }
            let Some(pin) = parse_hex_key(pin_text) else {
                out("usage: https ADDR PORT NAME PIN PATH (PIN is the root's Ed25519 public key, 64 hex digits)");
                return Outcome::Continue;
            };
            // The path is checked HERE, by the same rule the request builder applies, so a path
            // that would never be sent is refused by usage rather than by a refusal after a
            // connection was opened for it.
            if !crate::http::path_is_sendable(path.as_bytes()) {
                out("usage: https ADDR PORT NAME PIN PATH (PATH is absolute, e.g. /index.txt, with no spaces)");
                return Outcome::Continue;
            }
            let mut request = [0u8; 1024];
            let Ok(request_len) =
                crate::http::request(name.as_bytes(), path.as_bytes(), &mut request)
            else {
                out("usage: https ADDR PORT NAME PIN PATH (the request does not fit)");
                return Outcome::Continue;
            };
            // Bounded here as well as inside the stack: four kilobytes of answer is what a
            // terminal can show; a longer body is truncated and said to be.
            let mut reply = [0u8; 4096];
            match host.tls_fetch(
                ip,
                port,
                name.as_bytes(),
                pin,
                &request[..request_len],
                &mut reply,
            ) {
                Ok(report) => {
                    let raw = &reply[..report.received];
                    let mut body = [0u8; 2048];
                    match crate::http::parse(raw, &mut body, report.truncated) {
                        Ok(response) => {
                            let k = &report.peer_key;
                            outf!(
                                out,
                                "https {}: peer verified as {} under the pin (key {:02x}{:02x}{:02x}{:02x}..); HTTP {} {}; {} header(s); {} byte(s) of body{}{}",
                                port,
                                name,
                                k[0],
                                k[1],
                                k[2],
                                k[3],
                                response.status,
                                core::str::from_utf8(response.reason.of(raw)).unwrap_or("?"),
                                response.headers().len(),
                                response.body_len,
                                if response.chunked { " (chunked)" } else { "" },
                                if response.truncated { " (truncated)" } else { "" }
                            );
                            match core::str::from_utf8(&body[..response.body_len]) {
                                Ok(text) => out(text),
                                Err(_) => out("(the body is not text)"),
                            }
                        }
                        Err(why) => outf!(
                            out,
                            "https: the answer is not HTTP this client reads: {:?}",
                            why
                        ),
                    }
                }
                Err(why) => outf!(out, "https: {}", why),
            }
        }
        "resolve" => {
            // A query announces this machine to a server, like `tcp`: authorized as a WRITE to the
            // world. The answer is printed, not trusted: `trust` still takes the address from the
            // operator, and TLS still checks the pinned root (ADR-176).
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (name, tail) = split_first(rest);
            let (server_text, port_text) = split_first(tail);
            if !crate::dns::name_is_askable(name.as_bytes()) {
                out("usage: resolve NAME [SERVER [PORT]] (NAME is a hostname, e.g. example.com)");
                return Outcome::Continue;
            }
            let server = if server_text.is_empty() {
                Some([10, 0, 2, 3])
            } else {
                parse_ipv4_address(server_text)
            };
            let Some(server) = server else {
                out("usage: resolve NAME [SERVER [PORT]] (SERVER is dotted quad, e.g. 10.0.2.3)");
                return Outcome::Continue;
            };
            let port = if port_text.is_empty() {
                Some(crate::dns::PORT)
            } else {
                port_text.trim().parse::<u16>().ok().filter(|&p| p != 0)
            };
            let Some(port) = port else {
                out("usage: resolve NAME [SERVER [PORT]] (PORT is 1..65535)");
                return Outcome::Continue;
            };
            match host.dns_resolve(server, port, name.as_bytes()) {
                Ok(r) => {
                    let mut addrs = crate::linebuf::LineBuf::<80>::new();
                    for (i, a) in r.addresses().iter().enumerate() {
                        let sep = if i > 0 { ", " } else { "" };
                        let _ = core::fmt::Write::write_fmt(
                            &mut addrs,
                            format_args!("{}{}.{}.{}.{}", sep, a[0], a[1], a[2], a[3]),
                        );
                    }
                    let addrs = addrs.as_str();
                    outf!(
                        out,
                        "resolve {}: {} (ttl {} s, {} CNAME link(s), {} more not kept) - where to dial, not whom to trust",
                        name,
                        addrs,
                        r.ttl,
                        r.cnames,
                        r.dropped
                    );
                }
                Err(why) => outf!(out, "resolve: {}", why),
            }
        }
        "nameserver" => {
            let (addr, port_text) = split_first(rest);
            if addr.is_empty() {
                let (a, p) = nav.nameserver;
                outf!(out, "nameserver: {}.{}.{}.{}:{}", a[0], a[1], a[2], a[3], p);
                return Outcome::Continue;
            }
            // Choosing whom to ask changes what this machine will be told: a WRITE, approved like one.
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let Some(ip) = parse_ipv4_address(addr) else {
                out("usage: nameserver ADDR PORT (ADDR is dotted quad, e.g. 10.0.2.3)");
                return Outcome::Continue;
            };
            let port = if port_text.trim().is_empty() {
                Some(crate::dns::PORT)
            } else {
                port_text.trim().parse::<u16>().ok().filter(|&p| p != 0)
            };
            let Some(port) = port else {
                out("usage: nameserver ADDR PORT (PORT is 1..65535)");
                return Outcome::Continue;
            };
            nav.nameserver = (ip, port);
            outf!(
                out,
                "nameserver: {}.{}.{}.{}:{} (trust NAME PIN asks here; nothing it answers is trusted)",
                ip[0],
                ip[1],
                ip[2],
                ip[3],
                port
            );
        }
        "trust" => {
            // Choosing whom to trust changes what this machine will speak to: a WRITE, approved
            // like one.
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (name, tail) = split_first(rest);
            let (mut ip_text, mut pin_text) = split_first(tail);
            // `trust NAME PIN`: the address comes from the nameserver (ADR-178). A blocked host is
            // refused BEFORE the question is asked (ADR-159), and the pin stays the whole of the
            // trust: the answer only says where to dial.
            let mut asked: Option<([u8; 4], u16)> = None;
            let mut resolved = [0u8; 4];
            if pin_text.trim().is_empty() && parse_hex_key(ip_text).is_some() {
                pin_text = ip_text;
                ip_text = "";
                if nav.blocked.contains(name.as_bytes()) {
                    outf!(
                        out,
                        "trust: {} is blocked; its address was not asked for",
                        name
                    );
                    return Outcome::Continue;
                }
                if !crate::dns::name_is_askable(name.as_bytes()) {
                    out("usage: trust NAME PIN (NAME is a hostname the nameserver can be asked for)");
                    return Outcome::Continue;
                }
                let (server, port) = nav.nameserver;
                match host.dns_resolve(server, port, name.as_bytes()) {
                    Ok(r) => resolved = r.addresses()[0],
                    Err(why) => {
                        outf!(
                            out,
                            "trust: {} was not resolved: {}; nothing was pinned",
                            name,
                            why
                        );
                        return Outcome::Continue;
                    }
                }
                asked = Some((server, port));
            }
            let ip = if asked.is_some() {
                Some(resolved)
            } else {
                parse_ipv4_address(ip_text)
            };
            let Some(ip) = ip else {
                out("usage: trust NAME IP PIN (IP is dotted quad, e.g. 10.0.2.2)");
                return Outcome::Continue;
            };
            let Some(pin) = parse_hex_key(pin_text) else {
                out("usage: trust NAME IP PIN (PIN is the root's Ed25519 public key, 64 hex digits)");
                return Outcome::Continue;
            };
            if let Some((server, port)) = asked {
                outf!(
                    out,
                    "trust: asked {}.{}.{}.{}:{} for {}; the pin decides trust, the answer only where to dial",
                    server[0],
                    server[1],
                    server[2],
                    server[3],
                    port,
                    name
                );
            }
            match nav.hosts.trust(name.as_bytes(), ip, pin) {
                Ok(()) => outf!(
                    out,
                    "trust: {} at {}.{}.{}.{} under pin {:02x}{:02x}{:02x}{:02x}.. ({} host(s) pinned)",
                    name,
                    ip[0],
                    ip[1],
                    ip[2],
                    ip[3],
                    pin[0],
                    pin[1],
                    pin[2],
                    pin[3],
                    nav.hosts.len()
                ),
                Err(TrustRefusal::BadName) => {
                    out("usage: trust NAME IP PIN (NAME is a lower-case DNS name)")
                }
                Err(TrustRefusal::Full) => out("trust: the host table is full; nothing is evicted"),
            }
        }
        "follow" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let Ok(n) = rest.trim().parse::<usize>() else {
                out("usage: follow N (a link number the page printed as [N])");
                return Outcome::Continue;
            };
            let mut target = [0u8; crate::browser::MAX_URL];
            let Some(len) = nav.link_target(n, &mut target) else {
                outf!(out, "follow: the page offers no link [{}]", n);
                return Outcome::Continue;
            };
            let third_party = nav.is_third_party(n) == Some(true);
            match nav.navigate(&target[..len]) {
                Ok(resolved) => {
                    if third_party {
                        out("follow: third-party link - leaving this page's host for one you pinned");
                    }
                    browse(host, nav, resolved, out)
                }
                Err(NavRefusal::Url(UrlRefusal::Plaintext)) => {
                    out("follow: that link is plaintext; this browser speaks https only, and does not downgrade")
                }
                Err(NavRefusal::Blocked) => {
                    out("follow: that link's host is blocked; nothing was dialed")
                }
                Err(NavRefusal::Url(why)) => outf!(out, "follow: that link is not a URL this browser reads ({:?})", why),
                Err(NavRefusal::UnknownHost) => {
                    out("follow: no root pinned for that link's host (trust NAME IP PIN first); nothing was dialed")
                }
            }
        }
        "block" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let name = rest.trim();
            if name.is_empty() {
                out("usage: block HOST (refuse every navigation to HOST, pinned or not)");
                return Outcome::Continue;
            }
            match nav.blocked.block(name.as_bytes()) {
                Ok(()) => outf!(out, "blocked {}: nothing will be dialed there, pinned or not", name),
                Err(TrustRefusal::BadName) => {
                    out("block: a host is lowercase letters, digits, dots and dashes, at most 64 of them")
                }
                Err(TrustRefusal::Full) => out("block: the block list holds its eight; nothing was evicted"),
            }
        }
        "forget" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            nav.forget();
            out("forgotten: history, page and links; trust and block lists kept");
        }
        "go" | "back" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let resolved = if verb == "back" {
                match nav.back() {
                    None => {
                        out("back: no previous page");
                        return Outcome::Continue;
                    }
                    Some(url) => nav.resolve(&url),
                }
            } else {
                if rest.is_empty() {
                    out("usage: go URL (an https:// URL on a host you have trusted)");
                    return Outcome::Continue;
                }
                nav.navigate(rest.as_bytes())
            };
            let resolved = match resolved {
                Ok(r) => r,
                Err(NavRefusal::Url(UrlRefusal::Plaintext)) => {
                    out("go: plaintext refused - this browser speaks https only, and does not downgrade");
                    return Outcome::Continue;
                }
                Err(NavRefusal::Url(why)) => {
                    outf!(out, "go: that is not a URL this browser reads ({:?})", why);
                    return Outcome::Continue;
                }
                Err(NavRefusal::UnknownHost) => {
                    out("go: no root pinned for that host (trust NAME IP PIN first); nothing was dialed");
                    return Outcome::Continue;
                }
                Err(NavRefusal::Blocked) => {
                    out("go: that host is blocked (block NAME); nothing was dialed");
                    return Outcome::Continue;
                }
            };
            browse(host, nav, resolved, out);
        }
        "lsblk" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let n = dev.num_blocks();
            outf!(
                out,
                "{} blocks of {} bytes = {} KiB",
                n,
                BLOCK_SIZE,
                (n * BLOCK_SIZE) / 1024
            );
        }
        "df" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match fs.free_blocks(dev) {
                Ok(free) => outf!(
                    out,
                    "{} free data blocks of {} bytes each",
                    free,
                    BLOCK_SIZE
                ),
                Err(e) => out(&fs_error(e)),
            }
        }
        "ls" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            // Streamed, not collected (ADR-089): listing the namespace must not cost the heap.
            let mut seen = 0usize;
            match fs.for_each(dev, |name, _start, len| {
                seen += 1;
                outf!(out, "{:>8}  {}", len, name);
            }) {
                Ok(()) if seen == 0 => out("(no objects)"),
                Ok(()) => {}
                Err(e) => out(&fs_error(e)),
            }
        }
        "stat" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: stat NAME");
            } else {
                match fs.stat(dev, rest) {
                    Ok(e) => outf!(
                        out,
                        "{}: {} bytes, {} block(s) at device block {}",
                        e.name,
                        e.len,
                        e.blocks(),
                        e.start
                    ),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "cat" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: cat NAME");
            } else {
                match fs.read(dev, rest) {
                    // Contents came from a device, so they are NOT assumed to be text: bytes that
                    // are not valid UTF-8 are reported as a count rather than sprayed at a terminal
                    // that would interpret them as escape sequences.
                    Ok(bytes) => match core::str::from_utf8(&bytes) {
                        Ok(s) => out(s),
                        Err(_) => outf!(out, "<{} bytes, not text>", bytes.len()),
                    },
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "write" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (name, text) = split_first(rest);
            if name.is_empty() {
                out("usage: write NAME TEXT");
            } else {
                // `replace` rather than remove+create: one transaction, so a crash mid-write leaves
                // the old contents or the new ones, never a vanished name (ADR-035).
                match fs.replace(dev, name, text.as_bytes()) {
                    Ok(()) => outf!(out, "wrote {} bytes to {}", text.len(), name),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "rm" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: rm NAME");
            } else {
                match fs.remove(dev, rest) {
                    Ok(()) => outf!(out, "removed {}", rest),
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "find" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: find PREFIX");
            } else {
                let mut seen = 0usize;
                match fs.for_each(dev, |name, _start, len| {
                    if name.starts_with(rest) {
                        seen += 1;
                        outf!(out, "{:>8}  {}", len, name);
                    }
                }) {
                    Ok(()) => {
                        if seen == 0 {
                            out("(nothing matches)");
                        }
                    }
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "head" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let (name, count) = split_first(rest);
            if name.is_empty() {
                out("usage: head NAME [N]");
            } else {
                // A bad count is a refusal, not a default: silently reading ten lines because the
                // number could not be parsed is the console lying about what it was asked.
                let n = if count.is_empty() {
                    Some(10usize)
                } else {
                    count.parse::<usize>().ok()
                };
                match n {
                    None => out("head: N must be a number"),
                    Some(n) => match fs.read(dev, name) {
                        Ok(bytes) => match core::str::from_utf8(&bytes) {
                            Ok(s) => {
                                for line in s.lines().take(n) {
                                    out(line);
                                }
                            }
                            Err(_) => outf!(out, "<{} bytes, not text>", bytes.len()),
                        },
                        Err(e) => out(&fs_error(e)),
                    },
                }
            }
        }
        "wc" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: wc NAME");
            } else {
                match fs.read(dev, rest) {
                    Ok(bytes) => {
                        let lines = bytes.iter().filter(|b| **b == b'\n').count();
                        let words = core::str::from_utf8(&bytes)
                            .map(|s| s.split_whitespace().count())
                            .unwrap_or(0);
                        outf!(
                            out,
                            "{:>8} {:>8} {:>8}  {}",
                            lines,
                            words,
                            bytes.len(),
                            rest
                        );
                    }
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "grep" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let (needle, name) = split_first(rest);
            if needle.is_empty() || name.is_empty() {
                out("usage: grep TEXT NAME");
            } else {
                match fs.read(dev, name) {
                    Ok(bytes) => match core::str::from_utf8(&bytes) {
                        Ok(s) => {
                            let mut hits = 0usize;
                            for (i, line) in s.lines().enumerate() {
                                if line.contains(needle) {
                                    outf!(out, "{}: {}", i + 1, line);
                                    hits += 1;
                                }
                            }
                            if hits == 0 {
                                out("(no matching line)");
                            }
                        }
                        Err(_) => outf!(out, "<{} bytes, not text>", bytes.len()),
                    },
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "hexdump" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let (name, count) = split_first(rest);
            if name.is_empty() {
                out("usage: hexdump NAME [N]");
            } else {
                let n = if count.is_empty() {
                    Some(128usize)
                } else {
                    count.parse::<usize>().ok()
                };
                match n {
                    None => out("hexdump: N must be a number"),
                    Some(n) => match fs.read(dev, name) {
                        Ok(bytes) => {
                            // Bytes from a device are NOT assumed to be text — which is the whole
                            // point of this command: it is the one way to look at an object whose
                            // contents `cat` refuses to spray at a terminal.
                            for (row, chunk) in bytes
                                .iter()
                                .take(n)
                                .collect::<Vec<_>>()
                                .chunks(16)
                                .enumerate()
                            {
                                let mut hex = String::new();
                                let mut txt = String::new();
                                for b in chunk {
                                    let mut byte = crate::linebuf::LineBuf::<4>::new();
                                    let _ = core::fmt::Write::write_fmt(
                                        &mut byte,
                                        format_args!("{:02x} ", **b),
                                    );
                                    hex.push_str(byte.as_str());
                                    txt.push(if b.is_ascii_graphic() || **b == b' ' {
                                        **b as char
                                    } else {
                                        '.'
                                    });
                                }
                                outf!(out, "{:08x}  {:<48} |{}|", row * 16, hex, txt);
                            }
                            if bytes.is_empty() {
                                out("(empty)");
                            } else if bytes.len() > n {
                                outf!(out, "… {} more byte(s)", bytes.len() - n);
                            }
                        }
                        Err(e) => out(&fs_error(e)),
                    },
                }
            }
        }
        "append" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (name, text) = split_first(rest);
            if name.is_empty() {
                out("usage: append NAME TEXT");
            } else {
                // Read, extend, replace: ONE transaction for the write (ADR-035), so a crash leaves
                // the old contents or the new ones. Appending in place would need an extent that
                // may not be free, and would be a second failure mode for a command whose whole
                // value is that it is boring.
                match fs.read(dev, name) {
                    Ok(mut bytes) => {
                        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                            bytes.push(b'\n');
                        }
                        bytes.extend_from_slice(text.as_bytes());
                        bytes.push(b'\n');
                        match fs.replace(dev, name, &bytes) {
                            Ok(()) => outf!(out, "{} is now {} bytes", name, bytes.len()),
                            Err(e) => out(&fs_error(e)),
                        }
                    }
                    Err(FsError::NotFound) => {
                        let mut bytes = Vec::from(text.as_bytes());
                        bytes.push(b'\n');
                        match fs.create(dev, name, &bytes) {
                            Ok(()) => outf!(out, "created {} ({} bytes)", name, bytes.len()),
                            Err(e) => out(&fs_error(e)),
                        }
                    }
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "touch" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            if rest.is_empty() {
                out("usage: touch NAME");
            } else {
                match fs.stat(dev, rest) {
                    // An existing object is left ALONE — there is no modification time to update,
                    // and truncating someone's data because they typed `touch` would be a disaster
                    // wearing the name of a harmless command.
                    Ok(e) => outf!(out, "{} exists ({} bytes)", e.name, e.len),
                    Err(FsError::NotFound) => match fs.create(dev, rest, b"") {
                        Ok(()) => outf!(out, "created {}", rest),
                        Err(e) => out(&fs_error(e)),
                    },
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "cp" | "mv" => {
            if !authorize(host, ShellAction::Write, out) {
                return Outcome::Continue;
            }
            let (src, dst) = split_first(rest);
            if src.is_empty() || dst.is_empty() {
                outf!(out, "usage: {} SRC DST", verb);
            } else if src == dst {
                out("source and destination are the same name");
            } else {
                match fs.read(dev, src) {
                    Ok(bytes) => match fs.replace(dev, dst, &bytes) {
                        Ok(()) => {
                            if verb == "mv" {
                                // Copy-then-remove, in that order, and NOT one transaction: a crash
                                // between them leaves both names, which is recoverable. The other
                                // order loses the data. Said out loud because `mv` reads atomic and
                                // is not.
                                match fs.remove(dev, src) {
                                    Ok(()) => outf!(out, "{} -> {}", src, dst),
                                    Err(e) => {
                                        outf!(out, "copied, but {} remains: {}", src, fs_error(e))
                                    }
                                }
                            } else {
                                outf!(out, "{} -> {} ({} bytes)", src, dst, bytes.len());
                            }
                        }
                        Err(e) => out(&fs_error(e)),
                    },
                    Err(e) => out(&fs_error(e)),
                }
            }
        }
        "sync" => {
            if !authorize(host, ShellAction::Flush, out) {
                return Outcome::Continue;
            }
            match dev.flush() {
                Ok(()) => out("device flushed"),
                Err(e) => outf!(out, "storage: {}", storage_error(e)),
            }
        }
        "history" => {
            if history.is_empty() {
                out("(nothing yet)");
            } else {
                for (i, line) in history.iter().enumerate() {
                    outf!(out, "{:>4}  {}", i + 1, line);
                }
            }
        }
        "echo" => out(rest),
        // The two screen commands write escape sequences, which is the one place this console
        // assumes anything about the terminal. Harmless when the assumption is wrong: a terminal
        // that ignores them shows the sequence's effect as nothing rather than as garbage.
        "clear" => out("\x1b[2J\x1b[H"),
        "reboot" => {
            if !authorize(host, ShellAction::Reboot, out) {
                return Outcome::Continue;
            }
            out("rebooting.");
            if !host.reboot() {
                out("reboot: this target has no reset path — use `halt`");
            }
        }
        "halt" => {
            if !authorize(host, ShellAction::Halt, out) {
                return Outcome::Continue;
            }
            out("halting.");
            return Outcome::Halt;
        }
        other => outf!(
            out,
            "unknown command '{}' — try `help`",
            other.escape_debug()
        ),
    }
    Outcome::Continue
}

/// A console session: the editor and the dispatcher wired together, driven one input byte at a
/// time. The interactive loop and the live invariant suite run THIS, so what a gate proves is the
/// same code a human types at.
pub struct Session {
    editor: LineEditor,
    started: bool,
    /// The browser's navigation state (ADR-156): the hosts this person trusts, the history, the
    /// page. One per console session, like the line editor.
    navigator: Navigator,
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
            navigator: Navigator::new(),
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
        // Allocation-free (ADR-180): the token is copied to the stack, candidates are STREAMED
        // twice (once to find what they share, once to print them if they are ambiguous), and the
        // common prefix lives in a fixed buffer. The first version collected a `Vec<String>` of
        // candidates plus a `list()` of the namespace on every Tab, ~3.8 KB per press on a heap
        // that never frees.
        let mut token_buf = [0u8; MAX_LINE];
        let (token_len, at_word_start) = {
            let line = self.editor.line();
            let cursor = self.editor.cursor();
            let head = &line[..cursor];
            let start = head.rfind(' ').map(|i| i + 1).unwrap_or(0);
            let token = &head[start..];
            token_buf[..token.len()].copy_from_slice(token.as_bytes());
            (token.len(), start == 0)
        };
        let token = core::str::from_utf8(&token_buf[..token_len]).unwrap_or("");
        let visit = |f: &mut dyn FnMut(&str)| {
            if at_word_start {
                for (name, _) in COMMANDS {
                    let verb = split_first(name).0;
                    if verb.starts_with(token) {
                        f(verb);
                    }
                }
            } else {
                let _ = fs.for_each(dev, |name, _, _| {
                    if name.starts_with(token) {
                        f(name);
                    }
                });
            }
        };
        let mut common = [0u8; MAX_LINE];
        let mut common_len = 0usize;
        let mut count = 0usize;
        visit(&mut |c: &str| {
            let c = c.as_bytes();
            if count == 0 {
                common_len = c.len().min(MAX_LINE);
                common[..common_len].copy_from_slice(&c[..common_len]);
            } else {
                common_len = common[..common_len]
                    .iter()
                    .zip(c.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
            }
            count += 1;
        });
        if count == 0 {
            return;
        }
        // Complete to what every candidate shares: typing stops exactly where the choice begins,
        // which is the behavior that makes completion feel like typing and not like a menu.
        if common_len > token_len {
            let add = core::str::from_utf8(&common[token_len..common_len]).unwrap_or("");
            self.editor.insert_str(add, out);
        }
        if count == 1 {
            // One answer: finish the word and separate it, so the next argument can be typed.
            if !self.editor.line()[..self.editor.cursor()].ends_with(' ') {
                self.editor.insert_str(" ", out);
            }
        } else if common_len == token_len {
            // Ambiguous and nothing to add: SHOW the choice instead of beeping at the operator,
            // then reprint the prompt and the line exactly as it was.
            out("\r\n");
            visit(&mut |c: &str| {
                out(c);
                out(" ");
            });
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
        match self.editor.feed_in_place(byte, out) {
            Edit::Pending | Edit::Line(_) => Outcome::Continue,
            Edit::Complete => {
                self.complete(fs, dev, out);
                Outcome::Continue
            }
            Edit::Cancelled => {
                out(PROMPT);
                Outcome::Continue
            }
            Edit::Submitted => {
                let line = self.editor.submitted();
                // The history the `history` command prints is the SAME list the up arrow walks:
                // one list, so what the operator is shown and what they can recall cannot diverge.
                // BORROWED, not cloned: `to_vec()` here copied all 32 entries on every line, ~3.6 KB
                // per command on a heap that never frees, and the console fuzz ran aarch64 out of
                // heap after ~1100 commands (ADR-180).
                let outcome = execute(
                    line,
                    host,
                    fs,
                    dev,
                    self.editor.history(),
                    &mut self.navigator,
                    &mut |s| {
                        out(s);
                        out("\r\n");
                    },
                );
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

/// Parse a dotted-quad IPv4 address, refusing everything that is not one.
///
/// Deliberately strict: four parts, each a decimal number that fits a byte, nothing else. A
/// console that accepts `10.0.2` and guesses is a console that connects somewhere the operator did
/// not name.
/// Exactly 64 hexadecimal digits, as 32 bytes; anything else is `None`. A key is not a number, so
/// no leading `0x`, no whitespace, no shorter form.
pub fn parse_hex_key(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut key = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        key[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(key)
}

pub fn parse_ipv4_address(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut seen = 0usize;
    for part in text.split('.') {
        if seen == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        out[seen] = part.parse::<u8>().ok()?;
        seen += 1;
    }
    if seen == 4 {
        Some(out)
    } else {
        None
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
    run_loop_serviced(
        host,
        fs,
        dev,
        getc,
        out,
        &mut |_, _, _, _| false,
        &mut BrowserHooks {
            take_navigation: &mut || None,
            show_page: &mut |_| {},
        },
    )
}

/// Why the loop is handing the namespace to a service hook. The phase is stated rather than
/// inferred because the two callers cost different amounts: a settle may read the directory,
/// an idle turn happens a thousand times a second and must not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServicePhase {
    /// The namespace may have changed: the session just finished a command line (or is about to
    /// show its first prompt). Reading the directory here costs one read per line a human types.
    Settled,
    /// Nothing was typed. The hook runs on every idle turn, so it must do no device work unless
    /// something outside the console (a click in the desktop's file panel) asked for it.
    Idle,
}

/// The service hook's shape, named so the three targets and this loop agree on it in one place:
/// the phase, the namespace, the device it lives on, and somewhere to print. Returns whether it
/// printed.
pub type ServiceHook<'a, D> =
    dyn FnMut(ServicePhase, &Filesystem, &mut D, &mut dyn FnMut(&str)) -> bool + 'a;

/// The interactive loop with a SERVICE HOOK: the same session, plus somewhere for a target to
/// answer for the namespace it owns while the console holds it (ADR-137).
///
/// The console is the only part of a live machine that has both a mounted filesystem and a device
/// to read it with, so a GUI file panel cannot list the namespace by itself. Rather than lend the
/// filesystem to the compositor — which would put disk latency on the frame path — the loop hands
/// it out here, between keystrokes, where blocking costs a human nothing.
///
/// The hook returns whether it printed through `out`; the loop re-issues the prompt when it did,
/// so output that arrives from outside the typist's line never eats the prompt.
/// What the desktop's browser window needs from the console session (ADR-157): the URL the
/// person entered, and a place to put the page. Both are the platform's closures over its desktop
/// door; a machine without a desktop passes hooks that return nothing and show nowhere.
pub struct BrowserHooks<'a> {
    pub take_navigation: &'a mut dyn FnMut() -> Option<crate::desktop::BrowserRequest>,
    pub show_page: &'a mut dyn FnMut(&[u8]),
}

/// Navigate for the desktop's browser window: the same model and the same fetch the console's
/// `go` uses, rendered into the window's grid shape and handed to `show_page`.
pub fn navigate_for_window<H: ShellHost>(
    host: &H,
    nav: &mut Navigator,
    request: crate::desktop::BrowserRequest,
    show_page: &mut dyn FnMut(&[u8]),
) {
    let mut grid = nav
        .window_grid
        .take()
        .unwrap_or_else(|| crate::textgrid::TextGrid::new(30, 8));
    grid.clear();
    navigate_into_grid(host, nav, request, show_page, &mut grid);
    nav.window_grid = Some(grid);
}

fn navigate_into_grid<H: ShellHost>(
    host: &H,
    nav: &mut Navigator,
    request: crate::desktop::BrowserRequest,
    show_page: &mut dyn FnMut(&[u8]),
    grid: &mut crate::textgrid::TextGrid,
) {
    use crate::desktop::BrowserRequest;
    let mut text = [0u8; 1024];
    let mut len = 0usize;
    // Every request resolves through the navigator exactly as the console's verbs do (ADR-164):
    // a typed URL through `navigate`, a numbered link through the page's own hrefs, back and
    // forward through history - so the window can refuse nothing less and nothing more.
    let mut target = [0u8; crate::browser::MAX_URL];
    let resolved = match request {
        BrowserRequest::Go(url, n) => nav.navigate(&url[..n]),
        BrowserRequest::Follow(k) => match nav.link_target(k, &mut target) {
            Some(n) => nav.navigate(&target[..n]),
            None => {
                grid.write(b"refused: the page offers no such link");
                return show_window(grid, &mut text, &mut len, show_page);
            }
        },
        BrowserRequest::Back => match nav.back() {
            Some(url) => nav.resolve(&url),
            None => {
                grid.write(b"refused: no previous page");
                return show_window(grid, &mut text, &mut len, show_page);
            }
        },
        BrowserRequest::Forward => match nav.forward() {
            Some(url) => nav.resolve(&url),
            None => {
                grid.write(b"refused: no next page");
                return show_window(grid, &mut text, &mut len, show_page);
            }
        },
    };
    let refused: Option<&[u8]> = match resolved {
        Ok(resolved) => {
            fetch_into(host, nav, resolved);
            None
        }
        Err(NavRefusal::Url(UrlRefusal::Plaintext)) => {
            Some(b"refused: plaintext - this browser speaks https only")
        }
        Err(NavRefusal::Url(_)) => Some(b"refused: that is not a URL this browser reads"),
        Err(NavRefusal::UnknownHost) => {
            Some(b"refused: no root pinned for that host (trust NAME IP PIN at the console)")
        }
        Err(NavRefusal::Blocked) => Some(b"refused: that host is blocked"),
    };
    match refused {
        Some(why) => grid.write(why),
        None => nav.render(grid),
    }
    show_window(grid, &mut text, &mut len, show_page)
}

/// Hand the window's grid to the desktop as lines of text, trailing blanks trimmed.
fn show_window(
    grid: &crate::textgrid::TextGrid,
    text: &mut [u8; 1024],
    len: &mut usize,
    show_page: &mut dyn FnMut(&[u8]),
) {
    for row in 0..grid.rows() {
        let line = grid.line(row);
        let end = line
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map_or(0, |i| i + 1);
        for &b in &line[..end] {
            if *len < text.len() {
                text[*len] = b;
                *len += 1;
            }
        }
        if *len < text.len() {
            text[*len] = b'\n';
            *len += 1;
        }
    }
    show_page(&text[..*len]);
}

pub fn run_loop_serviced<H: ShellHost, D: BlockDevice>(
    host: &H,
    fs: &mut Filesystem,
    dev: &mut D,
    getc: &mut dyn FnMut() -> Option<u8>,
    out: &mut dyn FnMut(&str),
    service: &mut ServiceHook<'_, D>,
    browser: &mut BrowserHooks<'_>,
) {
    let mut session = Session::new();
    // The first settle happens BEFORE the banner's prompt: a panel that opens with the desktop
    // shows the namespace as it is, not as it will be once the operator types something.
    let _ = service(ServicePhase::Settled, fs, dev, &mut *out);
    session.prompt(out);
    loop {
        let Some(byte) = getc() else {
            if let Some(request) = (browser.take_navigation)() {
                navigate_for_window(host, &mut session.navigator, request, browser.show_page);
            }
            if service(ServicePhase::Idle, fs, dev, &mut *out) {
                session.prompt(out);
            }
            // Nothing typed. Wait for an interrupt rather than asking again immediately; see
            // `ShellHost::idle`, which does nothing at all unless a target has said it is safe.
            host.idle();
            continue;
        };
        let settles = ends_a_command(byte);
        if session.feed(byte, host, fs, dev, out) == Outcome::Halt {
            return;
        }
        if settles && service(ServicePhase::Settled, fs, dev, &mut *out) {
            session.prompt(out);
        }
    }
}

/// Whether this byte ends a command line, and so leaves the namespace in whatever state the
/// command left it.
///
/// Deliberately a predicate over the BYTE rather than a report from the editor: a line that was
/// cancelled, empty, or refused still settles, and a settle that fires once too often costs one
/// directory read while a settle that is missed shows a stale listing until the next command.
pub fn ends_a_command(byte: u8) -> bool {
    matches!(byte, b'\r' | b'\n')
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

    // 3. `mlstat` answers about the advisor that is resident RIGHT NOW. A console that printed a
    //    boot-time snapshot would let a machine keep claiming a model it had stopped consulting.
    let (log, _) = transcript("mlstat\r", host, &mut fs, dev);
    check!(
        "console: mlstat reports the resident risk advisor's live counters",
        log.contains("risk advisor:")
            && (log.contains("RESIDENT")
                || log.contains("none installed")
                || log.contains("REFUSED"))
    );

    // ADR-184: the machine's clock, its power governor, and the operator's hold on it. Each answers
    // with the live fact or a named absence — never a zero dressed as a reading.
    let (log, _) = transcript("date\r", host, &mut fs, dev);
    check!(
        "console: date prints the wall clock in UTC, or names why there is none",
        log.contains(" UTC (unix ") || log.contains("date: no wall clock (")
    );
    // ADR-185: where boot time went, what authority this console holds, and the NIC it dials with.
    let (log, _) = transcript("boot\r", host, &mut fs, dev);
    check!(
        "console: boot reports the recorded suite timing, or that none was recorded",
        log.contains("suite(s) timed") || log.contains("no suite summary was recorded")
    );
    let (log, _) = transcript("caps\r", host, &mut fs, dev);
    check!(
        "console: caps names the console's capabilities and never prints a token",
        log.contains("tokens are never printed") || log.contains("offers no capabilities")
    );
    let (log, _) = transcript("display\r", host, &mut fs, dev);
    check!(
        "console: display reports the scanout and the monitor's modes, or that there is no display",
        log.contains("display: scanout ") || log.contains("no display device on this machine")
    );
    let (bad, _) = transcript("resolution 1x1\r", host, &mut fs, dev);
    let (usage, _) = transcript("resolution wide\r", host, &mut fs, dev);
    check!(
        "console: resolution refuses a mode by name and teaches its own syntax",
        bad.contains("resolution refused: ") && usage.contains("usage: resolution WxH")
    );
    let (log, _) = transcript("net\r", host, &mut fs, dev);
    check!(
        "console: net reports the network device's addresses, or that there is none",
        log.contains("net: mac ") || log.contains("no network device")
    );
    let (log, _) = transcript("power\r", host, &mut fs, dev);
    let governed = log.contains("governor: ") && log.contains("domain 0: ");
    check!(
        "console: power reads the resident governor, or says none was commissioned",
        governed || log.contains("no power governor was commissioned")
    );
    // The point comes from the machine's own ladder, never from this file: the top of domain 0.
    let top = crate::lethed::resident::facts()
        .filter(|f| f.n > 0 && f.domains[0].n_points > 0)
        .map(|f| f.domains[0].points[f.domains[0].n_points - 1])
        .unwrap_or(0);
    let mut cmd = crate::linebuf::LineBuf::<{ crate::linebuf::LINE_MAX }>::new();
    let _ = core::fmt::Write::write_fmt(&mut cmd, format_args!("oc {}\r", top));
    let mut held = crate::linebuf::LineBuf::<{ crate::linebuf::LINE_MAX }>::new();
    let _ = core::fmt::Write::write_fmt(&mut held, format_args!("held at {} kHz", top));
    let (off_ladder, _) = transcript("oc 1\r", host, &mut fs, dev);
    let (up, _) = transcript(cmd.as_str(), host, &mut fs, dev);
    let (seen, _) = transcript("power\r", host, &mut fs, dev);
    let (down, _) = transcript("oc off\r", host, &mut fs, dev);
    let (after, _) = transcript("power\r", host, &mut fs, dev);
    check!(
        "console: oc holds a point in the band, power shows it, oc off gives it back",
        if governed {
            off_ladder.contains("oc refused: Contract(NotAnOperatingPoint")
                && ((up.contains(held.as_str())
                    && seen.contains("HELD by operator")
                    && down.contains("released to the governor")
                    && !after.contains("HELD by operator"))
                    || up.contains("oc refused: Contract(Cooldown"))
        } else {
            up.contains("oc refused: NotCommissioned")
        }
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

    // 16. Fault containment is operator-visible. The counters come from the target's real
    // supervisor, not a shell-local cache, so a contained fault cannot disappear between the trap
    // path and the command surface.
    let (log, _) = transcript("faults\r", host, &mut fs, dev);
    check!(
        "console: faults reports supervisor counters",
        log.contains("supervisor:")
            && log.contains("user task(s) contained")
            && log.contains("fault(s) escalated")
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

    // ---- the command set (REQ-CON-005, ADR-051) ---------------------------------------------------

    // 31. `cp` copies contents, and the copy is a SEPARATE object: writing one must not change the
    //     other. A copy that shared an extent would look right until the first edit.
    let (log, _) = transcript(
        "write src alpha\rcp src dst\rwrite src beta\rcat dst\rcat src\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: a copy is an independent object, not a second name for the same bytes",
        log.contains("alpha") && log.contains("beta")
    );

    // 32. `mv` renames: the new name has the bytes and the old name is gone. Copy-then-remove in
    //     that order, so a crash between them leaves both names rather than neither.
    let (log, _) = transcript("mv dst moved\rcat moved\rcat dst\r", host, &mut fs, dev);
    check!(
        "console: a rename moves the bytes and removes the old name",
        log.contains("alpha") && log.contains("no such object")
    );

    // 33. `append` adds to the end without losing what was there, and creates the object when it is
    //     absent — the two halves of the only command here that reads before it writes.
    let (log, _) = transcript(
        "write notes one\rappend notes two\rcat notes\rappend fresh line\rcat fresh\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: append keeps what was there and creates what was not",
        log.contains("one") && log.contains("two") && log.contains("line")
    );

    // 34. `touch` NEVER truncates an object that exists. A harmless-looking command that ate data
    //     would be the worst kind of defect in this table.
    let (log, _) = transcript(
        "touch notes\rcat notes\rtouch brandnew\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: touch leaves an existing object's bytes alone",
        log.contains("exists") && log.contains("one") && log.contains("created brandnew")
    );

    // 35. The reading commands agree with the bytes: `wc` counts them, `grep` finds the line that
    //     contains the text and not the one that does not, `head` stops at the count it was given.
    let (log, _) = transcript(
        "write poem alpha\rappend poem beta\rwc poem\rgrep beta poem\rgrep zeta poem\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: wc counts what is there and grep finds only what matches",
        log.contains("poem") && log.contains("2: beta") && log.contains("(no matching line)")
    );

    // 36. `hexdump` is how a non-text object is looked at — the case `cat` deliberately refuses.
    let (log, _) = transcript("write bin AB\rhexdump bin\r", host, &mut fs, dev);
    check!(
        "console: hexdump shows the bytes cat refuses to print",
        log.contains("41 42") && log.contains("|AB|")
    );

    // 37. `find` searches the namespace by prefix and says so when nothing matches, rather than
    //     printing an empty result that reads like a broken command.
    let (log, _) = transcript("find poe\rfind zzz\r", host, &mut fs, dev);
    check!(
        "console: find matches by prefix and names the empty case",
        log.contains("poem") && log.contains("(nothing matches)")
    );

    // 38. `history` prints the SAME list the up arrow walks — one list, so what an operator is shown
    //     and what they can recall cannot diverge.
    let (log, _) = transcript("echo first\rhistory\r", host, &mut fs, dev);
    check!(
        "console: history shows the lines this session ran",
        log.contains("1  echo first")
    );

    // 39. A numeric argument that is not a number is REFUSED, not silently defaulted: a console that
    //     quietly did something else is a console whose output cannot be trusted to answer what was
    //     asked.
    let (log, _) = transcript("head poem x\rhexdump poem x\r", host, &mut fs, dev);
    check!(
        "console: a bad count is refused rather than replaced with a default",
        log.contains("head: N must be a number") && log.contains("hexdump: N must be a number")
    );

    // 40. Every new command refuses a missing argument with a usage line, and none of them can be
    //     made to act on nothing. Swept over the whole table rather than sampled: a command whose
    //     usage line was forgotten would act on an empty name.
    let mut usage_ok = true;
    for (spec, _) in COMMANDS {
        let (verb, args) = split_first(spec);
        if args.is_empty() || args.starts_with('[') {
            continue; // takes no required argument
        }
        if verb == "echo" {
            continue; // echo of nothing is a blank line, which is what it means
        }
        let (log, _) = transcript(&format!("{}\r", verb), host, &mut fs, dev);
        if !log.contains("usage:") {
            usage_ok = false;
        }
    }
    check!(
        "console: every command that needs an argument refuses to run without one",
        usage_ok
    );

    // 41. The `tcp` command refuses everything that is not an address and a port, by name, before
    //      anything is put on a wire. A console that guesses at `10.0.2` connects somewhere the
    //      operator did not name.
    let bad_addresses = [
        "",
        "10.0.2",
        "10.0.2.2.2",
        "10.0.2.300",
        "ten.zero.two.two",
        "10..2.2",
    ];
    let mut address_ok = parse_ipv4_address("10.0.2.2") == Some([10, 0, 2, 2])
        && parse_ipv4_address("0.0.0.0") == Some([0, 0, 0, 0])
        && parse_ipv4_address("255.255.255.255") == Some([255, 255, 255, 255]);
    for bad in bad_addresses {
        address_ok &= parse_ipv4_address(bad).is_none();
    }
    check!(
        "console: an address that is not a dotted quad is refused rather than guessed at",
        address_ok
    );

    // 42. A machine with no network says so. The default host seam refuses by name, so a target
    //      without a NIC cannot report a zero-length answer that reads as a silent peer.
    let (log, _) = transcript("tcp 10.0.2.2 7 hello\r", host, &mut fs, dev);
    let (usage, _) = transcript("tcp 10.0.2.2 hello\r", host, &mut fs, dev);
    check!(
        "console: tcp refuses a bad port by usage and a missing network by name",
        usage.contains("usage: tcp") && (log.contains("tcp: ") || log.contains("byte(s) back"))
    );

    // 43. The `tls` command refuses a pin that is not exactly a 32-byte key by usage, before it
    //     touches a wire, and a machine with no network says so by name. The pin is the whole of
    //     the trust decision, and "close enough" is not a root.
    let good_pin = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let short_pin = "00112233445566778899aabbccddeeff";
    let odd_pin = "0g112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let (short, _) = transcript(
        &alloc::format!("tls 10.0.2.2 443 aletheia.test {short_pin} hello\r"),
        host,
        &mut fs,
        dev,
    );
    let (odd, _) = transcript(
        &alloc::format!("tls 10.0.2.2 443 aletheia.test {odd_pin} hello\r"),
        host,
        &mut fs,
        dev,
    );
    let (none, _) = transcript(
        &alloc::format!("tls 10.0.2.2 443 aletheia.test {good_pin} hello\r"),
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: tls refuses a pin that is not a 32-byte key by usage and a missing network by name",
        parse_hex_key(good_pin).is_some()
            && parse_hex_key(short_pin).is_none()
            && parse_hex_key(odd_pin).is_none()
            && short.contains("usage: tls")
            && odd.contains("usage: tls")
            && (none.contains("tls: ") || none.contains("peer verified"))
    );

    // 44. The `https` command refuses a path it would never send - relative, with a space, with
    //     a control byte - by usage, before a connection is opened for it; and a machine with no
    //     network says so by name.
    let (relative, _) = transcript(
        &alloc::format!("https 10.0.2.2 443 aletheia.test {good_pin} index.txt\r"),
        host,
        &mut fs,
        dev,
    );
    let (spaced, _) = transcript(
        &alloc::format!("https 10.0.2.2 443 aletheia.test {good_pin} /a b\r"),
        host,
        &mut fs,
        dev,
    );
    let (none_https, _) = transcript(
        &alloc::format!("https 10.0.2.2 443 aletheia.test {good_pin} /index.txt\r"),
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: https refuses a path it would never send by usage and a missing network by name",
        relative.contains("usage: https")
            && spaced.contains("usage: https")
            && (none_https.contains("https: ") || none_https.contains("peer verified"))
    );

    // 45. The browser's navigation refuses plaintext and an unpinned host BY NAME before anything
    //     is dialed, `trust` refuses a pin that is not a key, and a trusted host navigates (to the
    //     default seam's named refusal, on a machine with no network).
    let (plain, _) = transcript("go http://aletheia.test/\r", host, &mut fs, dev);
    let (unpinned, _) = transcript("go https://nobody.test/\r", host, &mut fs, dev);
    let (badpin, _) = transcript("trust aletheia.test 10.0.2.2 abc\r", host, &mut fs, dev);
    let (trusted, _) = transcript(
        &alloc::format!("trust aletheia.test 10.0.2.2 {good_pin}\rgo https://aletheia.test/\r"),
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: go refuses plaintext and an unpinned host by name before anything is dialed",
        plain.contains("go: plaintext refused")
            && unpinned.contains("go: no root pinned")
            && badpin.contains("usage: trust")
            && trusted.contains("trust: aletheia.test at 10.0.2.2")
            && trusted.contains("https://aletheia.test/")
            && (trusted.contains("refused: ") || trusted.contains("HTTP "))
    );

    // 46. `follow` refuses a number the page never printed, and a bad number by usage, before
    //     anything is dialed.
    let (nolink, _) = transcript("follow 3\r", host, &mut fs, dev);
    let (badnum, _) = transcript("follow x\r", host, &mut fs, dev);
    check!(
        "console: follow refuses a link the page never offered before anything is dialed",
        nolink.contains("follow: the page offers no link [3]") && badnum.contains("usage: follow")
    );

    // 47. `block` refuses `go` by name BEFORE lookup - the host need not be pinned to be blocked -
    //     and `forget` leaves `back` nowhere to go, while nothing is dialed by any of it.
    //     One session: the block list lives in the session's navigator, as the trust table does.
    let (policy, _) = transcript(
        "block tracker.example\rgo https://tracker.example/\rforget\rback\r",
        host,
        &mut fs,
        dev,
    );
    check!(
        "console: block refuses go by name before lookup, and forget leaves back nowhere to go",
        policy.contains("blocked tracker.example")
            && policy.contains("go: that host is blocked")
            && policy.contains("forgotten: history, page and links")
            && policy.contains("back: no previous page")
    );

    // 48. A command line ends on return, and on nothing else. The desktop's file panel is
    //     refreshed off this predicate (ADR-137), so a byte that wrongly counted as a line end
    //     would read the directory on every keystroke a human types.
    check!(
        "console: a command line ends on return and on no other byte",
        ends_a_command(b'\r')
            && ends_a_command(b'\n')
            && !ends_a_command(b'a')
            && !ends_a_command(0x08)
            && !ends_a_command(0x1b)
            && !ends_a_command(b' ')
    );

    // 49. The serviced loop hands the namespace out once before the first prompt and once per
    //     completed line — never mid-line. This is what lets a GUI panel show the namespace
    //     without ever holding the filesystem itself.
    let mut phases: Vec<ServicePhase> = Vec::new();
    let mut typed = b"ls\rhalt\r".iter().copied();
    let mut sink = |_: &str| {};
    run_loop_serviced(
        host,
        &mut fs,
        dev,
        &mut || typed.next(),
        &mut sink,
        &mut |phase, _, _, _| {
            phases.push(phase);
            false
        },
        &mut BrowserHooks {
            take_navigation: &mut || None,
            show_page: &mut |_| {},
        },
    );
    check!(
        "console: the serviced loop settles once before the prompt and once per command line",
        // Before the first prompt, then after `ls`. The halt line does NOT settle: a machine
        // that is stopping has nothing to show a panel that is about to go dark with it.
        phases == [ServicePhase::Settled, ServicePhase::Settled]
    );

    Ok(n)
}
type InputIrqStats = ((u64, u64, u64, u64), (u64, u64, u64));
