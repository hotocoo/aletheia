//! The hostile page (ADR-161): a deterministic property campaign over the browser stack.
//!
//! Every byte the browser reads arrives from a peer that may be an adversary: the HTTP head and
//! body, the HTML, the URLs a page offers. The boot suites prove named behaviours on named inputs;
//! this campaign generates THOUSANDS of adversarial inputs from a seed - malformed heads, bodies
//! that lie about their length, markup with unterminated tags, script bodies, control bytes, deep
//! nesting, URLs of every byte - and checks the properties that must hold for ALL of them:
//!
//! * nothing panics, nothing writes past a buffer (guard bytes stay untouched);
//! * every byte the renderer shows is printable ASCII or a newline, and script/style CONTENT
//!   never reaches the page;
//! * every bound is honoured (links, hrefs, title, body, headers) and every cut is SAID;
//! * the same input renders and parses identically twice (no hidden state);
//! * a URL that parses writes back to a URL that parses to the same value;
//! * a request line can never be split by its inputs: a host or path carrying CR/LF is refused;
//! * a navigator driven by random operations never resolves an unpinned or blocked host and never
//!   holds more history than its ring.
//!
//! The generator is tiny and dependency-free, like `property_campaign.rs`. A failure prints a
//! `PROPERTY FAILURE seed=... case=...` line that reproduces it, and the failing document is
//! shrunk toward a smaller counterexample before the panic is re-raised.

use kernel_core::browser::{parse_url, NavRefusal, Navigator, HISTORY, MAX_HOST, MAX_URL};
use kernel_core::content::{render, HREF_CAP, MAX_INPUT, MAX_LINKS, TITLE_CAP};
use kernel_core::http::{parse, path_is_sendable, request, HttpRefusal, MAX_HEADERS};
use kernel_core::textgrid::TextGrid;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Small deterministic generator (splitmix64), the same shape `property_campaign.rs` uses.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize % (hi - lo + 1))
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.range(1, one_in) == 1
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(0, items.len() - 1)]
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
    fn bytes_range(&mut self, lo: usize, hi: usize) -> Vec<u8> {
        let n = self.range(lo, hi);
        self.bytes(n)
    }
}

fn seed_and_count() -> (u64, usize) {
    let seed = std::env::var("ALETHEIA_PROPERTY_SEED")
        .ok()
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0xA1E7_0210_5EED);
    let count = std::env::var("ALETHEIA_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(32);
    assert!(
        count > 0,
        "the campaign must execute at least one generated case"
    );
    (seed, count)
}

/// Run `body` for every generated case; on the first failure print the reproducing line, shrink
/// the input with `shrink` while it still fails, and re-raise.
fn campaign<T: Clone + std::fmt::Debug>(
    what: &str,
    seed: u64,
    count: usize,
    mut generate: impl FnMut(&mut Gen, usize) -> T,
    check: impl Fn(&T) + Copy,
    shrink: impl Fn(&T) -> Vec<T>,
) {
    let mut g = Gen(seed ^ (what.len() as u64).wrapping_mul(0x1000_0000_01B3));
    for i in 0..count {
        let input = generate(&mut g, i);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| check(&input))) {
            let mut smallest = input.clone();
            let mut progressed = true;
            while progressed {
                progressed = false;
                for candidate in shrink(&smallest) {
                    if catch_unwind(AssertUnwindSafe(|| check(&candidate))).is_err() {
                        smallest = candidate;
                        progressed = true;
                        break;
                    }
                }
            }
            eprintln!(
                "PROPERTY FAILURE campaign={what} seed=0x{seed:016x} case={i} minimized={smallest:?}"
            );
            std::panic::resume_unwind(payload);
        }
    }
}

fn halves(v: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if v.len() > 1 {
        out.push(v[..v.len() / 2].to_vec());
        out.push(v[v.len() / 2..].to_vec());
        out.push(v[..v.len() - 1].to_vec());
        out.push(v[1..].to_vec());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The renderer.
// ---------------------------------------------------------------------------------------------

const TAGS: [&str; 22] = [
    "p", "h1", "h2", "h6", "br", "hr", "ul", "ol", "li", "pre", "a", "title", "div", "span", "b",
    "table", "script", "style", "template", "iframe", "object", "embed",
];
const ENTITIES: [&str; 9] = [
    "&amp;",
    "&lt;",
    "&gt;",
    "&quot;",
    "&#39;",
    "&#65;",
    "&#x41;",
    "&bogus;",
    "&#99999999999;",
];
/// A marker that only ever appears INSIDE script/style-like content; it must never be shown.
const HIDDEN: &[u8] = b"NEVERSHOWN";

fn gen_document(g: &mut Gen) -> Vec<u8> {
    let mut d: Vec<u8> = Vec::new();
    let target = match g.range(0, 9) {
        0 => g.range(0, 16),
        1..=7 => g.range(16, 4096),
        _ => g.range(MAX_INPUT - 64, MAX_INPUT + 512),
    };
    let mut depth: Vec<&str> = Vec::new();
    while d.len() < target {
        match g.range(0, 15) {
            0..=3 => {
                let t = *g.pick(&TAGS);
                d.extend_from_slice(b"<");
                d.extend_from_slice(t.as_bytes());
                if g.chance(3) {
                    d.extend_from_slice(b" href=\"");
                    match g.range(0, 3) {
                        0 => d.extend_from_slice(b"/rel/path"),
                        1 => d.extend_from_slice(b"https://other.example:8443/x"),
                        2 => d.extend_from_slice(b"http://plain.example/"),
                        _ => d.extend(g.bytes_range(0, HREF_CAP + 40)),
                    }
                    d.extend_from_slice(b"\"");
                }
                if g.chance(4) {
                    d.extend_from_slice(b" onclick=\"alert(1)\" data-x='");
                    d.extend(g.bytes_range(0, 200));
                    d.extend_from_slice(b"'");
                }
                if g.chance(9) {
                    // An unterminated tag: the rest of the document is markup.
                    d.extend(g.bytes_range(0, 64));
                    break;
                }
                d.extend_from_slice(b">");
                if matches!(
                    t,
                    "script" | "style" | "template" | "iframe" | "object" | "embed"
                ) {
                    d.extend_from_slice(HIDDEN);
                    d.extend(g.bytes_range(0, 300).into_iter().filter(|&b| b != b'<'));
                    if !g.chance(5) {
                        d.extend_from_slice(b"</");
                        d.extend_from_slice(t.as_bytes());
                        d.extend_from_slice(b">");
                    }
                } else {
                    depth.push(t);
                }
            }
            4 => {
                if let Some(t) = depth.pop() {
                    d.extend_from_slice(b"</");
                    d.extend_from_slice(t.as_bytes());
                    d.extend_from_slice(b">");
                }
            }
            5..=8 => {
                let n = g.range(1, 40);
                for _ in 0..n {
                    let b = g.range(0x20, 0x7e) as u8;
                    d.push(if b == b'<' || b == b'&' { b'x' } else { b });
                }
            }
            9 => d.extend_from_slice(g.pick(&ENTITIES).as_bytes()),
            10 => d.extend_from_slice(b"<!-- comment with <tags> and & -->"),
            11 => d.extend_from_slice(b"<!DOCTYPE html>"),
            // Raw bytes 0..255, controls included. A raw `<` is written `<1`, which the HTML
            // tokenizer reads as a literal `<`: a bare one could start a tag (`<v<template>` is
            // ONE tag named `v<template`) and hide the next opener from a real browser too.
            12 => {
                for b in g.bytes_range(1, 24) {
                    d.push(b);
                    if b == b'<' {
                        d.push(b'1');
                    }
                }
            }
            13 => d.extend_from_slice(b"\x1b[31m\x07\x00\r\n\t"),
            14 => {
                for _ in 0..g.range(1, 60) {
                    d.extend_from_slice(b"<div>");
                }
            }
            _ => d.extend_from_slice(b" \n \t  "),
        }
    }
    d
}

fn check_document(doc: &[u8]) {
    let mut g = Gen(doc.len() as u64 ^ 0x5eed);
    let width = g.range(1, 120);
    let rows = g.range(1, 80);
    let cap = g.range(0, 4096);
    // The output buffer is followed by guard bytes that must never change.
    let mut buf = vec![0xA5u8; cap + 64];
    let (out, guard) = buf.split_at_mut(cap);
    let rendered = render(doc, out, width, rows);
    assert!(
        guard.iter().all(|&b| b == 0xA5),
        "the renderer wrote past its buffer"
    );
    assert!(rendered.text.len() <= cap, "text longer than the buffer");
    for &b in rendered.text {
        assert!(
            b == b'\n' || (0x20..0x7f).contains(&b),
            "a non-printable byte {b:#04x} reached the page"
        );
    }
    assert!(
        !rendered.text.windows(HIDDEN.len()).any(|w| w == HIDDEN),
        "script/style content reached the page"
    );
    assert!(rendered.links() <= MAX_LINKS, "more links than the bound");
    for n in 1..=rendered.links() {
        let href = rendered.link(n).expect("a numbered link has an href");
        assert!(href.len() <= HREF_CAP, "an href longer than its cap");
    }
    assert!(rendered.link(0).is_none() && rendered.link(rendered.links() + 1).is_none());
    assert!(
        rendered.title().len() <= TITLE_CAP,
        "a title longer than its cap"
    );
    if doc.len() > MAX_INPUT {
        assert!(
            rendered.cut,
            "a document past the input bound was not said to be cut"
        );
    }
    let lines = rendered.text.split(|&b| b == b'\n').count();
    assert!(lines <= rows + 1, "more lines than rows: {lines} > {rows}");
    if width >= 8 {
        for line in rendered.text.split(|&b| b == b'\n') {
            assert!(line.len() <= width, "a line wider than the grid");
        }
    }
    // Determinism: the same document renders to the same bytes, links and title.
    let mut again = vec![0u8; cap];
    let second = render(doc, &mut again, width, rows);
    assert_eq!(rendered.text, second.text, "rendering is not deterministic");
    assert_eq!(rendered.links(), second.links());
    assert_eq!(rendered.title(), second.title());
    assert_eq!(rendered.cut, second.cut);
    assert_eq!(rendered.dropped, second.dropped);
}

// ---------------------------------------------------------------------------------------------
// The HTTP reader.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct HttpCase {
    raw: Vec<u8>,
    /// The body the peer MEANT to send when the head is well formed, else `None`.
    honest_body: Option<Vec<u8>>,
    raw_truncated: bool,
}

fn gen_http(g: &mut Gen) -> HttpCase {
    let body_len = match g.range(0, 6) {
        0 => 0,
        1..=4 => g.range(1, 600),
        _ => g.range(2000, 5000),
    };
    let body = g.bytes(body_len);
    let mut raw = Vec::new();
    let status = *g.pick(&[200u16, 204, 301, 404, 500, 999, 0, 7]);
    let version = *g.pick(&[
        "HTTP/1.1", "HTTP/1.0", "HTTP/2", "HTTP/1.1", "http/1.1", "HTTP/1.x",
    ]);
    raw.extend_from_slice(format!("{version} {status} Reason Words\r\n").as_bytes());
    let chunked = g.chance(3);
    let honest =
        version.starts_with("HTTP/1.") && version != "HTTP/1.x" && (100..=999).contains(&status);
    let mut lie = false;
    let extra_headers = g.range(0, 6);
    for i in 0..extra_headers {
        match g.range(0, 6) {
            0 => raw.extend_from_slice(format!("X-H{i}: value {i}\r\n").as_bytes()),
            1 => {
                raw.extend_from_slice(b"X-Long: ");
                raw.extend(std::iter::repeat_n(b'a', g.range(1, 1200)));
                raw.extend_from_slice(b"\r\n");
            }
            2 => {
                raw.extend_from_slice(b" folded continuation\r\n");
                lie = true;
            }
            3 => {
                raw.extend_from_slice(b"NoColonHere\r\n");
                lie = true;
            }
            4 => {
                raw.extend_from_slice(b"Set-Cookie: id=1; Path=/\r\n");
            }
            5 => {
                for k in 0..MAX_HEADERS + 2 {
                    raw.extend_from_slice(format!("X-Many-{k}: v\r\n").as_bytes());
                }
                lie = true;
            }
            _ => raw.extend_from_slice(b"Content-Type: text/html\r\n"),
        }
    }
    if chunked {
        if g.chance(4) {
            raw.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            lie = true; // two framings at once: ambiguous, refused
        }
        raw.extend_from_slice(b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let mut at = 0;
        while at < body.len() {
            let n = g.range(1, (body.len() - at).min(700));
            if g.chance(20) {
                raw.extend_from_slice(b"FFFFFFFFFF\r\n"); // a chunk size past the digit bound
                lie = true;
            }
            raw.extend_from_slice(format!("{n:x}\r\n").as_bytes());
            raw.extend_from_slice(&body[at..at + n]);
            raw.extend_from_slice(b"\r\n");
            at += n;
        }
        raw.extend_from_slice(b"0\r\n\r\n");
    } else {
        let declared = if g.chance(5) {
            lie = true;
            g.range(0, 10_000)
        } else {
            body.len()
        };
        raw.extend_from_slice(
            format!("Content-Length: {declared}\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        raw.extend_from_slice(&body);
    }
    // Mutations: flip bytes, truncate, or prepend garbage.
    let mut raw_truncated = false;
    match g.range(0, 9) {
        0 => {
            let n = g.range(1, 8);
            for _ in 0..n {
                if !raw.is_empty() {
                    let i = g.range(0, raw.len() - 1);
                    raw[i] = g.next() as u8;
                }
            }
            lie = true;
        }
        1 => {
            raw.truncate(g.range(0, raw.len()));
            lie = true;
        }
        2 => {
            raw_truncated = true;
            lie = true;
        }
        3 => {
            let mut garbage = g.bytes_range(1, 40);
            garbage.extend_from_slice(&raw);
            raw = garbage;
            lie = true;
        }
        _ => {}
    }
    HttpCase {
        raw,
        honest_body: if honest && !lie { Some(body) } else { None },
        raw_truncated,
    }
}

fn check_http(case: &HttpCase) {
    let mut g = Gen(case.raw.len() as u64 ^ 0x7770);
    let cap = g.range(0, 6000);
    let mut buf = vec![0xC3u8; cap + 64];
    let (body, guard) = buf.split_at_mut(cap);
    let result = parse(&case.raw, body, case.raw_truncated);
    assert!(
        guard.iter().all(|&b| b == 0xC3),
        "the reader wrote past the body buffer"
    );
    match result {
        Ok(r) => {
            assert!(r.body_len <= cap, "body_len past the buffer");
            assert!(r.headers().len() <= MAX_HEADERS);
            for (n, v) in r.headers() {
                // Every span lies inside the raw bytes.
                let _ = n.of(&case.raw);
                let _ = v.of(&case.raw);
            }
            let _ = r.reason.of(&case.raw);
            if let Some(honest) = &case.honest_body {
                let shown = honest.len().min(cap);
                assert_eq!(
                    &body[..r.body_len],
                    &honest[..shown],
                    "an honest body was misread"
                );
                assert_eq!(
                    r.truncated,
                    honest.len() > cap,
                    "truncation not said exactly when it happened"
                );
            }
        }
        Err(e) => {
            // Any refusal is a legitimate answer to a lie; an honest, complete answer must parse.
            if let Some(honest) = &case.honest_body {
                assert!(
                    e == HttpRefusal::HeaderTooLong,
                    "an honest response of {} body bytes was refused: {e:?}",
                    honest.len()
                );
            }
        }
    }
    // Determinism.
    let mut again = vec![0u8; cap];
    let second = parse(&case.raw, &mut again, case.raw_truncated);
    assert_eq!(result.is_ok(), second.is_ok());
    if let (Ok(a), Ok(b)) = (result, second) {
        assert_eq!(a.status, b.status);
        assert_eq!(a.body_len, b.body_len);
        assert_eq!(a.truncated, b.truncated);
    }
}

// ---------------------------------------------------------------------------------------------
// URLs, requests and the navigator.
// ---------------------------------------------------------------------------------------------

fn gen_url(g: &mut Gen) -> Vec<u8> {
    match g.range(0, 5) {
        0 => g.bytes_range(0, MAX_URL + 16),
        1 => {
            let mut u = b"https://".to_vec();
            u.extend((0..g.range(0, MAX_HOST + 8)).map(|_| *g.pick(b"abcz019.-:/ \x00%\xff")));
            u
        }
        2 => {
            let mut u = b"http://".to_vec();
            u.extend_from_slice(b"aletheia.test/");
            u.extend(g.bytes_range(0, 30));
            u
        }
        3 => format!(
            "https://aletheia.test:{}/{}",
            g.range(0, 70_000),
            "p".repeat(g.range(0, 600))
        )
        .into_bytes(),
        4 => b"https://aletheia.test\r\nHost: evil\r\n/x".to_vec(),
        _ => format!("https://{}", "a".repeat(g.range(0, MAX_HOST + 4))).into_bytes(),
    }
}

fn check_url(text: &[u8]) {
    let parsed = parse_url(text);
    if let Ok(url) = parsed {
        assert!(url.host().len() <= MAX_HOST && !url.host().is_empty());
        assert!(url
            .host()
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'));
        assert!(url.port >= 1);
        assert!(path_is_sendable(url.path()));
        // Round trip: what the URL writes back parses to the same URL.
        let mut grid = TextGrid::new(MAX_URL as u32 + 8, 1);
        url.write_to(&mut grid);
        let line = grid.line(0);
        let end = line
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map_or(0, |i| i + 1);
        assert_eq!(
            parse_url(&line[..end]),
            Ok(url),
            "a URL did not round-trip through its text"
        );
        // A request for it can never be split: no CR or LF anywhere in what goes on the wire.
        let mut req = [0u8; 1024];
        if let Ok(n) = request(url.host(), url.path(), &mut req) {
            let lines = req[..n].split(|&b| b == b'\n').count();
            // Request line, four headers, the empty line: six line ends, seven pieces.
            assert_eq!(lines, 7, "a request with other than four headers");
            assert!(!url.host().iter().any(|&b| b == b'\r' || b == b'\n'));
        }
    }
    // Determinism.
    assert_eq!(parsed, parse_url(text));
}

fn check_request(input: &(Vec<u8>, Vec<u8>)) {
    let (host, path) = input;
    let mut out = [0u8; 2048];
    let mut guard = [0u8; 16];
    let r = request(host, path, &mut out);
    assert!(guard.iter_mut().all(|b| {
        let ok = *b == 0;
        *b = 0;
        ok
    }));
    if let Ok(n) = r {
        let req = &out[..n];
        assert!(req.starts_with(b"GET /"));
        assert!(req.ends_with(b"\r\n\r\n"));
        // The only CR/LF are the line ends this client wrote: nothing from the inputs.
        let crlf = req.windows(2).filter(|w| *w == b"\r\n").count();
        assert_eq!(crlf, 6, "an input smuggled a line end onto the wire");
        assert!(host.iter().all(|&b| (0x21..0x7f).contains(&b)));
        assert!(path.iter().all(|&b| (0x21..0x7f).contains(&b)));
    } else {
        assert!(
            host.is_empty()
                || host.len() > 255
                || host.iter().any(|&b| !(0x21..0x7f).contains(&b))
                || !path_is_sendable(path)
                || host.len() + path.len() > 1900,
            "a sendable host and path were refused: {r:?}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Trust(u8),
    Block(u8),
    Go(u8, u8),
    Back,
    Forward,
    Forget,
    Page(u8),
}

fn gen_ops(g: &mut Gen) -> Vec<Op> {
    (0..g.range(1, 200))
        .map(|_| match g.range(0, 9) {
            0 => Op::Trust(g.range(0, 11) as u8),
            1 => Op::Block(g.range(0, 11) as u8),
            2..=4 => Op::Go(g.range(0, 11) as u8, g.range(0, 5) as u8),
            5 => Op::Back,
            6 => Op::Forward,
            7 => Op::Forget,
            _ => Op::Page(g.range(0, 3) as u8),
        })
        .collect()
}

fn host_name(i: u8) -> Vec<u8> {
    format!("host{i}.test").into_bytes()
}

fn check_ops(ops: &[Op]) {
    let mut nav = Navigator::new();
    let mut pinned: Vec<u8> = Vec::new();
    let mut blocked: Vec<u8> = Vec::new();
    for op in ops {
        match *op {
            Op::Trust(h) => {
                if nav
                    .hosts
                    .trust(&host_name(h), [10, 0, 2, h], [h; 32])
                    .is_ok()
                    && !pinned.contains(&h)
                {
                    pinned.push(h);
                }
            }
            Op::Block(h) => {
                if nav.blocked.block(&host_name(h)).is_ok() && !blocked.contains(&h) {
                    blocked.push(h);
                }
            }
            Op::Go(h, p) => {
                let scheme = if p == 4 { "http" } else { "https" };
                let url = format!("{scheme}://host{h}.test/p{p}");
                let before = nav.history_len();
                match nav.navigate(url.as_bytes()) {
                    Ok(r) => {
                        assert!(pinned.contains(&h), "an unpinned host resolved");
                        assert!(!blocked.contains(&h), "a blocked host resolved");
                        assert_eq!(r.pin, [h; 32], "resolved to a pin that is not the host's");
                        // A new navigation drops the forward pages (so history can shrink) and
                        // becomes the current page with nothing ahead of it.
                        let after = nav.history_len();
                        assert!(
                            after >= 1 && after <= (before + 1).min(HISTORY),
                            "{before} -> {after}"
                        );
                        assert_eq!(
                            nav.current().map(|u| u.host().to_vec()),
                            Some(r.url.host().to_vec())
                        );
                        assert!(
                            nav.forward().is_none(),
                            "a new navigation left a forward page"
                        );
                        nav.set_page(r.url, 200, b"OK", b"body", false);
                    }
                    Err(NavRefusal::Blocked) => assert!(blocked.contains(&h)),
                    Err(NavRefusal::UnknownHost) => {
                        assert!(!pinned.contains(&h) && !blocked.contains(&h))
                    }
                    Err(NavRefusal::Url(_)) => {
                        assert_eq!(p, 4, "an https URL was refused as a URL")
                    }
                }
            }
            Op::Back => {
                if let Some(url) = nav.back() {
                    match nav.resolve(&url) {
                        Ok(r) => assert!(
                            pinned.iter().any(|&h| host_name(h) == url.host()) && r.url == url
                        ),
                        Err(NavRefusal::Blocked) => {
                            assert!(blocked.iter().any(|&h| host_name(h) == url.host()))
                        }
                        Err(other) => {
                            panic!("back into a page that was fetched refused as {other:?}")
                        }
                    }
                }
            }
            Op::Forward => {
                let _ = nav.forward();
            }
            Op::Forget => {
                nav.forget();
                assert_eq!(nav.history_len(), 0);
                assert!(nav.page().is_none());
                assert_eq!(nav.link_count(), 0);
            }
            Op::Page(k) => {
                let doc: &[u8] = match k {
                    0 => b"<a href=\"/x\">x</a><a href=\"https://host1.test/\">y</a>",
                    1 => b"<a href=\"http://host2.test/\">z</a>",
                    _ => b"plain text, no links",
                };
                let mut shown = [0u8; 512];
                let rendered = render(doc, &mut shown, 30, 8);
                nav.set_links(&rendered);
                assert_eq!(nav.link_count(), rendered.links());
                for n in 1..=nav.link_count() {
                    let mut target = [0u8; MAX_URL];
                    if let Some(len) = nav.link_target(n, &mut target) {
                        // Following a link is a navigation like any other: the same refusals.
                        let _ = nav.navigate(&target[..len]).map(|r| {
                            let h = r.url.host().to_vec();
                            assert!(pinned.iter().any(|&p| host_name(p) == h));
                            assert!(!blocked.iter().any(|&p| host_name(p) == h));
                        });
                    }
                }
            }
        }
        assert!(nav.history_len() <= HISTORY, "history past its ring");
        assert!(nav.hosts.len() <= kernel_core::browser::MAX_HOSTS);
        assert!(nav.blocked.len() <= kernel_core::browser::MAX_BLOCKED);
    }
}

// ---------------------------------------------------------------------------------------------
// The campaign.
// ---------------------------------------------------------------------------------------------

#[test]
fn hostile_documents_render_bounded_printable_and_deterministic() {
    let (seed, count) = seed_and_count();
    campaign(
        "renderer",
        seed,
        count * 8,
        |g, _| gen_document(g),
        |d| check_document(d),
        |d| halves(d),
    );
}

#[test]
fn hostile_responses_are_read_bounded_or_refused_never_misread() {
    let (seed, count) = seed_and_count();
    campaign(
        "http",
        seed,
        count * 8,
        |g, _| gen_http(g),
        check_http,
        |c| {
            halves(&c.raw)
                .into_iter()
                .map(|raw| HttpCase {
                    raw,
                    honest_body: None,
                    raw_truncated: c.raw_truncated,
                })
                .collect()
        },
    );
}

#[test]
fn hostile_urls_parse_round_trip_and_never_split_a_request() {
    let (seed, count) = seed_and_count();
    campaign(
        "url",
        seed,
        count * 16,
        |g, _| gen_url(g),
        |u| check_url(u),
        |u| halves(u),
    );
    campaign(
        "request",
        seed,
        count * 8,
        |g, _| (g.bytes_range(0, 300), g.bytes_range(0, 700)),
        check_request,
        |(h, p)| {
            let mut v: Vec<(Vec<u8>, Vec<u8>)> =
                halves(h).into_iter().map(|h2| (h2, p.clone())).collect();
            v.extend(halves(p).into_iter().map(|p2| (h.clone(), p2)));
            v
        },
    );
}

#[test]
fn a_navigator_under_random_operations_never_resolves_what_the_operator_did_not_allow() {
    let (seed, count) = seed_and_count();
    campaign(
        "navigator",
        seed,
        count * 4,
        |g, _| gen_ops(g),
        |ops| check_ops(ops),
        |ops| {
            let mut v = Vec::new();
            if ops.len() > 1 {
                v.push(ops[..ops.len() / 2].to_vec());
                v.push(ops[ops.len() / 2..].to_vec());
                v.push(ops[..ops.len() - 1].to_vec());
            }
            v
        },
    );
}

/// Nightly seed 0xA1E7_0210_5EED case 839 (2026-09-23..25): a stray `<1u` with no `>` before a
/// real `<template>` was read as one tag running to the template's own `>`, so the template's
/// content rendered. A `<` that opens no markup is now a literal character.
#[test]
fn a_stray_angle_bracket_does_not_swallow_the_next_hidden_element() {
    for doc in [
        &b"a <1u junk <template href=\"x\">NEVERSHOWN</template> b"[..],
        b"a < <script>NEVERSHOWN</script> b",
        b"a << <style>NEVERSHOWN</style> b",
        b"a <1 <iframe>NEVERSHOWN</iframe> <",
    ] {
        let mut out = [0u8; 256];
        let r = render(doc, &mut out, 80, 10);
        assert!(
            !r.text.windows(HIDDEN.len()).any(|w| w == HIDDEN),
            "{:?}",
            r.text
        );
        assert!(r.text.starts_with(b"a <"), "{:?}", r.text);
        check_document(doc);
    }
}
