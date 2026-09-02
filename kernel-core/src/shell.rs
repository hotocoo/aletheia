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
    engine.evaluate(
        action.capability(),
        &crate::spine::Target::default(),
        offered,
    ) == crate::spine::Decision::Allow
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
    /// The machine's live input session, if the target installed one (ALET-P2-021's hardware
    /// rung, ADR-080). Default `None` — a target with no desktop says so instead of reporting
    /// zeros that would look like a session nobody can steer.
    fn input_facts(&self) -> Option<InputFacts> {
        None
    }
    /// Processors this kernel brought up. Defaulted to one because a target that has not answered
    /// the question has exactly one core it is sure about, and claiming more would be a claim about
    /// hardware nobody enumerated.
    fn cpu_count(&self) -> usize {
        1
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
    ("mem", "physical memory, in frames and bytes"),
    ("faults", "supervisor containment and escalation counters"),
    (
        "input",
        "the machine's input session: cursor, focus, counters",
    ),
    (
        "mlstat",
        "the resident risk advisor: what it is, and what it has done since boot",
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

fn authorize<H: ShellHost>(host: &H, action: ShellAction, out: &mut dyn FnMut(&str)) -> bool {
    if host.authorize(action) {
        true
    } else {
        out(&format!("permission denied: {}", action.label()));
        false
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
                        Some((trees, nodes, compares)) => out(&format!(
                            "risk advisor: RESIDENT — {} trees, {} nodes, worst case {} compares per advice",
                            trees, nodes, compares
                        )),
                        // Named absence: a machine running without advice says which check refused
                        // the model, never merely omits the line.
                        None => match crate::mlsched::resident::model_error() {
                            Some(e) => out(&format!("risk advisor: REFUSED — {:?}", e)),
                            None => out("risk advisor: installed with no model (control arm)"),
                        },
                    }
            out(&format!(
                "advices: {} ({} low / {} elevated / {} abstain: {} band, {} degenerate input)",
                s.advices, s.low, s.elevated, s.abstain, s.band_abstain, s.degenerate_abstain
            ));
            out(&format!(
                "decisive: {}.{}% — {} out-of-box arrival(s) declined",
                s.decisive_permille() / 10,
                s.decisive_permille() % 10,
                s.out_of_range
            ));
            out(&format!(
                        "watching: {} dispatch(es), {} finished / {} failed / {} evicted, {} housekeeping tick(s)",
                        s.schedules, s.finished, s.failed, s.evicted, s.ticks
                    ));
            // The falsifiable one. A model consulted in a burst at boot and never since has
            // a longest gap equal to its uptime, and this line says so.
            out(&format!(
                "continuity: first advice at {}s, last at {}s (span {}s), longest gap {}s",
                s.first_advice_secs,
                s.last_advice_secs,
                s.span_secs(),
                s.max_gap_secs
            ));
            // The falsifiable line: a historical gap closes only when the NEXT advice
            // arrives, so an advisor that fell silent keeps reporting the small gaps it
            // managed while busy. Silence is measured against the machine's own clock and
            // grows with it.
            out(&format!(
                "silence: {}s since the last advice, as of the machine's clock at {}s",
                s.silence_secs(),
                s.last_tick_secs
            ));
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
                out(&format!("  {:<18} {}", name, doc));
            }
        }
        "ver" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            out(VERSION);
            out(&format!(
                "target {}, {} processor(s), privilege level {}",
                host.arch(),
                host.cpu_count(),
                host.privilege()
            ));
            // Said here rather than only in a document, because the person most likely to
            // over-claim about this system is the one sitting in front of it.
            out("this console runs in kernel space over the kernel's own objects; it is not a");
            out("user-mode shell over a syscall ABI, and nothing here is production-ready.");
        }
        "arch" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            out(&format!(
                "{} (privilege level {}, {} processor(s))",
                host.arch(),
                host.privilege(),
                host.cpu_count()
            ));
        }
        "uptime" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let ns = host.uptime_ns();
            let secs = ns / 1_000_000_000;
            out(&format!(
                "up {}h {:02}m {:02}s ({} ns since boot)",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60,
                ns
            ));
        }
        "mem" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let (free, total) = (host.free_frames(), host.total_frames());
            let bytes = host.frame_bytes();
            out(&format!(
                "frames: {} free / {} total ({} used)",
                free,
                total,
                total.saturating_sub(free)
            ));
            out(&format!(
                "memory: {} MiB free / {} MiB managed ({} B per frame)",
                (free * bytes) / (1024 * 1024),
                (total * bytes) / (1024 * 1024),
                bytes
            ));
            out(&format!(
                "input: {} byte(s) dropped since boot",
                host.input_dropped()
            ));
        }
        "faults" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            out(&format!(
                "supervisor: {} user task(s) contained, {} fault(s) escalated",
                host.supervisor_terminated(),
                host.supervisor_escalations()
            ));
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
                    out(&format!(
                        "session: held; events posted {} dropped {} refused {}",
                        f.events_posted, f.dropped, f.refusals
                    ));
                    match f.cursor {
                        Some((x, y)) => out(&format!("cursor: ({}, {}) shown", x, y)),
                        None => out("cursor: hidden"),
                    }
                    match f.focus {
                        Some(id) => out(&format!("focus: surface {} ({} queued)", id, f.queued)),
                        None => out("focus: none"),
                    }
                    out(&format!(
                        "devices: keyboard {} events, pointer {} events",
                        f.kb_events, f.pt_events
                    ));
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
        "lsblk" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            let n = dev.num_blocks();
            out(&format!(
                "{} blocks of {} bytes = {} KiB",
                n,
                BLOCK_SIZE,
                (n * BLOCK_SIZE) / 1024
            ));
        }
        "df" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match fs.free_blocks(dev) {
                Ok(free) => out(&format!(
                    "{} free data blocks of {} bytes each",
                    free, BLOCK_SIZE
                )),
                Err(e) => out(&fs_error(e)),
            }
        }
        "ls" => {
            if !authorize(host, ShellAction::Inspect, out) {
                return Outcome::Continue;
            }
            match fs.list(dev) {
                Ok(entries) if entries.is_empty() => out("(no objects)"),
                Ok(entries) => {
                    for e in entries {
                        out(&format!("{:>8}  {}", e.len, e.name));
                    }
                }
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
                        Err(_) => out(&format!("<{} bytes, not text>", bytes.len())),
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
                    Ok(()) => out(&format!("wrote {} bytes to {}", text.len(), name)),
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
                    Ok(()) => out(&format!("removed {}", rest)),
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
                match fs.list(dev) {
                    Ok(entries) => {
                        let mut seen = 0usize;
                        for e in entries.iter().filter(|e| e.name.starts_with(rest)) {
                            out(&format!("{:>8}  {}", e.len, e.name));
                            seen += 1;
                        }
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
                            Err(_) => out(&format!("<{} bytes, not text>", bytes.len())),
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
                        out(&format!(
                            "{:>8} {:>8} {:>8}  {}",
                            lines,
                            words,
                            bytes.len(),
                            rest
                        ));
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
                                    out(&format!("{}: {}", i + 1, line));
                                    hits += 1;
                                }
                            }
                            if hits == 0 {
                                out("(no matching line)");
                            }
                        }
                        Err(_) => out(&format!("<{} bytes, not text>", bytes.len())),
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
                                    hex.push_str(&format!("{:02x} ", **b));
                                    txt.push(if b.is_ascii_graphic() || **b == b' ' {
                                        **b as char
                                    } else {
                                        '.'
                                    });
                                }
                                out(&format!("{:08x}  {:<48} |{}|", row * 16, hex, txt));
                            }
                            if bytes.is_empty() {
                                out("(empty)");
                            } else if bytes.len() > n {
                                out(&format!("… {} more byte(s)", bytes.len() - n));
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
                            Ok(()) => out(&format!("{} is now {} bytes", name, bytes.len())),
                            Err(e) => out(&fs_error(e)),
                        }
                    }
                    Err(FsError::NotFound) => {
                        let mut bytes = Vec::from(text.as_bytes());
                        bytes.push(b'\n');
                        match fs.create(dev, name, &bytes) {
                            Ok(()) => out(&format!("created {} ({} bytes)", name, bytes.len())),
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
                    Ok(e) => out(&format!("{} exists ({} bytes)", e.name, e.len)),
                    Err(FsError::NotFound) => match fs.create(dev, rest, b"") {
                        Ok(()) => out(&format!("created {}", rest)),
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
                out(&format!("usage: {} SRC DST", verb));
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
                                    Ok(()) => out(&format!("{} -> {}", src, dst)),
                                    Err(e) => out(&format!(
                                        "copied, but {} remains: {}",
                                        src,
                                        fs_error(e)
                                    )),
                                }
                            } else {
                                out(&format!("{} -> {} ({} bytes)", src, dst, bytes.len()));
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
                Err(e) => out(&format!("storage: {}", storage_error(e))),
            }
        }
        "history" => {
            if history.is_empty() {
                out("(nothing yet)");
            } else {
                for (i, line) in history.iter().enumerate() {
                    out(&format!("{:>4}  {}", i + 1, line));
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
                // The history the `history` command prints is the SAME list the up arrow walks:
                // one list, so what the operator is shown and what they can recall cannot diverge.
                let history = self.editor.history().to_vec();
                let outcome = execute(&line, host, fs, dev, &history, &mut |s| {
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
        let Some(byte) = getc() else {
            // Nothing typed. Wait for an interrupt rather than asking again immediately; see
            // `ShellHost::idle`, which does nothing at all unless a target has said it is safe.
            host.idle();
            continue;
        };
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

    Ok(n)
}
