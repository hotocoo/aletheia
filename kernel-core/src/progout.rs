//! What a console-started program writes, held without allocating (ADR-204).
//!
//! `SYS_WRITE_CONSOLE` is served on the trap path, where nothing may allocate (ADR-086) and nothing
//! may be lost without saying so. The sink keeps the first [`CAPACITY`] bytes a run writes and
//! COUNTS the rest; the console shows what was kept and says how much was not.

use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Target};
use crate::usermem::UserSlice;

/// Bytes one run may leave for the console.
pub const CAPACITY: usize = 256;

/// The authority a console `run` hands its program (ADR-204): one `console.output` capability,
/// minted when the run starts and gone when it ends. The chain is the operator's `run` (itself
/// authorized as `system.schedule`) -> this grant -> `SYS_WRITE_CONSOLE`; a task started any other
/// way holds no grant and is refused by the same evaluation every syscall effect goes through.
pub struct Grant {
    engine: CapEngine,
    tokens: [CapToken; 2],
}

/// The action `SYS_WRITE_CONSOLE` is authorized as.
pub const ACTION: &str = "console.output";
/// The action `SYS_FS_READ` is authorized as (ADR-207).
pub const FS_READ: &str = "fs.read";

impl Grant {
    pub fn new(secret: u64) -> Self {
        let mut engine = CapEngine::new(secret, 0);
        let output = engine.mint("program:run", ACTION, Scope::All, Constraints::none());
        let read = engine.mint("program:run", FS_READ, Scope::All, Constraints::none());
        Grant {
            engine,
            tokens: [output, read],
        }
    }

    /// Whether this run's program may perform `action`.
    pub fn allows(&self, action: &str) -> bool {
        self.engine
            .evaluate(action, &Target::default(), &self.tokens)
            == Decision::Allow
    }
}

/// A `SYS_FS_READ` the trap handler admitted (ADR-207), for the run loop to serve once the program
/// is off the CPU: the loop is back in the console's address space with the namespace in reach,
/// and it reads and writes the program's pages through their physical frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadRequest {
    /// Offset of the name in the page `name_in_stack` names.
    pub name_off: usize,
    pub name_len: usize,
    /// The name lies in the stack page (else in the code page).
    pub name_in_stack: bool,
    /// Offset of the buffer in the page `buf_in_data` names.
    pub buf_off: usize,
    pub buf_len: usize,
    /// The buffer lies in the data page (ADR-210), else in the stack page.
    pub buf_in_data: bool,
}

/// Admit `SYS_FS_READ(name, name_len, buf, buf_len)` or refuse it (`None`), touching no memory.
/// The name must lie inside ONE of the program's two pages (served through physical frames, which
/// are not adjacent, a straddling name would have to be stitched: refused) and be at most
/// [`crate::fs::MAX_NAME`] bytes; the buffer must lie inside the STACK page only - written through
/// its frame, a buffer in the read+execute code page would rewrite the program's text.
pub fn admit_read(
    grant: Option<&Grant>,
    name: u64,
    name_len: u64,
    buf: u64,
    buf_len: u64,
    w: Window,
) -> Option<ReadRequest> {
    let (code_va, stack_va, stack_top) = (w.code_va, w.stack_va, w.stack_top);
    if !grant.is_some_and(|g| g.allows(FS_READ)) {
        return None;
    }
    let name_len = usize::try_from(name_len).ok()?;
    let buf_len = usize::try_from(buf_len).ok()?;
    if name_len == 0 || name_len > crate::fs::MAX_NAME {
        return None;
    }
    let (name_in_stack, page) = if UserSlice::validate(name, name_len, code_va, stack_va).is_ok() {
        (false, code_va)
    } else if UserSlice::validate(name, name_len, stack_va, stack_top).is_ok() {
        (true, stack_va)
    } else {
        return None;
    };
    // The buffer goes in the stack page or the data page - never the read+execute code page, where
    // a write through its frame would rewrite the program's own text.
    let buf_in_data = UserSlice::validate(buf, buf_len, stack_top, w.data_top).is_ok();
    if !buf_in_data {
        UserSlice::validate(buf, buf_len, stack_va, stack_top).ok()?;
    }
    Some(ReadRequest {
        name_off: (name - page) as usize,
        name_len,
        name_in_stack,
        buf_off: (buf - if buf_in_data { stack_top } else { stack_va }) as usize,
        buf_len,
        buf_in_data,
    })
}

/// Serve an admitted read (ADR-207) over the program's two pages as the run loop sees them - its
/// code page and its stack page, each viewed through its physical frame. The name is copied out
/// first (it may lie in the stack page, which is also where the object lands). Returns the object's
/// full length, or `u64::MAX` when the name is not UTF-8 or the namespace refuses it.
pub fn serve_read(
    req: &ReadRequest,
    services: &mut dyn ProgramServices,
    code_page: &[u8],
    stack_page: &mut [u8],
    data_page: &mut [u8],
) -> u64 {
    let mut name = [0u8; crate::fs::MAX_NAME];
    let src = if req.name_in_stack {
        &*stack_page
    } else {
        code_page
    };
    let Some(bytes) = src.get(req.name_off..req.name_off + req.name_len) else {
        return u64::MAX;
    };
    name[..req.name_len].copy_from_slice(bytes);
    let Ok(name) = core::str::from_utf8(&name[..req.name_len]) else {
        return u64::MAX;
    };
    let target = if req.buf_in_data {
        data_page
    } else {
        stack_page
    };
    let Some(buf) = target.get_mut(req.buf_off..req.buf_off + req.buf_len) else {
        return u64::MAX;
    };
    match services.read_object(name, buf) {
        Ok(len) => len as u64,
        Err(_) => u64::MAX,
    }
}

/// What a program may ask of the namespace that started it (ADR-207).
pub trait ProgramServices {
    /// Copy object `name` into `out`; the object's full length, or why not.
    fn read_object(&mut self, name: &str, out: &mut [u8]) -> Result<usize, crate::fs::FsError>;
}

/// A mounted namespace, as a running program may read it (ADR-207): the console hands its own, the
/// boot suite a scratch one. Reads do not allocate.
pub struct FsServices<'a, D: crate::storage::BlockDevice> {
    pub fs: &'a crate::fs::Filesystem,
    pub dev: &'a D,
}

impl<D: crate::storage::BlockDevice> ProgramServices for FsServices<'_, D> {
    fn read_object(&mut self, name: &str, out: &mut [u8]) -> Result<usize, crate::fs::FsError> {
        self.fs.read_into(self.dev, name, out)
    }
}

/// A run with no namespace at all: every read is refused.
pub struct NoServices;

impl ProgramServices for NoServices {
    fn read_object(&mut self, _: &str, _: &mut [u8]) -> Result<usize, crate::fs::FsError> {
        Err(crate::fs::FsError::NotFound)
    }
}

/// Serve one `SYS_WRITE_CONSOLE(addr, len)`: refuse without a grant, refuse a range outside the
/// program's window `[user_start, user_end)` (which, on every target, is exactly its two mapped
/// pages), otherwise hand the range to `read` - the target's copy, in the task's address space -
/// and keep what fits. Returns the bytes kept, or `u64::MAX` when refused; nothing is appended on a
/// refusal.
/// The three pages a program has (ADR-210): code, stack, then data. `data_top` is the end of the
/// data page when the program declared one, else the stack top - a program without a data segment
/// has no third page and no address there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub code_va: u64,
    pub stack_va: u64,
    pub stack_top: u64,
    pub data_top: u64,
}

impl Window {
    /// The whole span a program may name: its code page through its last mapped page.
    pub fn end(self) -> u64 {
        self.data_top
    }
}

pub fn serve_write<'a>(
    grant: Option<&Grant>,
    sink: &mut OutputSink,
    addr: u64,
    len: u64,
    user_start: u64,
    user_end: u64,
    read: impl FnOnce(UserSlice) -> &'a [u8],
) -> u64 {
    if !grant.is_some_and(|g| g.allows(ACTION)) {
        return u64::MAX;
    }
    let Ok(len) = usize::try_from(len) else {
        return u64::MAX;
    };
    match UserSlice::validate(addr, len, user_start, user_end) {
        Ok(range) => sink.append(read(range)) as u64,
        Err(_) => u64::MAX,
    }
}

/// One run's output: the bytes kept and the number refused for want of room.
pub struct OutputSink {
    buf: [u8; CAPACITY],
    len: usize,
    dropped: u64,
}

impl OutputSink {
    pub const fn new() -> Self {
        OutputSink {
            buf: [0; CAPACITY],
            len: 0,
            dropped: 0,
        }
    }

    /// Empty the sink for the next run.
    pub fn reset(&mut self) {
        self.len = 0;
        self.dropped = 0;
    }

    /// Keep as much of `bytes` as fits; count the rest. Returns the bytes kept.
    pub fn append(&mut self, bytes: &[u8]) -> usize {
        let room = CAPACITY - self.len;
        let kept = bytes.len().min(room);
        self.buf[self.len..self.len + kept].copy_from_slice(&bytes[..kept]);
        self.len += kept;
        self.dropped += (bytes.len() - kept) as u64;
        kept
    }

    /// The bytes kept, in order.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Bytes refused because the sink was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for OutputSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static PAGE: [u8; 64] = [b'a'; 64];

    /// A program's three pages: code at 0x1000, stack at 0x2000, data at 0x3000.
    const W: Window = Window {
        code_va: 0x1000,
        stack_va: 0x2000,
        stack_top: 0x3000,
        data_top: 0x4000,
    };
    /// The same program without a data segment: its last page is its stack.
    const W_NODATA: Window = Window {
        data_top: 0x3000,
        ..W
    };

    #[test]
    fn a_read_is_admitted_only_with_the_grant_a_one_page_name_and_a_writable_buffer() {
        let g = Grant::new(9);
        let ok = admit_read(Some(&g), 0x1080, 5, 0x2100, 64, W);
        assert_eq!(
            ok,
            Some(ReadRequest {
                name_off: 0x80,
                name_len: 5,
                name_in_stack: false,
                buf_off: 0x100,
                buf_len: 64,
                buf_in_data: false
            })
        );
        assert!(
            admit_read(Some(&g), 0x2ff0, 5, 0x2000, 16, W)
                .unwrap()
                .name_in_stack
        );
        // The buffer may lie in the data page, and its offset is then that page's (ADR-210); a
        // program with no data page has no address there.
        let into_data = admit_read(Some(&g), 0x1080, 5, 0x3040, 16, W).unwrap();
        assert!(into_data.buf_in_data && into_data.buf_off == 0x40);
        assert_eq!(admit_read(Some(&g), 0x1080, 5, 0x3040, 16, W_NODATA), None);
        assert_eq!(admit_read(None, 0x1080, 5, 0x2100, 64, W), None, "no grant");
        assert_eq!(
            admit_read(Some(&g), 0x1ffe, 4, 0x2100, 64, W),
            None,
            "name straddles the pages"
        );
        assert_eq!(
            admit_read(Some(&g), 0x1080, 5, 0x1100, 64, W),
            None,
            "buffer in the code page"
        );
        assert_eq!(
            admit_read(Some(&g), 0x1080, 5, 0x2ff0, 64, W),
            None,
            "buffer past the stack top"
        );
        assert_eq!(
            admit_read(Some(&g), 0x1080, 0, 0x2100, 64, W),
            None,
            "empty name"
        );
        assert_eq!(
            admit_read(Some(&g), 0x1080, 41, 0x2100, 64, W),
            None,
            "name past MAX_NAME"
        );
        assert_eq!(
            admit_read(Some(&g), 0x0800, 5, 0x2100, 64, W),
            None,
            "name outside"
        );
    }

    struct One;
    impl ProgramServices for One {
        fn read_object(&mut self, name: &str, out: &mut [u8]) -> Result<usize, crate::fs::FsError> {
            if name != "note" {
                return Err(crate::fs::FsError::NotFound);
            }
            let body = b"just words";
            let n = body.len().min(out.len());
            out[..n].copy_from_slice(&body[..n]);
            Ok(body.len())
        }
    }

    #[test]
    fn a_served_read_lands_in_the_stack_page_and_reports_the_full_length() {
        let mut code = [0u8; 4096];
        code[0x80..0x84].copy_from_slice(b"note");
        let mut stack = [0u8; 4096];
        let mut data = [0u8; 4096];
        let req = ReadRequest {
            name_off: 0x80,
            name_len: 4,
            name_in_stack: false,
            buf_off: 0x200,
            buf_len: 4,
            buf_in_data: false,
        };
        assert_eq!(
            serve_read(&req, &mut One, &code, &mut stack, &mut data),
            10,
            "full length"
        );
        assert_eq!(&stack[0x200..0x204], b"just", "cut to the buffer");
        // The same read, landing in the data page instead (ADR-210).
        let into_data = ReadRequest {
            buf_off: 0x40,
            buf_len: 16,
            buf_in_data: true,
            ..req
        };
        assert_eq!(
            serve_read(&into_data, &mut One, &code, &mut stack, &mut data),
            10
        );
        assert_eq!(&data[0x40..0x4a], b"just words");
        // The name may live in the stack page, where the object also lands.
        stack[0x10..0x14].copy_from_slice(b"note");
        let req = ReadRequest {
            name_off: 0x10,
            name_len: 4,
            name_in_stack: true,
            buf_off: 0x10,
            buf_len: 16,
            buf_in_data: false,
        };
        assert_eq!(serve_read(&req, &mut One, &code, &mut stack, &mut data), 10);
        assert_eq!(&stack[0x10..0x1a], b"just words");
        // Unknown and non-UTF-8 names are refused.
        code[0x80..0x84].copy_from_slice(b"nope");
        let req = ReadRequest {
            name_off: 0x80,
            name_len: 4,
            name_in_stack: false,
            buf_off: 0,
            buf_len: 4,
            buf_in_data: false,
        };
        assert_eq!(
            serve_read(&req, &mut One, &code, &mut stack, &mut data),
            u64::MAX
        );
        code[0x80] = 0xff;
        assert_eq!(
            serve_read(&req, &mut One, &code, &mut stack, &mut data),
            u64::MAX
        );
        assert_eq!(
            serve_read(&req, &mut NoServices, &code, &mut stack, &mut data),
            u64::MAX
        );
    }

    #[test]
    fn a_write_needs_the_grant_and_a_range_inside_the_window() {
        let grant = Grant::new(7);
        let mut sink = OutputSink::new();
        let read = |r: UserSlice| &PAGE[..r.len()];
        // No grant: refused, nothing kept.
        assert_eq!(
            serve_write(None, &mut sink, 0x1000, 4, 0x1000, 0x3000, read),
            u64::MAX
        );
        assert!(sink.bytes().is_empty());
        // Inside the window: kept.
        assert_eq!(
            serve_write(Some(&grant), &mut sink, 0x1000, 4, 0x1000, 0x3000, read),
            4
        );
        // Outside, straddling, overflowing, past the copy budget: refused, nothing appended.
        for (addr, len) in [
            (0x0800u64, 4u64),
            (0x2ffe, 4),
            (u64::MAX - 1, 4),
            (0x1000, (crate::usermem::MAX_USER_COPY + 1) as u64),
            (0xffff_0000_0000_0000, 4),
        ] {
            assert_eq!(
                serve_write(Some(&grant), &mut sink, addr, len, 0x1000, 0x3000, read),
                u64::MAX
            );
        }
        assert_eq!(sink.bytes(), b"aaaa");
    }

    #[test]
    fn keeps_what_fits_and_counts_the_rest() {
        let mut s = OutputSink::new();
        assert_eq!(s.append(b"hello"), 5);
        assert_eq!(s.bytes(), b"hello");
        assert_eq!(s.append(&[b'x'; 300]), CAPACITY - 5);
        assert_eq!(s.bytes().len(), CAPACITY);
        assert_eq!(s.dropped(), 300 - (CAPACITY as u64 - 5));
        assert_eq!(s.append(b"more"), 0);
        assert_eq!(s.dropped(), 300 - (CAPACITY as u64 - 5) + 4);
        s.reset();
        assert!(s.bytes().is_empty());
        assert_eq!(s.dropped(), 0);
    }
}
