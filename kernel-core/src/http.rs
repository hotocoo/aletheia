//! An HTTP/1.1 client, bounded by construction (REQ-WEB-001, ADR-155; Lethe stage N3).
//!
//! The request is a `GET` with `Connection: close`: one conversation, one answer, then the peer
//! closes and the TLS pump (ADR-151) returns. The response is parsed from the bytes the pump
//! collected, into the caller's own buffers, with every length the peer names checked against the
//! bytes that arrived before anything is read past them. Nothing here allocates.
//!
//! ## What is refused, by name
//!
//! A status line that is not `HTTP/1.x NNN reason`; a header without a colon, with a leading space
//! (obsolete folding), or longer than a line may be; more headers than this client will hold; a
//! chunk size that is not hexadecimal or a chunk that runs past the bytes; a body shorter than the
//! `Content-Length` the peer promised; and a response that carries BOTH `Content-Length` and
//! `Transfer-Encoding: chunked` — RFC 7230 §3.3.3 says which wins, and two parsers that disagree
//! about a body boundary is the whole of a request-smuggling bug, so this client refuses the
//! ambiguity rather than resolving it.
//!
//! A body longer than the caller's buffer is TRUNCATED and said to be, never overflowed: on a heap
//! that never frees (ADR-063) there is no "grow the buffer", and a client that buffered whatever the
//! peer sent would be a client the peer could fill.

/// The most headers this client will hold.
pub const MAX_HEADERS: usize = 32;
/// The longest header line, name and value.
pub const MAX_HEADER_LINE: usize = 1024;
/// The longest status line.
pub const MAX_STATUS_LINE: usize = 256;
/// The longest request path this client will send.
pub const MAX_PATH: usize = 512;
/// The most hexadecimal digits a chunk size may have (RFC 7230 puts no limit; this client does).
pub const MAX_CHUNK_DIGITS: usize = 7;

/// Why a request was not built or a response not read. Each is a different fact about the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpRefusal {
    /// The path is not absolute (`/...`), or carries whitespace or a control byte, or is too long.
    BadPath,
    /// The request does not fit the caller's buffer.
    RequestTooLong,
    /// The bytes end before the head (status line and headers) does.
    Incomplete,
    /// The status line is not `HTTP/1.x NNN reason`.
    BadStatusLine,
    /// The version is not HTTP/1.0 or HTTP/1.1.
    BadVersion,
    /// A header line without a colon, or beginning with whitespace (obsolete line folding).
    BadHeader,
    /// A header line longer than this client will hold.
    HeaderTooLong,
    /// More headers than this client will hold.
    TooManyHeaders,
    /// A chunk size that is not hexadecimal, too long, or a chunk that does not end in CRLF.
    BadChunk,
    /// Both `Content-Length` and `Transfer-Encoding: chunked`: two body boundaries.
    Ambiguous,
    /// A `Content-Length` that is not a number this client will read.
    BadContentLength,
}

/// A byte range inside the raw response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub len: usize,
}

impl Span {
    pub fn of<'a>(&self, raw: &'a [u8]) -> &'a [u8] {
        &raw[self.start..self.start + self.len]
    }
}

/// What a response said, as spans into the raw bytes plus the body copied into the caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: Span,
    headers: [(Span, Span); MAX_HEADERS],
    header_count: usize,
    /// Bytes of body written into the caller's buffer.
    pub body_len: usize,
    /// The body was longer than the caller's buffer; the rest was dropped, not written.
    pub truncated: bool,
    /// The body arrived in chunks and was reassembled.
    pub chunked: bool,
}

impl Response {
    /// The headers, in order, as `(name, value)` spans.
    pub fn headers(&self) -> &[(Span, Span)] {
        &self.headers[..self.header_count]
    }

    /// The value of the first header named `name` (case-insensitively), if any.
    pub fn header<'a>(&self, raw: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
        self.headers()
            .iter()
            .find(|(n, _)| n.of(raw).eq_ignore_ascii_case(name))
            .map(|(_, v)| v.of(raw))
    }
}

/// Whether `path` is something this client will put on a request line: absolute, ASCII, visible,
/// no whitespace, bounded.
pub fn path_is_sendable(path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && path[0] == b'/'
        && path.iter().all(|&b| (0x21..0x7f).contains(&b))
}

/// Build `GET path HTTP/1.1` for `host` into `out`, returning its length. `Connection: close` is
/// not optional: it is what makes the answer end.
/// The one user agent this client ever sends (ADR-159): no platform, no language, no build, so
/// no two machines running this kernel can be told apart by it.
pub const USER_AGENT: &[u8] = b"aletheia/0.1";

pub fn request(host: &[u8], path: &[u8], out: &mut [u8]) -> Result<usize, HttpRefusal> {
    if !path_is_sendable(path) {
        return Err(HttpRefusal::BadPath);
    }
    if host.is_empty() || host.len() > 255 || !host.iter().all(|&b| (0x21..0x7f).contains(&b)) {
        return Err(HttpRefusal::BadPath);
    }
    let parts: [&[u8]; 8] = [
        b"GET ",
        path,
        b" HTTP/1.1\r\nHost: ",
        host,
        b"\r\nUser-Agent: ",
        USER_AGENT,
        b"\r\nAccept: */*\r\nConnection: close\r\n",
        b"\r\n",
    ];
    let total: usize = parts.iter().map(|p| p.len()).sum();
    if total > out.len() {
        return Err(HttpRefusal::RequestTooLong);
    }
    let mut at = 0;
    for p in parts {
        out[at..at + p.len()].copy_from_slice(p);
        at += p.len();
    }
    Ok(total)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn parse_decimal(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0usize, |acc, &b| acc * 10 + (b - b'0') as usize),
    )
}

fn parse_hex(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || digits.len() > MAX_CHUNK_DIGITS {
        return None;
    }
    let mut v = 0usize;
    for &b in digits {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as usize;
    }
    Some(v)
}

/// Copy as much of `src` as fits into `dst[*len..]`, reporting whether anything was dropped.
fn take(dst: &mut [u8], len: &mut usize, src: &[u8]) -> bool {
    let room = dst.len() - *len;
    let n = src.len().min(room);
    dst[*len..*len + n].copy_from_slice(&src[..n]);
    *len += n;
    n < src.len()
}

/// Read a whole response from `raw` — everything the peer sent before closing — writing the body
/// into `body`. The head is read as spans; the body is copied, decoded from chunks when the peer
/// sent them, and truncated to the buffer with the truncation reported.
///
/// `raw_truncated` says the TRANSPORT already cut the bytes (the TLS pump filled its caller's
/// buffer and dropped the rest). Then a body shorter than its `Content-Length`, or a chunk that
/// runs out of bytes, is a truncated body — what arrived, said to be cut — not an incomplete
/// response. The head must still be whole: a cut head is a response nobody can read.
pub fn parse(raw: &[u8], body: &mut [u8], raw_truncated: bool) -> Result<Response, HttpRefusal> {
    // The status line.
    let line_end = find(raw, b"\r\n", 0).ok_or(HttpRefusal::Incomplete)?;
    if line_end > MAX_STATUS_LINE {
        return Err(HttpRefusal::BadStatusLine);
    }
    let line = &raw[..line_end];
    if line.len() < 12 || &line[..5] != b"HTTP/" {
        return Err(HttpRefusal::BadStatusLine);
    }
    if &line[5..8] != b"1.1" && &line[5..8] != b"1.0" {
        return Err(HttpRefusal::BadVersion);
    }
    if line[8] != b' ' || !line[9..12].iter().all(|b| b.is_ascii_digit()) {
        return Err(HttpRefusal::BadStatusLine);
    }
    let status = ((line[9] - b'0') as u16) * 100
        + ((line[10] - b'0') as u16) * 10
        + (line[11] - b'0') as u16;
    let reason = if line.len() == 12 {
        Span { start: 12, len: 0 }
    } else if line[12] == b' ' {
        Span {
            start: 13,
            len: line.len() - 13,
        }
    } else {
        return Err(HttpRefusal::BadStatusLine);
    };

    // The headers, one line at a time, to the blank line.
    let mut headers = [(Span::default(), Span::default()); MAX_HEADERS];
    let mut header_count = 0usize;
    let mut at = line_end + 2;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let end = find(raw, b"\r\n", at).ok_or(HttpRefusal::Incomplete)?;
        if end == at {
            at += 2;
            break;
        }
        if end - at > MAX_HEADER_LINE {
            return Err(HttpRefusal::HeaderTooLong);
        }
        let hline = &raw[at..end];
        if hline[0] == b' ' || hline[0] == b'\t' {
            return Err(HttpRefusal::BadHeader);
        }
        let colon = hline
            .iter()
            .position(|&b| b == b':')
            .ok_or(HttpRefusal::BadHeader)?;
        if colon == 0 || hline[..colon].iter().any(|&b| b == b' ' || b == b'\t') {
            return Err(HttpRefusal::BadHeader);
        }
        let mut vstart = colon + 1;
        while vstart < hline.len() && (hline[vstart] == b' ' || hline[vstart] == b'\t') {
            vstart += 1;
        }
        let mut vend = hline.len();
        while vend > vstart && (hline[vend - 1] == b' ' || hline[vend - 1] == b'\t') {
            vend -= 1;
        }
        if header_count == MAX_HEADERS {
            return Err(HttpRefusal::TooManyHeaders);
        }
        let name = Span {
            start: at,
            len: colon,
        };
        let value = Span {
            start: at + vstart,
            len: vend - vstart,
        };
        if name.of(raw).eq_ignore_ascii_case(b"content-length") {
            let n = parse_decimal(value.of(raw)).ok_or(HttpRefusal::BadContentLength)?;
            if content_length.is_some_and(|c| c != n) {
                return Err(HttpRefusal::Ambiguous);
            }
            content_length = Some(n);
        }
        if name.of(raw).eq_ignore_ascii_case(b"transfer-encoding")
            && value.of(raw).eq_ignore_ascii_case(b"chunked")
        {
            chunked = true;
        }
        headers[header_count] = (name, value);
        header_count += 1;
        at = end + 2;
    }
    if chunked && content_length.is_some() {
        return Err(HttpRefusal::Ambiguous);
    }

    // The body.
    let mut body_len = 0usize;
    let mut truncated = false;
    if chunked {
        loop {
            let size_end = match find(raw, b"\r\n", at) {
                Some(e) => e,
                None if raw_truncated => {
                    truncated = true;
                    break;
                }
                None => return Err(HttpRefusal::Incomplete),
            };
            let size_line = &raw[at..size_end];
            // Chunk extensions (`;name=value`) are refused: nothing here reads them, and a
            // parser that skips what it does not read is a parser two peers can disagree about.
            let size = parse_hex(size_line).ok_or(HttpRefusal::BadChunk)?;
            at = size_end + 2;
            if size == 0 {
                // Trailers, if any, up to the final blank line. Not read.
                loop {
                    let end = find(raw, b"\r\n", at).ok_or(HttpRefusal::Incomplete)?;
                    let blank = end == at;
                    at = end + 2;
                    if blank {
                        break;
                    }
                }
                break;
            }
            if at + size + 2 > raw.len() {
                if raw_truncated {
                    let have = raw.len().saturating_sub(at);
                    take(body, &mut body_len, &raw[at..at + have]);
                    truncated = true;
                    break;
                }
                return Err(HttpRefusal::Incomplete);
            }
            if &raw[at + size..at + size + 2] != b"\r\n" {
                return Err(HttpRefusal::BadChunk);
            }
            truncated |= take(body, &mut body_len, &raw[at..at + size]);
            at += size + 2;
        }
    } else if let Some(n) = content_length {
        if raw.len() - at < n {
            if !raw_truncated {
                return Err(HttpRefusal::Incomplete);
            }
            truncated = true;
            take(body, &mut body_len, &raw[at..]);
        } else {
            truncated |= take(body, &mut body_len, &raw[at..at + n]);
        }
    } else {
        truncated |= take(body, &mut body_len, &raw[at..]);
    }

    Ok(Response {
        status,
        reason,
        headers,
        header_count,
        body_len,
        truncated,
        chunked,
    })
}

/// The HTTP contract, proved on every CPU at boot. No network: the request builder and the response
/// reader are pure, and every case here is bytes a server could send.
pub fn http_suite(
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

    // 1 — the request is exactly what RFC 7230 says, and ends the conversation by asking to.
    {
        let mut out = [0u8; 256];
        let len = request(b"aletheia.test", b"/hello", &mut out).unwrap_or(0);
        let want = b"GET /hello HTTP/1.1\r\nHost: aletheia.test\r\nUser-Agent: aletheia/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n";
        check!(
            out[..len] == want[..],
            "http: a GET carries the host, asks the peer to close, and ends with the blank line"
        );
    }

    // 2 — a path that is not absolute, or carries whitespace or a control byte, or is too long,
    //     never reaches a request line.
    {
        let mut out = [0u8; 1024];
        let long = [b'a'; MAX_PATH + 1];
        check!(
            request(b"h", b"hello", &mut out) == Err(HttpRefusal::BadPath)
                && request(b"h", b"/a b", &mut out) == Err(HttpRefusal::BadPath)
                && request(b"h", b"/a\r\nX: y", &mut out) == Err(HttpRefusal::BadPath)
                && request(b"h", b"", &mut out) == Err(HttpRefusal::BadPath)
                && request(b"h", &long, &mut out) == Err(HttpRefusal::BadPath)
                && request(b"h", b"/ok", &mut [0u8; 8]) == Err(HttpRefusal::RequestTooLong),
            "http: a path that is not absolute, or carries whitespace or control bytes, is refused before it is sent"
        );
    }

    // 3 — a Content-Length response reads to its status, reason, headers and exact body.
    {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let mut body = [0u8; 32];
        let ok = match parse(raw, &mut body, false) {
            Ok(r) => {
                r.status == 200
                    && r.reason.of(raw) == b"OK"
                    && r.headers().len() == 2
                    && r.header(raw, b"content-type") == Some(&b"text/plain"[..])
                    && r.body_len == 5
                    && &body[..5] == b"hello"
                    && !r.truncated
                    && !r.chunked
            }
            Err(_) => false,
        };
        check!(
            ok,
            "http: a Content-Length response reads to its status, headers and exact body"
        );
    }

    // 4 — a chunked response is reassembled exactly; the trailer is skipped; the zero chunk ends it.
    {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nExpires: never\r\n\r\n";
        let mut body = [0u8; 64];
        let ok = match parse(raw, &mut body, false) {
            Ok(r) => r.chunked && r.body_len == 23 && &body[..23] == b"Wikipedia in\r\n\r\nchunks.",
            Err(_) => false,
        };
        check!(
            ok,
            "http: a chunked body is reassembled exactly and the zero chunk ends it"
        );
    }

    // 5 — a body longer than the caller's buffer is truncated and said to be, never written past
    //     the buffer.
    {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789";
        let mut guarded = [0xAAu8; 8];
        let ok = match parse(raw, &mut guarded[..4], false) {
            Ok(r) => r.truncated && r.body_len == 4,
            Err(_) => false,
        };
        // The transport's own cut: the same response with its last bytes never delivered is an
        // incomplete response when the bytes are all there were, and a truncated body when the
        // transport says it dropped the rest.
        let cut = &raw[..raw.len() - 4];
        let mut b2 = [0u8; 16];
        let as_incomplete = parse(cut, &mut b2, false);
        let as_truncated = parse(cut, &mut b2, true);
        check!(
            ok && &guarded[..4] == b"0123"
                && guarded[4..].iter().all(|&b| b == 0xAA)
                && as_incomplete == Err(HttpRefusal::Incomplete)
                && as_truncated.is_ok_and(|r| r.truncated && r.body_len == 6),
            "http: a body larger than the caller's buffer is truncated, said so, never overflowed"
        );
    }

    // 6 — malformed heads are refused by name, each for its own reason.
    {
        let mut body = [0u8; 8];
        let mut many = [0u8; 4096];
        let head = b"HTTP/1.1 200 OK\r\n";
        many[..head.len()].copy_from_slice(head);
        let mut at = head.len();
        for _ in 0..(MAX_HEADERS + 1) {
            let h = b"X-A: b\r\n";
            many[at..at + h.len()].copy_from_slice(h);
            at += h.len();
        }
        many[at..at + 2].copy_from_slice(b"\r\n");
        let too_long = {
            let mut v = [b'v'; MAX_HEADER_LINE + 40];
            v[..3].copy_from_slice(b"X: ");
            v
        };
        let mut long_raw = [0u8; 2048];
        long_raw[..head.len()].copy_from_slice(head);
        long_raw[head.len()..head.len() + too_long.len()].copy_from_slice(&too_long);
        let lr = head.len() + too_long.len();
        long_raw[lr..lr + 4].copy_from_slice(b"\r\n\r\n");
        check!(
            parse(b"HTTP/2 200 OK\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadVersion)
                && parse(b"HTTP/1.2 200 OK\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadVersion)
                && parse(b"HTTP/1.1 20 OK\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadStatusLine)
                && parse(b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadHeader)
                && parse(b"HTTP/1.1 200 OK\r\nX: a\r\n folded\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadHeader)
                && parse(b"HTTP/1.1 200 OK\r\nX-Length: 3", &mut body, false) == Err(HttpRefusal::Incomplete)
                && parse(&many[..at + 2], &mut body, false) == Err(HttpRefusal::TooManyHeaders)
                && parse(&long_raw[..lr + 4], &mut body, false) == Err(HttpRefusal::HeaderTooLong),
            "http: a bad status line, version, header, fold, unfinished head, or too many or too long headers is refused by name"
        );
    }

    // 7 — a chunk that is not what it says is refused: a size that is not hex, a size with an
    //     extension this client does not read, a chunk that runs past the bytes, a chunk not
    //     ending in CRLF.
    {
        let mut body = [0u8; 64];
        check!(
            parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nab\r\n0\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadChunk)
                && parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;ext=1\r\nab\r\n0\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadChunk)
                && parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab\r\n", &mut body, false) == Err(HttpRefusal::Incomplete)
                && parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabX\r\n0\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadChunk),
            "http: a chunk whose size is not hex, carries an extension, or does not end where it says is refused"
        );
    }

    // 8 — two body boundaries are an ambiguity this client refuses rather than resolves, and a
    //     Content-Length the bytes do not honour is incomplete, not a shorter body.
    {
        let mut body = [0u8; 64];
        check!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n0\r\n\r\n", &mut body, false) == Err(HttpRefusal::Ambiguous)
                && parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc", &mut body, false) == Err(HttpRefusal::Ambiguous)
                && parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc", &mut body, false) == Err(HttpRefusal::Incomplete)
                && parse(b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n", &mut body, false) == Err(HttpRefusal::BadContentLength),
            "http: two body boundaries are refused as ambiguous, and a short Content-Length body is incomplete"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_http_invariant() {
        let mut seen = 0;
        let n = http_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the http suite should hold");
        assert_eq!(n, 8);
        assert_eq!(seen, 8);
    }

    #[test]
    fn every_truncation_of_a_response_is_incomplete_or_a_refusal_never_a_wrong_answer() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        for cut in 0..raw.len() {
            let mut body = [0u8; 8];
            match parse(&raw[..cut], &mut body, false) {
                Ok(r) => panic!(
                    "cut {cut} parsed to a response with {} body bytes",
                    r.body_len
                ),
                Err(HttpRefusal::Incomplete) | Err(HttpRefusal::BadStatusLine) => {}
                Err(other) => panic!("cut {cut}: unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn a_response_with_no_length_reads_to_the_close() {
        let raw = b"HTTP/1.0 200 OK\r\n\r\nuntil the peer closes";
        let mut body = [0u8; 64];
        let r = parse(raw, &mut body, false).expect("parses");
        assert_eq!(&body[..r.body_len], b"until the peer closes");
        assert!(!r.truncated);
    }
}
