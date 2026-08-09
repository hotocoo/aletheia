//! Aletheia-owned transport seam for the Service API / IPC boundary (ADR-047; extends ADR-016).
//!
//! `service.rs` marshals `Request`/`Response` frames; it must not know what carries them. This
//! module owns that knowledge, and it is the ONLY place in the hosted Core permitted to name a
//! platform API. The native Aletheia kernel's capability-named endpoint becomes a third
//! implementation of the same two traits, not an edit to the service.
//!
//! An **endpoint** is named by a path on every host. What lives at that path differs:
//!
//! - **Unix** — a Unix-domain socket. The path IS the socket; filesystem permissions are the only
//!   thing bounding who may connect.
//! - **Windows** — `std` exposes no `UnixListener`, and this crate's transport is deliberately
//!   dependency-free, so the listener binds `127.0.0.1:0` and the path holds a small **rendezvous
//!   file** carrying the chosen port and a fresh 32-byte connect token. A client must read that file
//!   to connect at all, which is what binds connect authority back to filesystem permission.
//!
//! HOSTED-CONTRACT HONESTY (KC-IPC, restated for both backends): SAD §5 requires "no global
//! connectable namespace", and neither backend delivers one. A Unix socket path is locally
//! connectable; a loopback port is locally connectable by *any* process, which is why the Windows
//! backend authenticates the first frame against the token instead of trusting the connection. Both
//! are hosted approximations of a capability-named endpoint, and the Windows one is the weaker of
//! the two. Documented, not hidden.

use std::io::{Read, Write};
use std::time::Duration;

/// One accepted connection. Byte-oriented, synchronous, framed by the caller.
pub trait Conn: Read + Write + Send {
    /// Bound how long a stalled peer may hold the (sequential) accept loop. Best-effort: a backend
    /// that cannot express a timeout returns `Ok(())` rather than failing the connection.
    fn set_read_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()>;
}

/// A bound endpoint that yields connections. Dropping it releases the endpoint's name.
///
/// `Send`, because the serving loop routinely lives on a thread that is not the one that bound the
/// endpoint (the conformance suite does exactly this).
pub trait Listener: Send {
    /// Block until a peer connects. A backend that authenticates at connect time (Windows) has
    /// already done so by the time this returns, so the service never sees an unauthenticated conn.
    fn accept(&self) -> std::io::Result<Box<dyn Conn>>;
}

/// Bind `path` as a service endpoint.
pub fn bind(path: &str) -> std::io::Result<Box<dyn Listener>> {
    imp::bind(path)
}

/// Connect to a service endpoint previously bound at `path`.
pub fn connect(path: &str) -> std::io::Result<Box<dyn Conn>> {
    imp::connect(path)
}

/// A short, human-meaningful name for the active backend — reported by `aletheiad serve` so an
/// operator can see which approximation they are running under (they differ in posture, §KC-IPC).
pub fn backend_name() -> &'static str {
    imp::BACKEND
}

// --- Unix backend: the endpoint path IS the socket ---

#[cfg(unix)]
mod imp {
    use super::{Conn, Listener};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    pub const BACKEND: &str = "unix-socket";

    impl Conn for UnixStream {
        fn set_read_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()> {
            UnixStream::set_read_timeout(self, dur)
        }
    }

    struct UnixEndpoint {
        listener: UnixListener,
        path: String,
    }

    impl Listener for UnixEndpoint {
        fn accept(&self) -> std::io::Result<Box<dyn Conn>> {
            let (stream, _) = self.listener.accept()?;
            Ok(Box::new(stream))
        }
    }

    impl Drop for UnixEndpoint {
        fn drop(&mut self) {
            // The socket file outlives the listener otherwise, and a stale one makes the next bind
            // fail with EADDRINUSE against a socket nothing is listening on.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn bind(path: &str) -> std::io::Result<Box<dyn Listener>> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        Ok(Box::new(UnixEndpoint {
            listener,
            path: path.to_string(),
        }))
    }

    pub fn connect(path: &str) -> std::io::Result<Box<dyn Conn>> {
        Ok(Box::new(UnixStream::connect(path)?))
    }
}

// --- Windows backend: loopback TCP behind a token-bearing rendezvous file ---

#[cfg(windows)]
mod imp {
    use super::{Conn, Listener};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
    use std::time::Duration;

    pub const BACKEND: &str = "loopback-tcp+rendezvous-token";

    /// Length of the connect token. 32 bytes of OS entropy: large enough that guessing is not a
    /// strategy, small enough to sit in one frame ahead of the first request.
    const TOKEN_LEN: usize = 32;
    /// How long a freshly-accepted peer has to present the token before it is dropped. Short: the
    /// client sends it immediately, and a peer that does not is not a client.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

    impl Conn for TcpStream {
        fn set_read_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()> {
            TcpStream::set_read_timeout(self, dur)
        }
    }

    struct TcpEndpoint {
        listener: TcpListener,
        token: [u8; TOKEN_LEN],
        path: String,
    }

    impl Listener for TcpEndpoint {
        fn accept(&self) -> std::io::Result<Box<dyn Conn>> {
            // Loop rather than return: a peer that fails the handshake is not an error the service
            // should see — it is a stranger, and the endpoint stays open for the real client.
            loop {
                let (mut stream, _) = self.listener.accept()?;
                stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
                let mut offered = [0u8; TOKEN_LEN];
                if stream.read_exact(&mut offered).is_err() {
                    continue;
                }
                if !constant_time_eq(&offered, &self.token) {
                    continue;
                }
                stream.set_read_timeout(None)?;
                return Ok(Box::new(stream));
            }
        }
    }

    impl Drop for TcpEndpoint {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn bind(path: &str) -> std::io::Result<Box<dyn Listener>> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = random_token()?;

        // The rendezvous file is the endpoint's name AND its access control: a peer that cannot read
        // it cannot present the token. Written whole, then flushed, so a client never reads a
        // half-written line and connects to a truncated port number.
        let mut f = std::fs::File::create(path)?;
        writeln!(f, "aletheia-endpoint v1")?;
        writeln!(f, "port {port}")?;
        writeln!(f, "token {}", hex(&token))?;
        f.flush()?;
        drop(f);

        Ok(Box::new(TcpEndpoint {
            listener,
            token,
            path: path.to_string(),
        }))
    }

    pub fn connect(path: &str) -> std::io::Result<Box<dyn Conn>> {
        let text = std::fs::read_to_string(path)?;
        let (port, token) = parse_rendezvous(&text).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed endpoint rendezvous file",
            )
        })?;
        // A connect failure against an EXISTING rendezvous file means the file is stale: the server
        // that wrote it is gone and some other process may since have taken the port. Report it as a
        // dead endpoint rather than retrying into whatever is listening there now.
        let mut stream =
            TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    format!("endpoint {path} is not live ({e})"),
                )
            })?;
        stream.write_all(&token)?;
        stream.flush()?;
        Ok(Box::new(stream))
    }

    fn parse_rendezvous(text: &str) -> Option<(u16, [u8; TOKEN_LEN])> {
        let mut port = None;
        let mut token = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("port ") {
                port = v.trim().parse::<u16>().ok();
            } else if let Some(v) = line.strip_prefix("token ") {
                token = unhex(v.trim());
            }
        }
        Some((port?, token?))
    }

    /// 32 bytes from the OS CSPRNG. `BCryptGenRandom` via the process-preferred RNG — no external
    /// crate, and not a PRNG seeded from the clock (a predictable token is no token).
    fn random_token() -> std::io::Result<[u8; TOKEN_LEN]> {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                h_algorithm: *mut core::ffi::c_void,
                pb_buffer: *mut u8,
                cb_buffer: u32,
                dw_flags: u32,
            ) -> i32;
        }
        const USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        let mut buf = [0u8; TOKEN_LEN];
        // SAFETY: `buf` is a live, exclusively-borrowed 32-byte allocation and TOKEN_LEN is its
        // exact length; a null algorithm handle is what USE_SYSTEM_PREFERRED_RNG requires.
        let status = unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                buf.as_mut_ptr(),
                TOKEN_LEN as u32,
                USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(std::io::Error::other(format!(
                "BCryptGenRandom failed: {status:#x}"
            )));
        }
        Ok(buf)
    }

    /// Whole-length compare. An early-exit `==` on a secret, against a peer that can reconnect on a
    /// loopback socket as fast as it likes, is a timing oracle.
    fn constant_time_eq(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
        let mut diff = 0u8;
        for i in 0..TOKEN_LEN {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn unhex(s: &str) -> Option<[u8; TOKEN_LEN]> {
        if s.len() != TOKEN_LEN * 2 {
            return None;
        }
        let mut out = [0u8; TOKEN_LEN];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The seam itself, on whichever backend this host provides: bind, connect, round-trip bytes.
    /// This is the one test that proves `service.rs` can stop naming a platform.
    #[test]
    fn endpoint_round_trips_on_this_host() {
        let path = std::env::temp_dir()
            .join(format!("aletheia-transport-{}.ep", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&path).expect("bind endpoint");

        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().expect("accept");
            let mut buf = [0u8; 5];
            conn.read_exact(&mut buf).expect("read");
            conn.write_all(b"pong!").expect("write");
            conn.flush().expect("flush");
            buf
        });

        let mut client = connect(&path).expect("connect endpoint");
        client.write_all(b"ping!").expect("send");
        client.flush().expect("flush");
        let mut back = [0u8; 5];
        client.read_exact(&mut back).expect("recv");

        assert_eq!(&back, b"pong!");
        assert_eq!(&server.join().unwrap(), b"ping!");
        assert!(!backend_name().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Connecting to a name nothing has bound must FAIL, on every backend. A transport that
    /// succeeds against an unbound endpoint would let a client believe it reached the Core.
    #[test]
    fn connecting_to_an_unbound_endpoint_fails() {
        let path = std::env::temp_dir()
            .join(format!("aletheia-absent-{}.ep", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);
        assert!(connect(&path).is_err());
    }
}
