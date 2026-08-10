//! Integer-only risk advice from a frozen decision forest (ADR-056).
//!
//! Aletheia's scheduler faces the same question a few million times an hour: *is this task going to
//! die if I admit it?* That is a tabular-prediction question, not a language question, so asking the
//! intelligence runtime would be the wrong instrument at any price — four orders of magnitude too
//! slow, needing floating point this kernel does not have, and not reproducible run to run. This
//! module answers it with a gradient-boosted forest compiled to a flat table of integer compares:
//! a few hundred `i32` comparisons and one `i64` accumulator, no allocation after load, no floating
//! point anywhere, and the same answer every time for the same input.
//!
//! **Advisory by construction (INV-014).** `aletheia/src/intelligence.rs` declares the intelligence
//! runtime the only probabilistic stage whose output flows through the identical downstream pipeline
//! and never bypasses it. A forest in the kernel is a *second* probabilistic thing, so it is shaped
//! so that sentence stays true:
//!
//! * it returns an ordering *hint* — never a plan, an action, a capability, or an admission verdict;
//! * every invariant and capability check holds identically whether it is loaded, absent, or wrong;
//! * with no model loaded, scheduling is bit-identical to the model-free kernel
//!   (`tests/mlrisk.rs::advice_absent_matches_model_free_order`);
//! * it **abstains** rather than guesses — inside the conformal band, or outside the feature box the
//!   forest was fitted in; and
//! * absence is *named*: [`RiskAdvisor::load`] returns a [`ModelError`] the console can print, never
//!   a silent fall-back to a default answer. This mirrors `models/aletheia-lm.toml`, which exists
//!   before its weights do so that selecting it refuses by name instead of quietly serving something
//!   else.
//!
//! The blob (`ALTM1`) is produced by the `aletheia-ml` repository, which owns the corpus, the
//! training, the calibration, and the exporter; this module only *verifies and evaluates* it. Wrong
//! magic, wrong version, wrong feature count, a feature contract that does not match the one this
//! kernel was built against, a child index out of range, or a truncated tail are each a refusal at
//! load time. A model the kernel cannot verify is a model the kernel does not run.
use alloc::vec::Vec;

use crate::mlrisk_contract::{FEATURE_CONTRACT, N_FEATURES};

/// Bytes of the fixed header; the feature-range table follows immediately.
const HEADER_LEN: usize = 88;
const NODE_LEN: usize = 16;
const MAGIC: [u8; 4] = *b"ALTM";
const VERSION: u32 = 1;
/// `feature == LEAF` marks a leaf node; its `threshold` field then holds the fixed-point leaf value.
const LEAF: i32 = -1;

/// Why a blob was refused. Every variant is a *named* refusal — the kernel never degrades silently
/// into "advise nothing" without saying which check failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelError {
    /// Fewer bytes than the fixed header.
    TooShort,
    /// The first four bytes are not `ALTM`.
    BadMagic,
    /// A format version this kernel does not implement.
    UnsupportedVersion(u32),
    /// The blob's feature count differs from the compiled-in contract.
    FeatureCount { expected: usize, found: usize },
    /// The blob's feature-contract hash differs from the one this kernel was built against: the
    /// feature *meanings* have moved even though the count matches.
    ContractMismatch,
    /// Fixed-point scale differs from the compiled-in one, so every margin would be off by a factor.
    LeafScale { expected: u32, found: u32 },
    /// An empty forest cannot advise.
    EmptyForest,
    /// The declared table sizes do not match the byte length.
    Truncated,
    /// A root or child index points outside the node table, or a split names a feature that is not
    /// in the contract.
    BadIndex,
}

/// What the model has to say about one task. `Abstain` is a first-class answer, not a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Below the operating threshold and outside the abstain band: likely to complete.
    Low,
    /// Inside the conformal band, or outside the training feature box: no opinion.
    Abstain,
    /// At or above the cost-optimal operating threshold: likely to be evicted, killed or to fail.
    Elevated,
}

impl Verdict {
    /// Whether this verdict carries an opinion the scheduler may act on.
    pub fn is_decisive(self) -> bool {
        !matches!(self, Verdict::Abstain)
    }
}

/// A verdict plus the raw fixed-point margin it came from, so a caller (or the console) can show the
/// evidence rather than only the conclusion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Advice {
    pub verdict: Verdict,
    /// Log-odds scaled by `2^LEAF_FRAC_BITS`. The kernel never converts this to a probability: that
    /// would need `exp`, and the whole point is that it does not.
    pub margin: i64,
    /// True when the input fell outside the per-feature box seen in training.
    pub out_of_range: bool,
}

/// A verified, borrowed view of an `ALTM1` blob. Holds no allocation of its own: the node table is
/// read in place from the bytes the caller owns (`include_bytes!`, or a capability-scoped file read).
#[derive(Clone, Copy, Debug)]
pub struct RiskAdvisor<'a> {
    bytes: &'a [u8],
    n_trees: usize,
    n_nodes: usize,
    leaf_frac_bits: u32,
    base_margin: i64,
    threshold_margin: i64,
    abstain_lo: i64,
    abstain_hi: i64,
    ranges_off: usize,
    roots_off: usize,
    nodes_off: usize,
}

#[inline]
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn rd_i32(b: &[u8], off: usize) -> i32 {
    rd_u32(b, off) as i32
}

#[inline]
fn rd_i64(b: &[u8], off: usize) -> i64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    i64::from_le_bytes(v)
}

impl<'a> RiskAdvisor<'a> {
    /// Verify a blob and borrow it. Every failure is a named [`ModelError`]; nothing is repaired,
    /// defaulted, or guessed.
    pub fn load(bytes: &'a [u8]) -> Result<Self, ModelError> {
        if bytes.len() < HEADER_LEN {
            return Err(ModelError::TooShort);
        }
        if bytes[0..4] != MAGIC {
            return Err(ModelError::BadMagic);
        }
        let version = rd_u32(bytes, 4);
        if version != VERSION {
            return Err(ModelError::UnsupportedVersion(version));
        }
        let n_features = rd_u32(bytes, 8) as usize;
        if n_features != N_FEATURES {
            return Err(ModelError::FeatureCount {
                expected: N_FEATURES,
                found: n_features,
            });
        }
        let n_trees = rd_u32(bytes, 12) as usize;
        let n_nodes = rd_u32(bytes, 16) as usize;
        let leaf_frac_bits = rd_u32(bytes, 20);
        if leaf_frac_bits != crate::mlrisk_contract::LEAF_FRAC_BITS {
            return Err(ModelError::LeafScale {
                expected: crate::mlrisk_contract::LEAF_FRAC_BITS,
                found: leaf_frac_bits,
            });
        }
        if bytes[56..88] != FEATURE_CONTRACT {
            return Err(ModelError::ContractMismatch);
        }
        if n_trees == 0 || n_nodes == 0 {
            return Err(ModelError::EmptyForest);
        }

        let ranges_off = HEADER_LEN;
        let roots_off = ranges_off + 8 * n_features;
        let nodes_off = roots_off + 4 * n_trees;
        let want = nodes_off
            .checked_add(NODE_LEN.checked_mul(n_nodes).ok_or(ModelError::Truncated)?)
            .ok_or(ModelError::Truncated)?;
        if bytes.len() != want {
            return Err(ModelError::Truncated);
        }

        let me = RiskAdvisor {
            bytes,
            n_trees,
            n_nodes,
            leaf_frac_bits,
            base_margin: rd_i64(bytes, 24),
            threshold_margin: rd_i64(bytes, 32),
            abstain_lo: rd_i64(bytes, 40),
            abstain_hi: rd_i64(bytes, 48),
            ranges_off,
            roots_off,
            nodes_off,
        };
        me.verify_topology()?;
        Ok(me)
    }

    /// Walk every node once at load time so evaluation can be a tight loop with no bounds surprises:
    /// each root and child index is in range, and each split names a contract feature.
    fn verify_topology(&self) -> Result<(), ModelError> {
        for t in 0..self.n_trees {
            if rd_u32(self.bytes, self.roots_off + 4 * t) as usize >= self.n_nodes {
                return Err(ModelError::BadIndex);
            }
        }
        for n in 0..self.n_nodes {
            let base = self.nodes_off + NODE_LEN * n;
            let feature = rd_i32(self.bytes, base);
            if feature == LEAF {
                continue;
            }
            if feature < 0 || feature as usize >= N_FEATURES {
                return Err(ModelError::BadIndex);
            }
            let left = rd_i32(self.bytes, base + 8);
            let right = rd_i32(self.bytes, base + 12);
            if left < 0
                || right < 0
                || left as usize >= self.n_nodes
                || right as usize >= self.n_nodes
                // A child that points at or before its parent could make evaluation loop forever.
                // The exporter emits children strictly after the parent; enforce it rather than
                // trust it, so a corrupted blob cannot hang the scheduler.
                || (left as usize) <= n
                || (right as usize) <= n
            {
                return Err(ModelError::BadIndex);
            }
        }
        Ok(())
    }

    pub fn trees(&self) -> usize {
        self.n_trees
    }

    pub fn nodes(&self) -> usize {
        self.n_nodes
    }

    pub fn leaf_frac_bits(&self) -> u32 {
        self.leaf_frac_bits
    }

    pub fn threshold_margin(&self) -> i64 {
        self.threshold_margin
    }

    pub fn abstain_band(&self) -> (i64, i64) {
        (self.abstain_lo, self.abstain_hi)
    }

    /// Inclusive `(min, max)` seen in training for feature `i`.
    pub fn feature_range(&self, i: usize) -> (i32, i32) {
        let off = self.ranges_off + 8 * i;
        (rd_i32(self.bytes, off), rd_i32(self.bytes, off + 4))
    }

    /// True when any feature is outside the box the forest was fitted in — a question this blob was
    /// never asked.
    pub fn out_of_range(&self, features: &[i32; N_FEATURES]) -> bool {
        for (i, &v) in features.iter().enumerate() {
            let (lo, hi) = self.feature_range(i);
            if v < lo || v > hi {
                return true;
            }
        }
        false
    }

    /// Sum of leaf values plus the base margin, in fixed point. Pure integer arithmetic: an `i64`
    /// accumulator cannot overflow for any forest this format can express.
    pub fn margin(&self, features: &[i32; N_FEATURES]) -> i64 {
        let mut acc = self.base_margin;
        for t in 0..self.n_trees {
            let mut n = rd_u32(self.bytes, self.roots_off + 4 * t) as usize;
            loop {
                let base = self.nodes_off + NODE_LEN * n;
                let feature = rd_i32(self.bytes, base);
                let payload = rd_i32(self.bytes, base + 4);
                if feature == LEAF {
                    acc += payload as i64;
                    break;
                }
                // The trainer's rule is `x < threshold -> left`. Features are integers at their
                // contract scale and the exporter ceiled each threshold, so this integer compare is
                // not an approximation of the float one: it is the same decision.
                n = if features[feature as usize] < payload {
                    rd_i32(self.bytes, base + 8) as usize
                } else {
                    rd_i32(self.bytes, base + 12) as usize
                };
            }
        }
        acc
    }

    /// The full three-way verdict for one feature vector.
    pub fn advise(&self, features: &[i32; N_FEATURES]) -> Advice {
        let oor = self.out_of_range(features);
        let margin = self.margin(features);
        let verdict = if oor || (margin >= self.abstain_lo && margin <= self.abstain_hi) {
            Verdict::Abstain
        } else if margin >= self.threshold_margin {
            Verdict::Elevated
        } else {
            Verdict::Low
        };
        Advice {
            verdict,
            margin,
            out_of_range: oor,
        }
    }

    /// Worst-case compares per [`Self::advise`] call — the bound a scheduler needs before it agrees
    /// to call this on a hot path. Measured by walking the table, not asserted from a parameter.
    pub fn worst_case_compares(&self) -> usize {
        let mut total = 0usize;
        for t in 0..self.n_trees {
            let root = rd_u32(self.bytes, self.roots_off + 4 * t) as usize;
            total += self.depth_of(root);
        }
        total
    }

    fn depth_of(&self, node: usize) -> usize {
        // Children are verified to sit strictly after their parent, so this recursion is bounded by
        // the node count and cannot cycle.
        let base = self.nodes_off + NODE_LEN * node;
        if rd_i32(self.bytes, base) == LEAF {
            return 1;
        }
        let l = self.depth_of(rd_i32(self.bytes, base + 8) as usize);
        let r = self.depth_of(rd_i32(self.bytes, base + 12) as usize);
        1 + if l > r { l } else { r }
    }
}

// ---------------------------------------------------------------------------
// The bundled model, and the in-kernel suite that gates it on every target.
// ---------------------------------------------------------------------------

/// The frozen forest this kernel was built with, embedded in the image (ADR-056).
///
/// `include_bytes!` rather than a boot-time file read: the blob is *part of the build*, so its bytes
/// are covered by whatever attests the image, and a kernel can never be running a model its own
/// artifact hash does not account for. Loading it from a capability-scoped file at boot is deferred
/// (ADR-056), not rejected — it needs a signature check to be worth the extra trust boundary.
///
/// Embedding is not the same as trusting: [`RiskAdvisor::load`] verifies these bytes exactly as it
/// would verify bytes off a disk, and every target *checks the result at boot* rather than assuming
/// its own image is intact.
pub const BUNDLED_MODEL: &[u8] = include_bytes!("../models/aletheia_risk.altm");

/// The trainer's own integer margins and verdicts for a sample of held-out rows, emitted by the
/// `aletheia-ml` exporter. Parity is a *test*, not a claim (ADR-056), and it is asserted here — in
/// kernel space, on the real target — as well as on the host in `tests/mlrisk.rs`.
const PARITY_FIXTURE: &str = include_str!("../models/parity_fixture.tsv");

/// One row of [`PARITY_FIXTURE`]: the input, and what the trainer says about it.
struct FixtureRow {
    x: [i32; N_FEATURES],
    margin: i64,
    verdict: Verdict,
    out_of_range: bool,
}

/// Parse one fixture line, or `None` if it is not a well-formed row. A malformed fixture is a failed
/// check (the suite counts the rows it parsed), never a silently skipped one.
fn parse_fixture_row(line: &str) -> Option<FixtureRow> {
    let mut f = line.split_whitespace();
    let margin: i64 = f.next()?.parse().ok()?;
    let verdict = match f.next()? {
        "low" => Verdict::Low,
        "abstain" => Verdict::Abstain,
        "elevated" => Verdict::Elevated,
        _ => return None,
    };
    let out_of_range = f.next()? == "1";
    let mut x = [0i32; N_FEATURES];
    for slot in x.iter_mut() {
        *slot = f.next()?.parse().ok()?;
    }
    if f.next().is_some() {
        return None; // extra columns mean the fixture and this kernel disagree about the shape
    }
    Some(FixtureRow {
        x,
        margin,
        verdict,
        out_of_range,
    })
}

/// Every parseable row of the committed fixture.
fn fixture_rows() -> Vec<FixtureRow> {
    PARITY_FIXTURE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(parse_fixture_row)
        .collect()
}

/// The number of fixture lines that *should* have parsed — so a fixture whose rows silently stopped
/// being readable fails rather than shrinking the evidence.
fn fixture_line_count() -> usize {
    PARITY_FIXTURE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .count()
}

/// A minimal, *valid* `ALTM1` blob: one tree, one split on feature 0, two leaves.
///
/// The refusal checks below mutate this rather than the 100 KiB bundled blob, so the suite costs a
/// few hundred bytes of heap per check on targets whose kernel allocator is a bump allocator. It is
/// built to load successfully first (check 17), so that every refusal after it is a refusal of a
/// *specific* corruption rather than of a blob that was never acceptable to begin with.
fn synthetic_blob() -> Vec<u8> {
    let n_trees = 1usize;
    let n_nodes = 3usize;
    let mut b = Vec::with_capacity(HEADER_LEN + 8 * N_FEATURES + 4 * n_trees + NODE_LEN * n_nodes);
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&VERSION.to_le_bytes());
    b.extend_from_slice(&(N_FEATURES as u32).to_le_bytes());
    b.extend_from_slice(&(n_trees as u32).to_le_bytes());
    b.extend_from_slice(&(n_nodes as u32).to_le_bytes());
    b.extend_from_slice(&crate::mlrisk_contract::LEAF_FRAC_BITS.to_le_bytes());
    b.extend_from_slice(&0i64.to_le_bytes()); // base margin
    b.extend_from_slice(&0i64.to_le_bytes()); // threshold margin
    b.extend_from_slice(&(-1i64).to_le_bytes()); // abstain band lo
    b.extend_from_slice(&0i64.to_le_bytes()); // abstain band hi (band = [-1, 0])
    b.extend_from_slice(&FEATURE_CONTRACT);
    debug_assert_eq!(b.len(), HEADER_LEN);
    for _ in 0..N_FEATURES {
        b.extend_from_slice(&0i32.to_le_bytes()); // range lo
        b.extend_from_slice(&100i32.to_le_bytes()); // range hi
    }
    b.extend_from_slice(&0u32.to_le_bytes()); // the single root: node 0
                                              // node 0: if feature[0] < 10 -> node 1 else node 2
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&10i32.to_le_bytes());
    b.extend_from_slice(&1i32.to_le_bytes());
    b.extend_from_slice(&2i32.to_le_bytes());
    // node 1: leaf, -8 in fixed point (decisively Low)
    b.extend_from_slice(&LEAF.to_le_bytes());
    b.extend_from_slice(&(-8i32 << crate::mlrisk_contract::LEAF_FRAC_BITS).to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    // node 2: leaf, +8 in fixed point (decisively Elevated)
    b.extend_from_slice(&LEAF.to_le_bytes());
    b.extend_from_slice(&(8i32 << crate::mlrisk_contract::LEAF_FRAC_BITS).to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b
}

/// Offset of the first node in a blob with `n_trees` trees.
fn nodes_offset(n_trees: usize) -> usize {
    HEADER_LEN + 8 * N_FEATURES + 4 * n_trees
}

/// The order a [`crate::priosched::PriorityScheduler`] runs its admitted tasks in, drained to
/// completion. Used to compare a model-free schedule against an advised one *by observation*.
fn drained_order(sched: &mut crate::priosched::PriorityScheduler) -> Vec<u64> {
    let mut order = Vec::new();
    while let Some(t) = sched.schedule_next() {
        order.push(t.0);
        sched.finish(t);
    }
    order
}

/// The in-kernel risk-advisor invariants (REQ-ML-001, ADR-056) — the VM gate for the forest.
///
/// Arch-independent, like [`crate::selftest::run`]: all three CPU targets call this and format the
/// lines with their own `kprintln!`, so the invariants and their names are defined exactly once. It
/// proves, on the real target, against the image's own embedded blob:
///
/// * the bundled model **verifies at boot** and its hot-path cost is a *measured* bound;
/// * every margin and verdict matches the trainer **exactly** (parity is not a host-only claim);
/// * every way a blob can be wrong is a **named** refusal, never a silent degradation; and
/// * the advice is **advisory**: an abstaining model schedules bit-identically to the model-free
///   kernel, and priority is never traded for risk.
///
/// `Ok(n)` = all `n` passed; `Err((idx, name))` = check `idx` failed.
pub fn mlrisk_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::priosched::{Priority, PriorityScheduler};
    use crate::sched::TaskId;

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

    // 1 — the image's own blob verifies. A kernel that cannot verify its model does not run it.
    let loaded = RiskAdvisor::load(BUNDLED_MODEL);
    check!(
        loaded.is_ok(),
        "mlrisk: the bundled forest verifies at boot"
    );
    let model = match loaded {
        Ok(m) => m,
        Err(_) => return Err((n, "mlrisk: the bundled forest verifies at boot")),
    };

    // 2 — the hot-path cost is a number the scheduler can check, not a training parameter it must
    // trust. `worst_case_compares` walks the shipped table.
    {
        let bound = model.worst_case_compares();
        check!(
            model.trees() > 0 && model.nodes() > model.trees() && bound > 0 && bound < 100_000,
            "mlrisk: the worst-case compare bound is measured from the shipped table"
        );
    }

    // 3 — the evidence is present and whole: every non-comment fixture line parsed.
    let rows = fixture_rows();
    check!(
        rows.len() >= 64 && rows.len() == fixture_line_count(),
        "mlrisk: the committed parity fixture is present, whole, and large enough to be evidence"
    );

    // 4 — parity, in kernel space: the trainer's integer margins reproduce EXACTLY here.
    {
        let mut ok = true;
        for r in rows.iter() {
            if model.margin(&r.x) != r.margin {
                ok = false;
                break;
            }
        }
        check!(ok, "mlrisk: every margin matches the trainer exactly");
    }

    // 5 — and so does every three-way verdict and range-guard flag: same numbers, same decisions.
    {
        let mut ok = true;
        for r in rows.iter() {
            let a = model.advise(&r.x);
            if a.margin != r.margin || a.verdict != r.verdict || a.out_of_range != r.out_of_range {
                ok = false;
                break;
            }
        }
        check!(
            ok,
            "mlrisk: every verdict and range guard matches the trainer exactly"
        );
    }

    // 6 — the same input gives the same answer every time. A scheduler tiebreak that drifted run to
    // run would make the whole machine irreproducible.
    {
        let mut ok = true;
        for r in rows.iter().take(32) {
            let first = model.margin(&r.x);
            for _ in 0..8 {
                if model.margin(&r.x) != first {
                    ok = false;
                }
            }
        }
        check!(ok, "mlrisk: evaluation is deterministic");
    }

    // 7 — outside the box the forest was fitted in, the kernel declines instead of extrapolating.
    {
        let mut x = rows[0].x;
        x[0] = i32::MAX;
        let a = model.advise(&x);
        check!(
            a.out_of_range && a.verdict == Verdict::Abstain,
            "mlrisk: an input outside the training box abstains instead of extrapolating"
        );
    }

    // 8 — a minimal, well-formed blob LOADS and evaluates as built, so the refusals below are
    // refusals of specific corruptions rather than of a blob that could never be accepted.
    {
        let good = synthetic_blob();
        let ok = match RiskAdvisor::load(&good) {
            Ok(m) => {
                let mut lo = [0i32; N_FEATURES];
                lo[0] = 1; // takes the left leaf: -8 << LEAF_FRAC_BITS
                let mut hi = [0i32; N_FEATURES];
                hi[0] = 50; // takes the right leaf: +8 << LEAF_FRAC_BITS
                let scale = 1i64 << crate::mlrisk_contract::LEAF_FRAC_BITS;
                m.trees() == 1
                    && m.nodes() == 3
                    && m.margin(&lo) == -8 * scale
                    && m.margin(&hi) == 8 * scale
                    && m.advise(&lo).verdict == Verdict::Low
                    && m.advise(&hi).verdict == Verdict::Elevated
            }
            Err(_) => false,
        };
        check!(
            ok,
            "mlrisk: a minimal well-formed blob loads and evaluates as built"
        );
    }

    // 9..16 — every way a blob can be wrong is a NAMED refusal (ADR-056 constraint 5).
    check!(
        RiskAdvisor::load(&[]).err() == Some(ModelError::TooShort)
            && RiskAdvisor::load(&[0u8; HEADER_LEN - 1]).err() == Some(ModelError::TooShort),
        "mlrisk: a blob shorter than the header is refused as TooShort"
    );

    {
        let mut bad = synthetic_blob();
        bad[0] = b'X';
        check!(
            RiskAdvisor::load(&bad).err() == Some(ModelError::BadMagic),
            "mlrisk: wrong magic is refused by name"
        );
    }

    {
        let mut bad = synthetic_blob();
        bad[4..8].copy_from_slice(&99u32.to_le_bytes());
        check!(
            RiskAdvisor::load(&bad).err() == Some(ModelError::UnsupportedVersion(99)),
            "mlrisk: an unsupported format version is refused by name"
        );
    }

    {
        let mut bad = synthetic_blob();
        bad[8..12].copy_from_slice(&7u32.to_le_bytes());
        check!(
            RiskAdvisor::load(&bad).err()
                == Some(ModelError::FeatureCount {
                    expected: N_FEATURES,
                    found: 7,
                }),
            "mlrisk: a feature-count mismatch is refused by name"
        );
    }

    {
        let mut bad = synthetic_blob();
        bad[20..24].copy_from_slice(&11u32.to_le_bytes());
        check!(
            matches!(
                RiskAdvisor::load(&bad).err(),
                Some(ModelError::LeafScale { .. })
            ),
            "mlrisk: a different fixed-point scale is refused by name"
        );
    }

    {
        // The same shape with different feature MEANINGS — the failure a length check cannot catch.
        let mut bad = synthetic_blob();
        bad[56] ^= 0xFF;
        check!(
            RiskAdvisor::load(&bad).err() == Some(ModelError::ContractMismatch),
            "mlrisk: a moved feature contract is refused by name, not by length"
        );
    }

    {
        let mut short = synthetic_blob();
        short.truncate(short.len() - 4);
        let mut long = synthetic_blob();
        long.extend_from_slice(&[0, 0, 0, 0]);
        check!(
            RiskAdvisor::load(&short).err() == Some(ModelError::Truncated)
                && RiskAdvisor::load(&long).err() == Some(ModelError::Truncated),
            "mlrisk: a truncated or over-long table is refused by name"
        );
    }

    {
        let mut bad = synthetic_blob();
        bad[12..16].copy_from_slice(&0u32.to_le_bytes());
        check!(
            RiskAdvisor::load(&bad).err() == Some(ModelError::EmptyForest),
            "mlrisk: an empty forest is refused by name"
        );
    }

    {
        // A child pointing at or before its parent could make evaluation loop forever: the loader
        // must refuse the blob rather than let the scheduler hang on it.
        let mut bad = synthetic_blob();
        let node0 = nodes_offset(1);
        bad[node0 + 8..node0 + 12].copy_from_slice(&0i32.to_le_bytes());
        let mut oob = synthetic_blob();
        oob[node0 + 12..node0 + 16].copy_from_slice(&9999i32.to_le_bytes());
        check!(
            RiskAdvisor::load(&bad).err() == Some(ModelError::BadIndex)
                && RiskAdvisor::load(&oob).err() == Some(ModelError::BadIndex),
            "mlrisk: a backwards or out-of-range child index is refused by name"
        );
    }

    // 18 — ADVISORY, part 1: an abstaining model schedules bit-identically to the model-free kernel.
    // Asserted by running both and comparing the observed orders, not assumed from the code shape.
    {
        let mut plain = PriorityScheduler::new("kernel.endpoint.acquire");
        let mut advised = PriorityScheduler::new("kernel.endpoint.acquire");
        for (i, r) in rows.iter().take(8).enumerate() {
            let id = TaskId(i as u64 + 1);
            plain.admit(id, Priority(5));
            let mut x = r.x;
            x[0] = i32::MAX; // force the range guard, hence Abstain
            advised.admit_with_advice(id, Priority(5), model.advise(&x));
        }
        check!(
            drained_order(&mut plain) == drained_order(&mut advised),
            "mlrisk: an abstaining model schedules bit-identically to the model-free kernel"
        );
    }

    // 19/20 — ADVISORY, part 2: a decisive verdict may reorder EQUALS, and may never outrank
    // priority. Uses fixture rows the trainer itself labelled, so the inputs are not hand-picked.
    let low = rows.iter().find(|r| r.verdict == Verdict::Low);
    let elevated = rows.iter().find(|r| r.verdict == Verdict::Elevated);
    match (low, elevated) {
        (Some(low), Some(elevated)) => {
            {
                let mut s = PriorityScheduler::new("kernel.endpoint.acquire");
                s.admit_with_advice(TaskId(1), Priority(5), model.advise(&elevated.x));
                s.admit_with_advice(TaskId(2), Priority(5), model.advise(&low.x));
                let ordered = s.risk_of(TaskId(1)) == Some(Verdict::Elevated)
                    && s.risk_of(TaskId(2)) == Some(Verdict::Low)
                    && drained_order(&mut s) == [2, 1];
                check!(
                    ordered,
                    "mlrisk: a decisive verdict reorders tasks of EQUAL priority"
                );
            }
            {
                let mut s = PriorityScheduler::new("kernel.endpoint.acquire");
                s.admit_with_advice(TaskId(1), Priority(9), model.advise(&elevated.x));
                s.admit_with_advice(TaskId(2), Priority(5), model.advise(&low.x));
                check!(
                    drained_order(&mut s) == [1, 2],
                    "mlrisk: priority is never traded for risk"
                );
            }
        }
        _ => {
            check!(
                false,
                "mlrisk: a decisive verdict reorders tasks of EQUAL priority"
            );
        }
    }

    Ok(n)
}
