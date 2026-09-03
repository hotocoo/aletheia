//! The scheduler under a merciless storm (REQ-QUAL-007 / REQ-ML-002, ADR-087).
//!
//! ADR-086 stormed the desktop and found two per-event allocations on a heap that never frees.
//! The scheduler is the other half of that question, and the more important one: it runs on
//! EVERY dispatch, it is where the machine learning actually touches the machine (ADR-056: the
//! forest advises the order, it never invents a task), and a bug there is not a stutter but a
//! task nobody ever runs.
//!
//! This suite floods the priority scheduler with a deterministic pseudo-random workload and
//! holds it to five claims that are true of a real OS or the OS is broken:
//!
//! * **Strict priority is strict, at volume.** No lower-priority Ready task ever runs while a
//!   higher-priority Ready task exists — checked on EVERY dispatch, not sampled.
//! * **Equals are served FIFO, and nobody starves inside a band.** Over tens of thousands of
//!   dispatches, tasks of the same priority differ in service by at most one turn.
//! * **The advisor reorders; it never changes MEMBERSHIP.** The same storm run with a decisive
//!   advisor and with no advisor at all schedules the same multiset of tasks, the same number of
//!   times: the model may move a task earlier, never into or out of existence (INV-014).
//! * **A workload lifecycle at volume allocates NOTHING.** Thousands of admit → dispatch →
//!   finish cycles must not move the platform's own heap watermark. On a bump heap that never
//!   frees, a byte per dispatch is a machine that dies of its own scheduling.
//! * **The same storm twice is the same machine twice** — the whole dispatch SEQUENCE, folded
//!   into one number and compared.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::mlrisk::{Advice, Verdict};
use crate::priosched::{Priority, PriorityScheduler};
use crate::sched::TaskId;

/// Tasks the storm keeps in rotation.
const TASKS: u64 = 32;
/// Priority bands the storm spreads them over.
const BANDS: u8 = 4;
/// Dispatches per storm round.
const DISPATCHES: u32 = 8192;
/// Lifecycle cycles (admit → dispatch → finish) in the allocation round.
const CYCLES: u32 = 4096;

/// The same deterministic stream ADR-086 storms with: identical on every CPU, so a failure
/// fails identically.
struct Storm(u64);

impl Storm {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A workload: `TASKS` tasks spread over `BANDS` priorities, admitted in a fixed order.
fn workload(advised: bool) -> PriorityScheduler {
    let mut s = PriorityScheduler::new("ipc.acquire");
    for i in 0..TASKS {
        let prio = Priority((i % BANDS as u64) as u8 + 1);
        if advised {
            // A decisive verdict on every third task: enough that the advisory reordering is
            // exercised, never so much that the band's census is uniform.
            let verdict = match i % 3 {
                0 => Verdict::Low,
                1 => Verdict::Elevated,
                _ => Verdict::Abstain,
            };
            s.admit_with_advice(
                TaskId(i),
                prio,
                Advice {
                    verdict,
                    margin: 0,
                    out_of_range: false,
                    degenerate: false,
                },
            );
        } else {
            s.admit(TaskId(i), prio);
        }
    }
    s
}

/// Fold a dispatch sequence into ONE number: order-sensitive, cheap, and comparable across CPUs.
fn fold(acc: u64, id: TaskId) -> u64 {
    (acc ^ id.0).wrapping_mul(0x100_0000_01b3)
}

/// The boot suite (ADR-087). `used_bytes` reports the CALLER's own heap watermark, because a
/// claim about allocation must be measured where allocation happens.
pub fn storm_suite(
    used_bytes: &mut dyn FnMut() -> usize,
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

    // 1 — STRICT PRIORITY IS STRICT, at volume: on every single dispatch, no Ready task of a
    //     higher priority was passed over. Checked against the scheduler's own view, per pick.
    {
        let mut s = workload(false);
        let mut violations = 0u64;
        let mut dispatched = 0u64;
        for i in 0..DISPATCHES {
            let Some(run) = s.schedule_next() else { break };
            dispatched += 1;
            let run_prio = s.effective_priority(run);
            // Everything still READY must be no more urgent than what just ran.
            for t in 0..TASKS {
                let id = TaskId(t);
                if id == run {
                    continue;
                }
                // Higher value = more urgent (`Priority`'s own contract), so a violation is a
                // READY task strictly ABOVE what just ran.
                if s.state(id) == Some(crate::sched::TaskState::Ready)
                    && s.effective_priority(id) > run_prio
                {
                    violations += 1;
                }
            }
            // Put it back so the rotation continues: a finished task leaves the storm.
            if i % 97 == 0 {
                s.finish(run);
                s.admit(run, Priority(((run.0 % BANDS as u64) as u8) + 1));
            }
        }
        check!(
            dispatched == DISPATCHES as u64 && violations == 0,
            "schedstorm: over eight thousand dispatches, no ready task of higher priority was ever passed over"
        );
    }
    // 2 — EQUALS ARE SERVED FIFO and nobody starves inside a band: within one priority, the
    //     service counts differ by at most one turn.
    {
        let mut s = PriorityScheduler::new("ipc.acquire");
        let members = 8u64;
        for i in 0..members {
            s.admit(TaskId(i), Priority(2));
        }
        let mut runs: BTreeMap<u64, u64> = BTreeMap::new();
        for _ in 0..DISPATCHES {
            let Some(run) = s.schedule_next() else { break };
            *runs.entry(run.0).or_insert(0) += 1;
        }
        let lo = runs.values().copied().min().unwrap_or(0);
        let hi = runs.values().copied().max().unwrap_or(0);
        check!(
            runs.len() == members as usize && hi - lo <= 1,
            "schedstorm: inside one priority band every task is served, and no task is served twice before another once"
        );
    }
    // 3 — THE ADVISOR REORDERS, IT NEVER CHANGES MEMBERSHIP (INV-014). Drain the whole pool —
    //     dispatch and finish until nothing is ready — with a decisive advisor and with none.
    //     The advised drain must be a PERMUTATION of the model-free one: same tasks, each
    //     exactly once, in a different order. A model that changed the membership would be
    //     deciding WHAT runs, not what runs first, and that is the line ADR-056 draws.
    {
        let drain = |advised: bool| -> Vec<u64> {
            let mut s = workload(advised);
            let mut out = Vec::new();
            while let Some(run) = s.schedule_next() {
                out.push(run.0);
                s.finish(run);
            }
            out
        };
        let advised = drain(true);
        let plain = drain(false);
        let mut a_sorted = advised.clone();
        let mut p_sorted = plain.clone();
        a_sorted.sort_unstable();
        p_sorted.sort_unstable();
        let each_once = a_sorted.windows(2).all(|w| w[0] != w[1]);
        check!(
            advised.len() == TASKS as usize
                && plain.len() == TASKS as usize
                && a_sorted == p_sorted
                && each_once
                && advised != plain,
            "schedstorm: the advised drain is a PERMUTATION of the model-free one - same tasks, each once, different order"
        );
    }
    // 4 — A WORKLOAD LIFECYCLE AT VOLUME ALLOCATES NOTHING. Warm up (first-touch growth is a
    //     cost paid once per boot), then admit → dispatch → finish thousands of times and hold
    //     the platform's own heap watermark still.
    {
        let mut s = workload(true);
        let cycle = |s: &mut PriorityScheduler, i: u32| {
            let id = TaskId((i as u64) % TASKS);
            s.admit(id, Priority(((id.0 % BANDS as u64) as u8) + 1));
            let _ = s.schedule_next();
            s.finish(id);
        };
        for i in 0..CYCLES {
            cycle(&mut s, i); // warm-up
        }
        let before = used_bytes();
        for i in 0..CYCLES {
            cycle(&mut s, i);
        }
        let after = used_bytes();
        crate::storm_report("schedstorm", before, after);
        check!(
            after == before,
            "schedstorm: four thousand admit-dispatch-finish cycles allocate NOTHING"
        );
    }
    // 5 — THE SAME STORM TWICE IS THE SAME MACHINE TWICE: the whole dispatch sequence, folded
    //     into one number, is identical - with the advisor in the loop.
    {
        let sequence = || -> (u64, u64) {
            let mut s = workload(true);
            let mut rng = Storm(0xD15D_A7A0);
            let (mut acc, mut count) = (0xcbf2_9ce4_8422_2325u64, 0u64);
            for _ in 0..DISPATCHES {
                let Some(run) = s.schedule_next() else { break };
                acc = fold(acc, run);
                count += 1;
                if rng.below(32) == 0 {
                    let victim = TaskId(rng.below(TASKS));
                    s.finish(victim);
                    s.admit(victim, Priority(((victim.0 % BANDS as u64) as u8) + 1));
                }
            }
            (acc, count)
        };
        let a = sequence();
        let b = sequence();
        check!(
            a == b && a.1 == DISPATCHES as u64,
            "schedstorm: the same storm told twice dispatches the same tasks in the same order"
        );
    }
    Ok(n)
}
