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
model, and — the decisive gap — **no TCP and no TLS**. The network stack is virtio-net, ARP, DHCP
and UDPv4 (`kernel-core/src/virtionet.rs`, `arpcache.rs`, `dhcp.rs`, `udpv4.rs`). There is no TCP
module in the tree, and `grep -rn tcp kernel-core/src` returns nothing.

A browser needs, in order: TCP, TLS, HTTP, a URL/resource fetch model, a content parser, a layout
engine, and a renderer that can reach the compositor. Aletheia has the compositor, the window
manager and the surfaces (ADR-077 through ADR-136). It has none of the seven things above.

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
| N1 | **TCP** over the existing IPv4/virtio-net path: connection state machine, retransmission, windowing, teardown, all fail-closed and proved at boot like every other contract here | **not started** — the blocker for everything below |
| N2 | **TLS 1.3 client**: certificate validation against a pinned trust root, no downgrade path | not started |
| N3 | **HTTP/1.1 client** over N1+N2, bounded by construction (no unbounded response buffering on a heap that never frees — ADR-063) | not started |
| N4 | **A browser window in the desktop**: a managed window like the terminal and monitor, owning a URL/navigation state model, driven by the existing window manager and input session | not started |
| N5 | **Content**: a bounded, fail-closed subset renderer into the window's `TextGrid` / compositor surface. This is where "a browser" starts being a real word | not started |
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
