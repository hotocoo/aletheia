//! The browser's navigation model: URLs, the hosts a person has chosen to trust, history, and the
//! page a window shows (REQ-WEB-002, ADR-156; Lethe stage N4).
//!
//! Nothing here touches a wire. A URL is parsed, refused or resolved against the host table the
//! operator filled; the PLATFORM performs the fetch (ADR-151's conversation, ADR-155's request) and
//! hands the answer back as a page. That split is what keeps navigation provable without a network
//! and keeps every trust decision where Lethe puts it: with the person, per host, before a byte.
//!
//! ## Refused, by name
//!
//! `http://` is refused as plaintext — never downgraded to, never upgraded from (Lethe's
//! HTTPS-first rule, stage N6, adopted here because the model is where it belongs). A host nobody
//! pinned is refused before any address is dialed. A URL with a bad host, port or path is refused
//! for the part that is bad. The host table is bounded; the ninth host is refused, not evicted.

use crate::http::{path_is_sendable, MAX_PATH};
use crate::textgrid::TextGrid;

/// The most hosts an operator can pin.
pub const MAX_HOSTS: usize = 8;
/// The longest URL the model will read.
pub const MAX_URL: usize = 256;
/// The longest host name.
pub const MAX_HOST: usize = 64;
/// How many pages the history remembers.
pub const HISTORY: usize = 8;
/// How much of a page body a page keeps: what a window can show, and no more.
pub const BODY_CAP: usize = 2048;

/// Why a URL was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRefusal {
    /// The scheme is `http://`: plaintext, refused rather than downgraded to.
    Plaintext,
    /// The scheme is neither `https://` nor `http://`.
    NotHttps,
    /// No host between the scheme and the path.
    NoHost,
    /// A host with a byte outside `a-z`, `0-9`, `.` and `-`, or too long.
    BadHost,
    /// A port that is not `1..=65535`.
    BadPort,
    /// A path the request builder would refuse.
    BadPath,
    /// Longer than this model reads.
    TooLong,
}

/// Why navigation did not begin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavRefusal {
    Url(UrlRefusal),
    /// No pinned root for this host: nothing is dialed.
    UnknownHost,
}

/// Why a host was not pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustRefusal {
    BadName,
    /// The table holds its maximum; nothing is evicted to make room.
    Full,
}

/// A parsed `https://host[:port]/path`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Url {
    host: [u8; MAX_HOST],
    host_len: usize,
    pub port: u16,
    path: [u8; MAX_PATH],
    path_len: usize,
}

impl Url {
    pub fn host(&self) -> &[u8] {
        &self.host[..self.host_len]
    }
    pub fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }

    /// Write the URL back as text.
    pub fn write_to(&self, out: &mut TextGrid) {
        out.write(b"https://");
        out.write(self.host());
        if self.port != 443 {
            out.write(b":");
            write_decimal(out, self.port as usize);
        }
        out.write(self.path());
    }
}

fn write_decimal(out: &mut TextGrid, mut v: usize) {
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out.put(digits[n]);
    }
}

fn host_byte_ok(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'
}

/// Read a URL. Only `https://` is a URL this browser will navigate to.
pub fn parse_url(text: &[u8]) -> Result<Url, UrlRefusal> {
    if text.len() > MAX_URL {
        return Err(UrlRefusal::TooLong);
    }
    let rest = if let Some(r) = text.strip_prefix(b"https://") {
        r
    } else if text.starts_with(b"http://") {
        return Err(UrlRefusal::Plaintext);
    } else {
        return Err(UrlRefusal::NotHttps);
    };
    let authority_end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
    let (authority, path_part) = rest.split_at(authority_end);
    if authority.is_empty() {
        return Err(UrlRefusal::NoHost);
    }
    let (host, port) = match authority.iter().position(|&b| b == b':') {
        Some(i) => {
            let digits = &authority[i + 1..];
            if digits.is_empty() || digits.len() > 5 || !digits.iter().all(|b| b.is_ascii_digit()) {
                return Err(UrlRefusal::BadPort);
            }
            let p = digits.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32);
            if p == 0 || p > 65535 {
                return Err(UrlRefusal::BadPort);
            }
            (&authority[..i], p as u16)
        }
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(UrlRefusal::NoHost);
    }
    if host.len() > MAX_HOST || !host.iter().all(|&b| host_byte_ok(b)) {
        return Err(UrlRefusal::BadHost);
    }
    let path_src: &[u8] = if path_part.is_empty() {
        b"/"
    } else {
        path_part
    };
    if !path_is_sendable(path_src) {
        return Err(UrlRefusal::BadPath);
    }
    let mut url = Url {
        host: [0; MAX_HOST],
        host_len: host.len(),
        port,
        path: [0; MAX_PATH],
        path_len: path_src.len(),
    };
    url.host[..host.len()].copy_from_slice(host);
    url.path[..path_src.len()].copy_from_slice(path_src);
    Ok(url)
}

/// One host a person has chosen to trust: its name, where it is, and the root that vouches for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostEntry {
    name: [u8; MAX_HOST],
    name_len: usize,
    pub ip: [u8; 4],
    pub pin: [u8; 32],
}

impl HostEntry {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// The hosts this browser will speak to. Bounded, operator-filled, nothing pre-installed.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostTable {
    entries: [Option<HostEntry>; MAX_HOSTS],
}

impl HostTable {
    /// Pin `name` at `ip` under `pin`. A name already pinned is REPLACED - the person changed their
    /// mind - and a ninth name is refused, not evicted.
    pub fn trust(&mut self, name: &[u8], ip: [u8; 4], pin: [u8; 32]) -> Result<(), TrustRefusal> {
        if name.is_empty() || name.len() > MAX_HOST || !name.iter().all(|&b| host_byte_ok(b)) {
            return Err(TrustRefusal::BadName);
        }
        let mut entry = HostEntry {
            name: [0; MAX_HOST],
            name_len: name.len(),
            ip,
            pin,
        };
        entry.name[..name.len()].copy_from_slice(name);
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|e| e.is_some_and(|e| e.name() == name))
        {
            *slot = Some(entry);
            return Ok(());
        }
        match self.entries.iter_mut().find(|e| e.is_none()) {
            Some(slot) => {
                *slot = Some(entry);
                Ok(())
            }
            None => Err(TrustRefusal::Full),
        }
    }

    pub fn lookup(&self, name: &[u8]) -> Option<&HostEntry> {
        self.entries.iter().flatten().find(|e| e.name() == name)
    }

    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the platform must dial: everything a fetch needs, resolved and pinned, before a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub url: Url,
    pub ip: [u8; 4],
    pub pin: [u8; 32],
}

/// The page a window shows: what was asked, what came back, or why nothing did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub url: Url,
    pub status: u16,
    reason: [u8; 32],
    reason_len: usize,
    body: [u8; BODY_CAP],
    body_len: usize,
    pub truncated: bool,
    /// Set when nothing came back: the named reason the fetch or the navigation refused.
    pub failure: Option<&'static str>,
}

impl Page {
    pub fn reason(&self) -> &[u8] {
        &self.reason[..self.reason_len]
    }
    pub fn body(&self) -> &[u8] {
        &self.body[..self.body_len]
    }
}

/// Where the browser is: its hosts, its history and the page it shows.
#[derive(Clone, Copy, Debug)]
pub struct Navigator {
    pub hosts: HostTable,
    /// The links the current page offered (ADR-158), numbered as the renderer printed them.
    links: [([u8; crate::content::HREF_CAP], usize); crate::content::MAX_LINKS],
    link_count: usize,
    history: [Option<Url>; HISTORY],
    /// Entries in use, oldest first; the newest is at `len - 1`.
    len: usize,
    /// The entry the current page came from.
    cursor: usize,
    page: Option<Page>,
}

impl Default for Navigator {
    fn default() -> Self {
        Self::new()
    }
}

impl Navigator {
    pub fn new() -> Self {
        Navigator {
            hosts: HostTable::default(),
            links: [([0u8; crate::content::HREF_CAP], 0); crate::content::MAX_LINKS],
            link_count: 0,
            history: [None; HISTORY],
            len: 0,
            cursor: 0,
            page: None,
        }
    }

    /// Begin navigating to `text`: parse, resolve against the host table, record in history, and
    /// hand back what the platform must dial. Nothing is dialed here.
    pub fn navigate(&mut self, text: &[u8]) -> Result<Resolved, NavRefusal> {
        let url = parse_url(text).map_err(NavRefusal::Url)?;
        let entry = self
            .hosts
            .lookup(url.host())
            .ok_or(NavRefusal::UnknownHost)?;
        let resolved = Resolved {
            url,
            ip: entry.ip,
            pin: entry.pin,
        };
        self.push(url);
        Ok(resolved)
    }

    fn push(&mut self, url: Url) {
        // A new navigation truncates the forward history, as every browser does.
        if self.len > 0 && self.cursor + 1 < self.len {
            self.len = self.cursor + 1;
        }
        if self.len == HISTORY {
            self.history.copy_within(1..HISTORY, 0);
            self.len -= 1;
        }
        self.history[self.len] = Some(url);
        self.cursor = self.len;
        self.len += 1;
    }

    /// The previous page's URL, if any; the platform re-fetches it.
    pub fn back(&mut self) -> Option<Url> {
        if self.cursor == 0 || self.len == 0 {
            return None;
        }
        self.cursor -= 1;
        self.history[self.cursor]
    }

    /// The next page's URL, if any.
    pub fn forward(&mut self) -> Option<Url> {
        if self.cursor + 1 >= self.len {
            return None;
        }
        self.cursor += 1;
        self.history[self.cursor]
    }

    pub fn history_len(&self) -> usize {
        self.len
    }

    pub fn current(&self) -> Option<Url> {
        if self.len == 0 {
            None
        } else {
            self.history[self.cursor]
        }
    }

    /// Resolve a URL already in history (after `back`/`forward`) without re-recording it.
    pub fn resolve(&self, url: &Url) -> Result<Resolved, NavRefusal> {
        let entry = self
            .hosts
            .lookup(url.host())
            .ok_or(NavRefusal::UnknownHost)?;
        Ok(Resolved {
            url: *url,
            ip: entry.ip,
            pin: entry.pin,
        })
    }

    /// The platform's answer becomes the page.
    pub fn set_page(&mut self, url: Url, status: u16, reason: &[u8], body: &[u8], truncated: bool) {
        let mut page = Page {
            url,
            status,
            reason: [0; 32],
            reason_len: reason.len().min(32),
            body: [0; BODY_CAP],
            body_len: body.len().min(BODY_CAP),
            truncated: truncated || body.len() > BODY_CAP,
            failure: None,
        };
        page.reason[..page.reason_len].copy_from_slice(&reason[..page.reason_len]);
        page.body[..page.body_len].copy_from_slice(&body[..page.body_len]);
        self.page = Some(page);
        self.link_count = 0;
    }

    /// Nothing came back; the page says why.
    pub fn set_failure(&mut self, url: Url, why: &'static str) {
        self.page = Some(Page {
            url,
            status: 0,
            reason: [0; 32],
            reason_len: 0,
            body: [0; BODY_CAP],
            body_len: 0,
            truncated: false,
            failure: Some(why),
        });
        self.link_count = 0;
    }

    pub fn page(&self) -> Option<&Page> {
        self.page.as_ref()
    }

    /// Keep the links a rendered page offered, in the renderer's numbering. Replaces the last
    /// page's links: a link belongs to the page that offered it.
    pub fn set_links(&mut self, rendered: &crate::content::Rendered<'_>) {
        self.link_count = 0;
        for n in 1..=rendered.links() {
            if let Some(href) = rendered.link(n) {
                let (buf, len) = &mut self.links[self.link_count];
                let take = href.len().min(crate::content::HREF_CAP);
                buf[..take].copy_from_slice(&href[..take]);
                *len = take;
                self.link_count += 1;
            }
        }
    }

    pub fn link_count(&self) -> usize {
        self.link_count
    }

    /// The URL link `n` (1-based) points at, made absolute against the current page when it is a
    /// path: `/x` on `https://h:8443/a` is `https://h:8443/x`. A link that is not https, or not a
    /// path, comes back as written and is refused by `navigate` for what it is.
    pub fn link_target(&self, n: usize, out: &mut [u8; MAX_URL]) -> Option<usize> {
        if n == 0 || n > self.link_count {
            return None;
        }
        let (buf, len) = &self.links[n - 1];
        let href = &buf[..*len];
        if href.first() == Some(&b'/') {
            let page = self.page.as_ref()?;
            let mut grid = TextGrid::new(MAX_URL as u32, 1);
            grid.write(b"https://");
            grid.write(page.url.host());
            if page.url.port != 443 {
                grid.write(b":");
                write_decimal(&mut grid, page.url.port as usize);
            }
            grid.write(href);
            let line = grid.line(0);
            let end = line
                .iter()
                .rposition(|&b| b != b' ' && b != 0)
                .map_or(0, |i| i + 1);
            out[..end].copy_from_slice(&line[..end]);
            return Some(end);
        }
        let take = href.len().min(MAX_URL);
        out[..take].copy_from_slice(&href[..take]);
        Some(take)
    }

    /// Draw the page into a grid: the URL, the status, then the body wrapped to the grid's width
    /// and cut at its last row. A body the grid cannot show ends with a marker, never a scroll
    /// past the buffer.
    pub fn render(&self, grid: &mut TextGrid) {
        grid.clear();
        let Some(page) = self.page.as_ref() else {
            grid.write(b"https://\n(no page: trust HOST IP PIN, then go URL)");
            return;
        };
        page.url.write_to(grid);
        grid.put(b'\n');
        match page.failure {
            Some(why) => {
                grid.write(b"refused: ");
                grid.write(why.as_bytes());
                return;
            }
            None => {
                grid.write(b"HTTP ");
                write_decimal(grid, page.status as usize);
                grid.put(b' ');
                grid.write(page.reason());
                if page.truncated {
                    grid.write(b" (cut)");
                }
                grid.put(b'\n');
            }
        }
        let rows = grid.rows();
        for &b in page.body() {
            let (_, row) = grid.cursor();
            if row + 1 >= rows {
                break;
            }
            if b == b'\r' {
                continue;
            }
            grid.put(b);
        }
    }
}

/// The navigation contract, proved on every CPU at boot. No network: the model decides, the
/// platform dials.
pub fn browser_suite(
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
    let pin_a = [0x11u8; 32];
    let pin_b = [0x22u8; 32];

    // 1 - a URL parses to its host, port and path, with 443 and "/" as the defaults.
    {
        let a = parse_url(b"https://aletheia.test:8443/plain.txt");
        let b = parse_url(b"https://aletheia.test");
        let ok = match (a, b) {
            (Ok(a), Ok(b)) => {
                a.host() == b"aletheia.test"
                    && a.port == 8443
                    && a.path() == b"/plain.txt"
                    && b.port == 443
                    && b.path() == b"/"
            }
            _ => false,
        };
        check!(
            ok,
            "browser: a URL parses to host, port and path with 443 and / as the defaults"
        );
    }

    // 2 - plaintext is refused, never downgraded to: http:// has its own refusal.
    check!(
        parse_url(b"http://aletheia.test/") == Err(UrlRefusal::Plaintext)
            && parse_url(b"ftp://aletheia.test/") == Err(UrlRefusal::NotHttps)
            && parse_url(b"aletheia.test") == Err(UrlRefusal::NotHttps),
        "browser: http:// is refused as plaintext, never downgraded to"
    );

    // 3 - a bad host, port or path is refused for the part that is bad.
    check!(
        parse_url(b"https:///x") == Err(UrlRefusal::NoHost)
            && parse_url(b"https://Aletheia.Test/") == Err(UrlRefusal::BadHost)
            && parse_url(b"https://a b/") == Err(UrlRefusal::BadHost)
            && parse_url(b"https://a:0/") == Err(UrlRefusal::BadPort)
            && parse_url(b"https://a:70000/") == Err(UrlRefusal::BadPort)
            && parse_url(b"https://a:x/") == Err(UrlRefusal::BadPort)
            && parse_url(b"https://a/p q") == Err(UrlRefusal::BadPath)
            && parse_url(&[b'h'; MAX_URL + 1]) == Err(UrlRefusal::TooLong),
        "browser: a bad host, port or path is refused for the part that is bad"
    );

    // 4 - an unknown host is refused before any address is dialed; once trusted, it resolves to
    //     the pinned root and the address the person gave.
    {
        let mut nav = Navigator::new();
        let before = nav.navigate(b"https://aletheia.test/");
        let trusted = nav.hosts.trust(b"aletheia.test", [10, 0, 2, 2], pin_a);
        let after = nav.navigate(b"https://aletheia.test:8443/x");
        check!(
            before == Err(NavRefusal::UnknownHost)
                && trusted == Ok(())
                && after.is_ok_and(|r| r.ip == [10, 0, 2, 2] && r.pin == pin_a && r.url.port == 8443)
                && nav.history_len() == 1,
            "browser: an unknown host is refused before anything is dialed; a trusted one resolves to its pin and address"
        );
    }

    // 5 - the host table is bounded and re-trusting a name replaces its pin rather than adding.
    {
        let mut t = HostTable::default();
        let mut ok = true;
        for i in 0..MAX_HOSTS {
            let name = [b'a' + i as u8];
            ok &= t.trust(&name, [1, 1, 1, i as u8], pin_a).is_ok();
        }
        let ninth = t.trust(b"z", [9, 9, 9, 9], pin_a);
        let replaced = t.trust(b"a", [1, 1, 1, 0], pin_b);
        check!(
            ok && ninth == Err(TrustRefusal::Full)
                && replaced == Ok(())
                && t.len() == MAX_HOSTS
                && t.lookup(b"a").is_some_and(|e| e.pin == pin_b)
                && t.trust(b"", [0; 4], pin_a) == Err(TrustRefusal::BadName)
                && t.trust(b"Bad Host", [0; 4], pin_a) == Err(TrustRefusal::BadName),
            "browser: the host table is bounded, and trusting a name again replaces its pin"
        );
    }

    // 6 - history: back and forward walk it, a new navigation drops the forward pages, and the
    //     ring is bounded.
    {
        let mut nav = Navigator::new();
        nav.hosts.trust(b"h", [1, 1, 1, 1], pin_a).ok();
        for i in 0..(HISTORY + 2) {
            let mut url = *b"https://h/00";
            url[10] = b'0' + (i / 10) as u8;
            url[11] = b'0' + (i % 10) as u8;
            nav.navigate(&url).ok();
        }
        let bounded = nav.history_len() == HISTORY;
        let last = nav.current().is_some_and(|u| u.path() == b"/09");
        let b1 = nav.back().is_some_and(|u| u.path() == b"/08");
        let b2 = nav.back().is_some_and(|u| u.path() == b"/07");
        let f1 = nav.forward().is_some_and(|u| u.path() == b"/08");
        nav.navigate(b"https://h/new").ok();
        let dropped = nav.forward().is_none() && nav.history_len() == HISTORY;
        let mut fresh = Navigator::new();
        let none = fresh.back().is_none() && fresh.forward().is_none() && fresh.current().is_none();
        check!(
            bounded && last && b1 && b2 && f1 && dropped && none,
            "browser: history walks back and forward, a new page drops the forward pages, and the ring is bounded"
        );
    }

    // 7 - a page renders into a grid: URL, status, body wrapped to the width and cut at the last
    //     row, never past it.
    {
        let mut nav = Navigator::new();
        nav.hosts.trust(b"h", [1, 1, 1, 1], pin_a).ok();
        let r = nav.navigate(b"https://h:8443/p").ok();
        let body = [b'x'; 4 * 40];
        if let Some(r) = r {
            nav.set_page(r.url, 200, b"OK", &body, false);
        }
        let mut grid = TextGrid::new(40, 4);
        nav.render(&mut grid);
        let ok = grid.line(0).starts_with(b"https://h:8443/p")
            && grid.line(1).starts_with(b"HTTP 200 OK")
            && grid.line(2).iter().filter(|&&b| b == b'x').count() == 40
            && grid.line(3).iter().filter(|&&b| b == b'x').count() <= 40
            && grid.refused() == 0;
        check!(
            ok,
            "browser: a page renders as URL, status and a body cut at the grid's last row"
        );
    }

    // 8 - a page that failed names its reason with the URL it was asked for, and an over-long body
    //     is kept only to the page's bound and said to be cut.
    {
        let mut nav = Navigator::new();
        nav.hosts.trust(b"h", [1, 1, 1, 1], pin_a).ok();
        let r = nav.navigate(b"https://h/").ok();
        let mut grid = TextGrid::new(40, 4);
        if let Some(r) = r {
            nav.set_failure(
                r.url,
                "the peer's certificate is not one the pinned root signed",
            );
            nav.render(&mut grid);
        }
        let failed = grid.line(0).starts_with(b"https://h/")
            && grid.line(1).starts_with(b"refused: the peer's certificate");
        let big = [b'b'; BODY_CAP + 10];
        if let Some(r) = r {
            nav.set_page(r.url, 200, b"OK", &big, false);
        }
        let cut = nav
            .page()
            .is_some_and(|p| p.truncated && p.body().len() == BODY_CAP);
        check!(
            failed && cut,
            "browser: a failed page names its reason and its URL, and an over-long body is cut and said to be"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_navigation_invariant() {
        let mut seen = 0;
        let n = browser_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the browser suite should hold");
        assert_eq!(n, 8);
        assert_eq!(seen, 8);
    }
}
