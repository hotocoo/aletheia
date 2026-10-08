//! The OS keeps its thinking systems running (ADR-240).
//!
//! ADR-189 let `aletheiad model serve` start each selected model's server from its manifest, and
//! said what it did not do: a crashed server stayed down, and the whole session ended with it.
//! This module owns a server's life instead:
//!
//! * **Identity before trust.** A port is not a model. Before starting anything the supervisor asks
//!   the endpoint who is there: the right `serve_id` means it is already served (nothing is
//!   started twice); a different model on the port is refused by name, never shared. After a start,
//!   the server is READY only when the endpoint answers with the manifest's `serve_id`.
//! * **Bounded restarts.** A server that exits is started again after a back-off that doubles from
//!   [`RestartPolicy::backoff_start_ms`] to [`RestartPolicy::backoff_max_ms`]; more than
//!   [`RestartPolicy::budget`] exits inside [`RestartPolicy::window_ms`] is a crash loop, and the
//!   supervisor gives up and says so, rather than burning a CPU restarting a model that cannot load.
//! * **A server that stays up earns its back-off back:** one that was ready for a whole window
//!   before exiting restarts at the shortest back-off.
//!
//! * **A server never outlives its supervisor.** On Unix every server runs under a small `sh`
//!   tether that holds the supervisor's end of a pipe; when the supervisor goes away for any
//!   reason, a SIGKILL included, the pipe closes and the tether stops the server, so a dead
//!   supervisor cannot leave a model holding the port and the memory it was given.
//!
//! The decision is [`Restarts`], a value with an injected clock, so every rule above is proved on
//! the host without a model; [`Supervised`] is the process glue around it.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// How a supervised server is restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Exits tolerated inside one window before the supervisor gives up.
    pub budget: u32,
    pub window_ms: u64,
    pub backoff_start_ms: u64,
    pub backoff_max_ms: u64,
    /// How long a started server has to answer with its identity before the start counts as a
    /// failed one. A System-1 checkpoint loads in seconds; a cold System 2 can take a minute.
    pub ready_timeout_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy {
            budget: 5,
            window_ms: 60_000,
            backoff_start_ms: 500,
            backoff_max_ms: 8_000,
            ready_timeout_ms: 180_000,
        }
    }
}

/// What to do about a server that just exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Start it again after this long.
    Restart { after_ms: u64 },
    /// It exited `exits` times inside one window: a crash loop. Stop and say so.
    GiveUp { exits: u32, window_ms: u64 },
}

/// The restart ledger: exit times inside the window and the current back-off.
#[derive(Clone, Debug)]
pub struct Restarts {
    policy: RestartPolicy,
    exits: Vec<u64>,
    backoff_ms: u64,
    /// Restarts actually decided, since the supervisor began.
    pub restarts: u64,
}

impl Restarts {
    pub fn new(policy: RestartPolicy) -> Self {
        Restarts {
            policy,
            exits: Vec::new(),
            backoff_ms: policy.backoff_start_ms,
            restarts: 0,
        }
    }

    /// A server exited at `now_ms`, having been up since `up_since_ms` (`None`: it never became
    /// ready). Pure: the same calls give the same decisions.
    pub fn on_exit(&mut self, now_ms: u64, up_since_ms: Option<u64>) -> Decision {
        let window = self.policy.window_ms;
        self.exits.retain(|&t| now_ms.saturating_sub(t) < window);
        self.exits.push(now_ms);
        if self.exits.len() as u32 > self.policy.budget {
            return Decision::GiveUp {
                exits: self.exits.len() as u32,
                window_ms: window,
            };
        }
        if up_since_ms.is_some_and(|t| now_ms.saturating_sub(t) >= window) {
            self.backoff_ms = self.policy.backoff_start_ms;
        }
        let after_ms = self.backoff_ms;
        self.backoff_ms = (self.backoff_ms * 2).min(self.policy.backoff_max_ms);
        self.restarts += 1;
        Decision::Restart { after_ms }
    }
}

/// What the endpoint says before anything is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preflight {
    /// Nothing answers: start the server.
    Free,
    /// The selected model already answers there: start nothing.
    AlreadyServing,
    /// Some other model answers there: refuse, naming what was found.
    Occupied(Vec<String>),
}

/// Ask `endpoint` who is serving, against the manifest's `serve_id`.
pub fn preflight(endpoint: &str, serve_id: &str) -> Preflight {
    match super::llama::served_models(endpoint) {
        None => Preflight::Free,
        Some(_) if super::llama::serving_matches(endpoint, serve_id) => Preflight::AlreadyServing,
        Some(ids) => Preflight::Occupied(ids),
    }
}

/// Where a supervised server is in its life.
#[derive(Debug)]
enum State {
    /// Started at `since`; not yet answering with its identity.
    Starting {
        child: Child,
        since: Instant,
    },
    Ready {
        child: Child,
        since: Instant,
    },
    /// Waiting out a back-off before the next start.
    Waiting {
        until: Instant,
    },
    GaveUp,
}

/// The tether (Unix): the server's stdin is `/dev/null`; the supervisor's pipe is kept on fd 3 by
/// a watcher that stops the server when the pipe closes; the tether exits with the server's status.
#[cfg(unix)]
const TETHER: &str = r#"exec 3<&0
"$@" </dev/null &
c=$!
( read _ <&3; kill $c 2>/dev/null ) &
w=$!
wait $c
s=$?
kill $w 2>/dev/null
exit $s"#;

/// Run `c` under the tether: same program, arguments, environment and directory.
#[cfg(unix)]
pub fn tethered(c: Command) -> Command {
    let mut t = Command::new("sh");
    t.arg("-c")
        .arg(TETHER)
        .arg("sh")
        .arg(c.get_program())
        .args(c.get_args())
        .stdin(std::process::Stdio::piped());
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => t.env(k, v),
            None => t.env_remove(k),
        };
    }
    if let Some(d) = c.get_current_dir() {
        t.current_dir(d);
    }
    t
}

#[cfg(not(unix))]
pub fn tethered(c: Command) -> Command {
    c
}

/// What one [`Supervised::poll`] observed, for the caller to print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Started {
        pid: u32,
    },
    Ready {
        pid: u32,
        after_ms: u64,
    },
    Exited {
        code: Option<i32>,
        decision: Decision,
    },
    NotReady {
        after_ms: u64,
        decision: Decision,
    },
    SpawnFailed {
        error: String,
        decision: Decision,
    },
}

/// One server under supervision. `make` builds its command afresh for every start.
pub struct Supervised {
    pub id: String,
    endpoint: String,
    serve_id: String,
    make: Box<dyn Fn() -> Result<Command, String>>,
    restarts: Restarts,
    epoch: Instant,
    state: State,
}

impl Supervised {
    pub fn new(
        id: &str,
        endpoint: &str,
        serve_id: &str,
        policy: RestartPolicy,
        make: Box<dyn Fn() -> Result<Command, String>>,
    ) -> Self {
        Supervised {
            id: id.to_string(),
            endpoint: endpoint.to_string(),
            serve_id: serve_id.to_string(),
            make,
            restarts: Restarts::new(policy),
            epoch: Instant::now(),
            state: State::Waiting {
                until: Instant::now(),
            },
        }
    }

    fn ms(&self, t: Instant) -> u64 {
        t.duration_since(self.epoch).as_millis() as u64
    }

    pub fn gave_up(&self) -> bool {
        matches!(self.state, State::GaveUp)
    }

    pub fn ready(&self) -> bool {
        matches!(self.state, State::Ready { .. })
    }

    pub fn restarts(&self) -> u64 {
        self.restarts.restarts
    }

    fn after_exit(&mut self, now: Instant, up_since: Option<Instant>) -> Decision {
        let up = up_since.map(|t| self.ms(t));
        let d = self.restarts.on_exit(self.ms(now), up);
        self.state = match d {
            Decision::Restart { after_ms } => State::Waiting {
                until: now + Duration::from_millis(after_ms),
            },
            Decision::GiveUp { .. } => State::GaveUp,
        };
        d
    }

    /// Advance the server's life by one observation. Never blocks longer than one identity probe.
    pub fn poll(&mut self) -> Option<Event> {
        let now = Instant::now();
        let policy = self.restarts.policy;
        match &mut self.state {
            State::GaveUp => None,
            State::Waiting { until } => {
                if now < *until {
                    return None;
                }
                match (self.make)().and_then(|c| tethered(c).spawn().map_err(|e| e.to_string())) {
                    Ok(child) => {
                        let pid = child.id();
                        self.state = State::Starting { child, since: now };
                        Some(Event::Started { pid })
                    }
                    Err(error) => {
                        let decision = self.after_exit(now, None);
                        Some(Event::SpawnFailed { error, decision })
                    }
                }
            }
            State::Starting { child, since } => {
                let since = *since;
                if let Ok(Some(status)) = child.try_wait() {
                    let decision = self.after_exit(now, None);
                    return Some(Event::Exited {
                        code: status.code(),
                        decision,
                    });
                }
                if super::llama::serving_matches(&self.endpoint, &self.serve_id) {
                    let pid = child.id();
                    let State::Starting { child, .. } =
                        std::mem::replace(&mut self.state, State::GaveUp)
                    else {
                        unreachable!()
                    };
                    self.state = State::Ready { child, since: now };
                    return Some(Event::Ready {
                        pid,
                        after_ms: now.duration_since(since).as_millis() as u64,
                    });
                }
                let waited = now.duration_since(since).as_millis() as u64;
                if waited >= policy.ready_timeout_ms {
                    let _ = child.kill();
                    let _ = child.wait();
                    let decision = self.after_exit(now, None);
                    return Some(Event::NotReady {
                        after_ms: waited,
                        decision,
                    });
                }
                None
            }
            State::Ready { child, since } => {
                let since = *since;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let decision = self.after_exit(now, Some(since));
                        Some(Event::Exited {
                            code: status.code(),
                            decision,
                        })
                    }
                    _ => None,
                }
            }
        }
    }

    /// Stop the server, if one is running.
    pub fn stop(&mut self) {
        if let State::Starting { child, .. } | State::Ready { child, .. } = &mut self.state {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.state = State::GaveUp;
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RestartPolicy {
        RestartPolicy {
            budget: 3,
            window_ms: 10_000,
            backoff_start_ms: 100,
            backoff_max_ms: 400,
            ready_timeout_ms: 1_000,
        }
    }

    #[test]
    fn backoff_doubles_to_its_ceiling_and_a_crash_loop_gives_up() {
        let mut r = Restarts::new(policy());
        assert_eq!(r.on_exit(0, None), Decision::Restart { after_ms: 100 });
        assert_eq!(r.on_exit(1_000, None), Decision::Restart { after_ms: 200 });
        assert_eq!(r.on_exit(2_000, None), Decision::Restart { after_ms: 400 });
        assert_eq!(
            r.on_exit(3_000, None),
            Decision::GiveUp {
                exits: 4,
                window_ms: 10_000
            }
        );
        assert_eq!(r.restarts, 3);
    }

    #[test]
    fn exits_spread_wider_than_the_window_never_give_up() {
        let mut r = Restarts::new(policy());
        for i in 0..20u64 {
            let d = r.on_exit(i * 4_000, None);
            assert!(matches!(d, Decision::Restart { .. }), "exit {i}: {d:?}");
        }
    }

    #[test]
    fn a_server_that_stayed_up_a_whole_window_restarts_at_the_shortest_backoff() {
        let mut r = Restarts::new(policy());
        r.on_exit(0, None);
        r.on_exit(100, None);
        assert_eq!(r.on_exit(200, None), Decision::Restart { after_ms: 400 });
        // Up from 1 000 to 30 000: the old exits have left the window and the back-off resets.
        assert_eq!(
            r.on_exit(30_000, Some(1_000)),
            Decision::Restart { after_ms: 100 }
        );
    }
}
