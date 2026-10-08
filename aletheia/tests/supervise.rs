//! ADR-240: a supervised server restarts after it exits, gives up on a crash loop, and is refused
//! when another model already answers on its port. The servers are a few lines of stdlib Python
//! that answer `/v1/models` with an id and exit when told to, so no model is needed.

use aletheia::ai::supervise::{preflight, Decision, Event, Preflight, RestartPolicy, Supervised};
use std::process::Command;
use std::time::{Duration, Instant};

const FAKE: &str = r#"
import sys, json, threading, os
from http.server import BaseHTTPRequestHandler, HTTPServer
port, ident, life = int(sys.argv[1]), sys.argv[2], float(sys.argv[3])
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        b = json.dumps({"data": [{"id": ident}]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
srv = HTTPServer(("127.0.0.1", port), H)
if life > 0:
    threading.Timer(life, lambda: os._exit(3)).start()
srv.serve_forever()
"#;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fake(port: u16, id: &'static str, life: f32) -> Box<dyn Fn() -> Result<Command, String>> {
    Box::new(move || {
        let mut c = Command::new("python3");
        c.args(["-c", FAKE, &port.to_string(), id, &life.to_string()]);
        Ok(c)
    })
}

fn quick() -> RestartPolicy {
    RestartPolicy {
        budget: 2,
        window_ms: 30_000,
        backoff_start_ms: 50,
        backoff_max_ms: 200,
        ready_timeout_ms: 10_000,
    }
}

fn run_until(
    s: &mut Supervised,
    deadline: Duration,
    stop: impl Fn(&[Event]) -> bool,
) -> Vec<Event> {
    let t = Instant::now();
    let mut seen = Vec::new();
    while t.elapsed() < deadline && !stop(&seen) {
        if let Some(e) = s.poll() {
            seen.push(e);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

#[test]
fn a_server_that_exits_is_restarted_and_a_crash_loop_is_given_up() {
    let port = free_port();
    let ep = format!("http://127.0.0.1:{port}");
    // Each life ends 0.6 s after it starts: ready, exit, restart, ready, exit, ... until the
    // budget of 2 exits in 30 s is spent on the third.
    let mut s = Supervised::new(
        "fake",
        &ep,
        "fake-model",
        quick(),
        fake(port, "fake-model", 0.6),
    );
    let seen = run_until(&mut s, Duration::from_secs(30), |seen| {
        seen.iter().any(|e| {
            matches!(
                e,
                Event::Exited {
                    decision: Decision::GiveUp { .. },
                    ..
                }
            )
        })
    });
    let readies = seen
        .iter()
        .filter(|e| matches!(e, Event::Ready { .. }))
        .count();
    let exits: Vec<&Decision> = seen
        .iter()
        .filter_map(|e| match e {
            Event::Exited { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    assert!(s.gave_up(), "{seen:?}");
    assert_eq!(readies, 3, "{seen:?}");
    assert_eq!(
        exits,
        [
            &Decision::Restart { after_ms: 50 },
            &Decision::Restart { after_ms: 100 },
            &Decision::GiveUp {
                exits: 3,
                window_ms: 30_000
            }
        ],
        "{seen:?}"
    );
    assert_eq!(s.restarts(), 2);
}

#[test]
fn a_port_answered_by_another_model_is_refused_and_the_right_one_is_not_started_twice() {
    let port = free_port();
    let ep = format!("http://127.0.0.1:{port}");
    assert_eq!(preflight(&ep, "wanted"), Preflight::Free);
    let mut other = fake(port, "someone-else", 0.0)().unwrap().spawn().unwrap();
    let t = Instant::now();
    while preflight(&ep, "wanted") == Preflight::Free && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        preflight(&ep, "wanted"),
        Preflight::Occupied(vec!["someone-else".to_string()])
    );
    assert_eq!(preflight(&ep, "someone-else"), Preflight::AlreadyServing);
    let _ = other.kill();
    let _ = other.wait();
}

#[test]
fn a_server_that_never_says_who_it_is_counts_as_a_failed_start() {
    let port = free_port();
    let ep = format!("http://127.0.0.1:{port}");
    let policy = RestartPolicy {
        ready_timeout_ms: 1_500,
        ..quick()
    };
    // It serves, but as the wrong model: never READY, killed at the timeout, restarted.
    let mut s = Supervised::new("fake", &ep, "wanted", policy, fake(port, "impostor", 0.0));
    let seen = run_until(&mut s, Duration::from_secs(20), |seen| {
        seen.iter().any(|e| matches!(e, Event::NotReady { .. }))
    });
    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::NotReady {
                decision: Decision::Restart { .. },
                ..
            }
        )),
        "{seen:?}"
    );
    assert!(!s.ready());
}

/// A supervisor that dies, a SIGKILL included, takes its server with it: the kernel closes the
/// supervisor's end of the tether's pipe, which is exactly what dropping it here does.
#[cfg(unix)]
#[test]
fn a_server_stops_when_its_supervisors_pipe_closes() {
    use aletheia::ai::supervise::tethered;
    let port = free_port();
    let ep = format!("http://127.0.0.1:{port}");
    let mut tether = tethered(fake(port, "tethered", 0.0)().unwrap())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while preflight(&ep, "tethered") != Preflight::AlreadyServing
        && t.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(preflight(&ep, "tethered"), Preflight::AlreadyServing);
    drop(tether.stdin.take());
    let t = Instant::now();
    while preflight(&ep, "tethered") != Preflight::Free && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        preflight(&ep, "tethered"),
        Preflight::Free,
        "the server outlived its supervisor"
    );
    let status = tether.wait().unwrap();
    assert!(
        !status.success(),
        "the tether reports the server's end: {status:?}"
    );
}
