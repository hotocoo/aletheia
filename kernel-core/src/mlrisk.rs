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
