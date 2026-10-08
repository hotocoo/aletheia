//! A System-1 training corpus for the priority scheduler, on the System-1 decision wire
//! (ADR-186/187). Nothing here names a model or a backend: a row is the console corpus's shape, so
//! any trainer that reads `console-corpus.jsonl` reads this.
//!
//!     {"group": EPISODE, "state": SNAPSHOT, "question": {"type": "choice", "instructions": ...,
//!      "options": [{"label": "tN", "description": ...}]}, "answer": "tN", "kind": "schedule"}
//!
//! Every label is what [`PriorityScheduler::schedule_next`] actually returned: no rule is restated
//! here. An episode is a seeded workload built to confuse: few priority bands (ties everywhere),
//! endpoint acquire/wait/release chains with transitive donation, wait cycles, waiters stranded by a
//! holder that finished, advisory verdicts that reorder equals (ADR-056), and admissions mid-run.
//! The state also lists the ready pool by base priority, oldest first (ADR-231).
//! Before each dispatch with 2..=MAX_OPTIONS runnable tasks the machine is rendered and the question
//! asked. Each row is then checked: the winner is recomputed from the rendered facts alone and must
//! equal the scheduler's, so no row asks something its state does not answer.
//!
//!     cargo run --release -p kernel-core --example sched_corpus -- --rows 20000 --seed 56 > out.jsonl
//!
//! `--steps N` dispatches per episode (default 300); `--all` also keeps rows a unique highest base
//! priority answers by itself.
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::env;
use std::io::{self, BufWriter, Write};

use kernel_core::mlrisk::{Advice, Verdict};
use kernel_core::priosched::{Endpoint, Priority, PriorityScheduler};
use kernel_core::sched::{TaskId, TaskState};
use kernel_core::spine::{CapEngine, CapToken, Constraints, Scope};

const ACQ: &str = "endpoint.acquire";
const QUESTION: &str = "Which task does the priority scheduler run next?";
/// The trainer encodes state + options in 512 tokens (`laya_finetune.py` `max_len`); a row that is
/// truncated has an unlearnable label. ponytail: a char budget, not a tokenizer count.
const MAX_OPTIONS: usize = 12;
const MAX_CHARS: usize = 1400;
const MAX_LIVE: usize = 16;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

/// What the rendered snapshot says, kept beside the scheduler. The waiter lists and FIFO ages are
/// private to the scheduler, so they are mirrored from the events that change them.
#[derive(Default)]
struct Mirror {
    base: BTreeMap<u64, u8>,
    verdict: BTreeMap<u64, Verdict>,
    age: BTreeMap<u64, u64>,
    holder: BTreeMap<u64, u64>,
    waiters: BTreeMap<u64, Vec<u64>>,
    running: Option<u64>,
    tick: u64,
}

impl Mirror {
    /// A fresh FIFO age: the events that call the scheduler's `ready_enqueue`.
    fn stamp(&mut self, t: u64) {
        self.age.insert(t, self.tick);
        self.tick += 1;
    }

    /// Effective priority from the snapshot: base, raised by every (transitive) waiter on an
    /// endpoint the task holds; a task already visited contributes its base (cycles).
    fn eff(&self, t: u64, seen: &mut Vec<u64>) -> u8 {
        let base = self.base[&t];
        if seen.contains(&t) {
            return base;
        }
        seen.push(t);
        let mut best = base;
        for (ep, _) in self.holder.iter().filter(|(_, h)| **h == t) {
            for &w in self.waiters.get(ep).into_iter().flatten() {
                best = best.max(self.eff(w, seen));
            }
        }
        best
    }

    fn effective(&self, t: u64) -> u8 {
        self.eff(t, &mut Vec::new())
    }

    /// FIFO age as the scheduler will see it: the running task rejoins behind every equal.
    fn queued(&self, t: u64) -> u64 {
        if self.running == Some(t) {
            u64::MAX
        } else {
            self.age[&t]
        }
    }

    fn predict(&self, cands: &[u64]) -> u64 {
        let key = |t: &u64| (Reverse(self.effective(*t)), self.queued(*t));
        let leader = *cands
            .iter()
            .min_by_key(|t| key(t))
            .expect("cands non-empty");
        if self.verdict.get(&leader) != Some(&Verdict::Elevated) {
            return leader;
        }
        let band = self.effective(leader);
        cands
            .iter()
            .filter(|t| self.effective(**t) == band && self.verdict.get(t) == Some(&Verdict::Low))
            .min_by_key(|t| self.queued(**t))
            .copied()
            .unwrap_or(leader)
    }

    fn advice_word(&self, t: u64) -> &'static str {
        match self.verdict.get(&t) {
            Some(Verdict::Low) => "low risk",
            Some(Verdict::Elevated) => "elevated risk",
            _ => "no advice",
        }
    }

    /// The endpoint graph: who holds what, who waits in which order and at what base priority.
    fn render_state(&self) -> String {
        let mut s = match self.running {
            Some(r) => format!("running: t{r}."),
            None => "running: none.".to_string(),
        };
        let eps: Vec<u64> = self
            .holder
            .keys()
            .chain(
                self.waiters
                    .iter()
                    .filter(|(_, w)| !w.is_empty())
                    .map(|(e, _)| e),
            )
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for ep in eps {
            let holder = self
                .holder
                .get(&ep)
                .map_or("none".to_string(), |h| format!("t{h} p{}", self.base[h]));
            s.push_str(&format!(" e{ep}: holder {holder}"));
            let ws = self.waiters.get(&ep).map_or(&[][..], |w| &w[..]);
            if !ws.is_empty() {
                let list: Vec<String> = ws
                    .iter()
                    .map(|w| format!("t{w} p{}", self.base[w]))
                    .collect();
                s.push_str(&format!(", waiters {}", list.join(", ")));
            }
            s.push(';');
        }
        s
    }

    /// The ready pool as the scheduler keeps it, but keyed by BASE priority: oldest first within
    /// each band, the running task last. Donation is not applied here; the endpoint graph says it.
    fn render_pool(&self, by_age: &[u64]) -> String {
        let mut bands: BTreeMap<Reverse<u8>, Vec<String>> = BTreeMap::new();
        for &t in by_age {
            let word = match self.verdict.get(&t) {
                Some(Verdict::Low) => "low",
                Some(Verdict::Elevated) => "elevated",
                _ => "none",
            };
            let run = if self.running == Some(t) {
                " running"
            } else {
                ""
            };
            bands
                .entry(Reverse(self.base[&t]))
                .or_default()
                .push(format!("t{t} {word}{run}"));
        }
        let parts: Vec<String> = bands
            .iter()
            .map(|(Reverse(p), ts)| format!("p{p}: {}", ts.join(", ")))
            .collect();
        format!(
            "ready by base priority, oldest first: {}.",
            parts.join("; ")
        )
    }

    /// One runnable task: base priority, endpoints held, place in line among the runnable, advice.
    fn render_option(&self, t: u64, line: usize, of: usize) -> String {
        let held: Vec<String> = self
            .holder
            .iter()
            .filter(|(_, h)| **h == t)
            .map(|(e, _)| format!("e{e}"))
            .collect();
        let holds = if held.is_empty() {
            "holds nothing".to_string()
        } else {
            format!("holds {}", held.join(" "))
        };
        let when = if self.running == Some(t) {
            "running now, rejoins last".to_string()
        } else {
            format!("line {line} of {of}")
        };
        format!(
            "p{}, {holds}, {when}, {}",
            self.base[&t],
            self.advice_word(t)
        )
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// What decided a row: inheritance, the advisory tiebreak, FIFO age among equals, or (only kept
/// with `--all`) a unique highest base priority.
const KINDS: [&str; 3] = ["schedule-donation", "schedule-advice", "schedule-fifo"];

#[derive(Default)]
struct Stats {
    rows: usize,
    episodes: usize,
    /// Rows per kind; without `--all` each of [`KINDS`] is capped at an equal share of `--rows`.
    per_kind: BTreeMap<&'static str, usize>,
    quota: usize,
    skipped_budget: usize,
}

impl Stats {
    fn full(&self) -> bool {
        self.rows >= self.quota * KINDS.len()
    }
}

struct Episode<'a> {
    s: PriorityScheduler,
    m: Mirror,
    rng: &'a mut Rng,
    engine: CapEngine,
    cap: CapToken,
    bands: u64,
    n_ep: u64,
    next_id: u64,
}

impl Episode<'_> {
    fn admit(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        let base = 1 + self.rng.below(self.bands) as u8;
        let verdict = match self.rng.below(10) {
            0..=3 => Verdict::Abstain,
            4..=6 => Verdict::Low,
            _ => Verdict::Elevated,
        };
        let advice = Advice {
            verdict,
            margin: 0,
            out_of_range: false,
            degenerate: false,
        };
        self.s.admit_with_advice(TaskId(id), Priority(base), advice);
        self.m.base.insert(id, base);
        if verdict.is_decisive() {
            self.m.verdict.insert(id, verdict);
        }
        self.m.stamp(id);
    }

    fn live(&self) -> usize {
        self.m
            .base
            .keys()
            .filter(|t| self.s.state(TaskId(**t)) != Some(TaskState::Finished))
            .count()
    }

    /// The running task does one thing: acquire, wait, release, finish or keep running.
    fn act(&mut self, cur: u64) {
        let ep = self.rng.below(self.n_ep);
        let caps = [self.cap];
        match self.rng.below(100) {
            0..=34 if !self.m.holder.contains_key(&ep) => {
                self.s
                    .acquire(&self.engine, Endpoint(ep), TaskId(cur), &caps)
                    .expect("acquire a free endpoint");
                self.m.holder.insert(ep, cur);
            }
            35..=69 if self.m.holder.get(&ep).is_some_and(|h| *h != cur) => {
                self.s
                    .wait(&self.engine, Endpoint(ep), TaskId(cur), &caps)
                    .expect("wait on a held endpoint");
                self.m.waiters.entry(ep).or_default().push(cur);
                self.m.running = None;
            }
            70..=84 => {
                let held: Vec<u64> = self
                    .m
                    .holder
                    .iter()
                    .filter(|(_, h)| **h == cur)
                    .map(|(e, _)| *e)
                    .collect();
                if held.is_empty() {
                    return;
                }
                let ep = held[self.rng.below(held.len() as u64) as usize];
                match self
                    .s
                    .release(Endpoint(ep), TaskId(cur))
                    .expect("release a held endpoint")
                {
                    Some(TaskId(w)) => {
                        self.m.holder.insert(ep, w);
                        self.m
                            .waiters
                            .get_mut(&ep)
                            .expect("waiters")
                            .retain(|x| *x != w);
                        self.m.stamp(w);
                    }
                    None => {
                        self.m.holder.remove(&ep);
                    }
                }
            }
            85..=91 => {
                self.s.finish(TaskId(cur));
                self.m.holder.retain(|_, h| *h != cur);
                self.m.running = None;
            }
            _ => {}
        }
    }

    fn runnable(&self) -> Vec<u64> {
        self.m
            .base
            .keys()
            .copied()
            .filter(|t| {
                matches!(
                    self.s.state(TaskId(*t)),
                    Some(TaskState::Ready) | Some(TaskState::Running)
                )
            })
            .collect()
    }

    /// The question for this dispatch, or `None` when it would not be a fair row.
    fn row(&mut self, group: usize, cands: &[u64], st: &mut Stats) -> Option<(String, u64)> {
        if cands.len() < 2 || cands.len() > MAX_OPTIONS {
            return None;
        }
        let mut order = cands.to_vec();
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i as u64 + 1) as usize);
        }
        let mut by_age = cands.to_vec();
        by_age.sort_by_key(|t| self.m.queued(*t));
        let opts: Vec<String> = order
            .iter()
            .map(|t| {
                let line = 1 + by_age.iter().position(|x| x == t).expect("candidate");
                format!(
                    r#"{{"label": "t{t}", "description": "{}"}}"#,
                    esc(&self.m.render_option(*t, line, cands.len()))
                )
            })
            .collect();
        let state = format!("{} {}", self.m.render_state(), self.m.render_pool(&by_age));
        if state.len() + opts.iter().map(String::len).sum::<usize>() > MAX_CHARS {
            st.skipped_budget += 1;
            return None;
        }
        let line = format!(
            r#"{{"group": {group}, "state": "{}", "question": {{"type": "choice", "instructions": "{}", "options": [{}]}}, "answer": "#,
            esc(&state),
            esc(QUESTION),
            opts.join(", ")
        );
        Some((line, self.m.predict(cands)))
    }
}

fn episode(
    group: usize,
    rng: &mut Rng,
    steps: usize,
    keep_all: bool,
    out: &mut impl Write,
    st: &mut Stats,
) {
    let mut engine = CapEngine::new(0xACE, 1_000);
    let cap = engine.mint("task", ACQ, Scope::All, Constraints::none());
    let bands = 2 + rng.below(6);
    let n_ep = 2 + rng.below(7);
    let mut e = Episode {
        s: PriorityScheduler::new(ACQ),
        m: Mirror::default(),
        rng,
        engine,
        cap,
        bands,
        n_ep,
        next_id: 1,
    };
    let start = 3 + e.rng.below(10);
    for _ in 0..start {
        e.admit();
    }
    for _ in 0..steps {
        if let Some(cur) = e.m.running {
            e.act(cur);
        }
        if e.live() < MAX_LIVE && e.rng.chance(20) {
            e.admit();
        }
        let cands = e.runnable();
        let asked = e.row(group, &cands, st);
        let prev = e.m.running;
        let Some(TaskId(won)) = e.s.schedule_next() else {
            break;
        };
        if let Some((line, predicted)) = asked {
            assert_eq!(
                predicted, won,
                "snapshot does not determine the dispatch (group {group}): {line}"
            );
            let top_base = cands.iter().map(|t| e.m.base[t]).max().unwrap_or(0);
            let by_donation = e.m.base[&won] < top_base;
            let plain = cands
                .iter()
                .min_by_key(|t| (Reverse(e.m.effective(**t)), e.m.queued(**t)))
                .copied();
            let by_advice = plain != Some(won);
            let top = e.m.effective(won);
            let tied = cands.iter().filter(|t| e.m.effective(**t) == top).count() > 1;
            let kind = match (by_donation, by_advice, tied) {
                (true, _, _) => Some(KINDS[0]),
                (_, true, _) => Some(KINDS[1]),
                (_, _, true) => Some(KINDS[2]),
                _ if keep_all => Some("schedule-priority"),
                // A unique highest base priority answers itself.
                _ => None,
            };
            let room = |k: &&str| keep_all || st.per_kind.get(k).copied().unwrap_or(0) < st.quota;
            if let Some(kind) = kind.filter(room) {
                writeln!(out, r#"{line}"t{won}", "kind": "{kind}"}}"#).expect("write row");
                st.rows += 1;
                *st.per_kind.entry(kind).or_default() += 1;
            }
        }
        if let Some(p) = prev.filter(|p| *p != won) {
            e.m.stamp(p);
        }
        e.m.running = Some(won);
    }
}

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|v| {
            v.parse()
                .unwrap_or_else(|_| panic!("{name} wants a number, got {v}"))
        })
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let rows = arg(&args, "--rows", 20_000) as usize;
    let steps = arg(&args, "--steps", 300) as usize;
    let mut rng = Rng(arg(&args, "--seed", 56));
    let mut out = BufWriter::new(io::stdout().lock());
    let keep_all = args.iter().any(|a| a == "--all");
    let mut st = Stats {
        quota: if keep_all {
            rows
        } else {
            rows.div_ceil(KINDS.len())
        },
        ..Default::default()
    };
    while if keep_all { st.rows < rows } else { !st.full() } {
        episode(st.episodes, &mut rng, steps, keep_all, &mut out, &mut st);
        st.episodes += 1;
    }
    out.flush().expect("flush");
    eprintln!(
        "[sched_corpus] {} rows, {} episodes, {:?}; {} over budget skipped",
        st.rows, st.episodes, st.per_kind, st.skipped_budget
    );
}
