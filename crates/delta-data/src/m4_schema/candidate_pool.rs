use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use delta_core::{
    parse_decimal_lossless, quality_kind_impact, CalendarSessionKind, InstrumentKind,
    QualityImpact, Timeframe,
};
use rusqlite::params;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

use crate::calendar::query_calendar_session;
use crate::m4_schema::models::{
    dedupe_bars_by_session_date, has_unexplained_split_jump, CandidatePoolQuery,
    CandidatePoolResult, CandidateQualityFilter, TrainingCandidateSegment,
};
use crate::storage::{map_bar_row, SqliteStore, StorageError};

impl SqliteStore {
    /// Fetch candidate pool segments for a frozen `dataset_version` (`S-38`, `S-42`, `M4-A-02`):
    /// Finds instruments & contiguous finalized-bar windows of length `history_len + train_len + reveal_len`
    /// with:
    /// 1. NO missing calendar sessions or `coverage_gap`s,
    /// 2. NO `partial` / `warning` / `data_quality_issue` flags in the segment,
    /// 3. NO incomplete corporate actions (missing/zero/invalid split ratio or dividend amount,
    ///    conflicting multi-source ratios, or unexplained split-like price jumps),
    ///
    /// returning deterministically sorted candidates and canonical SHA-256 `candidate_pool_hash`.
    pub fn query_training_candidate_pool(
        &self,
        query: &CandidatePoolQuery,
    ) -> Result<CandidatePoolResult, StorageError> {
        self.ensure_known_dataset_version(query.dataset_version)?;
        if query.history_len == 0 || query.train_len == 0 {
            return Err(StorageError::IntegrityFailed(
                "history_len and train_len must both be >= 1".into(),
            ));
        }
        let required_len = query
            .history_len
            .checked_add(query.train_len)
            .and_then(|n| n.checked_add(query.reveal_len))
            .ok_or_else(|| StorageError::IntegrityFailed("segment length overflow".into()))?;

        let norm_cat = query.market_category.trim().to_ascii_lowercase();
        if !matches!(norm_cat.as_str(), "all" | "us_equity" | "crypto_spot") {
            return Err(StorageError::IntegrityFailed(format!(
                "unsupported market_category '{}' (expected 'all', 'us_equity', or 'crypto_spot')",
                query.market_category
            )));
        }
        let timeframe = Timeframe::parse(&query.timeframe).ok_or_else(|| {
            StorageError::IntegrityFailed(format!(
                "unsupported timeframe '{}' (expected '1d', '4h' or '1h')",
                query.timeframe
            ))
        })?;
        if timeframe.is_intraday() && norm_cat == "us_equity" {
            return Err(StorageError::IntegrityFailed(format!(
                "US equity/ETF training supports only 1d bars, not {timeframe}"
            )));
        }
        let price_only = query.quality_filter == CandidateQualityFilter::PriceAffectingOnly;

        let registry = self.load_instrument_registry()?;
        let mut instruments: Vec<_> = registry.instruments.values().cloned().collect();
        instruments.sort_by(|a, b| a.id.cmp(&b.id));

        let mut candidates = Vec::new();

        for inst in instruments {
            if let Some(ref target_id) = query.instrument_id {
                if &inst.id != target_id {
                    continue;
                }
            }
            let inst_cat = match inst.kind {
                InstrumentKind::SpotEquity | InstrumentKind::SpotEtf => "us_equity",
                InstrumentKind::SpotCrypto => "crypto_spot",
            };
            if norm_cat != "all" && inst_cat != norm_cat {
                continue;
            }
            if timeframe.is_intraday() && inst_cat != "crypto_spot" {
                continue;
            }

            // Load finalized raw bars at frozen dataset_version.
            let mut bar_stmt = self.conn().prepare_cached(
                "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                        open, high, low, close, volume, source, adjustment, is_final,
                        valid_from_version, valid_to_version, raw_payload_hash
                 FROM bar
                 WHERE instrument_id = ?1
                   AND timeframe = ?3
                   AND adjustment = 'raw'
                   AND is_final = 1
                   AND valid_from_version <= ?2
                   AND (valid_to_version IS NULL OR ?2 < valid_to_version)
                 ORDER BY session_date ASC, open_at ASC, id ASC",
            )?;
            let bar_rows = bar_stmt.query_map(
                params![inst.id, query.dataset_version, timeframe.as_str()],
                |r| Ok(map_bar_row(r)),
            )?;
            let mut raw_bars = Vec::new();
            let mut corrupt_bar_found = false;
            for r in bar_rows {
                match r? {
                    Ok(b) => raw_bars.push(b),
                    Err(_) => {
                        corrupt_bar_found = true;
                        break;
                    }
                }
            }
            if corrupt_bar_found {
                continue;
            }
            let mut bars = dedupe_bars_by_session_date(raw_bars);
            if let Some(sd) = query.start_date {
                bars.retain(|b| b.session_date >= sd);
            }
            if let Some(ed) = query.end_date {
                bars.retain(|b| b.session_date <= ed);
            }
            if bars.len() < required_len {
                continue;
            }

            // Load coverage_gap intervals for this instrument up to dataset_version.
            let mut gap_stmt = self.conn().prepare_cached(
                "SELECT start_date, end_date
                 FROM coverage_gap
                 WHERE instrument_id = ?1
                   AND dataset_version <= ?2
                   AND resolution != 'resolved'",
            )?;
            let mut stored_gaps: Vec<(NaiveDate, NaiveDate)> = Vec::new();
            for row in gap_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                let (s_str, e_str) = row?;
                if let (Ok(s), Ok(e)) = (
                    NaiveDate::parse_from_str(&s_str, "%Y-%m-%d"),
                    NaiveDate::parse_from_str(&e_str, "%Y-%m-%d"),
                ) {
                    stored_gaps.push((s, e));
                }
            }

            // Load data_quality_issue dates/kinds for this instrument up to dataset_version.
            let mut dqi_stmt = self.conn().prepare_cached(
                "SELECT dataset_version, session_date, kind
                 FROM data_quality_issue
                 WHERE instrument_id = ?1
                   AND dataset_version <= ?2",
            )?;
            let mut quality_issue_dates: BTreeSet<NaiveDate> = BTreeSet::new();
            let mut instrument_missing_ca_globally = false;
            let mut unparseable_dqi_date = false;
            for row in dqi_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (ver, d_str, kind) = row?;
                let kind_lc = kind.to_ascii_lowercase();
                if price_only && quality_kind_impact(&kind_lc) != QualityImpact::AffectsPrice {
                    continue;
                }
                // Ignore historical `data_revision` audit notices from strictly older versions;
                // exclude all warnings, partial, missing_corporate_action, price_mismatch, etc.
                if kind_lc == "data_revision" && ver < query.dataset_version {
                    continue;
                }
                if kind_lc == "missing_corporate_action" && ver == query.dataset_version {
                    instrument_missing_ca_globally = true;
                }
                if let Ok(d) = NaiveDate::parse_from_str(&d_str, "%Y-%m-%d") {
                    quality_issue_dates.insert(d);
                } else {
                    unparseable_dqi_date = true;
                }
            }
            if unparseable_dqi_date
                || (instrument_missing_ca_globally && quality_issue_dates.is_empty())
            {
                continue;
            }

            // Inspect raw corporate_action rows for this instrument at factor_version <= dataset_version
            // to detect incomplete ratios/amounts or multi-source disagreements.
            let mut ca_stmt = self.conn().prepare_cached(
                "SELECT action_type, ex_date, ratio_or_amount, currency, source
                 FROM corporate_action
                 WHERE instrument_id = ?1
                   AND factor_version <= ?2
                   AND factor_version = (
                       SELECT MAX(c.factor_version)
                       FROM corporate_action c
                       WHERE c.instrument_id = corporate_action.instrument_id
                         AND c.action_type = corporate_action.action_type
                         AND c.ex_date = corporate_action.ex_date
                         AND c.source = corporate_action.source
                         AND c.factor_version <= ?2
                   )
                 ORDER BY ex_date ASC",
            )?;
            let mut bad_ca_dates: BTreeSet<NaiveDate> = BTreeSet::new();
            let mut valid_splits_by_date: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
            let mut valid_divs_by_date: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
            let mut unparseable_ca_date = false;

            for row in ca_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })? {
                let (act_type, ex_str, ratio_str, _ccy, _src) = row?;
                let Ok(ex_date) = NaiveDate::parse_from_str(&ex_str, "%Y-%m-%d") else {
                    unparseable_ca_date = true;
                    break;
                };
                let trimmed = ratio_str.trim();
                let Ok(val) = parse_decimal_lossless(trimmed) else {
                    bad_ca_dates.insert(ex_date);
                    continue;
                };
                if val <= Decimal::ZERO {
                    bad_ca_dates.insert(ex_date);
                    continue;
                }
                match act_type.as_str() {
                    "split" => {
                        if let Some(&prev_val) = valid_splits_by_date.get(&ex_date) {
                            if prev_val != val {
                                bad_ca_dates.insert(ex_date);
                            }
                        } else {
                            valid_splits_by_date.insert(ex_date, val);
                        }
                    }
                    "cash_dividend" => {
                        if let Some(&prev_val) = valid_divs_by_date.get(&ex_date) {
                            if prev_val != val {
                                bad_ca_dates.insert(ex_date);
                            }
                        } else {
                            valid_divs_by_date.insert(ex_date, val);
                        }
                    }
                    _ => {
                        bad_ca_dates.insert(ex_date);
                    }
                }
            }
            if unparseable_ca_date {
                continue;
            }

            let split_date_set: BTreeSet<NaiveDate> =
                valid_splits_by_date.keys().copied().collect();
            let cal_venue = match inst.venue.as_str() {
                "NASDAQ" | "XNAS" => "XNAS",
                "NYSE" | "XNYS" => "XNYS",
                _ => "CRYPTO_24_7",
            };

            // Precompute whether each bar and each adjacent pair `(i-1, i)` is calendar-contiguous & clean.
            // Intraday bars must ALSO satisfy exact alignment & period duration on every bar (including the final bar).
            let bar_valid: Vec<bool> = bars
                .iter()
                .map(|b| {
                    b.is_final
                        && b.open > Decimal::ZERO
                        && b.high >= b.open.max(b.close)
                        && b.low <= b.open.min(b.close)
                        && b.low > Decimal::ZERO
                        && b.close > Decimal::ZERO
                        && b.volume >= Decimal::ZERO
                        && b.close_at > b.open_at
                        && (!timeframe.is_intraday()
                            || (timeframe.is_aligned(b.open_at)
                                && b.close_at == b.open_at + timeframe.duration()))
                        && !quality_issue_dates.contains(&b.session_date)
                        && !bad_ca_dates.contains(&b.session_date)
                        && matches!(
                            query_calendar_session(cal_venue, b.session_date).kind,
                            CalendarSessionKind::Regular
                                | CalendarSessionKind::EarlyClose
                                | CalendarSessionKind::Crypto24x7
                        )
                })
                .collect();

            // `adjacent_clean[i]` is true iff there are NO missing trading sessions, calendar_unknowns,
            // quality issues, or incomplete CAs strictly between `bars[i]` and `bars[i + 1]`.
            let mut adjacent_clean = vec![false; bars.len().saturating_sub(1)];
            for i in 0..adjacent_clean.len() {
                if timeframe.is_intraday() {
                    // Intraday crypto trades 24/7: the next bar must open exactly at this close.
                    adjacent_clean[i] = bars[i + 1].open_at == bars[i].close_at
                        && bars[i].close_at == bars[i].open_at + timeframe.duration();
                    continue;
                }
                let d_prev = bars[i].session_date;
                let d_next = bars[i + 1].session_date;
                if d_next <= d_prev {
                    continue;
                }
                let mut ok = true;
                let mut cur = d_prev.checked_add_days(chrono::Days::new(1));
                while let Some(d) = cur {
                    if d >= d_next {
                        break;
                    }
                    let sess = query_calendar_session(cal_venue, d);
                    match sess.kind {
                        CalendarSessionKind::Closed => {}
                        CalendarSessionKind::CalendarUnknown
                        | CalendarSessionKind::Regular
                        | CalendarSessionKind::EarlyClose
                        | CalendarSessionKind::Crypto24x7 => {
                            // Missing trading session or unknown calendar day!
                            ok = false;
                            break;
                        }
                    }
                    if quality_issue_dates.contains(&d) || bad_ca_dates.contains(&d) {
                        ok = false;
                        break;
                    }
                    cur = d.checked_add_days(chrono::Days::new(1));
                }
                adjacent_clean[i] = ok;
            }

            // Every per-window check below depends only on single bars or adjacent pairs, so it
            // is computed once and windows are tested with prefix counts (`M5-K01`).
            let prefix = |flags: &mut dyn Iterator<Item = bool>| -> Vec<usize> {
                let mut out = vec![0usize];
                let mut acc = 0usize;
                for bad in flags {
                    acc += usize::from(bad);
                    out.push(acc);
                }
                out
            };
            let invalid_bars = prefix(&mut bar_valid.iter().map(|ok| !ok));
            let unclean_pairs = prefix(&mut adjacent_clean.iter().map(|ok| !ok));
            let jump_pairs = prefix(
                &mut bars
                    .windows(2)
                    .map(|pair| has_unexplained_split_jump(pair, &split_date_set)),
            );
            let max_start = bars.len() - required_len;
            for start_idx in 0..=max_start {
                let end_idx = start_idx + required_len - 1;
                if invalid_bars[end_idx + 1] != invalid_bars[start_idx] {
                    continue;
                }
                if unclean_pairs[end_idx] != unclean_pairs[start_idx] {
                    continue;
                }

                let seg_start_date = bars[start_idx].session_date;
                let seg_end_date = bars[end_idx].session_date;

                // Stored gaps are daily-session gaps; intraday windows rely on the exact
                // open/close continuity check above.
                if !timeframe.is_intraday()
                    && stored_gaps
                        .iter()
                        .any(|(gs, ge)| *gs <= seg_end_date && *ge >= seg_start_date)
                {
                    continue;
                }
                if quality_issue_dates
                    .range(seg_start_date..=seg_end_date)
                    .next()
                    .is_some()
                {
                    continue;
                }
                if bad_ca_dates
                    .range(seg_start_date..=seg_end_date)
                    .next()
                    .is_some()
                {
                    continue;
                }

                // Walk backwards up to `query.warmup_bars` contiguous clean bars for indicator warmup (`S-37`).
                let mut warmup_idx = start_idx;
                let mut taken_warmup = 0usize;
                while taken_warmup < query.warmup_bars && warmup_idx > 0 {
                    let prev_idx = warmup_idx - 1;
                    if !bar_valid[prev_idx] || !adjacent_clean[prev_idx] {
                        break;
                    }
                    let prev_date = bars[prev_idx].session_date;
                    if !timeframe.is_intraday()
                        && stored_gaps
                            .iter()
                            .any(|(gs, ge)| *gs <= prev_date && *ge >= prev_date)
                    {
                        break;
                    }
                    warmup_idx = prev_idx;
                    taken_warmup += 1;
                }

                // Check for unexplained split-like jumps across `[warmup_idx..=end_idx]`.
                if jump_pairs[end_idx] != jump_pairs[warmup_idx] {
                    continue;
                }

                let hist_end_idx = start_idx + query.history_len - 1;
                let train_start_idx = start_idx + query.history_len;
                let train_end_idx = train_start_idx + query.train_len - 1;

                candidates.push(TrainingCandidateSegment {
                    instrument_id: inst.id.clone(),
                    venue: inst.venue.clone(),
                    symbol: inst.symbol.clone(),
                    market_category: inst_cat.to_string(),
                    quote_currency: inst.quote_asset_id.clone(),
                    warmup_start_at: bars[warmup_idx].open_at,
                    history_start_at: bars[start_idx].open_at,
                    initial_visible_until: bars[hist_end_idx].close_at,
                    train_start_at: bars[train_start_idx].open_at,
                    train_end_at: bars[train_end_idx].close_at,
                    reveal_end_at: bars[end_idx].close_at,
                    history_start_date: bars[start_idx].session_date,
                    train_start_date: bars[train_start_idx].session_date,
                    train_end_date: bars[train_end_idx].session_date,
                    reveal_end_date: bars[end_idx].session_date,
                    warmup_bar_count: taken_warmup,
                    history_len: query.history_len,
                    train_len: query.train_len,
                    reveal_len: query.reveal_len,
                });
            }
        }

        candidates.sort_by(|a, b| {
            a.instrument_id
                .cmp(&b.instrument_id)
                .then_with(|| a.history_start_at.cmp(&b.history_start_at))
                .then_with(|| a.train_start_at.cmp(&b.train_start_at))
                .then_with(|| a.train_end_at.cmp(&b.train_end_at))
                .then_with(|| a.reveal_end_at.cmp(&b.reveal_end_at))
        });

        let mut hasher = Sha256::new();
        hasher.update(
            format!(
                "m4-candidate-pool-v1|dsv={}|cat={}|w={}|h={}|t={}|r={}|count={}\n",
                query.dataset_version,
                norm_cat,
                query.warmup_bars,
                query.history_len,
                query.train_len,
                query.reveal_len,
                candidates.len()
            )
            .as_bytes(),
        );
        // M5 pools (non-daily timeframe or price-only quality rule) carry an extra header line;
        // M4 pools hash byte-for-byte as before so stored `candidate_pool_hash` values still match.
        if timeframe.is_intraday() || price_only {
            hasher.update(
                format!("m5-pool|tf={timeframe}|quality=price_only={price_only}\n").as_bytes(),
            );
        }
        for c in &candidates {
            hasher.update(
                format!(
                    "{}|{}|{}|{}|{}|{}|{}\n",
                    c.instrument_id,
                    c.warmup_start_at.to_rfc3339(),
                    c.history_start_at.to_rfc3339(),
                    c.initial_visible_until.to_rfc3339(),
                    c.train_start_at.to_rfc3339(),
                    c.train_end_at.to_rfc3339(),
                    c.reveal_end_at.to_rfc3339(),
                )
                .as_bytes(),
            );
        }
        let candidate_pool_hash = hex::encode(hasher.finalize());

        Ok(CandidatePoolResult {
            dataset_version: query.dataset_version,
            market_category: norm_cat,
            warmup_bars: query.warmup_bars,
            history_len: query.history_len,
            train_len: query.train_len,
            reveal_len: query.reveal_len,
            candidate_pool_hash,
            candidates,
        })
    }
}
