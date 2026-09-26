//! Redirects, followed under the navigation's own policy (ADR-190).
use kernel_core::browser::{parse_url, redirect_target, Navigator, MAX_REDIRECTS};
use kernel_core::fs::Filesystem;
use kernel_core::shell::{execute, ShellAction, ShellHost};
use kernel_core::storage::MemBlockDevice;
use kernel_core::tlsclient::TlsReport;

/// A peer that answers by path, from a script.
struct Peer;

fn answer(path: &str) -> String {
    let redirect = |code: u16, to: &str| {
        format!("HTTP/1.1 {code} Moved\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n")
    };
    match path {
        "/a" => redirect(302, "/b"),
        "/b" => redirect(301, "https://other.test/c"),
        "/c" => {
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\narrived".into()
        }
        "/loop" => redirect(302, "/loop"),
        "/down" => redirect(302, "http://example.test/x"),
        "/blocked" => redirect(307, "https://blocked.test/"),
        "/stranger" => redirect(308, "https://unknown.test/"),
        "/nowhere" => "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".into(),
        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
    }
}

impl ShellHost for Peer {
    fn arch(&self) -> &str {
        "test"
    }
    fn uptime_ns(&self) -> u64 {
        1
    }
    fn free_frames(&self) -> usize {
        1
    }
    fn total_frames(&self) -> usize {
        1
    }
    fn privilege(&self) -> u64 {
        1
    }
    fn authorize(&self, _a: ShellAction) -> bool {
        true
    }
    fn tls_fetch(
        &self,
        _ip: [u8; 4],
        _port: u16,
        _name: &[u8],
        _pin: [u8; 32],
        request: &[u8],
        reply: &mut [u8],
    ) -> Result<TlsReport, &'static str> {
        let req = core::str::from_utf8(request).unwrap();
        let path = req.split_whitespace().nth(1).unwrap();
        let a = answer(path);
        reply[..a.len()].copy_from_slice(a.as_bytes());
        Ok(TlsReport {
            received: a.len(),
            ..TlsReport::default()
        })
    }
}

fn session(lines: &[&str]) -> String {
    let mut dev = MemBlockDevice::new(96);
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    let mut nav = Navigator::new();
    let mut out = String::new();
    let pin = "11".repeat(32);
    for l in [
        format!("trust example.test 10.0.2.2 {pin}"),
        format!("trust other.test 10.0.2.3 {pin}"),
        format!("trust blocked.test 10.0.2.4 {pin}"),
        "block blocked.test".to_string(),
    ] {
        execute(&l, &Peer, &mut fs, &mut dev, &[], &mut nav, &mut |_| {});
    }
    for l in lines {
        execute(l, &Peer, &mut fs, &mut dev, &[], &mut nav, &mut |s| {
            out.push_str(s);
            out.push('\n')
        });
    }
    out
}

#[test]
fn a_chain_across_trusted_hosts_is_followed_and_the_history_names_where_it_ended() {
    let out = session(&["go https://example.test/a", "back"]);
    assert!(out.contains("https://other.test/c"), "{out}");
    assert!(out.contains("HTTP 200 OK (after 2 redirect(s))"), "{out}");
    assert!(out.contains("arrived"), "{out}");
}

#[test]
fn every_redirect_that_leaves_the_policy_is_refused_by_name() {
    for (path, why) in [
        ("/loop", "too many redirects"),
        ("/down", "the redirect would leave https"),
        ("/blocked", "the redirect names a blocked host"),
        (
            "/stranger",
            "the redirect names a host this machine does not trust",
        ),
        ("/nowhere", "a redirect with no Location"),
    ] {
        let out = session(&[&format!("go https://example.test{path}")]);
        assert!(out.contains(&format!("refused: {why}")), "{path}: {out}");
    }
}

#[test]
fn a_location_resolves_to_https_on_the_same_host_or_is_refused() {
    let from = parse_url(b"https://example.test:8443/x").unwrap();
    let t = redirect_target(&from, b" /y?z=1 ").unwrap();
    assert_eq!(
        (t.host(), t.port, t.path()),
        (&b"example.test"[..], 8443, &b"/y?z=1"[..])
    );
    let t = redirect_target(&from, b"https://other.test/").unwrap();
    assert_eq!((t.host(), t.port), (&b"other.test"[..], 443));
    for bad in [
        &b"http://example.test/"[..],
        b"//evil.test/",
        b"relative",
        b"",
    ] {
        assert!(
            redirect_target(&from, bad).is_err(),
            "{:?}",
            core::str::from_utf8(bad)
        );
    }
    assert!(MAX_REDIRECTS >= 1);
}
