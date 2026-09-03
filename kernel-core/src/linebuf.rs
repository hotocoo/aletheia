//! A line of output that costs no heap (REQ-CON-001 / REQ-QUAL-007, ADR-089).
//!
//! The console formatted every line it printed with `format!` — one heap allocation per line, on
//! a heap that never frees (ADR-063). A session is exactly a stream of lines, so an operator who
//! kept typing kept spending memory: measured at ~450 bytes per command before this module.
//!
//! [`LineBuf`] is the fixed-size answer: a stack buffer that implements [`core::fmt::Write`], so
//! every `write!` the console already knew how to do still works, and nothing is allocated.
//!
//! **Truncation is NAMED, never silent.** A line longer than the buffer stops at the last byte
//! that fits and the buffer remembers it was cut ([`LineBuf::truncated`]); the console appends its
//! own ellipsis where that matters. Writing UTF-8 across the boundary keeps the buffer VALID: a
//! partial multi-byte sequence is dropped rather than stored, because a console that could emit
//! half a character would be a console whose output cannot be trusted to round-trip.

use core::fmt::Write;

/// The default line width. Wide enough for every console line this kernel prints (the widest is a
/// wrapped `mlstat` census row), and small enough to live on a boot stack without thought.
pub const LINE_MAX: usize = 256;

/// A fixed-size line of formatted output.
pub struct LineBuf<const N: usize = LINE_MAX> {
    buf: [u8; N],
    len: usize,
    truncated: bool,
}

impl<const N: usize> Default for LineBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LineBuf<N> {
    pub const fn new() -> Self {
        LineBuf {
            buf: [0u8; N],
            len: 0,
            truncated: false,
        }
    }

    /// What has been written so far. Always valid UTF-8 (see the module docs).
    pub fn as_str(&self) -> &str {
        // SAFETY-equivalent argument without unsafe: `write_str` only ever copies whole UTF-8
        // sequences, so the prefix is valid by construction; `from_utf8` re-checks anyway and the
        // fallback keeps the console printing rather than panicking.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// Did anything have to be cut to fit?
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.truncated = false;
    }
}

impl<const N: usize> Write for LineBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for ch in s.chars() {
            let mut enc = [0u8; 4];
            let bytes = ch.encode_utf8(&mut enc).as_bytes();
            if self.len + bytes.len() > N {
                // Whole characters only: a half-written sequence would make `as_str` invalid.
                self.truncated = true;
                return Ok(()); // Ok: truncation is a REPORTED outcome, not a write failure.
            }
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
        Ok(())
    }
}

/// Format one console line on the stack and hand it to `out` — the allocation-free replacement
/// for `out(&format!(...))` (ADR-089).
#[macro_export]
macro_rules! outf {
    ($out:expr, $($arg:tt)*) => {{
        let mut __line = $crate::linebuf::LineBuf::<{ $crate::linebuf::LINE_MAX }>::new();
        let _ = core::fmt::Write::write_fmt(&mut __line, format_args!($($arg)*));
        $out(__line.as_str());
    }};
}

/// The suite for the buffer itself (ADR-089): bounded, exact, and UTF-8 safe at the edge.
pub fn linebuf_suite(
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

    // 1 — an ordinary line comes back exactly as written, and nothing is marked truncated.
    {
        let mut b = LineBuf::<32>::new();
        let _ = write!(b, "free {} of {}", 900, 1024);
        check!(
            b.as_str() == "free 900 of 1024" && !b.truncated(),
            "linebuf: a line that fits comes back byte-for-byte, untruncated"
        );
    }
    // 2 — a line longer than the buffer is CUT at the boundary and SAYS so; the prefix is exact.
    {
        let mut b = LineBuf::<8>::new();
        let _ = write!(b, "abcdefghij");
        check!(
            b.as_str() == "abcdefgh" && b.truncated(),
            "linebuf: an over-long line is cut at the boundary and reports that it was cut"
        );
    }
    // 3 — truncation never splits a character: a multi-byte glyph that does not fit is dropped
    //     whole, so what comes back is always valid UTF-8.
    {
        let mut b = LineBuf::<4>::new();
        let _ = write!(b, "ab\u{00e9}\u{00e9}"); // each 'é' is two bytes
        check!(
            b.as_str() == "ab\u{00e9}"
                && b.truncated()
                && b.as_str().len() == 4
                && core::str::from_utf8(b.as_str().as_bytes()).is_ok(),
            "linebuf: truncation drops a whole character rather than splitting one"
        );
    }
    // 4 — clear resets both the bytes and the truncation flag, so a reused buffer cannot inherit
    //     a previous line's verdict.
    {
        let mut b = LineBuf::<4>::new();
        let _ = write!(b, "toolong");
        b.clear();
        let _ = write!(b, "ok");
        check!(
            b.as_str() == "ok" && !b.truncated(),
            "linebuf: clearing resets the bytes and the truncation flag together"
        );
    }
    Ok(n)
}
