//! **The advisor takes the watch** — Lethe as a resident governor (REQ-ML-003, ADR-079).
//!
//! ADR-078 built an advisor and proved it: a frozen integer model, verified at load, consulted
//! through the power contract's own named APIs. What it did not build was a *resident*. The
//! advised path ran when a test called it, over a replayed fixture, with demand that some
//! caller had declared. STATUS said so in as many words: *no live governor thread exists yet*.
//! This module closes that named gap, and closes it without loosening a single bound.
//!
//! The question a resident governor raises is not "what should the clock be" — ADR-078 answered
//! that. It is **who gets to make the machine act, and how often**. A governor that runs off the
//! timer interrupt is reachable by anything that can make the timer fire. So the contract here is
//! about the *tick*, not the clock:
//!
//! * **The cadence is authority.** A tick is admitted only if it is strictly newer than the last
//!   admitted tick and at least [`Cadence::min_gap`] beyond it. A replayed timestamp is
//!   [`TickRefusal::NotMonotone`]; a too-eager one is [`TickRefusal::TooSoon`]. Neither advances
//!   any state. A compromised or berserk timer source therefore cannot drive DVFS churn — it can
//!   only be refused, by name, into a census.
//! * **A stale window is not a window.** If more than [`Cadence::max_gap`] passed, the world moved
//!   without us and every remembered sample is now a lie about a machine that no longer exists.
//!   The governor does not guess: it resyncs, withholds the advisor until a *full*
//!   [`DEMAND_WIN`]-sample window has refilled with post-gap truth, and says how many times it
//!   did so. Withholding is the ADR-056 posture applied to time.
//! * **The work per tick is bounded and constant.** Exactly one domain is serviced per tick,
//!   round-robin. A tick is one demand read, one sensor read, one depth-3 tree walk, and at most
//!   one contract act — independent of how many domains exist. The tick path performs no
//!   allocation: the attached domain list is a fixed array claimed once, not `domain_ids()`.
//! * **Demand is measured, not declared.** [`DemandMeter`] turns busy/idle tick accounting from
//!   the scheduler into a percentage over a disjoint window, and the governor is what feeds it to
//!   the contract. Nothing else declares demand on the governor's behalf.
//! * **The ceiling outranks the advisor.** While a domain's thermal cooldown is latched, the
//!   governor observes but does not act on it. The contract would permit a raise (a cooldown gates
//!   the overclock band, not the governor range); the governor declines anyway, because raising
//!   silicon the thermal contract just clamped is how a machine ends up oscillating at its trip
//!   point. Heat wins for the whole cooldown, counted as [`Census::cooldown_holds`].
//! * **No new authority is minted.** The governor holds no grant and offers no tokens, so the
//!   nominal ceiling binds it *structurally*, not by policy. There is no code path from here into
//!   the overclock band, and that is a property of construction rather than of intent.
//! * **Re-entry is refused, not survived.** The tick runs in the timer path and shares the
//!   observer and meters with it; a nested entry is [`TickRefusal::Reentered`] via the ADR-039
//!   [`ReentryGuard`], counted and never interleaved.
//!
//! Every act still flows through `govern_one_advised`, which is the ADR-078 sweep body lifted
//! unchanged — so the resident inherits that wave's proofs whole, including the one that matters
//! most: with the advisor absent, the advised path drives the machine through the *same* clock
//! sequence as the untouched ADR-076 baseline.
//!
//! `docs/adr/ADR-079-the-advisor-takes-the-watch.md` states the decision;
//! `kernel-core/tests/lethed.rs` proves it on the host; [`lethed_suite`] proves it on every boot
//! of all three targets.

use crate::lethe::{govern_one_advised, Advisor, GovernReport, PmObserver, DEMAND_WIN};
use crate::pm::{IdleState, PmEngine, MAX_DOMAINS};
use crate::reentry::ReentryGuard;

/// Default cadence floor: consecutive ticks closer together than this are refused.
pub const DEFAULT_MIN_GAP: u64 = 1;
/// Default staleness ceiling: a gap wider than this invalidates the remembered window.
pub const DEFAULT_MAX_GAP: u64 = 64;
/// Magnitude ceiling for a demand meter's counters. Past this the pair is halved, preserving the
/// ratio; see [`DemandMeter::account`].
pub const METER_CAP: u64 = u64::MAX / 2;

/// How often the governor is willing to be run.
///
/// Both bounds are inclusive-exclusive in the same direction: a gap of exactly `min_gap` is
/// admitted, a gap of exactly `max_gap` is admitted, and `max_gap + 1` is a resync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    /// Smallest tick gap that will be admitted. Rate-limits churn.
    pub min_gap: u64,
    /// Largest tick gap whose remembered history is still believed.
    pub max_gap: u64,
}

impl Cadence {
    /// A cadence is only usable if it admits something: `min_gap` must be nonzero (a zero-gap
    /// tick is the same instant twice) and must not exceed `max_gap`.
    pub fn is_sane(&self) -> bool {
        self.min_gap >= 1 && self.min_gap <= self.max_gap
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence {
            min_gap: DEFAULT_MIN_GAP,
            max_gap: DEFAULT_MAX_GAP,
        }
    }
}

/// Why a tick did not advance the governor. Every variant names what was wrong and, where a
/// number decided it, carries that number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickRefusal {
    /// The section was already active: a nested entry, or a second CPU (ADR-039).
    Reentered,
    /// The offered timestamp was not strictly newer than the last admitted one — a replayed or
    /// rolled-back clock.
    NotMonotone { last: u64, got: u64 },
    /// The tick came sooner than the cadence floor allows.
    TooSoon { gap: u64, min: u64 },
    /// Nothing is attached, so there is nothing to service.
    NoDomains,
    /// The configured cadence cannot admit anything (`min_gap` zero, or above `max_gap`).
    BadCadence,
}

/// What one admitted tick did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickOutcome {
    /// The tick was not admitted. No state moved.
    Refused(TickRefusal),
    /// A domain was serviced.
    Serviced {
        domain: u32,
        /// Was the advisor consulted, or withheld (warm-up, resync, or cooldown)?
        consulted: bool,
        /// Did the thermal cooldown hold this domain down this tick?
        cooldown_held: bool,
        /// What the advised step did, as ADR-078 counts it.
        report: GovernReport,
    },
}

/// Exact, saturating accounting of every tick the governor was ever offered.
///
/// The identity `offered == admitted` plus every `refused_*` bucket holds at every instant, and
/// is checked as a boot invariant by [`Census::balances`]. A census that does not add up means a
/// tick went somewhere unnamed, which is exactly the thing this module exists to make impossible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    pub offered: u64,
    pub admitted: u64,
    pub refused_reentry: u64,
    pub refused_not_monotone: u64,
    pub refused_too_soon: u64,
    pub refused_no_domains: u64,
    pub refused_bad_cadence: u64,
    /// Admitted ticks that resynced because the gap exceeded `max_gap`.
    pub resyncs: u64,
    /// Admitted ticks whose advisor was withheld because the window was not yet full.
    pub warmup_steps: u64,
    /// Admitted ticks where a latched cooldown held the domain and the advisor stood down.
    pub cooldown_holds: u64,
    /// Admitted ticks that actually consulted the advisor.
    pub consulted_steps: u64,
    /// Refusals the power contract named while advice was applied. Zero by construction.
    pub pm_refusals: u64,
}

impl Census {
    /// Does every tick ever offered land in exactly one named bucket?
    pub fn balances(&self) -> bool {
        self.admitted
            .saturating_add(self.refused_reentry)
            .saturating_add(self.refused_not_monotone)
            .saturating_add(self.refused_too_soon)
            .saturating_add(self.refused_no_domains)
            .saturating_add(self.refused_bad_cadence)
            == self.offered
    }
}

/// Busy/idle accounting for one domain, converted to a demand percentage over a disjoint window.
///
/// Disjoint is the point: [`take_pct`](Self::take_pct) consumes the window, so a busy burst is
/// counted once and cannot keep inflating later windows. Counters saturate rather than wrap, so a
/// month of uptime reports a ceiling instead of a small number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemandMeter {
    busy: u64,
    total: u64,
}

impl DemandMeter {
    pub const fn new() -> Self {
        DemandMeter { busy: 0, total: 0 }
    }

    /// Add one accounting interval: `busy` ticks of work out of `busy + idle` ticks elapsed.
    ///
    /// Counters are RENORMALIZED rather than saturated. Saturation would be a silent lie: once
    /// `busy` and `total` both pin to `u64::MAX` the ratio between them is destroyed, and a
    /// fully-loaded domain reports 1%. Halving both instead keeps the ratio while the magnitude
    /// stays bounded, so a window that is never consumed degrades in precision, never in truth.
    pub fn account(&mut self, busy: u64, idle: u64) {
        let add = busy.saturating_add(idle);
        while self.total > METER_CAP.saturating_sub(add) {
            self.busy /= 2;
            self.total /= 2;
            if self.total == 0 {
                break;
            }
        }
        self.busy = self.busy.saturating_add(busy.min(add));
        self.total = self.total.saturating_add(add);
    }

    /// Consume the window and report demand in 0..=100.
    ///
    /// An empty window is 0%, not "unknown": no elapsed time means no work was asked for. The
    /// arithmetic widens before it multiplies, so the ratio survives any magnitude the counters
    /// can hold, and the result is clamped so an over-reported `busy` cannot manufacture demand
    /// above full.
    pub fn take_pct(&mut self) -> u8 {
        let pct = if self.total == 0 {
            0
        } else {
            let b = self.busy.min(self.total) as u128;
            ((b * 100) / self.total as u128).min(100) as u8
        };
        self.busy = 0;
        self.total = 0;
        pct
    }

    /// Ticks accumulated but not yet consumed.
    pub fn pending(&self) -> u64 {
        self.total
    }
}

/// Lethe, resident: the thing that runs on the clock and services one domain per tick.
///
/// It owns the observation history and the demand meters; it borrows the engine and the advisor
/// at each tick rather than holding them, so the caller keeps the power contract exactly where it
/// already lives and this type adds no second owner of machine state.
pub struct ResidentGovernor {
    guard: ReentryGuard,
    obs: PmObserver,
    ids: [u32; MAX_DOMAINS],
    meters: [DemandMeter; MAX_DOMAINS],
    /// Consecutive post-resync observations for this domain. The advisor is consulted only once
    /// this reaches `DEMAND_WIN`, because a feature derived from a half-full window describes a
    /// machine that never existed.
    fresh: [usize; MAX_DOMAINS],
    n: usize,
    cadence: Cadence,
    last_tick: Option<u64>,
    cursor: usize,
    census: Census,
}

impl ResidentGovernor {
    pub fn new(cadence: Cadence) -> Self {
        ResidentGovernor {
            guard: ReentryGuard::new(),
            obs: PmObserver::new(),
            ids: [0; MAX_DOMAINS],
            meters: [DemandMeter::new(); MAX_DOMAINS],
            fresh: [0; MAX_DOMAINS],
            n: 0,
            cadence,
            last_tick: None,
            cursor: 0,
            census: Census::default(),
        }
    }

    /// Attach a registered domain to the watch. Idempotent; refuses past [`MAX_DOMAINS`] so the
    /// fixed arrays are a bound rather than a hope.
    pub fn attach(&mut self, id: u32) -> bool {
        if let Some(slot) = self.slot_of(id) {
            let _ = slot;
            return true;
        }
        if self.n >= MAX_DOMAINS {
            return false;
        }
        self.ids[self.n] = id;
        self.meters[self.n] = DemandMeter::new();
        self.fresh[self.n] = 0;
        self.n += 1;
        true
    }

    /// Domains on the watch.
    pub fn attached(&self) -> usize {
        self.n
    }

    pub fn cadence(&self) -> Cadence {
        self.cadence
    }

    pub fn census(&self) -> Census {
        self.census
    }

    /// Re-entries refused since construction. Nonzero means a nested tick really happened, which
    /// is a kernel bug even though no state was corrupted by it.
    pub fn reentry_refusals(&self) -> usize {
        self.guard.refusals()
    }

    /// Is this domain's window full enough for the advisor to be consulted?
    pub fn is_warm(&self, id: u32) -> bool {
        self.slot_of(id)
            .map(|s| self.fresh[s] >= DEMAND_WIN)
            .unwrap_or(false)
    }

    /// Record elapsed work for a domain. Called by whatever accounts CPU time — the scheduler on
    /// every context switch, or the idle path on every entry — not by the governor itself.
    pub fn account(&mut self, id: u32, busy: u64, idle: u64) {
        if let Some(s) = self.slot_of(id) {
            self.meters[s].account(busy, idle);
        }
    }

    /// The demand the next service of this domain will report, without consuming it.
    pub fn pending_ticks(&self, id: u32) -> Option<u64> {
        self.slot_of(id).map(|s| self.meters[s].pending())
    }

    /// Forget every remembered window. What a resync does, exposed because a caller that *knows*
    /// the machine changed underneath it (a resume, a topology change) should say so rather than
    /// wait for a gap to imply it.
    pub fn resync(&mut self) {
        for f in self.fresh.iter_mut().take(self.n) {
            *f = 0;
        }
    }

    /// One tick of the watch.
    ///
    /// `sense` is called at most once per tick, for the one domain being serviced, so a sensor
    /// read costs what one read costs no matter how many domains are attached.
    pub fn tick(
        &mut self,
        pm: &mut PmEngine,
        advisor: Option<&Advisor>,
        now: u64,
        sense: impl Fn(u32) -> i32,
    ) -> TickOutcome {
        self.census.offered = self.census.offered.saturating_add(1);

        let _entered = match self.guard.enter() {
            Some(e) => e,
            None => {
                self.census.refused_reentry = self.census.refused_reentry.saturating_add(1);
                return TickOutcome::Refused(TickRefusal::Reentered);
            }
        };

        if !self.cadence.is_sane() {
            self.census.refused_bad_cadence = self.census.refused_bad_cadence.saturating_add(1);
            return TickOutcome::Refused(TickRefusal::BadCadence);
        }
        if self.n == 0 {
            self.census.refused_no_domains = self.census.refused_no_domains.saturating_add(1);
            return TickOutcome::Refused(TickRefusal::NoDomains);
        }

        let mut stale = false;
        if let Some(last) = self.last_tick {
            if now <= last {
                self.census.refused_not_monotone =
                    self.census.refused_not_monotone.saturating_add(1);
                return TickOutcome::Refused(TickRefusal::NotMonotone { last, got: now });
            }
            let gap = now - last;
            if gap < self.cadence.min_gap {
                self.census.refused_too_soon = self.census.refused_too_soon.saturating_add(1);
                return TickOutcome::Refused(TickRefusal::TooSoon {
                    gap,
                    min: self.cadence.min_gap,
                });
            }
            stale = gap > self.cadence.max_gap;
        }

        // Admitted. From here the tick always moves state forward exactly once.
        self.last_tick = Some(now);
        self.census.admitted = self.census.admitted.saturating_add(1);
        if stale {
            self.census.resyncs = self.census.resyncs.saturating_add(1);
            for f in self.fresh.iter_mut().take(self.n) {
                *f = 0;
            }
        }

        let slot = self.cursor % self.n;
        self.cursor = (self.cursor + 1) % self.n;
        let id = self.ids[slot];

        // Measured demand enters the contract before anything reads it back.
        let pct = self.meters[slot].take_pct();
        if pm.set_demand(id, pct).is_err() {
            self.census.pm_refusals = self.census.pm_refusals.saturating_add(1);
        }

        // One sensor read, for this domain, on this tick. The contract owns what a trip means.
        let temp_mc = sense(id);
        pm.report_temperature(id, temp_mc, now);

        let cooling = pm.cooldown_remaining(id, now).is_some();
        self.fresh[slot] = self.fresh[slot].saturating_add(1);
        let warm = self.fresh[slot] >= DEMAND_WIN;
        let consulted = warm && !cooling;

        if !warm {
            self.census.warmup_steps = self.census.warmup_steps.saturating_add(1);
        }
        if cooling {
            self.census.cooldown_holds = self.census.cooldown_holds.saturating_add(1);
        }

        let report = if cooling {
            // The ceiling holds. Observe so the history stays true, but take no action on a
            // domain the thermal contract just clamped — and park it if it is genuinely idle,
            // which is the one act that can only help while cooling.
            let mut r = GovernReport::default();
            let current_idx = pm.point_index(id).unwrap_or(0);
            self.obs.observe(id, pct, temp_mc, current_idx, now);
            r.steps = 1;
            if pct == 0
                && pm.idle_state(id).is_none()
                && pm.enter_idle(id, IdleState::C1, now).is_ok()
            {
                r.parks = 1;
            }
            r
        } else {
            govern_one_advised(
                pm,
                if consulted { advisor } else { None },
                &mut self.obs,
                id,
                now,
                temp_mc,
            )
        };

        if consulted {
            self.census.consulted_steps = self.census.consulted_steps.saturating_add(1);
        }
        self.census.pm_refusals = self
            .census
            .pm_refusals
            .saturating_add(report.pm_refusals as u64);

        TickOutcome::Serviced {
            domain: id,
            consulted,
            cooldown_held: cooling,
            report,
        }
    }

    fn slot_of(&self, id: u32) -> Option<usize> {
        (0..self.n).find(|&i| self.ids[i] == id)
    }
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite. Kept small on purpose: the boot heap never frees (ADR-063), so
// boot proves the promises that must hold on this silicon, and the exhaustive sweeps live in
// kernel-core/tests/lethed.rs on the host.
// ---------------------------------------------------------------------------

/// Build the four-point, two-domain engine the suite drives deterministically.
fn suite_engine() -> PmEngine {
    use crate::pm::OperatingPoint;
    let mut pm = PmEngine::new(0x5E1F_0079);
    let ladder = [
        OperatingPoint {
            khz: 600_000,
            mv: 700,
        },
        OperatingPoint {
            khz: 1_200_000,
            mv: 800,
        },
        OperatingPoint {
            khz: 1_800_000,
            mv: 900,
        },
        OperatingPoint {
            khz: 2_400_000,
            mv: 1_050,
        },
    ];
    for id in [0u32, 1u32] {
        // Nominal at 1.8 GHz (ladder index 2), so index 3 is an overclock band the resident
        // must never reach; trip at 95 C.
        let _ = pm.register_domain(id, &ladder, 1_800_000, 2_400_000, 95_000);
    }
    pm
}

/// The boot-time proof that the watch is safe to stand.
pub fn lethed_suite(
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

    let advisor = Advisor::load(crate::lethe::BUNDLED_ADVISOR).ok();
    check!(
        advisor.is_some(),
        "lethed: the resident stands the watch with a verified advisor"
    );

    // 1 - a replayed clock moves nothing. The same timestamp twice, and a rolled-back one, are
    // both refused BY NAME, and neither advances the cursor or the history.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        g.attach(1);
        let first = g.tick(&mut pm, advisor.as_ref(), 100, |_| 40_000);
        let replay = g.tick(&mut pm, advisor.as_ref(), 100, |_| 40_000);
        let rollback = g.tick(&mut pm, advisor.as_ref(), 40, |_| 40_000);
        check!(
            matches!(first, TickOutcome::Serviced { .. })
                && matches!(
                    replay,
                    TickOutcome::Refused(TickRefusal::NotMonotone {
                        last: 100,
                        got: 100
                    })
                )
                && matches!(
                    rollback,
                    TickOutcome::Refused(TickRefusal::NotMonotone { last: 100, got: 40 })
                )
                && g.census().admitted == 1
                && g.census().refused_not_monotone == 2,
            "lethed: a replayed or rolled-back tick is refused by name and moves nothing"
        );
    }

    // 2 - a too-eager clock is rate-limited. A timer that fires faster than the cadence floor
    // cannot drive churn; it can only fill a refusal bucket.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence {
            min_gap: 10,
            max_gap: 1_000,
        });
        g.attach(0);
        g.tick(&mut pm, advisor.as_ref(), 1_000, |_| 40_000);
        let eager = g.tick(&mut pm, advisor.as_ref(), 1_005, |_| 40_000);
        let ok = g.tick(&mut pm, advisor.as_ref(), 1_010, |_| 40_000);
        check!(
            matches!(
                eager,
                TickOutcome::Refused(TickRefusal::TooSoon { gap: 5, min: 10 })
            ) && matches!(ok, TickOutcome::Serviced { .. })
                && g.census().refused_too_soon == 1,
            "lethed: a tick sooner than the cadence floor is refused and rate-limits churn"
        );
    }

    // 3 - the work per tick is constant: one domain serviced, round-robin, whatever the count.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        g.attach(1);
        let mut seen = [0u32; 4];
        for (i, s) in seen.iter_mut().enumerate() {
            *s = match g.tick(&mut pm, advisor.as_ref(), 10 + i as u64, |_| 40_000) {
                TickOutcome::Serviced { domain, .. } => domain,
                _ => 9999,
            };
        }
        check!(
            seen == [0, 1, 0, 1],
            "lethed: exactly one domain is serviced per tick, round-robin over the watch"
        );
    }

    // 4 - the advisor is withheld until a full window has refilled, and consulted after.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        let mut consults = 0u32;
        for t in 1..=(DEMAND_WIN as u64 + 4) {
            g.account(0, 60, 40);
            if let TickOutcome::Serviced { consulted, .. } =
                g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000)
            {
                if consulted {
                    consults += 1;
                }
            }
        }
        check!(
            g.census().warmup_steps == DEMAND_WIN as u64 - 1 && consults == 5 && g.is_warm(0),
            "lethed: the advisor is withheld until a full window of post-resync truth exists"
        );
    }

    // 5 - a gap wider than the staleness ceiling resyncs rather than guesses.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence {
            min_gap: 1,
            max_gap: 8,
        });
        g.attach(0);
        for t in 1..=(DEMAND_WIN as u64) {
            g.account(0, 50, 50);
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
        }
        let warm_before = g.is_warm(0);
        let after_gap = g.tick(&mut pm, advisor.as_ref(), DEMAND_WIN as u64 + 100, |_| {
            40_000
        });
        check!(
            warm_before
                && g.census().resyncs == 1
                && !g.is_warm(0)
                && matches!(
                    after_gap,
                    TickOutcome::Serviced {
                        consulted: false,
                        ..
                    }
                ),
            "lethed: a stale window is resynced and the advisor withheld, never guessed through"
        );
    }

    // 6 - demand is MEASURED: what the meter accounted is what the contract sees.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        g.account(0, 75, 25);
        g.tick(&mut pm, advisor.as_ref(), 1, |_| 40_000);
        let seen = pm.demand(0);
        g.account(0, 0, 100);
        g.tick(&mut pm, advisor.as_ref(), 2, |_| 40_000);
        check!(
            seen == Some(75) && pm.demand(0) == Some(0) && g.pending_ticks(0) == Some(0),
            "lethed: demand is measured from real accounting and each window is counted once"
        );
    }

    // 7 - the ceiling outranks the advisor: a tripped domain is held, not raised.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        for t in 1..=(DEMAND_WIN as u64 + 2) {
            g.account(0, 100, 0);
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
        }
        let hot_tick = DEMAND_WIN as u64 + 3;
        g.account(0, 100, 0);
        let hot = g.tick(&mut pm, advisor.as_ref(), hot_tick, |_| 99_000);
        let idx_after = pm.point_index(0);
        g.account(0, 100, 0);
        let next = g.tick(&mut pm, advisor.as_ref(), hot_tick + 1, |_| 40_000);
        check!(
            matches!(
                hot,
                TickOutcome::Serviced {
                    cooldown_held: true,
                    consulted: false,
                    ..
                }
            ) && idx_after == Some(0)
                && matches!(
                    next,
                    TickOutcome::Serviced {
                        cooldown_held: true,
                        ..
                    }
                )
                && pm.point_index(0) == Some(0)
                && g.census().cooldown_holds == 2,
            "lethed: while the thermal cooldown is latched the governor stands down and heat wins"
        );
    }

    // 8 - no reachable point exceeds nominal: the resident mints nothing and offers nothing.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        g.attach(1);
        let (nominal, _) = pm.governor_shape(0).unwrap_or((0, 1));
        let mut ever_above = false;
        for t in 1..=200u64 {
            g.account(0, 100, 0);
            g.account(1, 100, 0);
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
            for id in [0u32, 1u32] {
                if pm.point_index(id).unwrap_or(0) > nominal {
                    ever_above = true;
                }
            }
        }
        check!(
            !ever_above,
            "lethed: the resident holds no grant, so no reachable point leaves the governor range"
        );
    }

    // 9 - a demanded domain is never parked, over a long mixed run.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence::default());
        g.attach(0);
        let mut violation = false;
        for t in 1..=300u64 {
            let busy = if (t / 17) % 2 == 0 { 90 } else { 0 };
            g.account(0, busy, 100 - busy);
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
            if pm.demand(0).unwrap_or(0) > 0 && pm.idle_state(0).is_some() {
                violation = true;
            }
        }
        check!(
            !violation,
            "lethed: demanded silicon is never left parked by the resident governor"
        );
    }

    // 10 - re-entry is refused and counted, and the section reopens afterwards.
    {
        let g = ResidentGovernor::new(Cadence::default());
        let held = g.guard.enter().expect("first entry");
        let nested = g.guard.enter();
        let refusals_during = g.reentry_refusals();
        drop(held);
        check!(
            nested.is_none() && refusals_during == 1 && g.guard.enter().is_some(),
            "lethed: a nested tick is refused and counted, never interleaved"
        );
    }

    // 11 - the census balances: every tick ever offered is in exactly one named bucket.
    {
        let mut pm = suite_engine();
        let mut g = ResidentGovernor::new(Cadence {
            min_gap: 3,
            max_gap: 20,
        });
        g.attach(0);
        let mut now = 0u64;
        for i in 0..120u64 {
            now += i % 5;
            g.account(0, i % 100, 100 - (i % 100));
            g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
        }
        let c = g.census();
        check!(
            c.balances() && c.offered == 120 && c.admitted > 0 && c.pm_refusals == 0,
            "lethed: the tick census balances exactly and the contract refused nothing"
        );
    }

    // 12 - an unattached watch refuses rather than idling silently, and a bad cadence is named.
    {
        let mut pm = suite_engine();
        let mut empty = ResidentGovernor::new(Cadence::default());
        let none = empty.tick(&mut pm, advisor.as_ref(), 1, |_| 40_000);
        let mut bad = ResidentGovernor::new(Cadence {
            min_gap: 0,
            max_gap: 10,
        });
        bad.attach(0);
        let insane = bad.tick(&mut pm, advisor.as_ref(), 1, |_| 40_000);
        check!(
            matches!(none, TickOutcome::Refused(TickRefusal::NoDomains))
                && matches!(insane, TickOutcome::Refused(TickRefusal::BadCadence))
                && empty.census().refused_no_domains == 1
                && bad.census().refused_bad_cadence == 1,
            "lethed: an empty watch and an unusable cadence are refusals with names"
        );
    }

    // 13 - the watch is bounded: attaching past MAX_DOMAINS is refused, not grown into.
    {
        let mut g = ResidentGovernor::new(Cadence::default());
        let mut all_ok = true;
        for id in 0..MAX_DOMAINS as u32 {
            all_ok &= g.attach(id);
        }
        let over = g.attach(MAX_DOMAINS as u32);
        let dup = g.attach(0);
        check!(
            all_ok && !over && dup && g.attached() == MAX_DOMAINS,
            "lethed: the watch is capacity-bounded and attaching is idempotent"
        );
    }

    Ok(n)
}
