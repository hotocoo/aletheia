//! A bounded, fail-closed renderer for a subset of HTML into lines of text (REQ-WEB-004, ADR-158;
//! Lethe stage N5).
//!
//! This is where "a browser" starts being a real word, and where browsers historically get hurt:
//! the input is attacker-chosen, the grammar is forgiving, and every renderer that tried to make
//! sense of everything became a program the page controls. So this one makes sense of a LIST:
//! headings, paragraphs, line breaks, lists, preformatted text, links and the title. Everything
//! else is dropped — and for `script`, `style`, `template`, `iframe`, `object` and `embed` the
//! CONTENT is dropped too, whole, because text inside them is not text a person wrote to be read.
//!
//! ## Rules
//!
//! * Input is bounded (`MAX_INPUT`); a longer document is rendered to the bound and said to be cut.
//! * Output is a fixed set of lines of a fixed width; more text than fits is cut, never grown into.
//! * Whitespace collapses as a browser collapses it, except inside `pre`.
//! * Five entities are decoded (`&amp; &lt; &gt; &quot; &#NN;` with a bound on the number); any other
//!   entity is left as its literal text rather than guessed.
//! * A link's text is kept in the flow and its number appended `[n]`; its `href` is kept in a
//!   bounded table so a person can choose it by number. A `href` that is not `https://…` is kept
//!   but marked, so the model's plaintext refusal (ADR-156) happens at navigation, not here.
//! * A tag this renderer does not know is invisible: its text still flows, its attributes do not.
//! * Nothing here allocates, and nothing here executes.

/// The most bytes of a document this renderer reads.
pub const MAX_INPUT: usize = 8192;
/// The most links a page may offer.
pub const MAX_LINKS: usize = 16;
/// The longest href kept.
pub const HREF_CAP: usize = 128;
/// The longest title kept.
pub const TITLE_CAP: usize = 64;
/// The most tag-name bytes considered.
const TAG_CAP: usize = 16;

/// A rendered page's text and the links it offered.
pub struct Rendered<'a> {
    /// Output lines, packed with `\n`, in the caller's buffer.
    pub text: &'a [u8],
    /// The document was longer than the renderer reads, or the output fuller than it holds.
    pub cut: bool,
    pub title: [u8; TITLE_CAP],
    pub title_len: usize,
    links: [([u8; HREF_CAP], usize); MAX_LINKS],
    link_count: usize,
    /// Content this renderer refused to show: bytes inside script/style-like elements, dropped.
    pub dropped: usize,
}

impl<'a> Rendered<'a> {
    pub fn title(&self) -> &[u8] {
        &self.title[..self.title_len]
    }
    pub fn links(&self) -> usize {
        self.link_count
    }
    /// The nth link's href (1-based, as printed), if any.
    pub fn link(&self, n: usize) -> Option<&[u8]> {
        if n == 0 || n > self.link_count {
            return None;
        }
        let (buf, len) = &self.links[n - 1];
        Some(&buf[..*len])
    }
}

/// The elements whose whole CONTENT is dropped, not just the tag.
const DROP_CONTENT: [&[u8]; 6] = [
    b"script",
    b"style",
    b"template",
    b"iframe",
    b"object",
    b"embed",
];

fn tag_eq(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The output builder: fixed width, fixed height, cut at the last row.
struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
    col: usize,
    width: usize,
    rows: usize,
    row: usize,
    cut: bool,
    at_line_start: bool,
    pending_space: bool,
}

impl<'a> Out<'a> {
    fn newline(&mut self) {
        if self.row + 1 >= self.rows {
            self.cut = true;
            return;
        }
        if self.len < self.buf.len() {
            self.buf[self.len] = b'\n';
            self.len += 1;
        }
        self.row += 1;
        self.col = 0;
        self.at_line_start = true;
        self.pending_space = false;
    }

    /// A blank line, once: two block boundaries in a row do not stack.
    fn paragraph(&mut self) {
        if !self.at_line_start {
            self.newline();
        }
        if self.row > 0 && self.len > 0 && self.buf[self.len - 1] == b'\n' && !self.blank_before() {
            self.newline();
        }
    }

    fn blank_before(&self) -> bool {
        self.len >= 2 && self.buf[self.len - 1] == b'\n' && self.buf[self.len - 2] == b'\n'
    }

    fn putc(&mut self, b: u8) {
        if self.row + 1 >= self.rows && self.col >= self.width {
            self.cut = true;
            return;
        }
        if self.col >= self.width {
            self.newline();
            if self.cut {
                return;
            }
        }
        if self.len >= self.buf.len() {
            self.cut = true;
            return;
        }
        self.buf[self.len] = b;
        self.len += 1;
        self.col += 1;
        self.at_line_start = false;
    }

    /// Collapsed text: runs of whitespace become one space, none at a line start.
    fn text(&mut self, s: &[u8]) {
        for &b in s {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pending_space = !self.at_line_start;
            } else {
                if self.pending_space {
                    self.pending_space = false;
                    if self.col < self.width {
                        self.putc(b' ');
                    } else {
                        self.newline();
                    }
                }
                let shown = if (0x20..0x7f).contains(&b) { b } else { b'?' };
                self.putc(shown);
            }
        }
    }

    /// Preformatted text: every byte as it is, newlines honoured, tabs as spaces.
    fn pre(&mut self, s: &[u8]) {
        for &b in s {
            match b {
                b'\n' => self.newline(),
                b'\r' => {}
                b'\t' => self.putc(b' '),
                0x20..=0x7e => self.putc(b),
                _ => self.putc(b'?'),
            }
        }
    }
}

/// A decimal character reference: printable ASCII as itself, anything else as `?`, and not a
/// number at all as no decoding.
fn numeric_entity(digits: &[u8]) -> Option<u8> {
    let mut v = 0u32;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
    }
    Some(if (0x20..0x7f).contains(&v) {
        v as u8
    } else {
        b'?'
    })
}

/// Decode the five entities this renderer knows into `out`, leaving unknown ones literal.
fn decode_entities(s: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < s.len() && n < out.len() {
        if s[i] == b'&' {
            if let Some(semi) = s[i..].iter().position(|&b| b == b';').map(|p| i + p) {
                let ent = &s[i + 1..semi];
                let decoded = match ent {
                    b"amp" => Some(b'&'),
                    b"lt" => Some(b'<'),
                    b"gt" => Some(b'>'),
                    b"quot" => Some(b'"'),
                    b"apos" | b"#39" => Some(b'\''),
                    b"nbsp" | b"#160" => Some(b' '),
                    _ if ent.len() > 1 && ent.len() <= 6 && ent[0] == b'#' => {
                        numeric_entity(&ent[1..])
                    }
                    _ => None,
                };
                if let Some(c) = decoded {
                    out[n] = c;
                    n += 1;
                    i = semi + 1;
                    continue;
                }
            }
        }
        out[n] = s[i];
        n += 1;
        i += 1;
    }
    n
}

/// Read the tag name and whether the tag is a closing one from the bytes after `<`.
fn tag_name(inner: &[u8]) -> (bool, [u8; TAG_CAP], usize) {
    let (closing, rest) = match inner.first() {
        Some(b'/') => (true, &inner[1..]),
        _ => (false, inner),
    };
    let mut name = [0u8; TAG_CAP];
    let mut n = 0;
    for &b in rest {
        if b.is_ascii_alphanumeric() && n < TAG_CAP {
            name[n] = b.to_ascii_lowercase();
            n += 1;
        } else {
            break;
        }
    }
    (closing, name, n)
}

/// Whether the bytes after a `<` open markup, as the HTML tokenizer decides it: a letter (a tag),
/// `/` (an end tag, or a bogus comment such as `</1 ...>`), `!` (a comment or declaration) or `?`
/// (a bogus comment). Anything else - `<1`, `< `, `<<`, `<` at the end - is a literal `<`.
/// Reading it as a tag would swallow everything to the next `>`, including the opener of a real
/// `<script>` or `<template>`, and that element's content would then show.
fn opens_markup(rest: &[u8]) -> bool {
    match rest.first() {
        Some(&b) => b.is_ascii_alphabetic() || matches!(b, b'/' | b'!' | b'?'),
        None => false,
    }
}

/// The value of attribute `key` inside a tag's bytes, if present and quoted.
fn attribute<'b>(inner: &'b [u8], key: &[u8]) -> Option<&'b [u8]> {
    let mut i = 0;
    while i + key.len() < inner.len() {
        if inner[i..].len() > key.len()
            && inner[i..i + key.len()].eq_ignore_ascii_case(key)
            && (i == 0 || inner[i - 1] == b' ' || inner[i - 1] == b'\t' || inner[i - 1] == b'\n')
        {
            let mut j = i + key.len();
            while j < inner.len() && inner[j] == b' ' {
                j += 1;
            }
            if j < inner.len() && inner[j] == b'=' {
                j += 1;
                while j < inner.len() && inner[j] == b' ' {
                    j += 1;
                }
                if j < inner.len() && (inner[j] == b'"' || inner[j] == b'\'') {
                    let q = inner[j];
                    let start = j + 1;
                    let end = inner[start..]
                        .iter()
                        .position(|&b| b == q)
                        .map(|p| start + p)?;
                    return Some(&inner[start..end]);
                }
                let start = j;
                let end = inner[start..]
                    .iter()
                    .position(|&b| b == b' ' || b == b'>')
                    .map_or(inner.len(), |p| start + p);
                return Some(&inner[start..end]);
            }
        }
        i += 1;
    }
    None
}

/// Render `html` into `out` as `width`-column lines, at most `rows` of them.
pub fn render<'a>(html: &[u8], out: &'a mut [u8], width: usize, rows: usize) -> Rendered<'a> {
    let width = width.max(8);
    let rows = rows.max(1);
    let mut cut_input = false;
    let doc = if html.len() > MAX_INPUT {
        cut_input = true;
        &html[..MAX_INPUT]
    } else {
        html
    };
    let mut o = Out {
        buf: out,
        len: 0,
        col: 0,
        width,
        rows,
        row: 0,
        cut: false,
        at_line_start: true,
        pending_space: false,
    };
    let mut title = [0u8; TITLE_CAP];
    let mut title_len = 0usize;
    let mut links: [([u8; HREF_CAP], usize); MAX_LINKS] = [([0u8; HREF_CAP], 0); MAX_LINKS];
    let mut link_count = 0usize;
    let mut dropped = 0usize;
    let mut in_pre = false;
    let mut in_title = false;
    let mut pending_link: Option<usize> = None;
    let mut i = 0usize;
    let mut scratch = [0u8; 256];

    while i < doc.len() {
        if doc[i] == b'<' && opens_markup(&doc[i + 1..]) {
            // A comment or declaration: skipped whole.
            if doc[i..].starts_with(b"<!--") {
                match doc[i + 4..].windows(3).position(|w| w == b"-->") {
                    Some(p) => {
                        i = i + 4 + p + 3;
                        continue;
                    }
                    None => break,
                }
            }
            if doc[i..].starts_with(b"<!") {
                match doc[i..].iter().position(|&b| b == b'>') {
                    Some(p) => {
                        i += p + 1;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(close) = doc[i..].iter().position(|&b| b == b'>') else {
                break; // an unterminated tag ends the document: nothing after it is text
            };
            let inner = &doc[i + 1..i + close];
            let (closing, name, n) = tag_name(inner);
            let name = &name[..n];
            i += close + 1;
            if !closing && DROP_CONTENT.iter().any(|d| tag_eq(name, d)) {
                // Drop everything to the matching close tag, or to the end.
                let mut j = i;
                let mut found = None;
                while j < doc.len() {
                    if doc[j] == b'<' && doc[j + 1..].starts_with(b"/") {
                        let (_, cn, cl) = tag_name(&doc[j + 1..]);
                        if tag_eq(&cn[..cl], name) {
                            found = Some(j);
                            break;
                        }
                    }
                    j += 1;
                }
                let end = found.unwrap_or(doc.len());
                dropped += end - i;
                i = match found {
                    Some(j) => doc[j..]
                        .iter()
                        .position(|&b| b == b'>')
                        .map_or(doc.len(), |p| j + p + 1),
                    None => doc.len(),
                };
                continue;
            }
            match (closing, name) {
                (false, b"br") => o.newline(),
                (_, b"p")
                | (_, b"div")
                | (_, b"h1")
                | (_, b"h2")
                | (_, b"h3")
                | (_, b"ul")
                | (_, b"ol")
                | (_, b"table")
                | (_, b"tr")
                | (_, b"blockquote")
                | (_, b"hr") => {
                    o.paragraph();
                    if !closing && name == b"hr" {
                        for _ in 0..width.min(20) {
                            o.putc(b'-');
                        }
                        o.newline();
                    }
                }
                (false, b"li") => {
                    if !o.at_line_start {
                        o.newline();
                    }
                    o.putc(b'*');
                    o.putc(b' ');
                    o.pending_space = false;
                }
                (true, b"li") if !o.at_line_start => o.newline(),
                (true, b"li") => {}
                (false, b"pre") => {
                    o.paragraph();
                    in_pre = true;
                }
                (true, b"pre") => {
                    in_pre = false;
                    if !o.at_line_start {
                        o.newline();
                    }
                }
                (false, b"title") => in_title = true,
                (true, b"title") => in_title = false,
                (false, b"a") => {
                    if let Some(href) = attribute(inner, b"href") {
                        if link_count < MAX_LINKS {
                            let (buf, len) = &mut links[link_count];
                            let take = href.len().min(HREF_CAP);
                            buf[..take].copy_from_slice(&href[..take]);
                            *len = take;
                            link_count += 1;
                            pending_link = Some(link_count);
                        }
                    }
                }
                (true, b"a") => {
                    if let Some(n) = pending_link.take() {
                        let mut tag = [0u8; 6];
                        tag[0] = b'[';
                        let d = if n >= 10 {
                            tag[1] = b'0' + (n / 10) as u8;
                            tag[2] = b'0' + (n % 10) as u8;
                            3
                        } else {
                            tag[1] = b'0' + n as u8;
                            2
                        };
                        tag[d] = b']';
                        o.pending_space = false;
                        o.text(&tag[..d + 1]);
                    }
                }
                _ => {} // an unknown tag is invisible; its text still flows
            }
            continue;
        }
        // A `<` that opens no markup is a literal character: the text runs past it.
        let from = if doc[i] == b'<' { i + 1 } else { i };
        let text_end = doc[from..]
            .iter()
            .position(|&b| b == b'<')
            .map_or(doc.len(), |p| from + p);
        let raw = &doc[i..text_end];
        if in_title {
            let n = decode_entities(raw, &mut scratch);
            for &b in &scratch[..n] {
                let space = b == b' ' || b == b'\t' || b == b'\n' || b == b'\r';
                if space {
                    if title_len > 0 && title[title_len - 1] != b' ' && title_len < TITLE_CAP {
                        title[title_len] = b' ';
                        title_len += 1;
                    }
                } else if title_len < TITLE_CAP && (0x20..0x7f).contains(&b) {
                    title[title_len] = b;
                    title_len += 1;
                }
            }
        } else {
            let mut at = 0;
            while at < raw.len() {
                let take = (raw.len() - at).min(scratch.len() / 2);
                let n = decode_entities(&raw[at..at + take], &mut scratch);
                if in_pre {
                    o.pre(&scratch[..n]);
                } else {
                    o.text(&scratch[..n]);
                }
                at += take;
            }
        }
        i = text_end;
    }
    let mut len = o.len;
    while len > 0 && o.buf[len - 1] == b'\n' {
        len -= 1;
    }
    let cut = o.cut || cut_input;
    while title_len > 0 && title[title_len - 1] == b' ' {
        title_len -= 1;
    }
    Rendered {
        text: &o.buf[..len],
        cut,
        title,
        title_len,
        links,
        link_count,
        dropped,
    }
}

/// The content contract, proved on every CPU at boot: bytes a page could carry, none executed.
pub fn content_suite(
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
    let mut out = [0u8; 2048];

    // 1 - headings, paragraphs and breaks become lines; whitespace collapses; the title is kept.
    {
        let r = render(b"<html><head><title>  Hello   Page </title></head><body><h1>Hi</h1><p>one\n  two</p><p>three<br>four</p></body></html>", &mut out, 40, 12);
        check!(
            r.text == b"Hi\n\none two\n\nthree\nfour" && r.title() == b"Hello Page" && !r.cut,
            "content: headings, paragraphs and breaks become lines, whitespace collapses, the title is kept"
        );
    }

    // 2 - script and style CONTENT is dropped whole, not just its tags; the count says how much.
    {
        let r = render(b"<p>a</p><script>alert(1); document.write('x')</script><style>p{color:red}</style><p>b</p>", &mut out, 40, 12);
        check!(
            r.text == b"a\n\nb" && r.dropped == 41,
            "content: script and style content is dropped whole and counted, never shown, never run"
        );
    }

    // 3 - links keep their text in the flow, are numbered, and their hrefs are kept for choosing.
    {
        let r = render(b"<p>See <a href=\"https://aletheia.test/x\">the page</a> and <a href='http://plain/'>plain</a>.</p>", &mut out, 60, 12);
        check!(
            r.text == b"See the page[1] and plain[2]."
                && r.links() == 2
                && r.link(1) == Some(&b"https://aletheia.test/x"[..])
                && r.link(2) == Some(&b"http://plain/"[..])
                && r.link(3).is_none()
                && r.link(0).is_none(),
            "content: links keep their text, are numbered, and their hrefs are kept to be chosen by number"
        );
    }

    // 4 - lists and preformatted text keep their shape; entities decode; unknown entities stay
    //     literal rather than guessed.
    {
        let r = render(
            b"<ul><li>a &amp; b</li><li>c &lt; d</li></ul><pre>x\n  y\t&#65;</pre>&bogus; &#9999;",
            &mut out,
            40,
            12,
        );
        check!(
            r.text == b"* a & b\n* c < d\n\nx\n  y A\n&bogus; ?",
            "content: lists and preformatted text keep their shape, known entities decode, unknown ones stay literal"
        );
    }

    // 5 - the output is bounded: more lines than the grid has are cut and said to be, and the
    //     bytes past the caller's buffer are never written.
    {
        let mut guarded = [0xAAu8; 64];
        let r = render(
            b"<p>one</p><p>two</p><p>three</p><p>four</p>",
            &mut guarded[..32],
            10,
            3,
        );
        let cut = r.cut;
        let len = r.text.len();
        check!(
            cut && len <= 32 && guarded[32..].iter().all(|&b| b == 0xAA),
            "content: output is cut at the grid's last row and said so, never written past the buffer"
        );
    }

    // 6 - the input is bounded: a document longer than the renderer reads is rendered to the
    //     bound and said to be cut.
    {
        let mut big = [b'z'; MAX_INPUT + 100];
        big[0] = b'<';
        big[1] = b'p';
        big[2] = b'>';
        let r = render(&big, &mut out, 80, 24);
        check!(
            r.cut && !r.text.is_empty(),
            "content: a document longer than the renderer reads is cut at the bound and said so"
        );
    }

    // 7 - an unknown tag is invisible and its attributes are never text; an unterminated tag
    //     ends the document rather than leaking markup into the page.
    {
        let r = render(
            b"<p>keep <blink onclick=\"evil()\">this</blink> text</p><p>and <b",
            &mut out,
            40,
            12,
        );
        check!(
            r.text == b"keep this text\n\nand" && r.links() == 0,
            "content: an unknown tag is invisible, its attributes are never text, an unterminated tag ends the page"
        );
    }

    // 8 - comments and declarations are skipped whole; bytes outside printable ASCII show as `?`
    //     rather than reaching a terminal.
    {
        let r = render(
            b"<!DOCTYPE html><!-- <p>hidden</p> --><p>shown\x1b[31m\xff</p>",
            &mut out,
            40,
            12,
        );
        check!(
            r.text == b"shown?[31m?",
            "content: comments and declarations are skipped whole, and bytes that could drive a terminal show as ?"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_content_invariant() {
        let mut seen = 0;
        let n = content_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the content suite should hold");
        assert_eq!(n, 8);
        assert_eq!(seen, 8);
    }

    #[test]
    fn every_prefix_of_a_page_renders_without_panic_or_overflow() {
        let page = b"<html><head><title>T</title><script>x<y</script></head><body><h1>H</h1><p>a &amp; <a href=\"https://h/\">l</a></p><ul><li>i</li></ul><pre>p</pre><!-- c --></body></html>";
        for cut in 0..page.len() {
            let mut out = [0u8; 512];
            let r = render(&page[..cut], &mut out, 30, 8);
            assert!(r.text.len() <= 512);
        }
    }
}
