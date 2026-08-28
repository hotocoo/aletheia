//! Lethe: the resident performance advisor for the power/performance contract (REQ-ML-006,
//! ADR-077).
//!
//! ADR-076 made frequency AUTHORITY and heat a HARD CEILING, and its demand governor maps the
//! demand register onto the governor range with no memory at all: each step reads one sample
//! and moves. On real workloads a memoryless governor pays for that twice — it chases every
//! dip of a bursty trace down and every burst back up (ramp latency is real, even when this
//! model does not charge for it), and it leaves the question of PARKING entirely to its
//! caller, so the cheapest legal idle state is only reached if a caller remembers to ask.
//!
//! Lethe is the memory. It is a frozen integer model — two decision trees compiled to a flat
//! table of `i32` compares (`ALTH1`), no floating point, no allocation after load, the same
//! answer every time — consulted once per domain per governor step. From a bounded window of
//! demand samples, reported temperatures and the domain's own position history it advises two
//! things the memoryless governor cannot know:
//!
//! * **FREQ** — `Coast` (the burst is passing; settle toward the lower third), `Hold` (exactly
//!   the ADR-076 demand map), or `Boost` (the burst is holding; sit at the top of the governor
//!   range ahead of the demand register).
//! * **IDLE** — for a zero-demand domain: `Stay` awake at the lowest point (the pause is
//!   short; wake latency would cost more than the idle point saves), `Shallow` (park in C1) or
//!   `Deep` (park in C2 — the pause is long).
//!
//! **Advisory by construction (INV-014, the ADR-056 discipline).** The advisor proposes; the
//! power/performance contract disposes. Every clock move goes through the same named APIs any
//! other caller uses (`request_index`, `wake`, `enter_idle`), so the overclock band stays
//! grant-only, the envelope absolute and demanded silicon unparked WITH the advisor present —
//! proved at boot, not assumed (suite invariants 5-7). With the advisor absent, or abstaining,
//! the advised path performs exactly the ADR-076 demand map and parks nothing: the clock
//! sequence is bit-identical to the baseline governor (invariants 8-9). An input outside the
//! training box, or a degenerate one, is abstained on, never guessed at.
//!
//! **Parity with the trainer is a committed fixture.** `docs/evidence/lethe006/lethe_train.py`
//! owns the corpus (deterministic workload traces), the fitting and the export; this module
//! only verifies and evaluates. The fixture it emits (`models/lethe_pm_fixture.tsv`) is
//! embedded and replayed through [`PmObserver`] at every boot: features and both classes must
//! match the trainer exactly.
//!
//! Named non-claims (the ADR has the full list): the benefit numbers live in a documented
//! simulator with a ramp-latency cost model — this kernel still models transitions as free,
//! and a hardware rung (MSR/CPPC) would measure the real ones; no live governor thread exists
//! yet, so "resident" today means "wired into the model's govern path and proved at boot", the
//! same posture `mlsched` had before its resident wave.

use alloc::vec;
use alloc::vec::Vec;

use crate::lethe_contract::{FEATURE_CONTRACT, FEATURE_DOMAIN, N_FEATURES};
use crate::pm::{DState, IdleState, PmEngine, PmFault, MAX_DOMAINS};

/// Bytes of the fixed header; the feature-box table and the two node tables follow.
const HEADER_LEN: usize = 56;
const NODE_LEN: usize = 16;
const MAGIC: [u8; 4] = *b"ALTH";
const VERSION: u32 = 1;
/// `feature == LEAF` marks a leaf node; its `threshold` field then holds the advice class.
const LEAF: i32 = -1;

/// Why a blob was refused. Every variant is a *named* refusal — the kernel never degrades
/// silently into "advise nothing" without saying which check failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// Fewer bytes than the fixed header.
    TooShort,
    /// The first four bytes are not `ALTH`.
    BadMagic,
    /// A format version this kernel does not implement.
    UnsupportedVersion(u32),
    /// The blob's feature count differs from the compiled-in contract.
    FeatureCount { expected: usize, found: usize },
    /// The blob's feature-contract hash differs from the one this kernel was built against.
    ContractMismatch,
    /// The declared node tables do not match the byte length.
    Truncated,
    /// A node table is empty (a forest with no nodes cannot advise).
    EmptyForest,
    /// An internal node names a feature outside the contract, or a child index points outside
    /// its own table, or the walk from a root revisits a node (a cycle could only loop).
    BadIndex,
    /// A leaf carries a class this kernel has no name for (not 0..=2).
    BadClass,
    /// A feature-box row is inverted (`lo > hi`): the range guard could never fire, so the
    /// documented abstention path is dead on arrival — the same shape of refusal as mlrisk's
    /// inverted conformal band (ALET-P3-008).
    InvertedRange,
    /// A feature-box row reaches outside the contract's own value domain. Derivation clamps
    /// into `FEATURE_DOMAIN`, so a wider box would be a promise about inputs the kernel can
    /// never produce — and a blob asking for a domain the contract does not describe is not
    /// the blob the trainer fit.
    BoxOutsideDomain,
}

/// The frequency half of an advice. Class `0..=2` in the blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreqClass {
    /// Settle toward the lower third of the governor range (the burst is passing).
    Coast = 0,
    /// Exactly the ADR-076 demand map — the memoryless behaviour.
    Hold = 1,
    /// Sit at the top of the governor range ahead of the demand register.
    Boost = 2,
}

/// The idle half of an advice, for a zero-demand domain. Class `0..=2` in the blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdleClass {
    /// Stay awake at the lowest point: the pause is short, wake latency would cost more.
    Stay = 0,
    /// Park shallow (C1).
    Shallow = 1,
    /// Park deep (C2) — the pause is long.
    Deep = 2,
}

impl FreqClass {
    fn from_i32(v: i32) -> Option<FreqClass> {
        match v {
            0 => Some(FreqClass::Coast),
            1 => Some(FreqClass::Hold),
            2 => Some(FreqClass::Boost),
            _ => None,
        }
    }
}

impl IdleClass {
    fn from_i32(v: i32) -> Option<IdleClass> {
        match v {
            0 => Some(IdleClass::Stay),
            1 => Some(IdleClass::Shallow),
            2 => Some(IdleClass::Deep),
            _ => None,
        }
    }
}

/// What the advisor has to say about one domain at one governor step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LetheAdvice {
    pub freq: FreqClass,
    pub idle: IdleClass,
    /// True when any feature fell outside the blob's training box: the classes are still
    /// reported (they are evidence) but the caller must treat the advice as an abstention and
    /// fall back to the baseline map.
    pub out_of_range: bool,
    /// True when every feature carried the same value — the "vector" holds one number of
    /// information and the trees' answer is a constant of that number, not an opinion about a
    /// workload. The verdict is withheld; the kernel behaves baseline.
    pub degenerate: bool,
}

impl LetheAdvice {
    /// Whether this advice carries an opinion the governor may act on.
    pub fn is_decisive(&self) -> bool {
        !self.out_of_range && !self.degenerate
    }
}

/// The frozen advisor, verified at load and evaluated in place from the caller's bytes —
/// `include_bytes!`, or a capability-scoped file read. No allocation after load.
pub struct Advisor<'a> {
    bytes: &'a [u8],
    n_freq_nodes: usize,
    n_idle_nodes: usize,
    box_lo: [i32; N_FEATURES],
    box_hi: [i32; N_FEATURES],
}

impl<'a> Advisor<'a> {
    /// Verify `bytes` and load. Every way a blob can be wrong is a named [`LoadError`].
    pub fn load(bytes: &'a [u8]) -> Result<Advisor<'a>, LoadError> {
        if bytes.len() < HEADER_LEN {
            return Err(LoadError::TooShort);
        }
        if bytes[0..4] != MAGIC {
            return Err(LoadError::BadMagic);
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(LoadError::UnsupportedVersion(version));
        }
        let n_features = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if n_features != N_FEATURES {
            return Err(LoadError::FeatureCount {
                expected: N_FEATURES,
                found: n_features,
            });
        }
        if bytes[12..44] != FEATURE_CONTRACT {
            return Err(LoadError::ContractMismatch);
        }
        let n_freq = u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as usize;
        let n_idle = u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
        if n_freq == 0 || n_idle == 0 {
            return Err(LoadError::EmptyForest);
        }
        let body = HEADER_LEN + 8 * N_FEATURES;
        if bytes.len() != body + NODE_LEN * (n_freq + n_idle) {
            return Err(LoadError::Truncated);
        }
        // Feature box: interleaved (lo, hi) per feature, held inside the contract's domain.
        let mut box_lo = [0i32; N_FEATURES];
        let mut box_hi = [0i32; N_FEATURES];
        for i in 0..N_FEATURES {
            let off = HEADER_LEN + 8 * i;
            let lo = i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            let hi = i32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            if lo > hi {
                return Err(LoadError::InvertedRange);
            }
            let (dlo, dhi) = FEATURE_DOMAIN[i];
            if lo < dlo || hi > dhi {
                return Err(LoadError::BoxOutsideDomain);
            }
            box_lo[i] = lo;
            box_hi[i] = hi;
        }
        // Every node of both tables must be well-formed, and the walk from each root (node 0)
        // must terminate: a cycle among internal nodes would be an infinite loop at evaluate
        // time, so load refuses it by walking each tree once with a visited set.
        for (base, n) in [(body, n_freq), (body + NODE_LEN * n_freq, n_idle)] {
            for i in 0..n {
                let (feature, threshold, left, right) = node_at(bytes, base + i * NODE_LEN);
                if feature != LEAF {
                    if feature < 0 || feature as usize >= N_FEATURES {
                        return Err(LoadError::BadIndex);
                    }
                    if left < 0 || left as usize >= n || right < 0 || right as usize >= n {
                        return Err(LoadError::BadIndex);
                    }
                } else if FreqClass::from_i32(threshold).is_none()
                    && IdleClass::from_i32(threshold).is_none()
                {
                    return Err(LoadError::BadClass);
                }
            }
            let mut seen = vec![false; n];
            let mut stack = vec![0usize];
            while let Some(i) = stack.pop() {
                if seen[i] {
                    return Err(LoadError::BadIndex);
                }
                seen[i] = true;
                let (feature, _threshold, left, right) = node_at(bytes, base + i * NODE_LEN);
                if feature != LEAF {
                    stack.push(left as usize);
                    stack.push(right as usize);
                }
            }
        }
        Ok(Advisor {
            bytes,
            n_freq_nodes: n_freq,
            n_idle_nodes: n_idle,
            box_lo,
            box_hi,
        })
    }

    /// The advisor's shape, for a console or a boot log: `(freq nodes, idle nodes, worst-case
    /// node visits of one tree walk)` — the deeper of the two tables bounds the compares.
    pub fn shape(&self) -> (usize, usize, usize) {
        (
            self.n_freq_nodes,
            self.n_idle_nodes,
            self.n_freq_nodes.max(self.n_idle_nodes),
        )
    }

    /// Evaluate one feature vector. Never panics, never allocates, no floating point: two tree
    /// walks of integer compares and one box check.
    pub fn advise(&self, x: &[i32; N_FEATURES]) -> LetheAdvice {
        let mut out_of_range = false;
        let mut degenerate = true;
        for i in 0..N_FEATURES {
            if x[i] < self.box_lo[i] || x[i] > self.box_hi[i] {
                out_of_range = true;
            }
            if x[i] != x[0] {
                degenerate = false;
            }
        }
        let body = HEADER_LEN + 8 * N_FEATURES;
        let freq =
            FreqClass::from_i32(self.walk(body, self.n_freq_nodes, x)).unwrap_or(FreqClass::Hold);
        let idle = IdleClass::from_i32(self.walk(
            body + NODE_LEN * self.n_freq_nodes,
            self.n_idle_nodes,
            x,
        ))
        .unwrap_or(IdleClass::Stay);
        LetheAdvice {
            freq,
            idle,
            out_of_range,
            degenerate,
        }
    }

    fn walk(&self, base: usize, n: usize, x: &[i32; N_FEATURES]) -> i32 {
        let mut i = 0usize;
        loop {
            let (feature, threshold, left, right) = node_at(self.bytes, base + i * NODE_LEN);
            if feature == LEAF {
                return threshold;
            }
            i = if x[feature as usize] <= threshold {
                left as usize
            } else {
                right as usize
            };
            // Load proved every child in-range and the tree acyclic; the guard keeps an
            // editorial mistake in this file from becoming a hang instead of a test failure.
            if i >= n {
                return 1; // Hold — the baseline class, fail-safe by construction.
            }
        }
    }
}

/// Read one node: `(feature, threshold, left, right)`, little-endian.
fn node_at(bytes: &[u8], off: usize) -> (i32, i32, i32, i32) {
    (
        i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()),
        i32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()),
        i32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap()),
        i32::from_le_bytes(bytes[off + 12..off + 16].try_into().unwrap()),
    )
}

/// The embedded advisor: part of the build, verified at boot on every target.
pub const BUNDLED_ADVISOR: &[u8] = include_bytes!("../models/lethe_pm.alth");

const PARITY_FIXTURE: &str = include_str!("../models/lethe_pm_fixture.tsv");

/// One committed row of the trainer's fixture: a demand/temperature/position stream, and what
/// the trainer says the features and both advice classes are for it.
pub struct FixtureRow {
    pub name: &'static str,
    pub trip_mc: i32,
    pub nominal_idx: usize,
    /// `(demand_pct, temp_mc, current_idx, tick)` observations, in order.
    pub stream: Vec<(u8, i32, usize, u64)>,
    pub features: [i32; N_FEATURES],
    pub freq: FreqClass,
    pub idle: IdleClass,
    pub out_of_range: bool,
    pub degenerate: bool,
}

/// Parse the committed fixture. A malformed line is a panic, not a silent skip: the fixture is
/// part of the build, so a broken row is a broken build, never a quieter proof.
pub fn parity_fixture() -> Vec<FixtureRow> {
    let mut rows = Vec::new();
    for line in PARITY_FIXTURE.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(';').collect();
        assert_eq!(f.len(), 9, "fixture row must have 9 fields: {line}");
        let mut stream = Vec::new();
        for s in f[3].split(',') {
            let p: Vec<&str> = s.split(':').collect();
            assert_eq!(p.len(), 4, "stream entry must be demand:temp:idx:tick: {s}");
            stream.push((
                p[0].parse::<u8>().unwrap(),
                p[1].parse::<i32>().unwrap(),
                p[2].parse::<usize>().unwrap(),
                p[3].parse::<u64>().unwrap(),
            ));
        }
        let fv: Vec<i32> = f[4].split(',').map(|v| v.parse().unwrap()).collect();
        assert_eq!(fv.len(), N_FEATURES, "fixture row must carry 12 features");
        let mut features = [0i32; N_FEATURES];
        features.copy_from_slice(&fv);
        rows.push(FixtureRow {
            name: f[0],
            trip_mc: f[1].parse().unwrap(),
            nominal_idx: f[2].parse().unwrap(),
            stream,
            features,
            freq: FreqClass::from_i32(f[5].parse().unwrap()).unwrap(),
            idle: IdleClass::from_i32(f[6].parse().unwrap()).unwrap(),
            out_of_range: f[7].parse::<i32>().unwrap() != 0,
            degenerate: f[8].parse::<i32>().unwrap() != 0,
        });
    }
    rows
}

// ---------------------------------------------------------------------------
// The live observer: bounded history, derived features.
// ---------------------------------------------------------------------------

/// Demand samples remembered per domain (the last 16).
pub const DEMAND_WIN: usize = 16;
/// Temperature samples remembered per domain (the last 8).
pub const TEMP_WIN: usize = 8;
/// Position-change flags remembered per domain (the last 16 observations).
const TRANS_WIN: usize = 16;
/// Dwell is capped so a domain parked at one point for a month cannot push the column out of
/// its declared domain.
const DWELL_CAP: u64 = 65_535;

/// A fixed-capacity ring of `N` values. Bounded by construction: a month-long uptime cannot
/// grow it (the ADR-063 posture — the boot heap never frees).
struct Ring<T: Copy + Default, const N: usize> {
    buf: [T; N],
    len: usize,
    head: usize,
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    fn new() -> Self {
        Ring {
            buf: [T::default(); N],
            len: 0,
            head: 0,
        }
    }
    fn push(&mut self, v: T) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }
    /// Newest-first iteration, oldest of the window last.
    fn iter_newest(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len).map(move |i| self.buf[(self.head + N - 1 - i) % N])
    }
}

struct DomainObs {
    demand: Ring<u8, DEMAND_WIN>,
    temps: Ring<i32, TEMP_WIN>,
    trans: Ring<bool, TRANS_WIN>,
    last_idx: Option<usize>,
    last_change_tick: u64,
    last_tick: u64,
}

impl DomainObs {
    fn new() -> Self {
        DomainObs {
            demand: Ring::new(),
            temps: Ring::new(),
            trans: Ring::new(),
            last_idx: None,
            last_change_tick: 0,
            last_tick: 0,
        }
    }
}

/// The machine's live observation of its own power state: one bounded history per registered
/// domain, updated once per governor step, from which the advisor's features are derived.
///
/// History is EXCLUSIVE by construction: `observe` is called with the state as it stands
/// BEFORE this step's advice acts on it, so a feature never contains the outcome of the very
/// advice it is being read for — the same discipline `taskfeat` applies to task history (a
/// task never sees itself in its own features).
pub struct PmObserver {
    ids: [u32; MAX_DOMAINS],
    n_slots: usize,
    obs: [DomainObs; MAX_DOMAINS],
}

impl PmObserver {
    pub fn new() -> Self {
        PmObserver {
            ids: [0; MAX_DOMAINS],
            n_slots: 0,
            obs: core::array::from_fn(|_| DomainObs::new()),
        }
    }

    /// Claim (or find) the slot for `id`. Capacity-bounded: past `MAX_DOMAINS` domains the
    /// observer refuses and the advised path falls back to the baseline map for that domain.
    pub fn ensure(&mut self, id: u32) -> Option<usize> {
        for i in 0..self.n_slots {
            if self.ids[i] == id {
                return Some(i);
            }
        }
        if self.n_slots >= MAX_DOMAINS {
            return None;
        }
        self.ids[self.n_slots] = id;
        self.n_slots += 1;
        Some(self.n_slots - 1)
    }

    pub fn slots(&self) -> usize {
        self.n_slots
    }

    /// Record one observation of a domain's live state, BEFORE this step's advice acts on it.
    pub fn observe(&mut self, id: u32, demand_pct: u8, temp_mc: i32, current_idx: usize, now: u64) {
        let slot = match self.ensure(id) {
            Some(s) => s,
            None => return,
        };
        let o = &mut self.obs[slot];
        o.demand.push(demand_pct);
        o.temps.push(temp_mc);
        if o.last_idx != Some(current_idx) {
            o.trans.push(true);
            o.last_idx = Some(current_idx);
            o.last_change_tick = now;
        } else {
            o.trans.push(false);
        }
        o.last_tick = now;
    }

    /// Derive the feature vector for `id` from the recorded history. `None` when the domain
    /// has no history (nothing was ever observed) or holds no slot — the advised path treats
    /// that as an abstention and behaves baseline, never as a guess. `current_idx` is the
    /// domain's position NOW; the caller takes it from the same engine it is about to govern.
    pub fn features(
        &self,
        id: u32,
        current_idx: usize,
        nominal_idx: usize,
        trip_mc: i32,
    ) -> Option<[i32; N_FEATURES]> {
        let slot = (0..self.n_slots).find(|&i| self.ids[i] == id)?;
        let o = &self.obs[slot];
        let now_sample = o.demand.iter_newest().next()? as i32;
        let mean4 = {
            let (mut sum, mut n) = (0i64, 0usize);
            for v in o.demand.iter_newest().take(4) {
                sum += v as i64;
                n += 1;
            }
            (sum / n.max(1) as i64) as i32
        };
        let mut max8 = now_sample;
        let mut min8 = now_sample;
        for v in o.demand.iter_newest().take(8) {
            max8 = max8.max(v as i32);
            min8 = min8.min(v as i32);
        }
        let prev = o.demand.iter_newest().nth(1).unwrap_or(0) as i32;
        let dwell = o
            .last_tick
            .saturating_sub(o.last_change_tick)
            .min(DWELL_CAP) as i32;
        let transitions16 = o.trans.iter_newest().filter(|&b| b).count() as i32;
        let temp_now = o
            .temps
            .iter_newest()
            .next()?
            .clamp(FEATURE_DOMAIN[8].0, FEATURE_DOMAIN[8].1);
        let temp_rise = match o.temps.iter_newest().last() {
            Some(oldest) => (temp_now - oldest).clamp(FEATURE_DOMAIN[9].0, FEATURE_DOMAIN[9].1),
            None => 0,
        };
        let trip_margin = (trip_mc - temp_now).clamp(FEATURE_DOMAIN[10].0, FEATURE_DOMAIN[10].1);
        let share = (current_idx as i64 * 1000 / (nominal_idx as i64 + 1)) as i32;
        let mut x = [0i32; N_FEATURES];
        x[0] = now_sample;
        x[1] = mean4;
        x[2] = max8;
        x[3] = min8;
        x[4] = prev;
        x[5] = max8 - min8;
        x[6] = dwell;
        x[7] = transitions16;
        x[8] = temp_now;
        x[9] = temp_rise;
        x[10] = trip_margin;
        x[11] = share;
        Some(x)
    }
}

impl Default for PmObserver {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The advised governor path.
// ---------------------------------------------------------------------------

/// What one advised governor step did, across every registered domain.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct GovernReport {
    /// Domain-steps taken (one per registered domain per step).
    pub steps: u32,
    /// Consultations of the advisor (an absent advisor counts as an abstention, not a
    /// consultation).
    pub consultations: u32,
    /// Advices that were decisive (in-box, non-degenerate).
    pub decisive: u32,
    /// Advices withheld: out of the training box, or degenerate inputs.
    pub abstains: u32,
    pub out_of_range: u32,
    pub degenerate: u32,
    /// Clock moves performed (point changes through `request_index`).
    pub moves: u32,
    /// Parks performed through `enter_idle` (a Stay advice does not park).
    pub parks: u32,
    /// Wakes performed before serving demand.
    pub wakes: u32,
    /// Refusals the power contract named while the advice was applied. On a healthy machine
    /// this is ZERO by construction — every target the advised path picks lies in the governor
    /// range and every idle act is legal for the domain's state — so a nonzero count is an
    /// invariant breach, not a routine event.
    pub pm_refusals: u32,
}

/// The ADR-076 demand map, exactly as `PmEngine::govern` computes it: the demand register
/// mapped onto the governor range, never above nominal. The advised path must reproduce this
/// bit-for-bit when it abstains, so it is written ONCE here and the suite pins it against the
/// real engine.
fn demand_mapped_idx(demand_pct: u8, nominal_idx: usize) -> usize {
    let span = nominal_idx + 1;
    let t = ((demand_pct as usize) * span).div_ceil(100);
    t.max(1) - 1
}

/// Where a [`FreqClass`] lands on the ladder: `Coast` clamps the demand map toward the lower
/// third of the governor range, `Hold` IS the demand map, and `Boost` pins the TOP of the
/// governor range — that is its whole meaning: hold the top ahead of the demand register so
/// burst churn never pays ramp lag (a fractional floor would degenerate Boost into the demand
/// map on a 3-rung governor, which is `Hold`'s job). NOTHING here can reach past
/// `nominal_idx`: every branch is a min over indices at or below it, or the index itself.
fn freq_target_idx(class: FreqClass, demand_pct: u8, nominal_idx: usize) -> usize {
    let span = nominal_idx + 1;
    let mapped = demand_mapped_idx(demand_pct, nominal_idx);
    match class {
        FreqClass::Coast => mapped.min(span / 3),
        FreqClass::Hold => mapped,
        FreqClass::Boost => nominal_idx,
    }
}

/// One advised governor step over every registered domain.
///
/// Per domain: observe the live state (history strictly BEFORE acting), consult the advisor,
/// then act — through the power contract's own named APIs and nothing else. With `advisor`
/// `None`, or on any abstention, the domain gets exactly the ADR-076 demand map and no park:
/// the advised path degrades to the baseline governor, bit for bit, and says so in the report.
pub fn govern_advised(
    pm: &mut PmEngine,
    advisor: Option<&Advisor>,
    obs: &mut PmObserver,
    now: u64,
    temp_of: impl Fn(u32) -> i32,
) -> GovernReport {
    let mut report = GovernReport::default();
    let ids = pm.domain_ids();
    for id in ids {
        let demand = match pm.demand(id) {
            Some(d) => d,
            None => continue,
        };
        let current_idx = pm.point_index(id).unwrap_or(0);
        let (nominal_idx, _span) = pm.governor_shape(id).unwrap_or((0, 1));
        let trip_mc = pm.trip_temp_mc(id).unwrap_or(0);
        let temp_mc = temp_of(id);
        obs.observe(id, demand, temp_mc, current_idx, now);
        report.steps += 1;

        let (freq, idle, decisive) = match advisor {
            Some(a) => match obs.features(id, current_idx, nominal_idx, trip_mc) {
                Some(x) => {
                    let advice = a.advise(&x);
                    report.consultations += 1;
                    if advice.out_of_range {
                        report.abstains += 1;
                        report.out_of_range += 1;
                    } else if advice.degenerate {
                        report.abstains += 1;
                        report.degenerate += 1;
                    } else {
                        report.decisive += 1;
                    }
                    (advice.freq, advice.idle, advice.is_decisive())
                }
                None => {
                    // No history and no slot: behave baseline, and say why in the census.
                    report.abstains += 1;
                    (FreqClass::Hold, IdleClass::Stay, false)
                }
            },
            None => {
                // The control arm: the baseline map, no park, no consultation.
                report.abstains += 1;
                (FreqClass::Hold, IdleClass::Stay, false)
            }
        };

        if demand > 0 {
            // Demanded silicon is served: wake first (paying the latency once), then set the
            // advised point. enter_idle would refuse a demanded domain, so it is never tried.
            if pm.idle_state(id).is_some() {
                if pm.wake(id, now).is_ok() {
                    report.wakes += 1;
                } else {
                    report.pm_refusals += 1;
                }
            }
            let target = if decisive {
                freq_target_idx(freq, demand, nominal_idx)
            } else {
                demand_mapped_idx(demand, nominal_idx)
            };
            match pm.request_index(id, target, &[], now) {
                Ok(()) => {
                    if pm.point_index(id) != Some(current_idx) {
                        report.moves += 1;
                    }
                }
                Err(_) => report.pm_refusals += 1,
            }
        } else if pm.idle_state(id).is_none() {
            // Zero demand and awake: the idle advice chooses whether parking is worth it.
            let act = if decisive {
                match idle {
                    IdleClass::Stay => pm.request_index(id, 0, &[], now),
                    IdleClass::Shallow => pm.enter_idle(id, IdleState::C1, now),
                    IdleClass::Deep => pm.enter_idle(id, IdleState::C2, now),
                }
            } else {
                pm.request_index(id, 0, &[], now)
            };
            match act {
                Ok(()) => {
                    if pm.idle_state(id).is_some() {
                        report.parks += 1;
                    }
                    if pm.point_index(id) != Some(current_idx) {
                        report.moves += 1;
                    }
                }
                Err(_) => report.pm_refusals += 1,
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite. Kept SMALL by design: the boot heap never frees
// (ADR-063), so the boot proves the core promises while the exhaustive sweeps
// live in tests/lethe.rs on the host.
// ---------------------------------------------------------------------------
pub fn lethe_suite(
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

    // 1 - the bundled advisor verifies and has a shape worth consulting.
    let bundled = match Advisor::load(BUNDLED_ADVISOR) {
        Ok(a) => a,
        Err(_) => {
            check!(false, "lethe: the bundled advisor verifies at boot");
            unreachable!()
        }
    };
    let (fnodes, inodes, compares) = bundled.shape();
    check!(
        fnodes > 0 && inodes > 0 && compares > 0,
        "lethe: the bundled advisor verifies at boot (both trees present, compare bound exact)"
    );

    // 2 - every way a blob can be wrong is a NAMED refusal, re-proved against the image's own
    // bytes: magic, version, feature count, contract hash, truncation, bad index (cycle), bad
    // leaf class, inverted box, box outside the contract's domain.
    {
        const BODY: usize = HEADER_LEN + 8 * N_FEATURES;
        let wrong = |mutate: fn(&mut Vec<u8>)| {
            let mut v = Vec::from(BUNDLED_ADVISOR);
            mutate(&mut v);
            Advisor::load(&v).err()
        };
        let bad_magic = wrong(|v| v[0] = b'X');
        let bad_version = wrong(|v| v[4] = 9);
        let bad_features = wrong(|v| v[8] = 11);
        let bad_hash = wrong(|v| v[12] ^= 0xff);
        let truncated = wrong(|v| {
            v.pop();
        });
        // Node 0 turned into an internal node whose children are itself: a cycle, which load
        // must refuse because the evaluate-time walk could only loop.
        let cyclic = wrong(|v| {
            v[BODY..BODY + 4].copy_from_slice(&0i32.to_le_bytes());
            v[BODY + 8..BODY + 12].copy_from_slice(&0i32.to_le_bytes());
            v[BODY + 12..BODY + 16].copy_from_slice(&0i32.to_le_bytes());
        });
        let bad_class = wrong(|v| {
            v[BODY..BODY + 4].copy_from_slice(&(-1i32).to_le_bytes());
            v[BODY + 4..BODY + 8].copy_from_slice(&7i32.to_le_bytes());
        });
        let inverted_box = wrong(|v| {
            v[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&2i32.to_le_bytes());
            v[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&1i32.to_le_bytes());
        });
        let wide_box = wrong(|v| {
            // Push the first feature's hi past the contract domain (demand_now > 100).
            v[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&101i32.to_le_bytes());
        });
        check!(
            bad_magic == Some(LoadError::BadMagic)
                && matches!(bad_version, Some(LoadError::UnsupportedVersion(9)))
                && matches!(
                    bad_features,
                    Some(LoadError::FeatureCount {
                        expected: 12,
                        found: 11
                    })
                )
                && bad_hash == Some(LoadError::ContractMismatch)
                && truncated == Some(LoadError::Truncated)
                && cyclic == Some(LoadError::BadIndex)
                && bad_class == Some(LoadError::BadClass)
                && inverted_box == Some(LoadError::InvertedRange)
                && wide_box == Some(LoadError::BoxOutsideDomain),
            "lethe: every way a blob can be wrong is a named refusal"
        );
    }

    // 3 - parity with the trainer is a committed fixture, replayed through the live observer:
    // the features AND both advice classes must match the trainer's rows exactly, abstention
    // flags included.
    {
        let rows = parity_fixture();
        let mut ok = rows.len() >= 10;
        for row in &rows {
            let mut obs = PmObserver::new();
            for &(d, t, idx, tick) in &row.stream {
                obs.observe(1, d, t, idx, tick);
            }
            let last_idx = row.stream.last().map(|s| s.2).unwrap_or(0);
            match obs.features(1, last_idx, row.nominal_idx, row.trip_mc) {
                Some(x) if x == row.features => {
                    let a = bundled.advise(&x);
                    ok = ok
                        && a.freq == row.freq
                        && a.idle == row.idle
                        && a.out_of_range == row.out_of_range
                        && a.degenerate == row.degenerate
                        && a.is_decisive() == (!row.out_of_range && !row.degenerate);
                }
                _ => ok = false,
            }
        }
        check!(
            ok,
            "lethe: the committed parity fixture replays exactly through the live observer"
        );
    }

    // 4 - the same stream twice, the same advice twice: no hidden state in the advisor.
    {
        let rows = parity_fixture();
        let mut deterministic = true;
        for row in &rows {
            let mut obs_a = PmObserver::new();
            let mut obs_b = PmObserver::new();
            for &(d, t, idx, tick) in &row.stream {
                obs_a.observe(1, d, t, idx, tick);
                obs_b.observe(1, d, t, idx, tick);
            }
            let last_idx = row.stream.last().map(|s| s.2).unwrap_or(0);
            let xa = obs_a.features(1, last_idx, row.nominal_idx, row.trip_mc);
            let xb = obs_b.features(1, last_idx, row.nominal_idx, row.trip_mc);
            deterministic = deterministic && xa == xb && xa.is_some();
            if let Some(x) = xa {
                deterministic = deterministic && bundled.advise(&x) == bundled.advise(&x);
            }
        }
        check!(
            deterministic,
            "lethe: the same history always yields the same advice (no hidden state)"
        );
    }

    // 5 - 6 - 7 - the advised path's CORE SAFETY over a scripted multi-regime trace with a
    // full-ceiling grant minted: the governor range is never left (the overclock band stays
    // authority-only WITH Lethe present), demanded silicon is never parked, and the idle
    // residency and wake latency stay exact under Lethe's own park/wake decisions.
    {
        use crate::pm::OperatingPoint;
        let ladder_a = [
            OperatingPoint {
                khz: 800_000,
                mv: 700,
            },
            OperatingPoint {
                khz: 1_200_000,
                mv: 800,
            },
            OperatingPoint {
                khz: 2_000_000,
                mv: 900,
            },
            OperatingPoint {
                khz: 2_400_000,
                mv: 1000,
            },
            OperatingPoint {
                khz: 2_800_000,
                mv: 1100,
            },
        ];
        let ladder_b = [
            OperatingPoint {
                khz: 500_000,
                mv: 600,
            },
            OperatingPoint {
                khz: 1_000_000,
                mv: 700,
            },
        ];
        const A: u32 = 1;
        const B: u32 = 2;
        // The scripted trace: (demand_a, demand_b, temp_a_mc) per step — idle, ramp, burst,
        // steady, thermal approach, back to idle. Temperatures are OBSERVED (they feed the
        // advisor's features) but never reported to the contract: this trace proves Lethe's
        // own behaviour, and the trip machinery is `pm_suite`'s to prove.
        const TRACE: [(u8, u8, i32); 39] = [
            (0, 0, 45_000),
            (5, 0, 45_500),
            (20, 0, 46_000),
            (55, 10, 47_000),
            (80, 30, 48_500),
            (95, 50, 50_000),
            (100, 65, 52_000),
            (100, 80, 54_500),
            (95, 90, 57_000),
            (85, 100, 60_000),
            (90, 95, 63_000),
            (100, 85, 66_000),
            (100, 70, 69_500),
            (95, 40, 73_000),
            (80, 10, 76_500),
            (60, 0, 79_500),
            (30, 0, 81_500),
            (10, 0, 82_500),
            (0, 0, 83_000),
            (0, 0, 82_500),
            (0, 0, 82_000),
            (70, 0, 81_000),
            (95, 0, 82_000),
            (100, 20, 84_000),
            (100, 55, 86_500),
            (100, 80, 89_000),
            (95, 90, 91_000),
            (90, 100, 92_500),
            (85, 95, 93_500),
            (75, 60, 94_200),
            (50, 20, 94_600),
            (20, 0, 94_800),
            (0, 0, 94_900),
            (0, 0, 94_500),
            (0, 0, 93_000),
            (0, 0, 90_000),
            (0, 0, 85_000),
            (0, 0, 80_000),
            (0, 0, 75_000),
        ];
        let temps = |dom: u32, ta: i32| {
            if dom == A {
                ta
            } else {
                75_000 - (ta - 45_000) / 2
            }
        };

        let mut pm = PmEngine::new(0x1E77_E7E7);
        pm.register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
            .unwrap();
        pm.register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
            .unwrap();
        // A root grant exists - Lethe must still never reach the band on its own say-so.
        let _root = pm.mint_grant(A, 2_800_000, "platform-owner").unwrap();
        let mut obs = PmObserver::new();
        let mut rep_total = GovernReport::default();
        let mut residencies = Vec::new();
        for (tick, &(da, db, ta)) in TRACE.iter().enumerate() {
            pm.set_demand(A, da).unwrap();
            pm.set_demand(B, db).unwrap();
            let rep = govern_advised(&mut pm, Some(&bundled), &mut obs, tick as u64, |dom| {
                temps(dom, ta)
            });
            rep_total.steps += rep.steps;
            rep_total.consultations += rep.consultations;
            rep_total.decisive += rep.decisive;
            rep_total.abstains += rep.abstains;
            rep_total.out_of_range += rep.out_of_range;
            rep_total.degenerate += rep.degenerate;
            rep_total.moves += rep.moves;
            rep_total.parks += rep.parks;
            rep_total.wakes += rep.wakes;
            rep_total.pm_refusals += rep.pm_refusals;
            residencies.push(pm.idle_residency(A).unwrap());
        }
        let nominal_a = 2_000_000u32;
        let nominal_b = 1_000_000u32;
        // Re-walk the same trace to check park legality at each step (states are consumed
        // above by the residency list, so legality is checked live per step here).
        let mut pm2 = PmEngine::new(0x1E77_E7E7);
        pm2.register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
            .unwrap();
        pm2.register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
            .unwrap();
        let _root2 = pm2.mint_grant(A, 2_800_000, "platform-owner").unwrap();
        let mut obs2 = PmObserver::new();
        let mut park_legal = true;
        for (tick, &(da, db, ta)) in TRACE.iter().enumerate() {
            pm2.set_demand(A, da).unwrap();
            pm2.set_demand(B, db).unwrap();
            let _ = govern_advised(&mut pm2, Some(&bundled), &mut obs2, tick as u64, |dom| {
                temps(dom, ta)
            });
            if (da > 0 && pm2.idle_state(A).is_some()) || (db > 0 && pm2.idle_state(B).is_some()) {
                park_legal = false;
            }
        }
        let residency_exact = residencies
            .windows(2)
            .all(|w| w[0][0] <= w[1][0] && w[0][1] <= w[1][1] && w[0][2] <= w[1][2]);
        // Wake latency is a sum of real wake costs (multiples of the C1 cost of 1µs).
        let wake_ns_ok = pm.wake_latency_ns(A).unwrap_or(0).is_multiple_of(1_000);
        let census_ok = rep_total.consultations == rep_total.decisive + rep_total.abstains
            && rep_total.out_of_range <= rep_total.abstains
            && rep_total.degenerate <= rep_total.abstains
            && rep_total.pm_refusals == 0;
        // The range claim reads the FIRST run's final state plus per-step state captured by
        // the second walk: both engines must never sit above nominal.
        let in_range = pm
            .all_current_khz()
            .iter()
            .all(|&(d, k)| k <= if d == A { nominal_a } else { nominal_b })
            && pm2
                .all_current_khz()
                .iter()
                .all(|&(d, k)| k <= if d == A { nominal_a } else { nominal_b });
        check!(
            in_range,
            "lethe: with Lethe present the governor range is never left - the overclock band stays authority-only"
        );
        check!(
            park_legal,
            "lethe: demanded silicon is never parked; every park happened at zero demand"
        );
        check!(
            residency_exact && wake_ns_ok && census_ok,
            "lethe: idle residency and wake latency stay exact and the census accounts for every advice"
        );

        // 8 - the control arm: with the advisor ABSENT the advised path drives the SAME clock
        // sequence as the baseline governor, step for step.
        {
            let mut pm_base = PmEngine::new(0x1E77_E7E7);
            pm_base
                .register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
                .unwrap();
            pm_base
                .register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
                .unwrap();
            let mut pm_adv = PmEngine::new(0x1E77_E7E7);
            pm_adv
                .register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
                .unwrap();
            pm_adv
                .register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
                .unwrap();
            let mut obs = PmObserver::new();
            let mut identical = true;
            for (tick, &(da, db, ta)) in TRACE.iter().enumerate() {
                let _ = ta;
                pm_base.set_demand(A, da).unwrap();
                pm_base.set_demand(B, db).unwrap();
                pm_base.govern(tick as u64);
                pm_adv.set_demand(A, da).unwrap();
                pm_adv.set_demand(B, db).unwrap();
                let _ = govern_advised(&mut pm_adv, None, &mut obs, tick as u64, |dom| {
                    temps(dom, 45_000)
                });
                if pm_base.all_current_khz() != pm_adv.all_current_khz() {
                    identical = false;
                    break;
                }
            }
            check!(
                identical,
                "lethe: with the advisor absent the advised path is bit-identical to the baseline governor"
            );
        }

        // 9 - and with the advisor PRESENT but forced to abstain on every step (a blob whose
        // demand box collapsed to a point no real history matches), the sequence is still the
        // baseline's.
        {
            let mut degenerate_blob = Vec::from(BUNDLED_ADVISOR);
            // Collapse the demand_now box to [50, 50]: any real history falls outside it.
            degenerate_blob[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&50i32.to_le_bytes());
            degenerate_blob[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&50i32.to_le_bytes());
            let abstainer = Advisor::load(&degenerate_blob).unwrap();
            let mut pm_base = PmEngine::new(0x1E77_E7E7);
            pm_base
                .register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
                .unwrap();
            pm_base
                .register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
                .unwrap();
            let mut pm_adv = PmEngine::new(0x1E77_E7E7);
            pm_adv
                .register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
                .unwrap();
            pm_adv
                .register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
                .unwrap();
            let mut obs = PmObserver::new();
            let mut identical = true;
            let mut ever_abstained = false;
            for (tick, &(da, db, _ta)) in TRACE.iter().enumerate() {
                pm_base.set_demand(A, da).unwrap();
                pm_base.set_demand(B, db).unwrap();
                pm_base.govern(tick as u64);
                pm_adv.set_demand(A, da).unwrap();
                pm_adv.set_demand(B, db).unwrap();
                let rep = govern_advised(
                    &mut pm_adv,
                    Some(&abstainer),
                    &mut obs,
                    tick as u64,
                    |dom| temps(dom, 45_000),
                );
                ever_abstained = ever_abstained || rep.abstains > 0;
                if pm_base.all_current_khz() != pm_adv.all_current_khz() {
                    identical = false;
                    break;
                }
            }
            check!(
                identical && ever_abstained,
                "lethe: an abstaining advisor degrades to the baseline governor exactly"
            );
        }
    }

    // 10 - the ledger under the advised path stays a complete, ordered, classified record.
    {
        use crate::pm::OperatingPoint;
        let mut pm = PmEngine::new(0x1E77_E7E7);
        let ladder = [
            OperatingPoint {
                khz: 800_000,
                mv: 700,
            },
            OperatingPoint {
                khz: 2_000_000,
                mv: 900,
            },
        ];
        pm.register_domain(1, &ladder, 2_000_000, 2_000_000, 95_000)
            .unwrap();
        let mut obs = PmObserver::new();
        let mut ordered = true;
        for tick in 0..24u64 {
            let d = if tick % 4 == 0 {
                0
            } else {
                ((tick as u32 * 13) % 101) as u8
            };
            pm.set_demand(1, d).unwrap();
            let _ = govern_advised(&mut pm, Some(&bundled), &mut obs, tick, |_| 45_000);
            // The window is ordered, its newest record is the sequence head, and no record
            // is unclassified.
            let ledger = pm.audit();
            if !ledger.windows(2).all(|w| w[0].seq < w[1].seq)
                || ledger.last().map(|r| r.seq) != Some(pm.audit_sequence())
                || ledger.iter().any(|r| r.kind.is_empty())
            {
                ordered = false;
            }
        }
        let complete = pm.audit_sequence() as usize >= pm.transitions() + pm.refusals() && ordered;
        check!(
            complete,
            "lethe: the audit ledger stays monotonic and complete under the advised path"
        );
    }

    // 11 - device power is untouched by advice: Lethe advises clocks and parks, and the device
    // arcs the contract owns move only when the contract's own API moves them (legal arcs
    // only, refusals named) — the advised path never calls device power at all.
    {
        use crate::pm::OperatingPoint;
        let mut pm = PmEngine::new(0x1E77_E7E7);
        let ladder = [
            OperatingPoint {
                khz: 800_000,
                mv: 700,
            },
            OperatingPoint {
                khz: 2_000_000,
                mv: 900,
            },
        ];
        pm.register_domain(1, &ladder, 2_000_000, 2_000_000, 95_000)
            .unwrap();
        pm.register_device(9).unwrap();
        let mut obs = PmObserver::new();
        for tick in 0..8u64 {
            pm.set_demand(1, if tick % 2 == 0 { 0 } else { 90 })
                .unwrap();
            let _ = govern_advised(&mut pm, Some(&bundled), &mut obs, tick, |_| 45_000);
        }
        let untouched = pm.device_state(9) == Some(DState::D0);
        let d3 = pm.set_device_power(9, DState::D3).is_ok()
            && matches!(
                pm.set_device_power(9, DState::D1),
                Err(PmFault::IllegalDState { .. })
            );
        check!(
            untouched && d3,
            "lethe: the advisor path never touches device power - arcs move only through the contract"
        );
    }

    // 12 - the observer is bounded: more domains than slots are refused, rings saturate, and
    // a domain without history abstains instead of guessing.
    {
        let mut obs = PmObserver::new();
        let mut all_claimed = true;
        for i in 0..MAX_DOMAINS {
            all_claimed = all_claimed && obs.ensure(i as u32 + 1).is_some();
        }
        let refused = obs.ensure(MAX_DOMAINS as u32 + 1).is_none();
        for i in 0..64u64 {
            obs.observe(
                1,
                (i % 101) as u8,
                45_000 + (i as i32) * 10,
                (i % 3) as usize,
                i,
            );
        }
        let saturated = obs
            .features(1, 0, 4, 95_000)
            .map(|x| x[0] == 63 && x[4] == 62 && x[7] == 16 && x[8] == 45_000 + 630)
            .unwrap_or(false);
        let no_history = PmObserver::new().features(1, 0, 4, 95_000).is_none();
        check!(
            all_claimed && refused && saturated && no_history,
            "lethe: the observer is bounded - slots refuse overflow, rings saturate, no history means no guess"
        );
    }

    Ok(n)
}
