use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, Utc};
use delta_core::{
    parse_decimal_lossless, AdjustmentKind, BarRecord, CalendarSessionKind, CorporateActionRecord,
    CorporateActionType, CoverageGap, DataQualityIssue,
};
use rusqlite::params;
use rust_decimal::Decimal;

use super::*;
use crate::calendar::query_calendar_session;
use crate::m2_schema::{AiMessageRecord, AiRunRecord, ModelConnectionRecord, StructuredNoteFields};
use crate::storage::{parse_rfc3339_utc, SqliteStore, StorageError, CURRENT_SCHEMA_VERSION};

struct TestTempDir(PathBuf);

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "delta_m4_schema_{prefix}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create isolated temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn make_equity_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
    let sess = query_calendar_session("XNAS", d);
    let open_at = sess.open_at.expect("equity trading open_at");
    let close_at = sess.close_at.expect("equity trading close_at");
    let px = parse_decimal_lossless(close_px).unwrap();
    BarRecord {
        id: format!("BAR:{instrument_id}:1d:raw:yahoo:{d}:{dsv}"),
        instrument_id: instrument_id.to_string(),
        timeframe: "1d".to_string(),
        session: sess.kind.as_str().to_string(),
        session_date: d,
        open_at,
        close_at,
        open: px,
        high: px + Decimal::from(2),
        low: (px - Decimal::ONE).max(Decimal::ONE),
        close: px,
        volume: Decimal::from(100_000),
        source: "yahoo".to_string(),
        adjustment: AdjustmentKind::Raw,
        is_final: true,
        valid_from_version: dsv,
        valid_to_version: None,
        raw_payload_hash: None,
    }
}

fn make_crypto_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
    let sess = query_calendar_session("CRYPTO_24_7", d);
    let open_at = sess.open_at.expect("crypto open_at");
    let close_at = sess.close_at.expect("crypto close_at");
    let px = parse_decimal_lossless(close_px).unwrap();
    BarRecord {
        id: format!("BAR:{instrument_id}:1d:raw:okx:{d}:{dsv}"),
        instrument_id: instrument_id.to_string(),
        timeframe: "1d".to_string(),
        session: "24x7".to_string(),
        session_date: d,
        open_at,
        close_at,
        open: px,
        high: px + Decimal::from(50),
        low: (px - Decimal::from(50)).max(Decimal::ONE),
        close: px,
        volume: Decimal::from(1_500),
        source: "okx".to_string(),
        adjustment: AdjustmentKind::Raw,
        is_final: true,
        valid_from_version: dsv,
        valid_to_version: None,
        raw_payload_hash: None,
    }
}

fn trading_days_from(venue: &str, start: NaiveDate, count: usize) -> Vec<NaiveDate> {
    let mut out = Vec::with_capacity(count);
    let mut cur = start;
    while out.len() < count {
        let sess = query_calendar_session(venue, cur);
        if matches!(
            sess.kind,
            CalendarSessionKind::Regular
                | CalendarSessionKind::EarlyClose
                | CalendarSessionKind::Crypto24x7
        ) {
            out.push(cur);
        }
        cur = cur.checked_add_days(chrono::Days::new(1)).unwrap();
    }
    out
}

#[test]
fn test_m4_k01_and_a01_sql_truncation_with_sentinels_clock_monotonicity_and_reveal_gate() {
    let tmp = TestTempDir::new("k01_a01");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

    let aapl = store
        .register_market_instrument("NASDAQ", "AAPL", false)
        .unwrap();
    let btc = store
        .register_market_instrument("OKX", "BTC/USDT", false)
        .unwrap();

    let dsv = store
        .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
        .unwrap()
        .version_num;

    // 12 contiguous XNAS trading days starting 2025-01-02:
    // [0..2] warmup, [2..5] history, [5..9] train, [9..12] reveal.
    let eq_days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(), 12);
    let eq_bars: Vec<BarRecord> = eq_days
        .iter()
        .enumerate()
        .map(|(idx, &d)| make_equity_bar(&aapl, d, &(100 + idx as i64).to_string(), dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&eq_bars, dsv, None)
        .unwrap();

    // Insert a non-final bar inside the visible window to verify `is_final = 1` SQL filtering.
    store
        .conn()
        .execute(
            "INSERT INTO bar (
                id, instrument_id, timeframe, session, session_date, open_at, close_at,
                open, high, low, close, volume, source, adjustment, is_final,
                valid_from_version, valid_to_version, raw_payload_hash
             ) VALUES (
                'BAR:UNFINALIZED', ?1, '1d', 'regular', ?2, ?3, ?4,
                '7777777', '7777777', '7777777', '7777777', '1', 'intraday_preview', 'raw', 0,
                ?5, NULL, NULL
             )",
            params![
                aapl,
                eq_days[4].to_string(),
                eq_bars[4].open_at.to_rfc3339(),
                eq_bars[4].close_at.to_rfc3339(),
                dsv,
            ],
        )
        .unwrap();

    // Insert historical + future corporate actions.
    let past_div = CorporateActionRecord {
        id: "CA:AAPL:PAST_DIV".to_string(),
        instrument_id: aapl.clone(),
        action_type: CorporateActionType::CashDividend,
        ex_date: eq_days[3],
        pay_date: Some(eq_days[4]),
        ratio_or_amount: parse_decimal_lossless("0.25").unwrap(),
        currency: Some("USD".to_string()),
        announced_at: None,
        source: "yahoo".to_string(),
        factor_version: dsv,
        raw_payload_hash: None,
    };
    let future_split = CorporateActionRecord {
        id: "CA:AAPL:FUTURE_SENTINEL_SPLIT".to_string(),
        instrument_id: aapl.clone(),
        action_type: CorporateActionType::Split,
        ex_date: eq_days[6],
        pay_date: None,
        ratio_or_amount: Decimal::from(999_999),
        currency: None,
        announced_at: None,
        source: "yahoo".to_string(),
        factor_version: dsv,
        raw_payload_hash: None,
    };
    let future_div = CorporateActionRecord {
        id: "CA:AAPL:FUTURE_POISON_DIV".to_string(),
        instrument_id: aapl.clone(),
        action_type: CorporateActionType::CashDividend,
        ex_date: eq_days[10],
        pay_date: None,
        ratio_or_amount: Decimal::from(888_888),
        currency: Some("USD".to_string()),
        announced_at: None,
        source: "yahoo".to_string(),
        factor_version: dsv,
        raw_payload_hash: None,
    };
    store
        .upsert_corporate_actions(&[past_div, future_split, future_div])
        .unwrap();

    // Also commit 4 crypto bars and a future action on day[2] to verify UTC midnight ex_date boundary.
    let crypto_days = trading_days_from(
        "CRYPTO_24_7",
        NaiveDate::from_ymd_opt(2025, 2, 1).unwrap(),
        4,
    );
    let crypto_bars: Vec<BarRecord> = crypto_days
        .iter()
        .enumerate()
        .map(|(i, &d)| make_crypto_bar(&btc, d, &(90_000 + i as i64 * 100).to_string(), dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&crypto_bars, dsv, None)
        .unwrap();
    store
        .upsert_corporate_actions(&[CorporateActionRecord {
            id: "CA:BTC:NEXT_DAY".to_string(),
            instrument_id: btc.clone(),
            action_type: CorporateActionType::Split,
            ex_date: crypto_days[2],
            pay_date: None,
            ratio_or_amount: Decimal::from(555_555),
            currency: None,
            announced_at: None,
            source: "okx".to_string(),
            factor_version: dsv,
            raw_payload_hash: None,
        }])
        .unwrap();

    // Overwrite future bars (idx >= 5) and future corporate actions in SQLite with sentinels/poison.
    store
        .conn()
        .execute(
            "UPDATE bar SET open='999999999', high='999999999', low='999999999', close='999999999'
             WHERE instrument_id = ?1 AND session_date = ?2",
            params![aapl, eq_days[5].to_string()],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE bar SET open='POISON_FUTURE_BAR', high='POISON_FUTURE_BAR', low='POISON_FUTURE_BAR', close='POISON_FUTURE_BAR'
             WHERE instrument_id = ?1 AND session_date >= ?2",
            params![aapl, eq_days[6].to_string()],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE corporate_action SET ratio_or_amount = 'POISON_FUTURE_DIV'
             WHERE id = 'CA:AAPL:FUTURE_POISON_DIV'",
            [],
        )
        .unwrap();

    let warmup_start = eq_bars[0].open_at;
    let history_start = eq_bars[2].open_at;
    let initial_visible_until = eq_bars[4].close_at;

    let vis_bars_with_warmup = store
        .query_training_visible_final_bars(&TrainingBarPortQuery {
            instrument_id: aapl.clone(),
            timeframe: "1d".to_string(),
            source: None,
            dataset_version: dsv,
            warmup_start_at: Some(warmup_start),
            history_start_at: Some(history_start),
            visible_until: initial_visible_until,
            previous_visible_until: None,
        })
        .expect("SQL truncation must never read future sentinel or poison bars");
    assert_eq!(vis_bars_with_warmup.len(), 5);
    assert!(vis_bars_with_warmup.iter().all(|b| b.is_final
        && b.close_at <= initial_visible_until
        && b.close < Decimal::from(200)));

    let at_ver_truncated = store
        .query_bars_at_version(
            &aapl,
            "1d",
            AdjustmentKind::Raw,
            Some("yahoo"),
            dsv,
            Some(initial_visible_until),
        )
        .expect("query_bars_at_version SQL cutoff must avoid future poison rows");
    assert_eq!(at_ver_truncated.len(), 5);

    let vis_actions = store
        .query_training_visible_corporate_actions(&aapl, dsv, initial_visible_until)
        .expect("SQL corporate_action truncation must never read future sentinel/poison CAs");
    assert_eq!(vis_actions.len(), 1);
    assert_eq!(vis_actions[0].id, "CA:AAPL:PAST_DIV");

    let crypto_vis_actions = store
        .query_training_visible_corporate_actions(&btc, dsv, crypto_bars[1].close_at)
        .unwrap();
    assert!(
        crypto_vis_actions.is_empty(),
        "crypto midnight close_at must not leak next session's corporate action"
    );

    let backward_err = store
        .query_training_visible_final_bars(&TrainingBarPortQuery {
            instrument_id: aapl.clone(),
            timeframe: "1d".to_string(),
            source: None,
            dataset_version: dsv,
            warmup_start_at: Some(warmup_start),
            history_start_at: Some(history_start),
            visible_until: eq_bars[3].close_at,
            previous_visible_until: Some(initial_visible_until),
        })
        .unwrap_err();
    assert!(backward_err.to_string().contains("visible_until"));

    let now_str = "2026-11-12T10:00:00+00:00".to_string();
    let run = SimulationRunRecord {
        run_id: "RUN-K01".to_string(),
        kind: "training".to_string(),
        seed: 42,
        dataset_version: dsv,
        engine_version: "m4-v1".to_string(),
        fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
        quote_currency: "USD".to_string(),
        initial_cash: Decimal::from(100_000),
        status: SimRunStatus::Active,
        parent_run_id: None,
        latest_seq: 0,
        latest_state_hash: "HASH-0".to_string(),
        created_at: now_str.clone(),
        updated_at: now_str,
        finished_at: None,
    };
    let session = TrainingSessionRecord {
        run_id: "RUN-K01".to_string(),
        hidden_mode: true,
        market_category: "us_equity".to_string(),
        display_symbol: "标的 A".to_string(),
        real_instrument_id: aapl.clone(),
        candidate_pool_hash: "POOL-HASH".to_string(),
        redraw_count: 0,
        redraw_history: Vec::new(),
        warmup_start_at: warmup_start,
        history_start_at: history_start,
        train_start_at: eq_bars[5].open_at,
        train_end_at: eq_bars[8].close_at,
        reveal_end_at: eq_bars[11].close_at,
        visible_until: initial_visible_until,
        current_step_index: 0,
        total_steps: 4,
        revealed: false,
        timeframe: "1d".to_string(),
        practice_account_id: None,
        fill_rule_version: None,
    };
    store.create_training_run(&run, &session, None).unwrap();

    let run_bars_no_warmup = store
        .query_run_visible_final_bars("RUN-K01", false)
        .unwrap();
    assert_eq!(run_bars_no_warmup.len(), 3);
    let run_bars_with_warmup = store.query_run_visible_final_bars("RUN-K01", true).unwrap();
    assert_eq!(run_bars_with_warmup.len(), 5);

    assert!(store.query_post_finish_reveal("RUN-K01").is_err());
    assert!(store.mark_training_session_revealed("RUN-K01").is_err());
    assert!(store
        .conn()
        .execute(
            "UPDATE training_session SET revealed = 1 WHERE run_id = 'RUN-K01'",
            []
        )
        .is_err());
    assert!(store
        .conn()
        .execute(
            "UPDATE training_session SET visible_until = ?1 WHERE run_id = 'RUN-K01'",
            params![eq_bars[3].close_at.to_rfc3339()],
        )
        .is_err());

    store
        .conn()
        .execute(
            "UPDATE bar SET open='999999999', high='999999999', low='999999999', close='999999999'
             WHERE instrument_id = ?1 AND session_date >= ?2",
            params![aapl, eq_days[6].to_string()],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE corporate_action SET ratio_or_amount = '888888'
             WHERE id = 'CA:AAPL:FUTURE_POISON_DIV'",
            [],
        )
        .unwrap();

    store
        .commit_simulation_step_atomic(&SimulationStepCommit {
            run_id: "RUN-K01".to_string(),
            expected_prev_seq: 0,
            seq: 1,
            event_kind: "finish_session".to_string(),
            command_json: r#"{"cmd":"finish"}"#.to_string(),
            events_json: r#"[{"ev":"session_finished"}]"#.to_string(),
            new_status: SimRunStatus::Finished,
            new_visible_until: eq_bars[8].close_at,
            new_step_index: 4,
            new_state_hash: "HASH-FIN".to_string(),
            orders_to_upsert: Vec::new(),
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: Some(SimCheckpointRecord {
                run_id: "RUN-K01".to_string(),
                seq: 1,
                reason: SimCheckpointReason::Finish,
                state_json: r#"{"status":"finished"}"#.to_string(),
                state_hash: "HASH-FIN".to_string(),
                created_at: "2026-11-12T10:05:00+00:00".to_string(),
            }),
            committed_at: parse_rfc3339_utc("2026-11-12T10:05:00+00:00", "committed_at").unwrap(),
        })
        .unwrap();

    let reveal = store
        .query_post_finish_reveal("RUN-K01")
        .expect("post-finish reveal must succeed once status == 'finished'");
    assert_eq!(reveal.real_instrument_id, aapl);
    assert_eq!(reveal.real_symbol, "AAPL");
    assert_eq!(reveal.future_reveal_bars.len(), 3);
    assert_eq!(reveal.all_bars_through_reveal.len(), 12);
    assert_eq!(reveal.corporate_actions_through_reveal.len(), 3);

    let revealed_sess = store.mark_training_session_revealed("RUN-K01").unwrap();
    assert!(revealed_sess.revealed);
}

#[test]
fn test_m4_a02_candidate_pool_exclusions_determinism_and_redraw_rules() {
    let tmp = TestTempDir::new("a02_pool");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

    let inst_clean = store
        .register_market_instrument("NASDAQ", "CLEAN", false)
        .unwrap();
    let inst_calgap = store
        .register_market_instrument("NASDAQ", "CALGAP", false)
        .unwrap();
    let inst_covgap = store
        .register_market_instrument("NASDAQ", "COVGAP", false)
        .unwrap();
    let inst_warn = store
        .register_market_instrument("NASDAQ", "QUALWARN", false)
        .unwrap();
    let inst_badca = store
        .register_market_instrument("NASDAQ", "BADCA", false)
        .unwrap();
    let inst_unexp = store
        .register_market_instrument("NASDAQ", "UNEXPSPLIT", false)
        .unwrap();
    let inst_crypto = store
        .register_market_instrument("OKX", "ETH/USDT", false)
        .unwrap();

    let dsv = store
        .create_dataset_version("yahoo", "personal", "{}", "warning", "exact")
        .unwrap()
        .version_num;

    let eq_days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(), 10);
    let crypto_days = trading_days_from(
        "CRYPTO_24_7",
        NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(),
        10,
    );

    let clean_bars: Vec<BarRecord> = eq_days
        .iter()
        .enumerate()
        .map(|(i, &d)| {
            let px = if i < 5 { "400" } else { "100" };
            make_equity_bar(&inst_clean, d, px, dsv)
        })
        .collect();
    store
        .commit_bars_and_checkpoint(&clean_bars, dsv, None)
        .unwrap();
    store
        .upsert_corporate_actions(&[CorporateActionRecord {
            id: "CA:CLEAN:SPLIT4".to_string(),
            instrument_id: inst_clean.clone(),
            action_type: CorporateActionType::Split,
            ex_date: eq_days[5],
            pay_date: None,
            ratio_or_amount: Decimal::from(4),
            currency: None,
            announced_at: None,
            source: "yahoo".to_string(),
            factor_version: dsv,
            raw_payload_hash: None,
        }])
        .unwrap();

    let eth_bars: Vec<BarRecord> = crypto_days
        .iter()
        .enumerate()
        .map(|(i, &d)| make_crypto_bar(&inst_crypto, d, &(2500 + i as i64 * 10).to_string(), dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&eth_bars, dsv, None)
        .unwrap();

    let eq_days_11 = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(), 11);
    let calgap_bars: Vec<BarRecord> = eq_days_11
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .map(|(_, &d)| make_equity_bar(&inst_calgap, d, "150", dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&calgap_bars, dsv, None)
        .unwrap();

    let covgap_bars: Vec<BarRecord> = eq_days
        .iter()
        .map(|&d| make_equity_bar(&inst_covgap, d, "150", dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&covgap_bars, dsv, None)
        .unwrap();
    let gap = CoverageGap {
        id: "GAP:COVGAP:1".to_string(),
        dataset_version: dsv,
        instrument_id: inst_covgap.clone(),
        start_date: eq_days[4],
        end_date: eq_days[4],
        gap_kind: "provider_missing".to_string(),
        resolution: "open".to_string(),
    };
    store
        .conn()
        .execute(
            "INSERT INTO coverage_gap (id, dataset_version, instrument_id, start_date, end_date, gap_kind, resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                gap.id,
                gap.dataset_version,
                gap.instrument_id,
                gap.start_date.to_string(),
                gap.end_date.to_string(),
                gap.gap_kind,
                gap.resolution
            ],
        )
        .unwrap();

    let warn_bars: Vec<BarRecord> = eq_days
        .iter()
        .map(|&d| make_equity_bar(&inst_warn, d, "150", dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&warn_bars, dsv, None)
        .unwrap();
    store
        .record_quality_issues(&[DataQualityIssue {
            id: "DQI:QUALWARN:1".to_string(),
            dataset_version: dsv,
            instrument_id: inst_warn.clone(),
            session_date: eq_days[6],
            kind: "partial".to_string(),
            sources: "yahoo".to_string(),
            detail: "partial/warning bar".to_string(),
        }])
        .unwrap();

    let badca_bars: Vec<BarRecord> = eq_days
        .iter()
        .map(|&d| make_equity_bar(&inst_badca, d, "150", dsv))
        .collect();
    store
        .commit_bars_and_checkpoint(&badca_bars, dsv, None)
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO corporate_action (
                id, instrument_id, action_type, ex_date, pay_date,
                ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
             ) VALUES (?1, ?2, 'cash_dividend', ?3, NULL, '', 'USD', NULL, 'yahoo', ?4, NULL)",
            params!["CA:BADCA:EMPTY", inst_badca, eq_days[5].to_string(), dsv],
        )
        .unwrap();

    let unexp_bars: Vec<BarRecord> = eq_days
        .iter()
        .enumerate()
        .map(|(i, &d)| {
            let px = if i < 5 { "400" } else { "100" };
            make_equity_bar(&inst_unexp, d, px, dsv)
        })
        .collect();
    store
        .commit_bars_and_checkpoint(&unexp_bars, dsv, None)
        .unwrap();

    let query_all = CandidatePoolQuery {
        dataset_version: dsv,
        market_category: "all".to_string(),
        instrument_id: None,
        start_date: None,
        end_date: None,
        warmup_bars: 1,
        history_len: 3,
        train_len: 4,
        reveal_len: 2,
        timeframe: "1d".to_string(),
        quality_filter: Default::default(),
    };
    let pool_1 = store.query_training_candidate_pool(&query_all).unwrap();
    let pool_2 = store.query_training_candidate_pool(&query_all).unwrap();
    assert_eq!(pool_1.candidate_pool_hash, pool_2.candidate_pool_hash);
    assert_eq!(pool_1.candidates, pool_2.candidates);

    let selected_instruments: BTreeSet<String> = pool_1
        .candidates
        .iter()
        .map(|c| c.instrument_id.clone())
        .collect();
    assert_eq!(
        selected_instruments,
        BTreeSet::from([inst_clean.clone(), inst_crypto.clone()]),
        "only clean equity and clean crypto segments may enter candidate pool"
    );
    assert_eq!(pool_1.candidates.len(), 4);
    let warmup_1_seg = pool_1
        .candidates
        .iter()
        .find(|c| c.instrument_id == inst_clean && c.warmup_bar_count == 1)
        .expect("clean equity segment with 1 warmup bar");
    assert!(warmup_1_seg.warmup_start_at < warmup_1_seg.history_start_at);

    let pool_eq = store
        .query_training_candidate_pool(&CandidatePoolQuery {
            market_category: "us_equity".to_string(),
            ..query_all.clone()
        })
        .unwrap();
    assert!(pool_eq
        .candidates
        .iter()
        .all(|c| c.instrument_id == inst_clean));

    let seg0 = &pool_1.candidates[0];
    let seg1 = &pool_1.candidates[3];
    let now_str = "2026-11-12T11:00:00+00:00".to_string();
    store
        .create_training_run(
            &SimulationRunRecord {
                run_id: "RUN-REDRAW".to_string(),
                kind: "training".to_string(),
                seed: 777,
                dataset_version: dsv,
                engine_version: "m4-v1".to_string(),
                fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
                quote_currency: seg0.quote_currency.clone(),
                initial_cash: Decimal::from(50_000),
                status: SimRunStatus::Preparing,
                parent_run_id: None,
                latest_seq: 0,
                latest_state_hash: "H0".to_string(),
                created_at: now_str.clone(),
                updated_at: now_str,
                finished_at: None,
            },
            &TrainingSessionRecord {
                run_id: "RUN-REDRAW".to_string(),
                hidden_mode: true,
                market_category: "all".to_string(),
                display_symbol: "标的 A".to_string(),
                real_instrument_id: seg0.instrument_id.clone(),
                candidate_pool_hash: pool_1.candidate_pool_hash.clone(),
                redraw_count: 0,
                redraw_history: Vec::new(),
                warmup_start_at: seg0.warmup_start_at,
                history_start_at: seg0.history_start_at,
                train_start_at: seg0.train_start_at,
                train_end_at: seg0.train_end_at,
                reveal_end_at: seg0.reveal_end_at,
                visible_until: seg0.initial_visible_until,
                current_step_index: 0,
                total_steps: seg0.train_len as i64,
                revealed: false,
                timeframe: "1d".to_string(),
                practice_account_id: None,
                fill_rule_version: None,
            },
            None,
        )
        .unwrap();

    let redrawn = store
        .redraw_training_session_before_start(
            "RUN-REDRAW",
            seg1,
            "标的 B",
            "H0-REDRAWN",
            parse_rfc3339_utc("2026-11-12T11:01:00+00:00", "redrawn_at").unwrap(),
        )
        .unwrap();
    assert_eq!(redrawn.redraw_count, 1);
    assert_eq!(redrawn.redraw_history.len(), 1);
    assert_eq!(redrawn.real_instrument_id, seg1.instrument_id);
    assert_eq!(redrawn.display_symbol, "标的 B");

    store
        .start_training_run(
            "RUN-REDRAW",
            parse_rfc3339_utc("2026-11-12T11:02:00+00:00", "started_at").unwrap(),
        )
        .unwrap();

    assert!(store
        .redraw_training_session_before_start(
            "RUN-REDRAW",
            seg0,
            "标的 C",
            "H-ILLEGAL",
            parse_rfc3339_utc("2026-11-12T11:03:00+00:00", "redrawn_at").unwrap(),
        )
        .is_err());
    assert!(store
        .conn()
        .execute(
            "UPDATE training_session SET redraw_count = 99 WHERE run_id = 'RUN-REDRAW'",
            []
        )
        .is_err());
}

#[test]
fn test_m4_a08_fee_profile_immutability_versioning_and_builtin_provenance() {
    let tmp = TestTempDir::new("a08_fees");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

    let dsv = store
        .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
        .unwrap()
        .version_num;
    let aapl = store
        .register_market_instrument("NASDAQ", "AAPL", false)
        .unwrap();
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(), 4);
    let bars: Vec<BarRecord> = days
        .iter()
        .map(|&d| make_equity_bar(&aapl, d, "180", dsv))
        .collect();
    store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

    let builtins = store.list_fee_profile_versions().unwrap();
    assert_eq!(builtins.len(), 3);
    assert!(builtins
        .iter()
        .all(|b| b.is_builtin && b.is_scenario_assumption && !b.assumption_note.is_empty()));

    let mut try_mutate_builtin = store
        .get_fee_profile_version(BUILTIN_FEE_ZERO_COST_ID)
        .unwrap();
    try_mutate_builtin.fixed_fee_per_order = Decimal::from(5);
    assert!(store
        .mutate_fee_profile_version_in_place(&try_mutate_builtin)
        .is_err());
    assert!(store
        .delete_fee_profile_version(BUILTIN_FEE_ZERO_COST_ID)
        .is_err());

    let unverified_builtin = FeeProfileVersionRecord {
        id: "FPV:BAD:BROKER:v1".to_string(),
        profile_key: "bad_broker".to_string(),
        version_num: 1,
        name: "Unverified Broker Preset".to_string(),
        market_category: "us_equity".to_string(),
        venue: Some("NASDAQ".to_string()),
        notional_rate: Decimal::ZERO,
        per_unit_fee: Decimal::new(5, 3),
        fixed_fee_per_order: Decimal::ZERO,
        min_fee_per_order: Some(Decimal::ONE),
        max_fee_per_order: None,
        fee_currency_mode: FeeCurrencyMode::Quote,
        rounding_mode: FeeRoundingMode::HalfUp,
        rounding_scale: 2,
        charging_tier: FeeChargingTier::PerOrder,
        slippage_mode: SlippageMode::None,
        slippage_value: Decimal::ZERO,
        half_spread_bps: Decimal::ZERO,
        rules_json: "{}".to_string(),
        is_builtin: true,
        is_scenario_assumption: false,
        official_source_url: None,
        official_retrieved_at: None,
        assumption_note: "Missing official provenance".to_string(),
        created_at: "2026-11-12T00:00:00+00:00".to_string(),
    };
    assert!(store
        .insert_fee_profile_version(&unverified_builtin)
        .is_err());

    let v1_draft = FeeProfileVersionRecord {
        id: String::new(),
        profile_key: "my_us_desk".to_string(),
        version_num: 1,
        name: "My US Desk v1".to_string(),
        market_category: "us_equity".to_string(),
        venue: None,
        notional_rate: Decimal::ZERO,
        per_unit_fee: Decimal::new(5, 3),
        fixed_fee_per_order: Decimal::ONE,
        min_fee_per_order: Some(Decimal::ONE),
        max_fee_per_order: Some(Decimal::from(25)),
        fee_currency_mode: FeeCurrencyMode::Quote,
        rounding_mode: FeeRoundingMode::HalfUp,
        rounding_scale: 2,
        charging_tier: FeeChargingTier::PerOrder,
        slippage_mode: SlippageMode::Ticks,
        slippage_value: Decimal::ONE,
        half_spread_bps: Decimal::from(2),
        rules_json: r#"{"desk":"v1"}"#.to_string(),
        is_builtin: false,
        is_scenario_assumption: true,
        official_source_url: None,
        official_retrieved_at: None,
        assumption_note: "Custom desk assumption v1".to_string(),
        created_at: "2026-11-12T12:00:00+00:00".to_string(),
    };
    let v1 = store.create_next_fee_profile_version(v1_draft).unwrap();
    assert_eq!(v1.id, "FPV:MY_US_DESK:v1");
    assert_eq!(v1.version_num, 1);

    let make_run = |run_id: &str, fp_id: &str| {
        (
            SimulationRunRecord {
                run_id: run_id.to_string(),
                kind: "training".to_string(),
                seed: 101,
                dataset_version: dsv,
                engine_version: "m4-v1".to_string(),
                fee_profile_version_id: fp_id.to_string(),
                quote_currency: "USD".to_string(),
                initial_cash: Decimal::from(100_000),
                status: SimRunStatus::Active,
                parent_run_id: None,
                latest_seq: 0,
                latest_state_hash: "H0".to_string(),
                created_at: "2026-11-12T12:01:00+00:00".to_string(),
                updated_at: "2026-11-12T12:01:00+00:00".to_string(),
                finished_at: None,
            },
            TrainingSessionRecord {
                run_id: run_id.to_string(),
                hidden_mode: false,
                market_category: "us_equity".to_string(),
                display_symbol: "NASDAQ:AAPL".to_string(),
                real_instrument_id: aapl.clone(),
                candidate_pool_hash: "POOL-1".to_string(),
                redraw_count: 0,
                redraw_history: Vec::new(),
                warmup_start_at: bars[0].open_at,
                history_start_at: bars[0].open_at,
                train_start_at: bars[1].open_at,
                train_end_at: bars[2].close_at,
                reveal_end_at: bars[3].close_at,
                visible_until: bars[0].close_at,
                current_step_index: 0,
                total_steps: 2,
                revealed: false,
                timeframe: "1d".to_string(),
                practice_account_id: None,
                fill_rule_version: None,
            },
        )
    };

    let (r1, s1) = make_run("RUN-FP-1", &v1.id);
    store.create_training_run(&r1, &s1, None).unwrap();

    let mut mutated_v1 = v1.clone();
    mutated_v1.fixed_fee_per_order = Decimal::from(99);
    assert!(store
        .mutate_fee_profile_version_in_place(&mutated_v1)
        .is_err());
    assert!(store.delete_fee_profile_version(&v1.id).is_err());
    assert!(store
        .conn()
        .execute(
            "UPDATE fee_profile_version SET fixed_fee_per_order = '99' WHERE id = ?1",
            params![v1.id],
        )
        .is_err());
    assert!(store
        .conn()
        .execute(
            "DELETE FROM fee_profile_version WHERE id = ?1",
            params![v1.id],
        )
        .is_err());

    let mut v2_draft = v1.clone();
    v2_draft.name = "My US Desk v2".to_string();
    v2_draft.fixed_fee_per_order = Decimal::from(3);
    let v2 = store.create_next_fee_profile_version(v2_draft).unwrap();
    assert_eq!(v2.id, "FPV:MY_US_DESK:v2");
    assert_eq!(v2.version_num, 2);

    let (r2, s2) = make_run("RUN-FP-2", &v2.id);
    store.create_training_run(&r2, &s2, None).unwrap();

    let r1_fp = store
        .get_fee_profile_version(
            &store
                .get_simulation_run("RUN-FP-1")
                .unwrap()
                .fee_profile_version_id,
        )
        .unwrap();
    let r2_fp = store
        .get_fee_profile_version(
            &store
                .get_simulation_run("RUN-FP-2")
                .unwrap()
                .fee_profile_version_id,
        )
        .unwrap();
    assert_eq!(r1_fp.fixed_fee_per_order, Decimal::ONE);
    assert_eq!(r2_fp.fixed_fee_per_order, Decimal::from(3));
}

#[test]
fn test_m4_a09_and_a10_atomic_step_rollback_checkpoint_recovery_damage_and_retry() {
    let tmp = TestTempDir::new("a09_a10");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

    let dsv = store
        .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
        .unwrap()
        .version_num;
    let aapl = store
        .register_market_instrument("NASDAQ", "AAPL", false)
        .unwrap();
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 5, 1).unwrap(), 30);
    let bars: Vec<BarRecord> = days
        .iter()
        .enumerate()
        .map(|(i, &d)| make_equity_bar(&aapl, d, &(200 + i as i64).to_string(), dsv))
        .collect();
    store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

    let init_cp = SimCheckpointRecord {
        run_id: "RUN-A09".to_string(),
        seq: 0,
        reason: SimCheckpointReason::Initial,
        state_json: r#"{"step":0}"#.to_string(),
        state_hash: "HASH-0".to_string(),
        created_at: "2026-11-12T13:00:00+00:00".to_string(),
    };
    store
        .create_training_run(
            &SimulationRunRecord {
                run_id: "RUN-A09".to_string(),
                kind: "training".to_string(),
                seed: 20261112,
                dataset_version: dsv,
                engine_version: "m4-v1".to_string(),
                fee_profile_version_id: BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string(),
                quote_currency: "USD".to_string(),
                initial_cash: Decimal::from(100_000),
                status: SimRunStatus::Active,
                parent_run_id: None,
                latest_seq: 0,
                latest_state_hash: "HASH-0".to_string(),
                created_at: "2026-11-12T13:00:00+00:00".to_string(),
                updated_at: "2026-11-12T13:00:00+00:00".to_string(),
                finished_at: None,
            },
            &TrainingSessionRecord {
                run_id: "RUN-A09".to_string(),
                hidden_mode: true,
                market_category: "us_equity".to_string(),
                display_symbol: "标的 A".to_string(),
                real_instrument_id: aapl.clone(),
                candidate_pool_hash: "POOL-A09".to_string(),
                redraw_count: 0,
                redraw_history: Vec::new(),
                warmup_start_at: bars[0].open_at,
                history_start_at: bars[1].open_at,
                train_start_at: bars[3].open_at,
                train_end_at: bars[27].close_at,
                reveal_end_at: bars[29].close_at,
                visible_until: bars[2].close_at,
                current_step_index: 0,
                total_steps: 25,
                revealed: false,
                timeframe: "1d".to_string(),
                practice_account_id: None,
                fill_rule_version: None,
            },
            Some(&init_cp),
        )
        .unwrap();

    for seq in 1..=21i64 {
        let vis = bars[(2 + seq) as usize].close_at;
        let state_hash = format!("HASH-{seq}");
        let checkpoint = if seq == 20 {
            Some(SimCheckpointRecord {
                run_id: "RUN-A09".to_string(),
                seq: 20,
                reason: SimCheckpointReason::Periodic20,
                state_json: r#"{"step":20}"#.to_string(),
                state_hash: state_hash.clone(),
                created_at: "2026-11-12T13:20:00+00:00".to_string(),
            })
        } else {
            None
        };
        let (orders, fills, notes) = if seq == 21 {
            (
                vec![SimOrderRecord {
                    order_id: "ORD-1".to_string(),
                    run_id: "RUN-A09".to_string(),
                    instrument_id: aapl.clone(),
                    order_seq: 1,
                    order_type: "market".to_string(),
                    side: "buy".to_string(),
                    tif: "day".to_string(),
                    qty: Decimal::from(10),
                    limit_price: None,
                    stop_price: None,
                    parent_group_id: None,
                    group_role: None,
                    status: SimOrderStatus::Filled,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at: bars[22].close_at,
                    eligible_at: Some(bars[23].open_at),
                    updated_at: vis,
                    payload_json: "{}".to_string(),
                }],
                vec![SimFillRecord {
                    fill_id: "FILL-1".to_string(),
                    run_id: "RUN-A09".to_string(),
                    order_id: "ORD-1".to_string(),
                    instrument_id: aapl.clone(),
                    bar_close_at: vis,
                    side: "buy".to_string(),
                    qty: Decimal::from(10),
                    raw_price: Decimal::from(223),
                    fill_price: parse_decimal_lossless("223.05").unwrap(),
                    fee_amount: Decimal::ONE,
                    fee_currency: "USD".to_string(),
                    ambiguous: false,
                    payload_json: "{}".to_string(),
                }],
                vec![TrainingNoteRecord {
                    note_id: "TNOTE-1".to_string(),
                    run_id: "RUN-A09".to_string(),
                    bar_index: 21,
                    visible_until: vis,
                    rationale: "突破20日高点买入".to_string(),
                    emotion_tag: Some("专注".to_string()),
                    linked_order_id: Some("ORD-1".to_string()),
                    archived_journal_note_id: None,
                    created_at: "2026-11-12T13:21:00+00:00".to_string(),
                    updated_at: "2026-11-12T13:21:00+00:00".to_string(),
                }],
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

        store
            .commit_simulation_step_atomic(&SimulationStepCommit {
                run_id: "RUN-A09".to_string(),
                expected_prev_seq: seq - 1,
                seq,
                event_kind: "next_bar".to_string(),
                command_json: format!(r#"{{"seq":{seq}}}"#),
                events_json: "[]".to_string(),
                new_status: SimRunStatus::Active,
                new_visible_until: vis,
                new_step_index: seq,
                new_state_hash: state_hash,
                orders_to_upsert: orders,
                fills_to_insert: fills,
                notes_to_upsert: notes,
                checkpoint,
                committed_at: vis,
            })
            .unwrap();
    }

    set_sim_step_fault_injection_for_test(true);
    let crash_res = store.commit_simulation_step_atomic(&SimulationStepCommit {
        run_id: "RUN-A09".to_string(),
        expected_prev_seq: 21,
        seq: 22,
        event_kind: "next_bar".to_string(),
        command_json: r#"{"seq":22}"#.to_string(),
        events_json: "[]".to_string(),
        new_status: SimRunStatus::Paused,
        new_visible_until: bars[24].close_at,
        new_step_index: 22,
        new_state_hash: "HASH-22".to_string(),
        orders_to_upsert: vec![SimOrderRecord {
            order_id: "ORD-CRASH".to_string(),
            run_id: "RUN-A09".to_string(),
            instrument_id: aapl.clone(),
            order_seq: 2,
            order_type: "market".to_string(),
            side: "sell".to_string(),
            tif: "day".to_string(),
            qty: Decimal::from(10),
            limit_price: None,
            stop_price: None,
            parent_group_id: None,
            group_role: None,
            status: SimOrderStatus::Filled,
            reject_code: None,
            cancel_reason: None,
            submitted_at: bars[23].close_at,
            eligible_at: Some(bars[24].open_at),
            updated_at: bars[24].close_at,
            payload_json: "{}".to_string(),
        }],
        fills_to_insert: vec![SimFillRecord {
            fill_id: "FILL-CRASH".to_string(),
            run_id: "RUN-A09".to_string(),
            order_id: "ORD-CRASH".to_string(),
            instrument_id: aapl.clone(),
            bar_close_at: bars[24].close_at,
            side: "sell".to_string(),
            qty: Decimal::from(10),
            raw_price: Decimal::from(224),
            fill_price: Decimal::from(224),
            fee_amount: Decimal::ONE,
            fee_currency: "USD".to_string(),
            ambiguous: false,
            payload_json: "{}".to_string(),
        }],
        notes_to_upsert: Vec::new(),
        checkpoint: None,
        committed_at: bars[24].close_at,
    });
    assert!(crash_res.is_err());

    let run_after_crash = store.get_simulation_run("RUN-A09").unwrap();
    let sess_after_crash = store.get_training_session("RUN-A09").unwrap();
    assert_eq!(run_after_crash.latest_seq, 21);
    assert_eq!(run_after_crash.latest_state_hash, "HASH-21");
    assert_eq!(run_after_crash.status, SimRunStatus::Active);
    assert_eq!(sess_after_crash.current_step_index, 21);
    assert_eq!(sess_after_crash.visible_until, bars[23].close_at);
    assert_eq!(store.list_sim_orders("RUN-A09").unwrap().len(), 1);
    assert_eq!(store.list_sim_fills("RUN-A09").unwrap().len(), 1);
    assert_eq!(
        store.list_simulation_events("RUN-A09", None).unwrap().len(),
        21
    );

    let outcome = store
        .verify_or_mark_run_damaged("RUN-A09", |bundle| {
            assert_eq!(bundle.latest_checkpoint.as_ref().unwrap().seq, 20);
            assert_eq!(bundle.events_after_checkpoint.len(), 1);
            assert_eq!(bundle.events_after_checkpoint[0].seq, 21);
            Ok(vec![(21, "HASH-21".to_string())])
        })
        .unwrap();
    assert!(matches!(outcome, SimulationRecoveryOutcome::Recovered(_)));

    let retry_cp = SimCheckpointRecord {
        run_id: "RUN-A09-RETRY".to_string(),
        seq: 0,
        reason: SimCheckpointReason::Initial,
        state_json: r#"{"step":0}"#.to_string(),
        state_hash: "HASH-0".to_string(),
        created_at: "2026-11-12T13:30:00+00:00".to_string(),
    };
    let (child_run, child_sess) = store
        .create_retry_training_run(
            "RUN-A09",
            "RUN-A09-RETRY",
            bars[2].close_at,
            "HASH-0",
            Some(&retry_cp),
            parse_rfc3339_utc("2026-11-12T13:30:00+00:00", "retry_at").unwrap(),
        )
        .unwrap();
    assert_eq!(child_run.parent_run_id.as_deref(), Some("RUN-A09"));
    assert_eq!(child_run.seed, 20261112);
    assert_eq!(child_run.dataset_version, dsv);
    assert_eq!(
        child_run.fee_profile_version_id,
        BUILTIN_FEE_US_EQUITY_PER_SHARE_ID
    );
    assert_eq!(child_run.latest_seq, 0);
    assert_eq!(child_sess.real_instrument_id, aapl);
    assert_eq!(child_sess.visible_until, bars[2].close_at);
    assert_eq!(child_sess.current_step_index, 0);
    assert!(store.list_sim_orders("RUN-A09-RETRY").unwrap().is_empty());
    assert_eq!(store.list_sim_orders("RUN-A09").unwrap().len(), 1);

    store
        .conn()
        .execute(
            "UPDATE simulation_event SET state_hash = 'TAMPERED-HASH' WHERE run_id = 'RUN-A09' AND seq = 21",
            [],
        )
        .unwrap();
    let damaged_outcome = store
        .verify_or_mark_run_damaged("RUN-A09", |_b| Ok(vec![(21, "HASH-21".to_string())]))
        .unwrap();
    match damaged_outcome {
        SimulationRecoveryOutcome::MarkedDamaged { run, reason } => {
            assert_eq!(run.status, SimRunStatus::Damaged);
            assert!(reason.contains("TAMPERED-HASH"));
        }
        other => panic!("expected MarkedDamaged, got {other:?}"),
    }
    assert_eq!(
        store.get_simulation_run("RUN-A09").unwrap().status,
        SimRunStatus::Damaged
    );
    assert!(store
        .commit_simulation_step_atomic(&SimulationStepCommit {
            run_id: "RUN-A09".to_string(),
            expected_prev_seq: 21,
            seq: 22,
            event_kind: "next_bar".to_string(),
            command_json: "{}".to_string(),
            events_json: "[]".to_string(),
            new_status: SimRunStatus::Active,
            new_visible_until: bars[24].close_at,
            new_step_index: 22,
            new_state_hash: "HASH-22".to_string(),
            orders_to_upsert: Vec::new(),
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: None,
            committed_at: bars[24].close_at,
        })
        .is_err());
}

#[test]
fn test_m4_a11_and_a13_schema_5_migration_backup_ledger_isolation_delete_and_note_archive() {
    let tmp = TestTempDir::new("a11_a13");
    let lib_dir = tmp.path().join("lib_v4_to_v5");
    let pre_mig_restore_dir = tmp.path().join("restored_v4_pre_migration");
    let v5_backup_dir = tmp.path().join("backup_v5");
    let v5_restore_dir = tmp.path().join("restored_v5");
    let future_v6_dir = tmp.path().join("lib_v6_unsupported");

    {
        let mut store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC").unwrap();
        store.downgrade_to_schema_4_for_test().unwrap();

        store
            .upsert_account(
                "ACC-REAL",
                "Real Brokerage",
                "broker",
                "ibkr",
                None,
                "active",
            )
            .unwrap();
        store
            .conn()
            .execute("UPDATE library SET ledger_revision = 7", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO economic_event (
                    id, account_id, type, occurred_at, sequence_no, recorded_at,
                    asset_id, instrument_id, quantity, price, quote_currency,
                    fee_amount, fee_currency, cost_known, correction_group_id,
                    revision, active_in_correction_group
                 ) VALUES (
                    'EV-REAL-1', 'ACC-REAL', 'buy', '2025-01-10T15:00:00Z', 1, '2025-01-10T15:00:00Z',
                    'ASSET:EQUITY:XNAS:AAPL', 'INST:EQUITY:XNAS:AAPL', '25', '185.50', 'USD',
                    '1.25', 'USD', 1, 'CG-REAL-1', 1, 1
                 )",
                [],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO holding_projection (
                    account_id, asset_id, quantity, carrying_cost, cost_known, ledger_revision
                 ) VALUES ('ACC-REAL', 'ASSET:EQUITY:XNAS:AAPL', '25', '4638.75', 1, 7)",
                [],
            )
            .unwrap();

        store
            .save_note_revision(
                "NOTE-REAL-1",
                "实盘复盘笔记",
                "post_trade",
                true,
                "这是v0.1.1真实交易笔记。",
                &StructuredNoteFields::default(),
                &["实盘".to_string()],
            )
            .unwrap();

        store
            .upsert_model_connection(&ModelConnectionRecord {
                id: "MC-LOCAL".to_string(),
                protocol: "openai_compatible".to_string(),
                base_url: "http://127.0.0.1:11434/v1".to_string(),
                model_id: "qwen2.5".to_string(),
                credential_ref: None,
                context_window: 32768,
                capabilities_json: "{}".to_string(),
                timeouts_json: "{}".to_string(),
                budget_json: "{}".to_string(),
                updated_at: "2025-01-10T16:00:00Z".to_string(),
            })
            .unwrap();
        let ai_sess = store.create_ai_session("AISESS-V4").unwrap();
        assert_eq!(ai_sess.schema_version, 1);
        store
            .create_ai_run(&AiRunRecord {
                id: "AIRUN-V4".to_string(),
                session_id: "AISESS-V4".to_string(),
                generation: 1,
                scope_json: "{}".to_string(),
                dataset_version: 0,
                ledger_revision: 7,
                tool_schema_hash: "TSH-1".to_string(),
                model_connection_id: "MC-LOCAL".to_string(),
                budget_json: "{}".to_string(),
                state: "completed".to_string(),
                checkpoint_ref: None,
                report_json: None,
                usage_json: None,
                created_at: "2025-01-10T16:01:00Z".to_string(),
                updated_at: "2025-01-10T16:01:00Z".to_string(),
            })
            .unwrap();
        store
            .append_ai_message(&AiMessageRecord {
                id: "AIMSG-V4".to_string(),
                session_id: "AISESS-V4".to_string(),
                run_id: "AIRUN-V4".to_string(),
                turn_id: 1,
                sequence: 1,
                kind: "assistant".to_string(),
                payload_json: r#"{"text":"v4 ai summary"}"#.to_string(),
                status: "completed".to_string(),
                created_at: "2025-01-10T16:01:05Z".to_string(),
            })
            .unwrap();
        store.sync_manifest().unwrap();
    }

    assert_eq!(
        SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
        4
    );
    assert!(SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
        .unwrap()
        .is_empty());

    set_schema_5_migration_fault_injection_for_test(true);
    let mig_err = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
        .err()
        .expect("injected schema 5 migration fault must fail");
    assert!(matches!(mig_err, StorageError::MigrationFailed(_)));
    assert_eq!(
        SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
        4
    );
    assert!(SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
        .unwrap()
        .is_empty());

    let pre_backups = SqliteStore::list_pre_migration_backups(&lib_dir, 4).unwrap();
    assert_eq!(pre_backups.len(), 1);
    let restored_v4_ver = SqliteStore::restore_consistent_backup_without_migration(
        &pre_backups[0],
        &pre_mig_restore_dir,
    )
    .unwrap();
    assert_eq!(restored_v4_ver, 4);

    let mut store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
        .expect("upgrade Schema 4 -> current schema");
    assert_eq!(
        SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(
        SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
            .unwrap()
            .len(),
        SCHEMA_5_TABLES.len()
    );
    assert_eq!(store.query_ledger_revision().unwrap(), 7);
    assert_eq!(store.get_note("NOTE-REAL-1").unwrap().title, "实盘复盘笔记");
    assert_eq!(store.list_ai_messages("AISESS-V4").unwrap().len(), 1);

    let v4_reader_err = SqliteStore::check_schema_compatibility_at_version(&lib_dir, 4)
        .expect_err("Schema 4 reader must refuse Schema 5 library");
    assert!(v4_reader_err
        .to_string()
        .contains("database schema is newer than this build"));

    {
        let mut s6 = SqliteStore::open_or_init(&future_v6_dir, "USD", "USD", "UTC").unwrap();
        s6.conn_mut()
            .execute(
                "UPDATE library SET schema_version = ?1",
                [CURRENT_SCHEMA_VERSION + 1],
            )
            .unwrap();
    }
    let v6_err = SqliteStore::open_or_init(&future_v6_dir, "USD", "USD", "UTC")
        .err()
        .expect("opening a newer schema must fail clearly");
    assert!(v6_err
        .to_string()
        .contains("database schema is newer than this build"));

    let dsv = store
        .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
        .unwrap()
        .version_num;
    let aapl = store
        .register_market_instrument("NASDAQ", "AAPL", false)
        .unwrap();
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 6, 2).unwrap(), 6);
    let bars: Vec<BarRecord> = days
        .iter()
        .map(|&d| make_equity_bar(&aapl, d, "195", dsv))
        .collect();
    store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

    for rid in ["RUN-PARENT", "RUN-OTHER"] {
        let cp0 = SimCheckpointRecord {
            run_id: rid.to_string(),
            seq: 0,
            reason: SimCheckpointReason::Initial,
            state_json: "{}".to_string(),
            state_hash: "H0".to_string(),
            created_at: "2026-11-12T14:00:00+00:00".to_string(),
        };
        store
            .create_training_run(
                &SimulationRunRecord {
                    run_id: rid.to_string(),
                    kind: "training".to_string(),
                    seed: 99,
                    dataset_version: dsv,
                    engine_version: "m4-v1".to_string(),
                    fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
                    quote_currency: "USD".to_string(),
                    initial_cash: Decimal::from(100_000),
                    status: SimRunStatus::Active,
                    parent_run_id: None,
                    latest_seq: 0,
                    latest_state_hash: "H0".to_string(),
                    created_at: "2026-11-12T14:00:00+00:00".to_string(),
                    updated_at: "2026-11-12T14:00:00+00:00".to_string(),
                    finished_at: None,
                },
                &TrainingSessionRecord {
                    run_id: rid.to_string(),
                    hidden_mode: true,
                    market_category: "us_equity".to_string(),
                    display_symbol: "标的 A".to_string(),
                    real_instrument_id: aapl.clone(),
                    candidate_pool_hash: "POOL-A11".to_string(),
                    redraw_count: 0,
                    redraw_history: Vec::new(),
                    warmup_start_at: bars[0].open_at,
                    history_start_at: bars[0].open_at,
                    train_start_at: bars[1].open_at,
                    train_end_at: bars[3].close_at,
                    reveal_end_at: bars[5].close_at,
                    visible_until: bars[0].close_at,
                    current_step_index: 0,
                    total_steps: 3,
                    revealed: false,
                    timeframe: "1d".to_string(),
                    practice_account_id: None,
                    fill_rule_version: None,
                },
                Some(&cp0),
            )
            .unwrap();

        store
            .commit_simulation_step_atomic(&SimulationStepCommit {
                run_id: rid.to_string(),
                expected_prev_seq: 0,
                seq: 1,
                event_kind: "finish".to_string(),
                command_json: "{}".to_string(),
                events_json: "[]".to_string(),
                new_status: SimRunStatus::Finished,
                new_visible_until: bars[3].close_at,
                new_step_index: 3,
                new_state_hash: "H1".to_string(),
                orders_to_upsert: vec![SimOrderRecord {
                    order_id: "ORD-1".to_string(),
                    run_id: rid.to_string(),
                    instrument_id: aapl.clone(),
                    order_seq: 1,
                    order_type: "market".to_string(),
                    side: "buy".to_string(),
                    tif: "day".to_string(),
                    qty: Decimal::from(5),
                    limit_price: None,
                    stop_price: None,
                    parent_group_id: None,
                    group_role: None,
                    status: SimOrderStatus::Filled,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at: bars[1].close_at,
                    eligible_at: Some(bars[2].open_at),
                    updated_at: bars[2].close_at,
                    payload_json: "{}".to_string(),
                }],
                fills_to_insert: vec![SimFillRecord {
                    fill_id: "FILL-1".to_string(),
                    run_id: rid.to_string(),
                    order_id: "ORD-1".to_string(),
                    instrument_id: aapl.clone(),
                    bar_close_at: bars[2].close_at,
                    side: "buy".to_string(),
                    qty: Decimal::from(5),
                    raw_price: Decimal::from(195),
                    fill_price: Decimal::from(195),
                    fee_amount: Decimal::ZERO,
                    fee_currency: "USD".to_string(),
                    ambiguous: false,
                    payload_json: "{}".to_string(),
                }],
                notes_to_upsert: vec![TrainingNoteRecord {
                    note_id: format!("TN-{rid}"),
                    run_id: rid.to_string(),
                    bar_index: 2,
                    visible_until: bars[2].close_at,
                    rationale: "缩量回踩支撑做多".to_string(),
                    emotion_tag: Some("冷静".to_string()),
                    linked_order_id: Some("ORD-1".to_string()),
                    archived_journal_note_id: None,
                    created_at: "2026-11-12T14:02:00+00:00".to_string(),
                    updated_at: "2026-11-12T14:02:00+00:00".to_string(),
                }],
                checkpoint: Some(SimCheckpointRecord {
                    run_id: rid.to_string(),
                    seq: 1,
                    reason: SimCheckpointReason::Finish,
                    state_json: r#"{"done":true}"#.to_string(),
                    state_hash: "H1".to_string(),
                    created_at: "2026-11-12T14:05:00+00:00".to_string(),
                }),
                committed_at: bars[3].close_at,
            })
            .unwrap();
    }

    store
        .create_retry_training_run(
            "RUN-PARENT",
            "RUN-CHILD",
            bars[0].close_at,
            "H0",
            None,
            parse_rfc3339_utc("2026-11-12T14:10:00+00:00", "retry_at").unwrap(),
        )
        .unwrap();

    let archived_note = store
        .archive_training_note_to_journal(
            "RUN-PARENT",
            "TN-RUN-PARENT",
            "NOTE-ARCHIVED-SIM-1",
            "模拟训练归档复盘",
        )
        .unwrap();
    assert!(archived_note.body_markdown.contains("来自模拟训练"));
    assert!(archived_note.tags.iter().any(|t| t == "来自模拟训练"));

    let tn_after_archive = store.get_training_note("TN-RUN-PARENT").unwrap();
    assert_eq!(tn_after_archive.rationale, "缩量回踩支撑做多");
    assert_eq!(tn_after_archive.emotion_tag.as_deref(), Some("冷静"));
    assert_eq!(tn_after_archive.linked_order_id.as_deref(), Some("ORD-1"));
    assert_eq!(
        tn_after_archive.archived_journal_note_id.as_deref(),
        Some("NOTE-ARCHIVED-SIM-1")
    );

    assert_eq!(store.query_ledger_revision().unwrap(), 7);
    let real_ev_count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM economic_event", [], |r| r.get(0))
        .unwrap();
    assert_eq!(real_ev_count, 1);

    let export_snap = store.export_simulation_tables_snapshot().unwrap();
    assert_eq!(export_snap.fee_profile_versions.len(), 3);
    assert_eq!(export_snap.simulation_runs.len(), 3);
    assert_eq!(export_snap.training_sessions.len(), 3);
    assert_eq!(export_snap.sim_orders.len(), 2);
    assert_eq!(export_snap.sim_fills.len(), 2);
    assert_eq!(export_snap.simulation_events.len(), 2);
    assert_eq!(export_snap.sim_checkpoints.len(), 4);
    assert_eq!(export_snap.training_notes.len(), 2);

    let backup_manifest = store.create_consistent_backup(&v5_backup_dir).unwrap();
    for tbl in SCHEMA_5_TABLES {
        assert!(
            backup_manifest
                .table_row_counts
                .get(*tbl)
                .copied()
                .unwrap_or(0)
                > 0,
            "backup manifest must record non-zero row count for {tbl}"
        );
    }
    let restored_v5 =
        SqliteStore::restore_consistent_backup(&v5_backup_dir, &v5_restore_dir).unwrap();
    assert_eq!(
        restored_v5.export_simulation_tables_snapshot().unwrap(),
        export_snap
    );
    assert_eq!(restored_v5.query_ledger_revision().unwrap(), 7);
    drop(restored_v5);

    let preview = store.preview_delete_training_run("RUN-PARENT").unwrap();
    assert_eq!(preview.child_run_ids, vec!["RUN-CHILD".to_string()]);
    assert_eq!(preview.order_count, 1);
    assert_eq!(preview.fill_count, 1);
    assert_eq!(preview.note_count, 1);
    assert_eq!(preview.archived_journal_note_count, 1);
    assert_eq!(preview.checkpoint_count, 2);
    assert_eq!(preview.event_count, 1);

    let deleted = store.delete_training_run("RUN-PARENT").unwrap();
    assert_eq!(deleted, preview);
    assert!(store.get_simulation_run("RUN-PARENT").is_err());
    assert!(store.list_sim_orders("RUN-PARENT").unwrap().is_empty());
    assert!(store.list_sim_fills("RUN-PARENT").unwrap().is_empty());

    let surviving_child = store.get_simulation_run("RUN-CHILD").unwrap();
    assert_eq!(surviving_child.parent_run_id.as_deref(), Some("RUN-PARENT"));
    assert_eq!(store.list_sim_fills("RUN-OTHER").unwrap().len(), 1);
    assert_eq!(
        store.get_note("NOTE-ARCHIVED-SIM-1").unwrap().title,
        "模拟训练归档复盘"
    );
    assert_eq!(store.get_note("NOTE-REAL-1").unwrap().title, "实盘复盘笔记");
    assert_eq!(store.query_ledger_revision().unwrap(), 7);
}

#[test]
fn test_m5_k01_large_1h_pool_excludes_gap_and_jump_windows_quickly() {
    let tmp = TestTempDir::new("m5_k01_pool");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
    let inst = store
        .register_market_instrument("BINANCE", "BTC/USDT", false)
        .unwrap();
    let dsv = store
        .create_dataset_version("binance", "public_open", "{}", "exact", "exact")
        .unwrap()
        .version_num;
    const N: usize = 30_000;
    const GAP_AFTER: usize = 10_000;
    const JUMP_AT: usize = 20_000;
    let t0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2019, 1, 1, 0, 0, 0).unwrap();
    let mut bars = Vec::with_capacity(N);
    for h in 0..=N {
        if h == GAP_AFTER + 1 {
            continue;
        }
        let open_at = t0 + chrono::Duration::hours(h as i64);
        let close = if h >= JUMP_AT {
            Decimal::from(50)
        } else {
            Decimal::from(100)
        };
        bars.push(BarRecord {
            id: format!(
                "BAR:{inst}:1h:raw:binance:{}:{dsv}",
                open_at.format("%Y-%m-%dT%H:%MZ")
            ),
            instrument_id: inst.clone(),
            timeframe: "1h".into(),
            session: "utc_24_7".into(),
            session_date: open_at.date_naive(),
            open_at,
            close_at: open_at + chrono::Duration::hours(1),
            open: close,
            high: close + Decimal::ONE,
            low: close - Decimal::ONE,
            close,
            volume: Decimal::from(10),
            source: "binance".into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }
    store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();
    store.publish_dataset_version(dsv).unwrap();
    let query = CandidatePoolQuery {
        dataset_version: dsv,
        market_category: "crypto_spot".into(),
        instrument_id: Some(inst.clone()),
        start_date: None,
        end_date: None,
        warmup_bars: 0,
        history_len: 120,
        train_len: 250,
        reveal_len: 60,
        timeframe: "1h".into(),
        quality_filter: CandidateQualityFilter::PriceAffectingOnly,
    };
    let started = std::time::Instant::now();
    let pool = store.query_training_candidate_pool(&query).unwrap();
    let elapsed = started.elapsed();
    let len = 430usize;
    let total = bars.len() - len + 1;
    let excluded_by_pair = |pair: usize| (pair + 1).saturating_sub(len - 1)..=pair;
    let gap = excluded_by_pair(GAP_AFTER);
    let jump = excluded_by_pair(JUMP_AT - 2);
    assert!(gap.end() < jump.start());
    let expected = total - gap.clone().count() - jump.clone().count();
    assert_eq!(pool.candidates.len(), expected);
    let gap_hour = t0 + chrono::Duration::hours(GAP_AFTER as i64 + 1);
    let jump_hour = t0 + chrono::Duration::hours(JUMP_AT as i64);
    assert!(pool.candidates.iter().all(|c| {
        !(c.history_start_at <= gap_hour && c.reveal_end_at > gap_hour)
            && !(c.history_start_at < jump_hour && c.reveal_end_at > jump_hour)
    }));
    let budget = if cfg!(debug_assertions) { 20.0 } else { 2.0 };
    assert!(
        elapsed.as_secs_f64() < budget,
        "1h pool over {N} bars took {elapsed:?} (budget {budget} s)"
    );
}

#[test]
fn test_data_b04_b14_b15_b16_b17_b18_m4_m5_regressions() {
    let tmp = TestTempDir::new("b04_b18");
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
    let btc = store
        .register_market_instrument("BINANCE", "BTC/USDT", false)
        .unwrap();
    let dsv = store
        .create_dataset_version("binance", "public_open", "{}", "exact", "exact")
        .unwrap()
        .version_num;

    // 10 contiguous 1h bars + 1 truncated 11th bar (30m instead of 60m) to test DATA-B04 & DATA-B16.
    let t0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2025, 7, 1, 0, 0, 0).unwrap();
    let mut bars_1h = Vec::new();
    for h in 0..11i64 {
        let open_at = t0 + chrono::Duration::hours(h);
        let close_at = if h == 10 {
            open_at + chrono::Duration::minutes(30) // truncated final bar
        } else {
            open_at + chrono::Duration::hours(1)
        };
        let px = Decimal::from(60_000 + h * 10);
        bars_1h.push(BarRecord {
            id: format!("BAR:{btc}:1h:raw:binance:h{h}:{dsv}"),
            instrument_id: btc.clone(),
            timeframe: "1h".into(),
            session: "utc_24_7".into(),
            session_date: open_at.date_naive(),
            open_at,
            close_at,
            open: px,
            high: px + Decimal::from(5),
            low: px - Decimal::from(5),
            close: px,
            volume: Decimal::from(10),
            source: "binance".into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }
    store
        .commit_bars_and_checkpoint(&bars_1h, dsv, None)
        .unwrap();

    // DATA-B16: window ending on truncated bar h=10 must be excluded by bar_valid!
    let pool = store
        .query_training_candidate_pool(&CandidatePoolQuery {
            dataset_version: dsv,
            market_category: "crypto_spot".into(),
            instrument_id: Some(btc.clone()),
            start_date: None,
            end_date: None,
            warmup_bars: 0,
            history_len: 3,
            train_len: 4,
            reveal_len: 3,
            timeframe: "1h".into(),
            quality_filter: CandidateQualityFilter::PriceAffectingOnly,
        })
        .unwrap();
    assert_eq!(
        pool.candidates.len(),
        1,
        "only window [0..=9] should pass; window [1..=10] has truncated final bar"
    );
    assert_eq!(pool.candidates[0].reveal_end_at, bars_1h[9].close_at);

    // DATA-B04: abandoned 1h run must return full 1h reveal bars (not empty!).
    let seg = &pool.candidates[0];
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    store
        .create_training_run(
            &SimulationRunRecord {
                run_id: "RUN-1H-ABANDON".into(),
                kind: "training".into(),
                seed: 7,
                dataset_version: dsv,
                engine_version: "m5-v1".into(),
                fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.into(),
                quote_currency: "USDT".into(),
                initial_cash: Decimal::from(10_000),
                status: SimRunStatus::Active,
                parent_run_id: None,
                latest_seq: 0,
                latest_state_hash: "H0".into(),
                created_at: now_str.clone(),
                updated_at: now_str.clone(),
                finished_at: None,
            },
            &TrainingSessionRecord {
                run_id: "RUN-1H-ABANDON".into(),
                hidden_mode: true,
                market_category: "crypto_spot".into(),
                display_symbol: "标的 1H".into(),
                real_instrument_id: btc.clone(),
                candidate_pool_hash: pool.candidate_pool_hash.clone(),
                redraw_count: 0,
                redraw_history: Vec::new(),
                warmup_start_at: seg.warmup_start_at,
                history_start_at: seg.history_start_at,
                train_start_at: seg.train_start_at,
                train_end_at: seg.train_end_at,
                reveal_end_at: seg.reveal_end_at,
                visible_until: seg.initial_visible_until,
                current_step_index: 0,
                total_steps: 4,
                revealed: false,
                timeframe: "1h".into(),
                practice_account_id: None,
                fill_rule_version: Some("m5-v1".into()),
            },
            Some(&SimCheckpointRecord {
                run_id: "RUN-1H-ABANDON".into(),
                seq: 0,
                reason: SimCheckpointReason::Initial,
                state_json: "{}".into(),
                state_hash: "H0".into(),
                created_at: now_str.clone(),
            }),
        )
        .unwrap();

    // DATA-B14: cannot regress started run back to Preparing; order qty/limit/stop/tif amendments persist.
    let err_prep = store.commit_simulation_step_atomic(&SimulationStepCommit {
        run_id: "RUN-1H-ABANDON".into(),
        expected_prev_seq: 0,
        seq: 1,
        event_kind: "illegal_reset".into(),
        command_json: "{}".into(),
        events_json: "[]".into(),
        new_status: SimRunStatus::Preparing,
        new_visible_until: seg.initial_visible_until,
        new_step_index: 0,
        new_state_hash: "H-BAD".into(),
        orders_to_upsert: Vec::new(),
        fills_to_insert: Vec::new(),
        notes_to_upsert: Vec::new(),
        checkpoint: None,
        committed_at: now,
    });
    assert!(err_prep.is_err(), "active run must not revert to preparing");

    let ord_v1 = SimOrderRecord {
        order_id: "ORD-AMEND".into(),
        run_id: "RUN-1H-ABANDON".into(),
        instrument_id: btc.clone(),
        order_seq: 1,
        order_type: "limit".into(),
        side: "buy".into(),
        tif: "day".into(),
        qty: Decimal::from(1),
        limit_price: Some(Decimal::from(59_000)),
        stop_price: None,
        parent_group_id: None,
        group_role: None,
        status: SimOrderStatus::Active,
        reject_code: None,
        cancel_reason: None,
        submitted_at: bars_1h[2].close_at,
        eligible_at: Some(bars_1h[3].open_at),
        updated_at: bars_1h[2].close_at,
        payload_json: "{}".into(),
    };
    store
        .commit_simulation_step_atomic(&SimulationStepCommit {
            run_id: "RUN-1H-ABANDON".into(),
            expected_prev_seq: 0,
            seq: 1,
            event_kind: "submit".into(),
            command_json: "{}".into(),
            events_json: "[]".into(),
            new_status: SimRunStatus::Active,
            new_visible_until: bars_1h[3].close_at,
            new_step_index: 1,
            new_state_hash: "H1".into(),
            orders_to_upsert: vec![ord_v1.clone()],
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: None,
            committed_at: now,
        })
        .unwrap();

    let mut ord_v2 = ord_v1;
    ord_v2.qty = Decimal::from(2);
    ord_v2.limit_price = Some(Decimal::from(59_500));
    ord_v2.stop_price = Some(Decimal::from(59_400));
    ord_v2.tif = "gtc".into();
    store
        .commit_simulation_step_atomic(&SimulationStepCommit {
            run_id: "RUN-1H-ABANDON".into(),
            expected_prev_seq: 1,
            seq: 2,
            event_kind: "amend_and_abandon".into(),
            command_json: "{}".into(),
            events_json: "[]".into(),
            new_status: SimRunStatus::Abandoned,
            new_visible_until: bars_1h[4].close_at,
            new_step_index: 2,
            new_state_hash: "H2".into(),
            orders_to_upsert: vec![ord_v2],
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: None,
            committed_at: now,
        })
        .unwrap();

    let persisted_ord = store.get_sim_order("RUN-1H-ABANDON", "ORD-AMEND").unwrap();
    assert_eq!(persisted_ord.qty, Decimal::from(2));
    assert_eq!(persisted_ord.limit_price, Some(Decimal::from(59_500)));
    assert_eq!(persisted_ord.stop_price, Some(Decimal::from(59_400)));
    assert_eq!(persisted_ord.tif, "gtc");

    let reveal = store
        .query_post_finish_reveal("RUN-1H-ABANDON")
        .expect("abandoned 1h run must return reveal bars");
    assert_eq!(reveal.all_bars_through_reveal.len(), 10);
    assert_eq!(reveal.future_reveal_bars.len(), 3);

    // DATA-B18: seq=0 checkpoint hash mismatch against simulation_run.latest_state_hash marks damaged.
    store
        .create_training_run(
            &SimulationRunRecord {
                run_id: "RUN-SEQ0-TAMPER".into(),
                kind: "training".into(),
                seed: 8,
                dataset_version: dsv,
                engine_version: "m5-v1".into(),
                fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.into(),
                quote_currency: "USDT".into(),
                initial_cash: Decimal::from(10_000),
                status: SimRunStatus::Active,
                parent_run_id: None,
                latest_seq: 0,
                latest_state_hash: "H0-RUN".into(),
                created_at: now_str.clone(),
                updated_at: now_str.clone(),
                finished_at: None,
            },
            &TrainingSessionRecord {
                run_id: "RUN-SEQ0-TAMPER".into(),
                hidden_mode: false,
                market_category: "crypto_spot".into(),
                display_symbol: "BTC".into(),
                real_instrument_id: btc.clone(),
                candidate_pool_hash: pool.candidate_pool_hash.clone(),
                redraw_count: 0,
                redraw_history: Vec::new(),
                warmup_start_at: seg.warmup_start_at,
                history_start_at: seg.history_start_at,
                train_start_at: seg.train_start_at,
                train_end_at: seg.train_end_at,
                reveal_end_at: seg.reveal_end_at,
                visible_until: seg.initial_visible_until,
                current_step_index: 0,
                total_steps: 4,
                revealed: false,
                timeframe: "1h".into(),
                practice_account_id: None,
                fill_rule_version: Some("m5-v1".into()),
            },
            Some(&SimCheckpointRecord {
                run_id: "RUN-SEQ0-TAMPER".into(),
                seq: 0,
                reason: SimCheckpointReason::Initial,
                state_json: "{}".into(),
                state_hash: "H0-CP-TAMPERED".into(),
                created_at: now_str,
            }),
        )
        .unwrap();
    let seq0_res = store
        .verify_or_mark_run_damaged("RUN-SEQ0-TAMPER", |_| Ok(Vec::new()))
        .unwrap();
    assert!(matches!(
        seq0_res,
        SimulationRecoveryOutcome::MarkedDamaged { .. }
    ));

    // DATA-B17: practice account guards (non-negative opening deposit, positive allocation, non-existent/double-allocation rejected).
    assert!(store
        .create_practice_account("USD", Decimal::from(-100), now)
        .is_err());
    let pa = store
        .create_practice_account("USDT", Decimal::from(5_000), now)
        .unwrap();
    assert!(store
        .allocate_practice_run("NON-EXISTENT-RUN", &pa.id, Decimal::from(100), now)
        .is_err());
    assert!(store
        .allocate_practice_run("RUN-1H-ABANDON", &pa.id, Decimal::ZERO, now)
        .is_err());
    assert!(store
        .allocate_practice_run("RUN-1H-ABANDON", &pa.id, Decimal::from(-50), now)
        .is_err());
    assert_eq!(
        store.practice_account_balance(&pa.id).unwrap(),
        Decimal::from(5_000),
        "failed allocations must not alter practice balance"
    );
    store
        .allocate_practice_run("RUN-1H-ABANDON", &pa.id, Decimal::from(1_000), now)
        .unwrap();
    assert!(
        store
            .allocate_practice_run("RUN-1H-ABANDON", &pa.id, Decimal::from(500), now)
            .is_err(),
        "double-allocating same training_session must be rejected"
    );
    assert_eq!(
        store.practice_account_balance(&pa.id).unwrap(),
        Decimal::from(4_000)
    );

    // DATA-B15: corrupt timestamp in simulation_event must propagate IntegrityFailed (not silently default).
    store
        .conn()
        .execute(
            "UPDATE simulation_event SET visible_until = 'NOT-A-DATE' WHERE run_id = 'RUN-1H-ABANDON' AND seq = 1",
            [],
        )
        .unwrap();
    assert!(store
        .list_simulation_events("RUN-1H-ABANDON", None)
        .is_err());
    assert!(store.export_simulation_tables_snapshot().is_err());
}
