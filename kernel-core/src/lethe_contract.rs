//! The Lethe feature contract (REQ-ML-006, ADR-077).
//!
//! Lethe — the resident performance advisor for the power/performance contract (ADR-076) — is a
//! frozen integer model, and a frozen model is only as good as the agreement about what its input
//! columns MEAN. This module is that agreement: the feature order, the value domain of every
//! column, and a hash over all of it. The trainer (`docs/evidence/lethe006/lethe_train.py`)
//! computes the same hash over the same list and bakes it into the blob it exports, so a blob
//! whose feature *meanings* have moved is a named refusal at load time
//! ([`crate::lethe::LoadError::ContractMismatch`]) instead of a silently rotated set of columns —
//! the same discipline `mlrisk_contract` applies to the scheduling forest (ADR-056).
//!
//! Derivation lives in [`crate::lethe::PmObserver`]; this file carries only the identity.

/// Number of features the kernel must supply to [`crate::lethe::Advisor::advise`].
pub const N_FEATURES: usize = 12;

/// sha256 of `name:scale:monotone` over the feature contract, in order, joined with `\n`.
/// (Scale is 1 and monotone is `n` for every column: each value is already an integer, and none
/// of them has a globally monotone "better" direction — demand high serves work and costs
/// energy at once. The string is the trainer's identity stamp, not a policy.)
pub const FEATURE_CONTRACT: [u8; 32] = [
    0xbd, 0x9b, 0xcf, 0x4c, 0xde, 0x8f, 0x62, 0xe7, 0xa7, 0x3d, 0x7c, 0xd0, 0x37, 0x4c, 0x02, 0xac,
    0x42, 0x5c, 0x66, 0xcb, 0x9a, 0x39, 0x6d, 0xf3, 0xcc, 0x77, 0x74, 0xd2, 0x53, 0x01, 0xf3, 0x51,
];

/// Feature order. Index `i` of the slice passed to `advise` MUST be this feature.
pub const FEATURE_NAMES: [&str; N_FEATURES] = [
    "demand_now",
    "demand_mean4",
    "demand_max8",
    "demand_min8",
    "demand_prev",
    "demand_swing8",
    "dwell_at_point",
    "transitions16",
    "temp_now_mc",
    "temp_rise_mc",
    "trip_margin_mc",
    "current_share_pmille",
];

/// The value domain of every column, as the `(lo, hi)` the trainer fit and the kernel clamps to.
/// Derivation never produces a value outside these bounds; a blob may narrow its box inside them
/// (its range guard), never widen past them.
pub const FEATURE_DOMAIN: [(i32, i32); N_FEATURES] = [
    (0, 100),            // demand_now: the demand register, percent
    (0, 100),            // demand_mean4: mean of the last <=4 samples, floored
    (0, 100),            // demand_max8: max of the last <=8 samples
    (0, 100),            // demand_min8: min of the last <=8 samples
    (0, 100),            // demand_prev: the sample before the last
    (0, 100),            // demand_swing8: max8 - min8
    (0, 65_535),         // dwell_at_point: ticks since the current point was set (capped)
    (0, 16),             // transitions16: point changes within the last 16 observations
    (0, 150_000),        // temp_now_mc: reported die temperature, clamped
    (-50_000, 50_000),   // temp_rise_mc: temp_now minus the oldest of the last <=8 reports
    (-150_000, 150_000), // trip_margin_mc: trip point minus temp_now (negative = tripped)
    (0, 1000),           // current_share_pmille: current point as per-mille of the governor range
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_well_formed() {
        for (lo, hi) in FEATURE_DOMAIN {
            assert!(lo <= hi);
        }
    }
}
