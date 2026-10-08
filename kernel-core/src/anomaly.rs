//! A System-1 anomaly watch over the counters the kernel already keeps (ADR-242).
//!
//! The kernel counts what goes wrong: faults the supervisor contained, faults it escalated,
//! admissions the memory boundary refused, entries into memory pressure, programs reclaimed. Each
//! count is reported when someone asks (`faults`, `mlstat`), and nothing watched how fast they move.
//! A machine whose programs start faulting ten times a second looks, to every console command, like
//! a machine that once had ten faults.
//!
//! This module watches the RATE. Time is cut into one-second intervals; at each interval's end the
//! increase of every counter is compared with that counter's own baseline, an exponentially
//! weighted mean of its past increases, a fast one (weight 1/8) and a slow one (1/64), fixed point
//! x16, integers only. An increase above both `RATIO x` the larger mean and `FLOOR` after a warm-up is an anomaly: counted, and the latest kept with
//! the signal's name, the increase and the baseline it broke. The rule is a rate multiple, not a
//! deviation band: these counters are sparse event counts, and a deviation estimated from a few
//! isolated events widens the band until a real burst fits inside it (measured, ADR-242).
//!
//! * **Bounded and allocation-free.** A fixed number of signals, a fixed state per signal; an
//!   observation is a few integer operations per signal, so it can run on every idle pass.
//! * **Advisory.** It reports; it never kills, throttles or refuses. What a flagged interval should
//!   cause is a policy decision this module does not make (ADR-056's posture).
//! * **It adapts.** The baseline follows the machine, so a steady high rate stops being news, and
//!   a gap in observation is replayed as quiet intervals (capped), so a burst after a long idle is
//!   judged against a decayed, not a stale, baseline.

/// The signals watched, in report order.
pub const SIGNALS: [&str; N_SIGNALS] = [
    "faults contained",
    "faults escalated",
    "admissions refused",
    "pressure entries",
    "programs reclaimed",
];
pub const N_SIGNALS: usize = 5;

/// Baseline weights: 1/2^SHIFT (fast) and 1/2^SLOW_SHIFT (slow) of each new interval.
const SHIFT: u32 = 3;
const SLOW_SHIFT: u32 = 6;
/// Fixed-point scale of the baseline.
const ONE: u64 = 16;
/// How many times its baseline rate an interval must reach.
pub const RATIO: u64 = 4;
/// And above this many, so a quiet counter that moves by one or two is not an anomaly.
pub const FLOOR: u64 = 2;
/// Intervals a signal must have been watched before it can be flagged.
pub const WARMUP: u32 = 5;
/// Quiet intervals replayed for a gap in observation, at most.
const MAX_REPLAY: u64 = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Baseline {
    /// Follows the machine within a few intervals.
    mean: u64,
    /// Remembers its level for about a minute, so a short lull in a noisy counter does not make
    /// the noise news again.
    slow: u64,
    /// How much of each mean is real history rather than its zero start, 1.0 = `FULL`: the means
    /// are divided by these, so a baseline only a few intervals old is not biased toward zero.
    fast_seen: u64,
    slow_seen: u64,
    intervals: u32,
}

/// Fixed-point 1.0 for the bias correction.
const FULL: u64 = 1 << 16;

impl Baseline {
    /// Fold one interval's increase in.
    fn update(&mut self, inc: u64) {
        let x = inc.saturating_mul(ONE);
        let ewma = |m: u64, shift: u32| {
            if x >= m {
                m + ((x - m) >> shift)
            } else {
                m - ((m - x) >> shift)
            }
        };
        self.mean = ewma(self.mean, SHIFT);
        self.slow = ewma(self.slow, SLOW_SHIFT);
        self.fast_seen += (FULL - self.fast_seen) >> SHIFT;
        self.slow_seen += (FULL - self.slow_seen) >> SLOW_SHIFT;
        self.intervals = self.intervals.saturating_add(1);
    }

    /// The baseline level, x16: the larger of the two bias-corrected means.
    fn level(&self) -> u64 {
        let unbias = |m: u64, seen: u64| m.saturating_mul(FULL) / seen.max(1);
        unbias(self.mean, self.fast_seen).max(unbias(self.slow, self.slow_seen))
    }

    /// Whether `inc` breaks this baseline.
    fn breaks(&self, inc: u64) -> bool {
        self.intervals >= WARMUP
            && inc.saturating_mul(ONE) > RATIO.saturating_mul(self.level()).max(FLOOR * ONE)
    }
}

/// One flagged interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Index into [`SIGNALS`].
    pub signal: usize,
    /// The interval it was seen in (seconds since boot).
    pub at_secs: u64,
    /// How much the counter rose in that interval.
    pub increase: u64,
    /// The baseline mean it broke, in hundredths per interval.
    pub baseline_centi: u64,
}

/// What `mlstat` prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchStats {
    pub intervals: u64,
    pub flagged: u64,
    pub per_signal: [u64; N_SIGNALS],
    pub last: Option<Finding>,
}

/// The watch.
#[derive(Clone, Debug, Default)]
pub struct Watch {
    base: [Baseline; N_SIGNALS],
    /// Counter values at the start of the open interval, and that interval's second.
    open: Option<(u64, [u64; N_SIGNALS])>,
    stats: WatchStats,
}

impl Watch {
    pub const fn new() -> Self {
        const B: Baseline = Baseline {
            mean: 0,
            slow: 0,
            fast_seen: 0,
            slow_seen: 0,
            intervals: 0,
        };
        Watch {
            base: [B; N_SIGNALS],
            open: None,
            stats: WatchStats {
                intervals: 0,
                flagged: 0,
                per_signal: [0; N_SIGNALS],
                last: None,
            },
        }
    }

    pub fn stats(&self) -> WatchStats {
        self.stats
    }

    /// Observe the counters at `now_secs`. Closes the open interval when a new second has begun,
    /// judging and folding each signal's increase; returns how many signals were flagged.
    /// Counters that went backwards (a reset) count as no increase.
    pub fn observe(&mut self, now_secs: u64, counters: [u64; N_SIGNALS]) -> usize {
        let Some((at, start)) = self.open else {
            self.open = Some((now_secs, counters));
            return 0;
        };
        if now_secs <= at {
            return 0;
        }
        let mut flagged = 0;
        for (i, b) in self.base.iter_mut().enumerate() {
            let inc = counters[i].saturating_sub(start[i]);
            if b.breaks(inc) {
                flagged += 1;
                self.stats.per_signal[i] += 1;
                self.stats.last = Some(Finding {
                    signal: i,
                    at_secs: at,
                    increase: inc,
                    baseline_centi: b.level() * 100 / ONE,
                });
            }
            b.update(inc);
            // The seconds nobody observed were quiet ones for this counter's baseline.
            for _ in 0..(now_secs - at - 1).min(MAX_REPLAY) {
                b.update(0);
            }
        }
        self.stats.intervals += 1;
        self.stats.flagged += flagged as u64;
        self.open = Some((now_secs, counters));
        flagged
    }
}

/// The machine's own watch, fed by the console's loop on every pass (ADR-242).
pub mod resident {
    use super::{Watch, WatchStats, N_SIGNALS};
    use crate::sync::SpinLock;

    static WATCH: SpinLock<Watch> = SpinLock::new(Watch::new());

    pub fn observe(now_secs: u64, counters: [u64; N_SIGNALS]) -> usize {
        WATCH.lock().observe(now_secs, counters)
    }

    pub fn stats() -> WatchStats {
        WATCH.lock().stats()
    }
}

/// Boot suite: the detector's guarantees on deterministic streams, run on every CPU.
pub fn anomaly_suite(
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
    let quiet = [0u64; N_SIGNALS];
    // A small deterministic noise generator: 0..=3 events per interval on signal 0.
    let noisy = |t: u64| -> [u64; N_SIGNALS] {
        let mut c = quiet;
        c[0] = (0..t)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 7) % 4)
            .sum();
        c
    };

    // 1 — nothing is flagged before the warm-up, however large the first increases.
    {
        let mut w = Watch::new();
        let mut c = quiet;
        let mut hits = 0;
        for t in 0..=WARMUP as u64 {
            c[1] += 1000;
            hits += w.observe(t, c);
        }
        check!(
            hits == 0,
            "anomaly: no interval is flagged before its signal is warmed up"
        );
    }
    // 2 — stationary noise is never news.
    {
        let mut w = Watch::new();
        let mut hits = 0;
        for t in 0..600 {
            hits += w.observe(t, noisy(t));
        }
        check!(
            hits == 0 && w.stats().intervals == 599,
            "anomaly: six hundred intervals of steady noise raise nothing"
        );
    }
    // 3 — a burst after a quiet baseline is flagged in the interval it happens, on its signal only.
    {
        let mut w = Watch::new();
        let mut c = quiet;
        for t in 0..20 {
            w.observe(t, c);
        }
        // Nine events between the observations at 19 and 20: interval 19 is the one that broke.
        c[0] += 9;
        let flagged = w.observe(20, c);
        let after = w.observe(21, c);
        let last = w.stats().last;
        check!(
            flagged == 1
                && after == 0
                && last.map(|f| (f.signal, f.at_secs, f.increase)) == Some((0, 19, 9))
                && w.stats().per_signal == [1, 0, 0, 0, 0],
            "anomaly: a burst after quiet is flagged once, on its own signal, in its own interval"
        );
    }
    // 4 — sparse single events do not hide a burst: the shape the console showed (one contained
    //     fault every few seconds, then three in one interval) is flagged.
    {
        let mut w = Watch::new();
        let mut c = quiet;
        for t in 0..60u64 {
            if t % 7 == 3 || t % 11 == 5 {
                c[0] += 1;
            }
            w.observe(t, c);
        }
        let before = w.stats().per_signal[0];
        c[0] += 3;
        check!(
            before == 0 && w.observe(60, c) == 1,
            "anomaly: isolated single events are quiet, and three in one interval after them are flagged"
        );
    }
    // 5 — a single event on a quiet counter is under the floor.
    {
        let mut w = Watch::new();
        let mut c = quiet;
        for t in 0..20 {
            w.observe(t, c);
        }
        c[2] += 1;
        check!(
            w.observe(20, c) == 0,
            "anomaly: one event on a quiet counter is below the floor"
        );
    }
    // 6 — a rate that persists becomes the baseline: flagged at first, not forever.
    {
        let mut w = Watch::new();
        let mut c = quiet;
        for t in 0..20 {
            w.observe(t, c);
        }
        let mut flags = 0;
        for t in 20..120 {
            c[3] += 10;
            flags += w.observe(t, c);
        }
        let late = {
            let mut l = 0;
            for t in 120..140 {
                c[3] += 10;
                l += w.observe(t, c);
            }
            l
        };
        check!(
            flags > 0 && late == 0,
            "anomaly: a rate that persists is learned and stops being flagged"
        );
    }
    // 7 — a counter that goes backwards (a reset) is no increase, and the same stream twice gives
    //     the same verdicts.
    {
        let run = || {
            let mut w = Watch::new();
            let mut out = 0usize;
            for t in 0..200u64 {
                let mut c = noisy(t);
                c[4] = if t == 100 { 0 } else { t * 3 };
                out = out.wrapping_mul(31).wrapping_add(w.observe(t, c));
            }
            (out, w.stats())
        };
        let (a, sa) = run();
        let (b, sb) = run();
        check!(
            a == b && sa == sb,
            "anomaly: a reset counter is no increase, and the same stream twice gives the same verdicts"
        );
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suite_holds_on_the_host() {
        let mut names = alloc::vec::Vec::new();
        let n = anomaly_suite(|_, ok, name| {
            assert!(ok, "{name}");
            names.push(name);
        })
        .unwrap();
        assert_eq!(n, 7);
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn a_gap_is_replayed_as_quiet_so_the_baseline_decays() {
        let mut w = Watch::new();
        let mut c = [0u64; N_SIGNALS];
        // A busy baseline of 20 per interval...
        for t in 0..40 {
            c[0] += 20;
            w.observe(t, c);
        }
        // ...then five minutes nobody observed (replayed as 64 quiet intervals, the cap), then 30 in
        // one interval: against the baseline the quiet decayed, that is news.
        w.observe(339, c);
        c[0] += 30;
        w.observe(340, c);
        let flagged_after_gap = w.stats().per_signal[0];
        // Without the gap the same 30 would have been within the norm.
        let mut v = Watch::new();
        let mut d = [0u64; N_SIGNALS];
        for t in 0..40 {
            d[0] += 20;
            v.observe(t, d);
        }
        d[0] += 30;
        v.observe(40, d);
        assert_eq!(v.stats().per_signal[0], 0);
        assert!(flagged_after_gap >= 1);
    }
}
