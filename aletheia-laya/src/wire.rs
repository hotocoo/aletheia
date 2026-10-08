//! The decision wire over HTTP/1.1 on 127.0.0.1 (ADR-186): `GET /v1/models` (and `/health`) say
//! which model this is, `POST /v1/decide` answers. Bodies are bounded; one connection at a time,
//! one request per connection, as the sidecar it replaces.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// A console request and its options are a few KB; anything bigger is refused.
pub const MAX_BODY: usize = 256 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;

/// Whatever answers a decide request.
pub trait Decider {
    fn decide(&self, req: &Value) -> Result<Value, String>;
}

/// Serve one connection.
pub fn handle(conn: TcpStream, serve_id: &str, d: &dyn Decider) {
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Ok(read_half) = conn.try_clone() else {
        return;
    };
    let mut r = BufReader::new(read_half.take((MAX_BODY + 64 * MAX_HEADER_LINE) as u64));
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut len: Option<usize> = None;
    for _ in 0..64 {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().ok();
        }
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (code, body) = match (method, path) {
        ("GET", "/v1/models" | "/health") => (200, json!({"data": [{"id": serve_id}]})),
        ("POST", "/v1/decide") => match len {
            Some(n) if n > 0 && n <= MAX_BODY => {
                let mut b = vec![0; n];
                match r.read_exact(&mut b) {
                    Err(_) => (400, json!({"error": "the body ended early"})),
                    Ok(()) => match serde_json::from_slice::<Value>(&b)
                        .map_err(|e| e.to_string())
                        .and_then(|v| d.decide(&v))
                    {
                        Ok(v) => (200, v),
                        Err(e) => (400, json!({ "error": e })),
                    },
                }
            }
            _ => (
                413,
                json!({"error": format!("body must be 1..{MAX_BODY} bytes")}),
            ),
        },
        _ => (404, json!({"error": "not found"})),
    };
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        413 => "Payload Too Large",
        _ => "Not Found",
    };
    let b = body.to_string();
    let mut c = conn;
    let _ = write!(
        c,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    struct Echo;
    impl Decider for Echo {
        fn decide(&self, req: &Value) -> Result<Value, String> {
            match req["questions"].as_array() {
                Some(q) if !q.is_empty() => {
                    Ok(json!({"answers": [{"label": "x", "confidence": 1.0}]}))
                }
                _ => Err("no questions".into()),
            }
        }
    }

    fn round_trip(raw: &[u8]) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            handle(c, "test-model", &Echo);
        });
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        t.join().unwrap();
        out
    }

    #[test]
    fn identity_decide_and_refusals() {
        let id = round_trip(b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            id.starts_with("HTTP/1.1 200 OK") && id.contains("\"test-model\""),
            "{id}"
        );
        let body = r#"{"state":"s","questions":[{"type":"yesno"}]}"#;
        let ok = round_trip(
            format!(
                "POST /v1/decide HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        assert!(
            ok.starts_with("HTTP/1.1 200 OK") && ok.contains("\"answers\""),
            "{ok}"
        );
        let empty = r#"{"state":"s","questions":[]}"#;
        let bad = round_trip(
            format!(
                "POST /v1/decide HTTP/1.1\r\nContent-Length: {}\r\n\r\n{empty}",
                empty.len()
            )
            .as_bytes(),
        );
        assert!(
            bad.starts_with("HTTP/1.1 400") && bad.contains("no questions"),
            "{bad}"
        );
        let big = round_trip(
            format!(
                "POST /v1/decide HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            )
            .as_bytes(),
        );
        assert!(big.starts_with("HTTP/1.1 413"), "{big}");
        let none = round_trip(b"POST /v1/decide HTTP/1.1\r\n\r\n");
        assert!(none.starts_with("HTTP/1.1 413"), "{none}");
        let lost = round_trip(b"GET /etc/passwd HTTP/1.1\r\n\r\n");
        assert!(lost.starts_with("HTTP/1.1 404"), "{lost}");
    }
}
