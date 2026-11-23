use crate::decimal::canon_decimal;
use crate::indicators::filter_visible_final_bars;
use crate::model::{
    deduplicate_corporate_actions, AdjustmentKind, BarRecord, CorporateActionRecord,
    CorporateActionType,
};
use crate::sim::SimError;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Compute one deterministic 64-bit random word from `SHA-256(seed || purpose || counter)` (`S-38`).
/// Uses explicit big-endian byte length framing for `seed` and `purpose` to eliminate boundary collisions.
pub fn prng_block_u64(seed: &[u8], purpose: &str, counter: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update((seed.len() as u64).to_be_bytes());
    hasher.update(seed);
    hasher.update((purpose.len() as u64).to_be_bytes());
    hasher.update(purpose.as_bytes());
    hasher.update(counter.to_be_bytes());
    let digest = hasher.finalize();
    let mut first_eight = [0u8; 8];
    first_eight.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first_eight)
}

/// Deterministic counter-mode PRNG backed by `SHA-256(seed || purpose || counter)` (`S-38`).
/// Algorithm is frozen with `m4-sim-1` and independent of external RNG crate upgrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicPrng {
    seed: Vec<u8>,
    purpose: String,
    counter: u64,
}

impl DeterministicPrng {
    pub fn new(seed: impl AsRef<[u8]>, purpose: impl Into<String>) -> Self {
        Self {
            seed: seed.as_ref().to_vec(),
            purpose: purpose.into(),
            counter: 0,
        }
    }

    pub fn with_counter(seed: impl AsRef<[u8]>, purpose: impl Into<String>, counter: u64) -> Self {
        Self {
            seed: seed.as_ref().to_vec(),
            purpose: purpose.into(),
            counter,
        }
    }

    pub fn counter(&self) -> u64 {
        self.counter
    }

    pub fn next_u64(&mut self) -> u64 {
        let word = prng_block_u64(&self.seed, &self.purpose, self.counter);
        self.counter = self.counter.wrapping_add(1);
        word
    }

    /// Rejection-sampled unbiased integer in `[0, upper_exclusive)` (`S-38`).
    pub fn next_u64_below(&mut self, upper_exclusive: u64) -> Result<u64, SimError> {
        if upper_exclusive == 0 {
            return Err(SimError::InvalidSamplingRange(
                "upper_exclusive must be at least 1".into(),
            ));
        }
        // 2^64 mod upper_exclusive == upper_exclusive.wrapping_neg() % upper_exclusive
        let threshold = upper_exclusive.wrapping_neg() % upper_exclusive;
        loop {
            let candidate = self.next_u64();
            if candidate >= threshold {
                return Ok(candidate % upper_exclusive);
            }
        }
    }

    /// Rejection-sampled unbiased slice index in `[0, len)`.
    pub fn next_index(&mut self, len: usize) -> Result<usize, SimError> {
        let idx = self.next_u64_below(len as u64)?;
        Ok(idx as usize)
    }
}

/// Deterministically sample an initial candidate index plus `redraw_count` explicit pre-start redraws
/// from a candidate pool of size `pool_len` (`S-38`, `M4-A-02`).
/// When `pool_len > 1`, each redraw avoids immediately repeating the previous draw.
pub fn sample_candidate_sequence(
    seed: &str,
    pool_len: usize,
    redraw_count: u32,
) -> Result<Vec<usize>, SimError> {
    if pool_len == 0 {
        return Err(SimError::InvalidSamplingRange(
            "candidate pool is empty".into(),
        ));
    }
    let mut prng = DeterministicPrng::new(seed.as_bytes(), "training_candidate_sample");
    let mut history = Vec::with_capacity((redraw_count as usize) + 1);
    let first = prng.next_index(pool_len)?;
    history.push(first);
    for _ in 0..redraw_count {
        if pool_len == 1 {
            let _ = prng.next_u64();
            history.push(0);
        } else {
            let prev = *history.last().unwrap_or(&0);
            let offset = prng.next_index(pool_len - 1)?;
            let next = if offset >= prev { offset + 1 } else { offset };
            history.push(next);
        }
    }
    Ok(history)
}

/// Derive point-in-time split-adjusted bars at `visible_until` (`S-37`, `M4-A-03`):
/// - Strictly truncates bars to `is_final && close_at <= visible_until`.
/// - Applies ONLY `Split` corporate actions where `ex_date <= current_visible_session_date`
///   backwards to bars with `bar.session_date < ex_date`; future splits (`ex_date > cutoff`)
///   are excluded prior to validation so future sentinels can never leak.
/// - Cash dividends are never adjusted into prices (`S-37`).
pub fn derive_point_in_time_split_adjusted_bars(
    raw_bars: &[BarRecord],
    corporate_actions: &[CorporateActionRecord],
    visible_until: DateTime<Utc>,
) -> Result<Vec<BarRecord>, SimError> {
    let vis_refs = filter_visible_final_bars(raw_bars, Some(visible_until));
    if vis_refs.is_empty() {
        return Ok(Vec::new());
    }

    let utc_cutoff_date = visible_until.date_naive();
    let mut max_session_by_instrument: BTreeMap<String, NaiveDate> = BTreeMap::new();
    for bar in &vis_refs {
        let entry = max_session_by_instrument
            .entry(bar.instrument_id.clone())
            .or_insert(bar.session_date);
        if bar.session_date > *entry {
            *entry = bar.session_date;
        }
    }

    // Filter to visible Split actions BEFORE inspecting ratio_or_amount so future sentinel rows are untouched.
    let visible_splits: Vec<CorporateActionRecord> = corporate_actions
        .iter()
        .filter(|ca| {
            if ca.action_type != CorporateActionType::Split {
                return false;
            }
            let Some(&inst_max_session) = max_session_by_instrument.get(&ca.instrument_id) else {
                return false;
            };
            let cutoff = inst_max_session.min(utc_cutoff_date);
            ca.ex_date <= cutoff
        })
        .cloned()
        .collect();

    let dedup_splits = deduplicate_corporate_actions(&visible_splits);

    let mut out = Vec::with_capacity(vis_refs.len());
    for bar in vis_refs {
        let mut split_factor = Decimal::ONE;
        for action in &dedup_splits {
            if action.instrument_id != bar.instrument_id || action.ex_date <= bar.session_date {
                continue;
            }
            if action.ratio_or_amount <= Decimal::ZERO {
                return Err(SimError::InvalidCorporateAction {
                    action_id: action.id.clone(),
                    reason: format!(
                        "split ratio must be positive, got {}",
                        action.ratio_or_amount
                    ),
                });
            }
            split_factor = split_factor
                .checked_mul(action.ratio_or_amount)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_factor".into()))?;
        }

        let adjust_price = |raw_price: Decimal| -> Result<Decimal, SimError> {
            if split_factor == Decimal::ONE {
                return Ok(canon_decimal(raw_price));
            }
            let scale = 6_u32.max(raw_price.scale());
            let divided = raw_price
                .checked_div(split_factor)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_price".into()))?;
            Ok(canon_decimal(divided.round_dp_with_strategy(
                scale,
                RoundingStrategy::MidpointAwayFromZero,
            )))
        };

        let mut adjusted = bar.clone();
        adjusted.adjustment = AdjustmentKind::SplitAdjusted;
        adjusted.open = adjust_price(bar.open)?;
        adjusted.high = adjust_price(bar.high)?;
        adjusted.low = adjust_price(bar.low)?;
        adjusted.close = adjust_price(bar.close)?;
        adjusted.volume = canon_decimal(
            bar.volume
                .checked_mul(split_factor)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_volume".into()))?,
        );
        out.push(adjusted);
    }
    Ok(out)
}
