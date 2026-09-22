//! Lethe's policy contract, adopted natively (ADR-159; Lethe integration stage N6).
//!
//! Lethe is a browser whose README specifies and proves a handful of BEHAVIOURS rather than an
//! engine: HTTPS-first with plaintext refused rather than silently downgraded, tracker hosts refused
//! as third-party requests, a fixed low-entropy user agent, an ephemeral-by-default site-data store.
//! Those behaviours are what this tree can genuinely build in, and this module is where they are
//! proved - against the navigation model (`browser.rs`), the renderer (`content.rs`) and the HTTP
//! client (`http.rs`) as they are, on every CPU, at boot.
//!
//! Nothing here is engine code. Every invariant below is a statement about what the browser this
//! kernel already has CANNOT do: rewrite a scheme, dial a host the operator blocked, fetch a
//! resource a page named, vary its user agent, or remember a cookie.

use crate::browser::{
    parse_url, NavRefusal, Navigator, TrustRefusal, UrlRefusal, MAX_BLOCKED, MAX_URL,
};
use crate::content::render;
use crate::http::{parse, request, USER_AGENT};

/// A page that names resources on a tracker host in every way HTML can, and offers one link.
const TRACKER_PAGE: &[u8] = b"<html><head><title>t</title>\
<link rel=\"stylesheet\" href=\"https://tracker.example/s.css\">\
<script src=\"https://tracker.example/t.js\"></script></head><body>\
<img src=\"https://tracker.example/p.gif\" alt=\"\">\
<iframe src=\"https://tracker.example/f\"></iframe>\
<p>text <a href=\"https://other.example/\">elsewhere</a> and <a href=\"/local\">here</a></p>\
</body></html>";

/// Lethe's policy contract as boot invariants. Reports each (index, passed, name).
pub fn policy_suite(
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
    let pin = [0x11u8; 32];
    let addr = [10, 0, 2, 2];

    // 1 - HTTPS-first: plaintext is refused AS plaintext, to a trusted host too, and is not
    //     rewritten to https behind the person's back. Nothing is recorded; a link in a page that
    //     points at plaintext is refused the same way when followed.
    {
        let mut nav = Navigator::new();
        let _ = nav.hosts.trust(b"aletheia.test", addr, pin);
        let typed = nav.navigate(b"http://aletheia.test/plain.txt");
        let mut shown = [0u8; 512];
        let rendered = render(
            b"<a href=\"http://aletheia.test/x\">plain</a>",
            &mut shown,
            30,
            8,
        );
        nav.set_links(&rendered);
        let mut target = [0u8; MAX_URL];
        let followed = nav
            .link_target(1, &mut target)
            .map(|len| nav.navigate(&target[..len]));
        check!(
            typed == Err(NavRefusal::Url(UrlRefusal::Plaintext))
                && followed == Some(Err(NavRefusal::Url(UrlRefusal::Plaintext)))
                && nav.history_len() == 0
                && nav.page().is_none(),
            "policy: plaintext is refused as plaintext, never rewritten to https, and leaves no history"
        );
    }

    // 2 - the block list: a blocked host is refused BEFORE lookup, pinned or not, typed or reached
    //     by `back`; the list is bounded and the ninth entry is refused, not evicted.
    {
        let mut nav = Navigator::new();
        let _ = nav.hosts.trust(b"aletheia.test", addr, pin);
        let first = nav.navigate(b"https://aletheia.test/a");
        let url_a = first.ok().map(|r| r.url);
        let blocked = nav.blocked.block(b"aletheia.test");
        let typed = nav.navigate(b"https://aletheia.test/b");
        let returned = url_a.as_ref().map(|u| nav.resolve(u));
        let unpinned = nav.blocked.block(b"tracker.example").and_then(|_| {
            if nav.navigate(b"https://tracker.example/") == Err(NavRefusal::Blocked) {
                Ok(())
            } else {
                Err(TrustRefusal::BadName)
            }
        });
        let mut full = true;
        for i in 0..MAX_BLOCKED {
            let name = [b'a' + i as u8];
            // The two names above take two slots; the rest fill, then one more is refused.
            let r = nav.blocked.block(&name);
            full &= r.is_ok() || (i >= MAX_BLOCKED - 2 && r == Err(TrustRefusal::Full));
        }
        check!(
            first.is_ok()
                && blocked == Ok(())
                && typed == Err(NavRefusal::Blocked)
                && returned == Some(Err(NavRefusal::Blocked))
                && unpinned == Ok(())
                && nav.blocked.block(b"Bad Name") == Err(TrustRefusal::BadName)
                && full
                && nav.blocked.len() == MAX_BLOCKED
                && nav.history_len() == 1,
            "policy: a blocked host is refused before lookup, pinned or not, forward or back; the ninth block is refused, not evicted"
        );
    }

    // 3 - no third-party REQUESTS: the renderer has no network and makes none. A stylesheet, a
    //     script, an image and a frame on a tracker host become neither links nor text; only the
    //     anchors a person can choose survive, as numbered links.
    {
        let mut shown = [0u8; 1024];
        let rendered = render(TRACKER_PAGE, &mut shown, 40, 20);
        let no_tracker_text = !contains(rendered.text, b"tracker");
        check!(
            rendered.links() == 2
                && no_tracker_text
                && rendered.link(1) == Some(&b"https://other.example/"[..])
                && rendered.link(2) == Some(&b"/local"[..]),
            "policy: the renderer makes no requests - img, script, iframe and stylesheet sources become neither links nor text"
        );
    }

    // 4 - a link to another host is third-party BY NAME, and following it dials only a host the
    //     operator pinned and did not block: unknown is refused before lookup resolves nothing,
    //     blocked is refused before lookup, pinned resolves to ITS pin, never the page's.
    {
        let mut nav = Navigator::new();
        let _ = nav.hosts.trust(b"aletheia.test", addr, pin);
        let page_url = nav
            .navigate(b"https://aletheia.test/index.html")
            .map(|r| r.url)
            .ok();
        let mut shown = [0u8; 1024];
        let rendered = render(TRACKER_PAGE, &mut shown, 40, 20);
        let Some(page_url) = page_url else {
            check!(false, "policy: a link to another host is third-party by name and dials only a host the operator pinned and did not block");
            unreachable!();
        };
        nav.set_page(page_url, 200, b"OK", rendered.text, false);
        nav.set_links(&rendered);
        let third = nav.is_third_party(1);
        let same = nav.is_third_party(2);
        let mut target = [0u8; MAX_URL];
        let unknown = nav
            .link_target(1, &mut target)
            .map(|len| nav.navigate(&target[..len]));
        let other_pin = [0x33u8; 32];
        let _ = nav.hosts.trust(b"other.example", [10, 0, 2, 3], other_pin);
        let _ = nav.blocked.block(b"other.example");
        let blocked = nav
            .link_target(1, &mut target)
            .map(|len| nav.navigate(&target[..len]));
        let mut nav2 = Navigator::new();
        let _ = nav2.hosts.trust(b"aletheia.test", addr, pin);
        let _ = nav2.hosts.trust(b"other.example", [10, 0, 2, 3], other_pin);
        let _ = nav2.navigate(b"https://aletheia.test/index.html");
        nav2.set_page(page_url, 200, b"OK", rendered.text, false);
        nav2.set_links(&rendered);
        let pinned = nav2
            .link_target(1, &mut target)
            .map(|len| nav2.navigate(&target[..len]));
        check!(
            third == Some(true)
                && same == Some(false)
                && unknown == Some(Err(NavRefusal::UnknownHost))
                && blocked == Some(Err(NavRefusal::Blocked))
                && pinned.is_some_and(|r| r.is_ok_and(|r| r.pin == other_pin && r.ip == [10, 0, 2, 3]))
                && nav.history_len() == 1,
            "policy: a link to another host is third-party by name and dials only a host the operator pinned and did not block"
        );
    }

    // 5 - a fixed, low-entropy user agent: the request carries exactly four headers, the same
    //     four for every host and path, and the agent string names no platform, language or
    //     version that could tell one machine from another.
    {
        let mut a = [0u8; 512];
        let mut b = [0u8; 512];
        let la = request(b"aletheia.test", b"/plain.txt", &mut a).unwrap_or(0);
        let lb = request(b"other.example:8443", b"/x/y?z=1", &mut b).unwrap_or(0);
        let ha = after_host_line(&a[..la]);
        let hb = after_host_line(&b[..lb]);
        let header_lines = ha.map_or(0, |h| count(h, b"\r\n"));
        check!(
            la > 0
                && lb > 0
                && ha.is_some()
                && ha == hb
                && header_lines == 4 // User-Agent, Accept, Connection, and the empty line that ends them
                && USER_AGENT == b"aletheia/0.1"
                && contains(&a[..la], b"\r\nUser-Agent: aletheia/0.1\r\n")
                && !contains(&a[..la], b"Accept-Language")
                && !contains(&a[..la], b"Mozilla")
                && !contains(&a[..la], b"(")
                && !contains(&a[..la], b"Cookie"),
            "policy: the user agent is one fixed string - requests to different hosts differ only in host and path"
        );
    }

    // 6 - ephemeral by default, and nothing to be ephemeral WITH: a `Set-Cookie` in the answer is
    //     read as a header and kept nowhere; the next request is byte-identical to the first.
    {
        let raw: &[u8] = b"HTTP/1.1 200 OK\r\nSet-Cookie: id=7; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let mut body = [0u8; 64];
        let parsed = parse(raw, &mut body, false);
        let saw_cookie = parsed
            .as_ref()
            .is_ok_and(|r| r.header(raw, b"set-cookie") == Some(&b"id=7; Path=/"[..]));
        let mut nav = Navigator::new();
        let _ = nav.hosts.trust(b"aletheia.test", addr, pin);
        let mut first = [0u8; 512];
        let l1 = request(b"aletheia.test", b"/", &mut first).unwrap_or(0);
        if let Ok(r) = nav.navigate(b"https://aletheia.test/") {
            nav.set_page(r.url, 200, b"OK", &body[..2], false);
        }
        let mut second = [0u8; 512];
        let l2 = request(b"aletheia.test", b"/", &mut second).unwrap_or(0);
        check!(
            saw_cookie
                && l1 > 0
                && l1 == l2
                && first[..l1] == second[..l2]
                && !contains(&second[..l2], b"Cookie")
                && nav.page().is_some_and(|p| p.body() == b"ok"),
            "policy: no site data is kept - a Set-Cookie in the answer changes nothing about the next request"
        );
    }

    // 7 - `forget`: history, the page and its links go; the operator's trust and block lists
    //     stay, because they are configuration the person typed, not data a site left.
    {
        let mut nav = Navigator::new();
        let _ = nav.hosts.trust(b"aletheia.test", addr, pin);
        let _ = nav.blocked.block(b"tracker.example");
        let mut shown = [0u8; 1024];
        let rendered = render(TRACKER_PAGE, &mut shown, 40, 20);
        if let Ok(r) = nav.navigate(b"https://aletheia.test/index.html") {
            nav.set_page(r.url, 200, b"OK", rendered.text, false);
            nav.set_links(&rendered);
        }
        let before = (nav.history_len(), nav.page().is_some(), nav.link_count());
        nav.forget();
        check!(
            before == (1, true, 2)
                && nav.history_len() == 0
                && nav.back().is_none()
                && nav.page().is_none()
                && nav.link_count() == 0
                && nav.hosts.len() == 1
                && nav.blocked.len() == 1
                && nav.navigate(b"https://aletheia.test/again").is_ok(),
            "policy: forget empties history, page and links and keeps the operator's trust and block lists"
        );
    }

    // 8 - nothing pre-installed, nothing remembered: a fresh navigator has no hosts, no blocks,
    //     no history, no page and no links, so every https host is unknown until a person pins it.
    {
        let mut nav = Navigator::new();
        check!(
            nav.hosts.is_empty()
                && nav.blocked.is_empty()
                && nav.history_len() == 0
                && nav.page().is_none()
                && nav.link_count() == 0
                && nav.navigate(b"https://example.com/") == Err(NavRefusal::UnknownHost)
                && parse_url(b"https://example.com/").is_ok(),
            "policy: a fresh navigator holds nothing - no hosts, no blocks, no history, no page; every host is unknown"
        );
    }

    Ok(n)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Everything after the `Host:` line: the headers that must not depend on where we are going.
fn after_host_line(req: &[u8]) -> Option<&[u8]> {
    let host_at = req.windows(6).position(|w| w == b"Host: ")?;
    let end = host_at + req[host_at..].windows(2).position(|w| w == b"\r\n")? + 2;
    Some(&req[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_suite_holds_on_the_host() {
        let mut names = 0;
        let r = policy_suite(|_, passed, name| {
            names += 1;
            assert!(passed, "{name}");
        });
        assert_eq!(r, Ok(8));
        assert_eq!(names, 8);
    }
}
