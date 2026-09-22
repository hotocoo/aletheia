# Lethe — Aletheia's native browser: the integration contract

Lethe (`github.com/hotocoo/lethe`) is written as the native browser for Aletheia, and the two
repositories are meant to stay in step. This page says exactly what "built in" means today, what it
cannot mean yet and why, what is pinned, and how the pin is kept current. It exists so that nobody
— including a future wave of this project — has to guess how far the integration actually goes.

## The honest position

**Lethe does not run on the Aletheia kernel today, and this repository does not claim it does.**

Lethe is a C++ application whose rendering engine is the host operating system's web view:
WKWebView on macOS, WebKitGTK on Linux, WebView2 on Windows. It needs a libc, a POSIX-shaped
process and thread model, a filesystem with a user profile, a full TLS stack, and a browser engine
of roughly Chromium's size.

Aletheia is a `no_std` bare-metal Rust kernel. It has no libc, no C++ runtime, no hosted process
model, and — still the decisive gap — **no TLS** (the key schedule exists; the client does not). The network stack is virtio-net, ARP, DHCP, UDPv4
and, since ADR-138, **TCP** (`kernel-core/src/virtionet.rs`, `arpcache.rs`, `dhcp.rs`, `udpv4.rs`,
`tcp.rs`, `tcpconn.rs`). The TCP that exists is the transport's CONTRACT — a bounded state machine
proved at boot on all three CPUs — and it is not yet attached to virtio-net, so no live machine
opens a socket yet.

A browser needs, in order: TCP, TLS, HTTP, a URL/resource fetch model, a content parser, a layout
engine, and a renderer that can reach the compositor. Aletheia has the compositor, the window
manager and the surfaces (ADR-077 through ADR-137). Of the seven things above it now has the first
one for real — a live TCP conversation with a peer this repository did not write — and the other
six are not started.

So "make Lethe built in" decomposes into two honest tracks, and this page keeps them apart.

## Track 1 — the upstream stays in step (DELIVERED)

What this repository tracks today is the upstream **contract**, pinned by commit:

* `third_party/lethe.pin` records the upstream remote, branch and commit that Aletheia's
  integration surface is written against, with the date it was taken.
* `scripts/sync-lethe.sh` is the operator-run updater. It fetches the upstream, shows what moved
  since the pin, and rewrites the pin file. It is network-dependent and therefore not a CI gate.
* `scripts/check-lethe-pin.sh` IS a CI gate. It refuses a malformed pin, a pin that is not a full
  40-hex commit, a pin whose recorded date is in the future, and a pin this page does not describe.
  A pin nobody validates is a pin that silently rots.

This is deliberately modest: it guarantees that Aletheia always knows WHICH Lethe it is written
against, and that updating that answer is one command rather than an archaeology exercise. It does
not, and must not be read to, imply that the pinned code executes on the kernel.

## Track 2 — the kernel grows what a browser needs (STAGED, NOT DONE)

Each stage below is a real milestone with its own invariants, in dependency order. None of them is
"port Lethe"; all of them are things Aletheia needs to be an operating system regardless, which is
why they are worth doing in this order rather than chasing the browser directly.

| Stage | What it is | Status |
|---|---|---|
| N1 | **TCP** over the existing IPv4/virtio-net path: connection state machine, retransmission, windowing, teardown, all fail-closed and proved at boot like every other contract here | **DELIVERED (ADR-138/139/140)** — the transport's contract as a bounded state machine with no device in it (`tcp.rs` + `tcpconn.rs`, 24 boot invariants on all three CPUs), the join to a real link (`tcpnet.rs`, 3 more), and a LIVE conversation: the console's `tcp ADDR PORT TEXT` dials a real socket server on the host in `scripts/tcp-e2e.sh`, and the peer's own answer comes back on the serial line. One connection at a time, no listening socket, no DNS |
| N2 | **TLS 1.3 client**: certificate validation against a pinned trust root, no downgrade path | **delivered (ADR-141..151)** — the KEY SCHEDULE (`hkdf=9`), the KEY EXCHANGE (`x25519=7`), the RECORD LAYER (`tlsrecord=9`, interoperating with OpenSSL in both directions on the host) and the HANDSHAKE (`tlshandshake=9`) are delivered and proved on all three CPUs. The handshake is built so it CANNOT be used insecurely: certificate verification is a constructor argument, the only verifier this kernel ships refuses every peer, and the suites prove as a NEGATIVE that no sequence of messages reaches application traffic keys. SIGNATURE VERIFICATION is delivered too (ADR-145: SHA-512 `sha512=5` and Ed25519 verification `ed25519=8`, agreeing with OpenSSL on the host). CERTIFICATE READING is delivered too (ADR-146: a DER reader that refuses rather than reads, `x509=9`). THE PINNED VERIFIER delivered too (ADR-147: `PinnedRoot`, a leaf signed directly by one pinned Ed25519 root for the expected name inside its window at a caller-supplied time, `trust=9`; with it the handshake reaches CertificateVerify and stops there by name, `tlshandshake=10`). THE WALL CLOCK delivered too (ADR-148: every target reads its own real-time clock through one contract, a reading plausible or refused by name, `clock=7`; the platform's own time now builds the verifier). THE HANDSHAKE COMPLETES (ADR-149: CertificateVerify checked under the key the verifier named over this transcript, the client's Finished written once the server's verifies, `tlshandshake=12` reaching Done with application keys under a pinned root, the fixture signed by OpenSSL with a key this kernel lacks). THE JOIN (ADR-151): `tlsclient.rs` carries the handshake over ADR-143's records and ADR-140's TCP, allocating once; the console's `tls ADDR PORT NAME PIN TEXT` takes the name and the pinned root from the operator; proved on all three CPUs over a stand-in server (`tlsclient=8`) and LIVE against OpenSSL on the runner (`scripts/tls-e2e.sh`), with a wrong pin refused by name before a byte of the request leaves the guest. AN ENTROPY SOURCE (ADR-153): the ephemeral key is made from virtio-rng, every draw checked, and a machine without the device opens no conversation rather than falling back to the clock. **Delivered.** |
| N3 | **HTTP/1.1 client** over N1+N2, bounded by construction (no unbounded response buffering on a heap that never frees — ADR-063) | **delivered (ADR-155)** — `kernel-core/src/http.rs`: a `GET` with `Connection: close`, a reader that checks every peer-named length against the bytes that arrived, refuses two body boundaries as ambiguous rather than resolving them, and truncates a body larger than the caller's buffer and says so; `http=8` on all three CPUs; the console's `https ADDR PORT NAME PIN PATH` proved LIVE in `scripts/https-e2e.sh` against Python's `http.server` behind OpenSSL (Content-Length, chunked, truncated, 404, wrong pin refused) |
| N4 | **A browser window in the desktop**: a managed window like the terminal and monitor, owning a URL/navigation state model, driven by the existing window manager and input session | **delivered (ADR-156, ADR-157)** — the navigation model is delivered and proved: `kernel-core/src/browser.rs` (https-only URLs, plaintext refused as plaintext; a bounded operator-filled table of trusted hosts with their address and pinned root, unpinned hosts refused before any dial; a bounded history ring; a bounded page rendered into a `TextGrid`), `browser=8` on all three CPUs, reached from the console by `trust NAME IP PIN`, `go URL`, `back` and proved LIVE in `scripts/https-e2e.sh`. THE WINDOW (ADR-157): a fifth managed window `browser` with a typed URL line and the page the model rendered, `Alt+5`, a taskbar button; the desktop owns no network and no trust — it latches the URL and the console session navigates and pushes the page back; the live desktop gates require `5 managed windows` |
| N5 | **Content**: a bounded, fail-closed subset renderer into the window's `TextGrid` / compositor surface. This is where "a browser" starts being a real word | **DELIVERED (ADR-158)** — `kernel-core/src/content.rs` renders headings, paragraphs, breaks, lists, `pre`, links and the title into lines; script/style-like CONTENT is dropped whole and counted; any other tag is invisible and its attributes never text; input bounded at 8 KiB, output at the grid, links at sixteen; five named entities and decimal references decode, the rest stay literal; non-printable bytes show as `?`. `go` renders `text/html` answers through it and `follow N` opens link `[N]` (made absolute against the page, refused for plaintext or an unpinned host like any URL). `content=8` on all three CPUs; `console=50`; LIVE in `scripts/https-e2e.sh` against the real server's HTML page (heading, numbered link, list, never the script; `follow 1` fetches the plain page). No DOM, no CSS, no images, no forms |
| N6 | **Lethe's policy contract adopted natively**: Aletheia's own implementation of the behaviours Lethe's README specifies and proves — HTTPS-first with plaintext refused rather than silently downgraded, tracker hosts refused as third-party requests, a fixed low-entropy user agent, an ephemeral-by-default site-data store. These are policies, not engine code, and they port as invariants | not started |

N6 is the sense in which Lethe can genuinely be "built in" to Aletheia: not the C++ binary, but
its **security contract**, re-expressed as boot-proved kernel invariants in the same style as the
rest of this tree. The pin in Track 1 is what makes that possible to do faithfully — it names the
exact upstream revision whose behaviour the invariants must match.

## What would change this page

* If Lethe grows a `no_std` Rust core with no host-webview dependency, Track 2's N4–N6 collapse
  into vendoring that core, and this page should say so.
* If Aletheia grows a hosted POSIX personality capable of running a C++ application, Track 2
  becomes a porting problem rather than a reimplementation one. Nothing in the tree is heading
  that way today.

Until one of those happens, the accurate sentence is: *Aletheia tracks Lethe's upstream by commit
and intends to adopt its security contract natively; Lethe's engine does not run on the kernel.*
