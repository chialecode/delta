//! Production service tests shared by the desktop and the AI host (F-10).

use std::sync::Arc;
use std::time::Instant;

use chrono::TimeZone;
use delta_app::ai::gateway::AnalysisHost;
use delta_app::ai::session::{NewRun, RunBudget, SessionStore};
use delta_app::contracts::{ActorKind, CallContext};
use delta_core::events::{EconomicEvent, EventPayload};
use delta_core::ids::{AccountId, AssetId, EventId};
use delta_core::money::CurrencyCode;
use delta_core::valuation::{ManualKind, ManualMark};
use delta_core::{ScopeSnapshot, ScopeView};
use delta_infra::host::ProductionHost;
use delta_infra::sqlite::app::{
    delete_isolated, load_demo_lines, restore_backup, seed_synthetic_demo, NoteSession,
    SessionStatus,
};
use delta_infra::sqlite::migrate::normalize_timestamps;
use delta_infra::sqlite::store::Library;
use rust_decimal_macros::dec;
use serde_json::json;

fn lib(dir: &std::path::Path) -> Library {
    Library::create(&dir.join("lib.sqlite"), "USD").unwrap()
}

fn scope(accounts: &[&str], revision: i64) -> ScopeSnapshot {
    ScopeSnapshot::freeze(
        "scope-svc",
        accounts.iter().map(|a| AccountId::new(*a)).collect(),
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2026, 12, 31, 0, 0, 0).unwrap(),
        CurrencyCode::usd(),
        ScopeView::Portfolio,
        revision,
    )
}

fn budget() -> RunBudget {
    RunBudget {
        max_tool_calls: 4,
        max_model_requests: 4,
        context_window_tokens: 8000,
        max_duration_secs: 30,
    }
}

#[test]
fn r1_a_02_settings_survive_reopen_and_portfolio_membership_is_unique() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    assert_eq!(library.book_currency, "USD");
    library.set_reporting_currency("eur").unwrap();
    library.set_timezone("America/New_York").unwrap();
    library.ensure_account("a", "A", "broker").unwrap();
    library.ensure_account("b", "B", "wallet").unwrap();
    library.ensure_portfolio("p", "Main").unwrap();
    library
        .add_portfolio_member("p", "a", "2026-01-01T00:00:00Z")
        .unwrap();
    let err = library
        .add_portfolio_member("p", "a", "2026-02-01T00:00:00Z")
        .unwrap_err();
    assert!(err.to_string().contains("already"));
    drop(library);
    let opened = Library::open(&dir.path().join("lib.sqlite")).unwrap();
    assert_eq!(opened.book_currency, "USD");
    assert_eq!(opened.reporting_currency().unwrap(), "EUR");
    assert_eq!(opened.timezone().unwrap(), "America/New_York");
    assert_eq!(
        opened.portfolio_accounts("p").unwrap(),
        vec!["a".to_string()]
    );
}

#[test]
fn r1_a_03_csv_preview_commit_is_idempotent_and_rejects_a_stale_preview() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library.ensure_asset("AAPL", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")
        .unwrap();
    let csv = "\
occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote
2026-01-02T00:00:00Z,deposit,USD,100,,,src-dep,,
2026-01-03T15:00:00Z,buy,AAPL,1,10,0,src-a,NASDAQ:AAPL,USD
2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-b,NASDAQ:AAPL,USD
not-a-time,buy,AAPL,1,10,0,src-bad,NASDAQ:AAPL,USD
";
    let preview = library.preview_csv("acc", csv, "map-1").unwrap();
    assert_eq!(preview.rows[3].state, "error");
    let valid: Vec<i64> = preview
        .rows
        .iter()
        .filter(|r| r.state == "valid")
        .map(|r| r.row_no)
        .collect();
    let receipt = library
        .commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &valid)
        .unwrap();
    assert_eq!(receipt.accepted, 3);
    assert_eq!(receipt.rejected, 1);
    let again = library
        .commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &valid)
        .unwrap();
    assert_eq!(again.accepted, 3);
    assert_eq!(library.recorded_events(None).unwrap().len(), 3);
    let stale = library.commit_preview(&preview.batch_id, "map-2", &preview.file_hash, &valid);
    assert!(
        stale.is_err(),
        "changed mapping must reject the old preview"
    );

    let overlap = "\
occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote
2026-01-04T00:00:00Z,deposit,USD,5,,,src-dep,,
2026-01-05T00:00:00Z,deposit,USD,7,,,src-new,,
";
    let second = library.preview_csv("acc", overlap, "map-1").unwrap();
    assert_eq!(second.rows[0].state, "already_imported");
    assert!(
        second.rows[0].reason.contains("already imported"),
        "{}",
        second.rows[0].reason
    );
    // Accepting the same source again fails the batch and writes nothing.
    let before = library.recorded_events(None).unwrap().len();
    let accepted_existing = library.accept_independent_sources(
        &second.batch_id,
        "map-1",
        &second.file_hash,
        &[second.rows[1].row_no],
        &[second.rows[0].row_no],
    );
    assert!(accepted_existing.is_err());
    assert_eq!(library.recorded_events(None).unwrap().len(), before);
    // The same source is skipped automatically; the new row is written.
    let receipt = library
        .commit_preview(
            &second.batch_id,
            "map-1",
            &second.file_hash,
            &[second.rows[1].row_no],
        )
        .unwrap();
    assert_eq!((receipt.accepted, receipt.skipped), (1, 1));
    let deps = library
        .recorded_events(None)
        .unwrap()
        .into_iter()
        .filter(|e| e.source_ref.as_deref() == Some("src-dep"))
        .count();
    assert_eq!(deps, 1, "a duplicate source does not double-post");
}

#[test]
fn r1_a_03_overflow_quantity_is_a_row_error() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    let csv = format!(
        "occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote\n2026-01-02T00:00:00Z,deposit,USD,{},,,src-big,,\n",
        "9".repeat(40)
    );
    let preview = library.preview_csv("acc", &csv, "map-1").unwrap();
    assert_eq!(preview.rows[0].state, "error");
    assert!(
        preview.rows[0].reason.contains("overflow") || preview.rows[0].reason.contains("decimal")
    );
}

/// Same-file identical fills stay independent. A later file with a new source
/// and the same time, symbol, quantity, price and fee is only a suspect.
#[test]
fn r1_a_03_content_fingerprint_is_suspected_until_skipped_or_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_account("other", "B", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library.ensure_asset("AAPL", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")
        .unwrap();
    let header = "occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote\n";
    let first = format!(
        "{header}2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-a,NASDAQ:AAPL,USD\n2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-b,NASDAQ:AAPL,USD\n"
    );
    let preview = library.preview_csv("acc", &first, "map-1").unwrap();
    assert!(
        preview.rows.iter().all(|r| r.state == "valid"),
        "same-file identical fills stay independent: {:?}",
        preview.rows.iter().map(|r| &r.state).collect::<Vec<_>>()
    );
    let valid: Vec<i64> = preview.rows.iter().map(|r| r.row_no).collect();
    let receipt = library
        .commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &valid)
        .unwrap();
    assert_eq!(receipt.accepted, 2);

    let again = format!(
        "{header}2026-01-03T15:00:00Z,buy,AAPL,2,10.0,0,src-c,NASDAQ:AAPL,USD\n2026-01-06T00:00:00Z,deposit,USD,1,,,src-d,,\n"
    );
    let second = library.preview_csv("acc", &again, "map-1").unwrap();
    assert_eq!(second.rows[0].state, "duplicate_suspect");
    assert!(
        second.rows[0].reason.contains("src-a") || second.rows[0].reason.contains("src-b"),
        "{}",
        second.rows[0].reason
    );
    assert_eq!(second.rows[1].state, "valid");
    let undecided = library.commit_preview(
        &second.batch_id,
        "map-1",
        &second.file_hash,
        &[second.rows[1].row_no],
    );
    assert!(undecided.is_err(), "a suspect needs an explicit decision");
    assert_eq!(library.recorded_events(None).unwrap().len(), 2);

    let skipped = library
        .skip_suspected_rows(
            &second.batch_id,
            "map-1",
            &second.file_hash,
            &[second.rows[1].row_no],
            &[second.rows[0].row_no],
        )
        .unwrap();
    assert_eq!((skipped.accepted, skipped.skipped), (1, 1));
    assert_eq!(library.recorded_events(None).unwrap().len(), 3);

    let third = format!("{header}2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-e,NASDAQ:AAPL,USD\n");
    let suspect = library.preview_csv("acc", &third, "map-1").unwrap();
    assert_eq!(suspect.rows[0].state, "duplicate_suspect");
    let accepted = library
        .accept_independent_sources(
            &suspect.batch_id,
            "map-1",
            &suspect.file_hash,
            &[],
            &[suspect.rows[0].row_no],
        )
        .unwrap();
    assert_eq!(accepted.accepted, 1);
    let sources: Vec<_> = library
        .recorded_events(None)
        .unwrap()
        .into_iter()
        .filter_map(|e| e.source_ref)
        .collect();
    assert!(sources.iter().any(|s| s == "src-e"), "{sources:?}");
    assert!(sources.iter().any(|s| s == "src-a"), "{sources:?}");

    // Another account is not a suspect of this account's fingerprint.
    let elsewhere = library.preview_csv("other", &third, "map-1").unwrap();
    assert_eq!(
        elsewhere.rows[0].state, "valid",
        "{}",
        elsewhere.rows[0].reason
    );
}

#[test]
fn r1_a_04_correction_keeps_the_original_and_stales_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let at = chrono::Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
    library
        .record_events(
            &[EconomicEvent {
                id: EventId("dep1".into()),
                account_id: AccountId("acc".into()),
                occurred_at: at,
                recorded_at: at,
                seq: 1,
                source_ref: Some("dep1".into()),
                correction_group: None,
                revision: 0,
                reverses: None,
                payload: EventPayload::CashDeposit {
                    asset: AssetId("USD".into()),
                    amount: dec!(100),
                },
            }],
            None,
        )
        .unwrap();
    let report = library.save_active_report("old total 100", "acc").unwrap();
    library
        .fail_next_event_write
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // The failure hook is on CSV commit, not on an already-open correction
    // path. A failed commit must leave the ledger untouched.
    let csv = "\
occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote
2026-02-02T00:00:00Z,deposit,USD,1,,,src-fail,,
";
    let preview = library.preview_csv("acc", csv, "map-1").unwrap();
    let failed = library.commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &[1]);
    assert!(failed.is_err());
    assert_eq!(library.recorded_events(None).unwrap().len(), 1);

    let replacement = library.correct_cash_amount("dep1", "80").unwrap();
    assert_ne!(replacement, "dep1");
    let original = library.event_payload_json("dep1").unwrap();
    assert!(original.contains("100"));
    assert_eq!(library.report_status(&report).unwrap(), "stale");
    let cash = library
        .account_lines()
        .unwrap()
        .into_iter()
        .find(|l| l.asset == "USD")
        .unwrap();
    assert_eq!(cash.quantity, "80");
}

#[test]
fn r1_a_05_service_lines_match_the_hand_computed_demo() {
    let dir = tempfile::tempdir().unwrap();
    let lines = load_demo_lines(dir.path()).unwrap();
    let qty = |account, asset| {
        lines
            .iter()
            .find(|l| l.account == account && l.asset == asset)
            .map(|l| (l.quantity.as_str(), l.cost.as_str()))
            .unwrap_or_else(|| panic!("missing {account} {asset}"))
    };
    assert_eq!(qty("acc-us", "USD"), ("1438", "—"));
    assert_eq!(qty("acc-us", "AAPL"), ("6", "600.6"));
    assert_eq!(qty("acc-crypto", "USDT"), ("9400", "—"));
    assert_eq!(qty("acc-crypto", "BNB"), ("0.009", "540"));
    assert_eq!(qty("acc-wallet", "BTC"), ("0.5", "10060"));
}

#[test]
fn r1_a_08_adjusted_bars_do_not_replace_the_raw_close_used_for_valuation() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let file = delta_infra::sqlite::app::MarketFile {
        dataset_version: "v1".into(),
        source: "file".into(),
        bars: vec![bar("10", "raw"), bar("99", "split")],
        fx: vec![],
        sessions: vec![],
        actions: vec![],
    };
    library.import_market_file(&file).unwrap();
    let closes = library
        .closes_visible("NASDAQ:AAA", "2026-03-02T00:00:00Z", "raw")
        .unwrap();
    assert_eq!(closes, vec!["10".to_string()]);
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let at = chrono::Utc.with_ymd_and_hms(2026, 3, 1, 16, 0, 0).unwrap();
    library
        .record_events(
            &[EconomicEvent {
                id: EventId("open".into()),
                account_id: AccountId("acc".into()),
                occurred_at: at,
                recorded_at: at,
                seq: 1,
                source_ref: None,
                correction_group: None,
                revision: 0,
                reverses: None,
                payload: EventPayload::OpeningPosition {
                    asset: AssetId("AAA".into()),
                    quantity: dec!(2),
                    cost: Some(delta_core::events::OpeningCost::Known {
                        total: dec!(16),
                        currency: CurrencyCode::usd(),
                    }),
                },
            }],
            None,
        )
        .unwrap();
    let summary = library
        .portfolio_value(
            &scope(&["acc"], 1),
            chrono::Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0).unwrap(),
        )
        .unwrap();
    let position = summary
        .positions
        .iter()
        .find(|p| p.asset.0 == "AAA")
        .expect("position");
    assert_eq!(position.price.as_ref().unwrap().price, dec!(10));
}

fn bar(close: &str, adjustment: &str) -> delta_infra::sqlite::app::MarketBar {
    delta_infra::sqlite::app::MarketBar {
        instrument: "NASDAQ:AAA".into(),
        session_date: "2026-03-01".into(),
        open_at: "2026-03-01T14:30:00Z".into(),
        close_at: "2026-03-01T21:00:00Z".into(),
        open: close.into(),
        high: close.into(),
        low: close.into(),
        close: close.into(),
        volume: "1".into(),
        adjustment: adjustment.into(),
    }
}

#[test]
fn r1_a_10_file_calendar_and_dataset_versions_stay_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let v1 = delta_infra::sqlite::app::MarketFile {
        dataset_version: "ds-1".into(),
        source: "file".into(),
        bars: vec![bar("10", "raw")],
        fx: vec![delta_infra::sqlite::app::MarketFx {
            base: "EUR".into(),
            quote: "USD".into(),
            rate: "1.1".into(),
            effective_at: "2026-03-01T00:00:00+00:00".into(),
            observed_at: "2026-03-01T00:00:00Z".into(),
        }],
        sessions: vec![
            session(
                "XNYS",
                "2026-03-02",
                "holiday",
                "2026-03-02T14:30:00Z",
                "2026-03-02T21:00:00Z",
            ),
            session(
                "XNYS",
                "2026-03-03",
                "half",
                "2026-03-03T14:30:00Z",
                "2026-03-03T18:00:00Z",
            ),
        ],
        actions: vec![],
    };
    library.import_market_file(&v1).unwrap();
    let mut later = bar("11", "raw");
    later.session_date = "2026-03-04".into();
    later.open_at = "2026-03-04T14:30:00Z".into();
    later.close_at = "2026-03-04T21:00:00Z".into();
    let v2 = delta_infra::sqlite::app::MarketFile {
        dataset_version: "ds-2".into(),
        source: "file".into(),
        bars: vec![later],
        fx: vec![],
        sessions: vec![],
        actions: vec![],
    };
    library.import_market_file(&v2).unwrap();
    assert!(library.dataset_count("ds-1").unwrap() >= 1);
    assert_eq!(library.dataset_count("ds-2").unwrap(), 1);
    let holiday = chrono::Utc.with_ymd_and_hms(2026, 3, 2, 15, 0, 0).unwrap();
    assert_eq!(
        library.classify_session("XNYS", holiday).unwrap(),
        SessionStatus::Holiday
    );
    let half = chrono::Utc.with_ymd_and_hms(2026, 3, 3, 15, 0, 0).unwrap();
    assert_eq!(
        library.classify_session("XNYS", half).unwrap(),
        SessionStatus::Half
    );
    let after_half = chrono::Utc.with_ymd_and_hms(2026, 3, 3, 19, 0, 0).unwrap();
    assert_eq!(
        library.classify_session("XNYS", after_half).unwrap(),
        SessionStatus::Closed
    );
    let sunday = chrono::Utc.with_ymd_and_hms(2026, 3, 8, 15, 0, 0).unwrap();
    assert_eq!(
        library.classify_session("XNYS", sunday).unwrap(),
        SessionStatus::Missing
    );
    assert_eq!(
        library.classify_session("CRYPTO", sunday).unwrap(),
        SessionStatus::Open
    );
    let visible = library
        .closes_visible("NASDAQ:AAA", "2026-03-01T21:00:00Z", "raw")
        .unwrap();
    assert_eq!(visible, vec!["10".to_string()]);
}

fn session(
    venue: &str,
    date: &str,
    kind: &str,
    open_at: &str,
    close_at: &str,
) -> delta_infra::sqlite::app::MarketSession {
    delta_infra::sqlite::app::MarketSession {
        venue: venue.into(),
        session_date: date.into(),
        timezone: "America/New_York".into(),
        open_at: open_at.into(),
        close_at: close_at.into(),
        kind: kind.into(),
    }
}

#[test]
fn r1_a_10_timestamp_offsets_normalize_to_utc_nanos() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.with(|c| {
        c.execute(
            "INSERT INTO economic_event (id, account_id, occurred_at, recorded_at, seq, revision, payload)
             VALUES ('e1', 'acc', '2026-01-01T08:00:00+08:00', '2026-01-01T00:00:00Z', 1, 0, '{}')",
            [],
        )
        .unwrap();
    });
    library.with(|c| normalize_timestamps(c).unwrap());
    let raw = library.with(|c| {
        c.query_row(
            "SELECT occurred_at FROM economic_event WHERE id = 'e1'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    });
    assert!(raw.ends_with('Z'), "{raw}");
    assert!(raw.contains('.'), "{raw}");
    assert!(raw.starts_with("2026-01-01T00:00:00"), "{raw}");
}

#[test]
fn r1_a_12_chart_context_restores_and_links_cannot_exceed_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library
        .save_chart_context(
            "NASDAQ:AAPL",
            "1d",
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            "raw",
            "{\"ma\":20}",
        )
        .unwrap();
    let saved = library
        .latest_chart_context("NASDAQ:AAPL")
        .unwrap()
        .unwrap();
    assert_eq!(saved.0, "1d");
    assert_eq!(saved.3, "raw");
    library.ensure_account("acc", "A", "broker").unwrap();
    let note = library
        .save_journal("n", "body", &[], None, Some("acc"))
        .unwrap();
    library.allocate_link(&note, "fill-1", "6", "10").unwrap();
    let err = library
        .allocate_link(&note, "fill-1", "5", "10")
        .unwrap_err();
    assert!(err.to_string().contains("exceeds"));
    let at = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library
        .record_events(
            &[EconomicEvent {
                id: EventId("fill-1".into()),
                account_id: AccountId("acc".into()),
                occurred_at: at,
                recorded_at: at,
                seq: 1,
                source_ref: Some("fill-1".into()),
                correction_group: None,
                revision: 0,
                reverses: None,
                payload: EventPayload::CashDeposit {
                    asset: AssetId("USD".into()),
                    amount: dec!(1),
                },
            }],
            None,
        )
        .unwrap();
    assert!(library.link_notices(&note).unwrap().is_empty());
    library.correct_cash_amount("fill-1", "2").unwrap();
    assert!(!library.link_notices(&note).unwrap().is_empty());
    library.ensure_asset("AAPL", "equity").unwrap();
    library.ensure_asset("MSFT", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")
        .unwrap();
    library
        .ensure_instrument("NASDAQ:MSFT", "MSFT", "USD", "NASDAQ")
        .unwrap();
    library.add_watch("NASDAQ:AAPL").unwrap();
    library.add_watch("NASDAQ:AAPL").unwrap();
    assert_eq!(
        library.watchlist().unwrap(),
        vec!["NASDAQ:AAPL".to_string()]
    );
    let found = library.search_instruments("AAPL").unwrap();
    assert_eq!(found, vec!["NASDAQ:AAPL".to_string()]);
    assert!(library
        .search_instruments("MSFT")
        .unwrap()
        .iter()
        .all(|id| id.contains("MSFT")));
}

#[test]
fn r1_a_13_autosave_failure_keeps_the_draft_and_search_stays_in_scope() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("a", "A", "broker").unwrap();
    library.ensure_account("b", "B", "broker").unwrap();
    let mut note = NoteSession::new("标题", "a");
    note.edit("苹果突破");
    note.autosave(&library).unwrap();
    assert!(note.saved_ok);
    library
        .fail_next_journal
        .store(true, std::sync::atomic::Ordering::SeqCst);
    note.edit("还没写完的句子");
    assert!(note.autosave(&library).is_err());
    assert!(!note.saved_ok);
    assert_eq!(note.draft, "还没写完的句子");
    assert_eq!(note.saved, "苹果突破");
    note.edit("回踩均线");
    note.autosave(&library).unwrap();
    assert!(library
        .search_journal("苹果", 10, Some(&["a".into()]))
        .unwrap()
        .is_empty());
    assert_eq!(
        library
            .search_journal("回踩", 10, Some(&["a".into()]))
            .unwrap()
            .len(),
        1
    );
    library
        .save_journal("other", "回踩机密", &[], None, Some("b"))
        .unwrap();
    assert!(library
        .search_journal("回踩", 10, Some(&["a".into()]))
        .unwrap()
        .iter()
        .all(|(_, title, _)| title != "other"));
    library
        .save_journal("loose", "回踩无账户", &[], None, None)
        .unwrap();
    assert!(library
        .search_journal("无账户", 10, Some(&["a".into()]))
        .unwrap()
        .is_empty());
    assert!(library
        .search_journal("苹果 OR 机密", 10, None)
        .unwrap()
        .is_empty());
    library.save_template("prefix", "前").unwrap();
    library.save_template("suffix", "后").unwrap();
    assert_eq!(library.render_note("中").unwrap(), "前中后");
}

#[test]
fn r1_a_14_attachment_hash_detects_a_missing_or_corrupt_file() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let (id, hash) = library
        .save_attachment(b"chart-bytes", "image/png")
        .unwrap();
    assert_eq!(library.read_attachment(&id).unwrap(), b"chart-bytes");
    let path = library.attachments_path().join(&hash);
    std::fs::write(&path, b"tampered").unwrap();
    assert!(library.read_attachment(&id).is_err());
    std::fs::remove_file(&path).unwrap();
    assert!(library
        .read_attachment(&id)
        .unwrap_err()
        .to_string()
        .contains("missing"));
    library.with(|c| {
        c.execute(
            "UPDATE attachment SET relative_path = '../outside' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    });
    assert!(library.read_attachment(&id).is_err());
}

#[test]
fn r1_a_15_production_host_matches_account_lines_and_rejects_a_wider_scope() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(seed_synthetic_demo(&dir.path().join("demo.sqlite")).unwrap());
    let host = ProductionHost::new(library.clone());
    let lines = library.account_lines().unwrap();
    let snap = scope(
        &["acc-us", "acc-crypto", "acc-wallet"],
        library.ledger_revision().unwrap(),
    );
    let session = library.create_session(&library.id).unwrap();
    let run = library
        .create_run(&NewRun {
            session_id: session,
            generation: 1,
            scope_snapshot: snap.clone(),
            tool_schema_hash: "t".into(),
            model_ref: "m".into(),
            budget: budget(),
        })
        .unwrap();
    let ctx = CallContext {
        request_id: "req".into(),
        library_id: library.id.clone(),
        actor_kind: ActorKind::Ai,
        run_id: Some(run.id),
        generation: Some(1),
        scope_ref: snap.scope_ref.clone(),
        deadline: None,
    };
    let args = json!({
        "scope": {
            "account_ids": ["acc-us"],
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-02-01T00:00:00Z",
            "reporting_currency": "USD"
        },
        "as_of": "2026-02-01T00:00:00Z"
    });
    let env = host.execute("get_portfolio_summary", &ctx, &args).unwrap();
    let tool_lines = env.value["lines"].as_array().unwrap();
    let usd = lines
        .iter()
        .find(|l| l.account == "acc-us" && l.asset == "USD")
        .unwrap();
    let tool_usd = tool_lines
        .iter()
        .find(|l| l["account"] == "acc-us" && l["asset"] == "USD")
        .unwrap();
    assert_eq!(tool_usd["quantity"], usd.quantity);
    assert_eq!(tool_usd["cost"], usd.cost);
    assert!(env.evidence_refs.iter().all(|r| r.starts_with("res:")));
    let wide = json!({
        "scope": {
            "account_ids": ["acc-us", "acc-evil"],
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-02-01T00:00:00Z",
            "reporting_currency": "USD"
        },
        "as_of": "2026-02-01T00:00:00Z"
    });
    let err = host
        .execute("get_portfolio_summary", &ctx, &wide)
        .unwrap_err();
    assert_eq!(err.code(), "SCOPE_DENIED");
}

#[test]
fn r1_a_17_stale_ledger_revision_warns_without_widening_the_frozen_scope() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(seed_synthetic_demo(&dir.path().join("demo.sqlite")).unwrap());
    assert!(library.ledger_revision().unwrap() > 0);
    let host = ProductionHost::new(library.clone());
    let snap = scope(&["acc-us"], 0);
    let session = library.create_session(&library.id).unwrap();
    let run = library
        .create_run(&NewRun {
            session_id: session,
            generation: 1,
            scope_snapshot: snap.clone(),
            tool_schema_hash: "t".into(),
            model_ref: "m".into(),
            budget: budget(),
        })
        .unwrap();
    let ctx = CallContext {
        request_id: "req".into(),
        library_id: library.id.clone(),
        actor_kind: ActorKind::Ai,
        run_id: Some(run.id),
        generation: Some(1),
        scope_ref: snap.scope_ref.clone(),
        deadline: None,
    };
    let env = host
        .execute(
            "get_data_quality",
            &ctx,
            &json!({
                "scope": {
                    "account_ids": ["acc-us"],
                    "start_at": "2026-01-01T00:00:00Z",
                    "end_at": "2026-12-31T00:00:00Z",
                    "reporting_currency": "USD"
                }
            }),
        )
        .unwrap();
    assert!(
        env.warnings.iter().any(|w| w.contains("stale")),
        "{:?}",
        env.warnings
    );
}

#[test]
fn r1_a_09_manual_mark_is_visible_to_the_shared_wealth_query() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let at = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    library
        .add_manual_mark(&ManualMark {
            account_id: AccountId("acc".into()),
            asset: AssetId("loan".into()),
            value: dec!(20),
            currency: CurrencyCode::usd(),
            as_of: at,
            kind: ManualKind::Liability,
            valid_until: None,
        })
        .unwrap();
    let wealth = library.wealth(&scope(&["acc"], 0), at).unwrap();
    assert_eq!(wealth.liability_value, dec!(20));
    assert_eq!(wealth.known_net_worth, dec!(-20));
}

#[test]
fn r1_a_20_worker_env_diagnostics_and_config_probes_drop_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let secret = "probe-secret-r1-9f3a";
    let kept = delta_infra::sqlite::app::sanitize_worker_env(&[
        ("PATH", "/usr/bin"),
        ("OPENAI_API_KEY", secret),
        ("MODEL_TOKEN", secret),
    ]);
    assert_eq!(kept, vec![("PATH".into(), "/usr/bin".into())]);
    assert!(delta_infra::sqlite::app::implicit_config_files().is_empty());
    let diag = library.diagnostics_bundle().unwrap();
    assert!(!diag.contains(secret));
    assert!(diag.contains("credentials=omitted"));
    let stored = delta_infra::sqlite::app::store_os_credential("delta-r1-probe", secret);
    if let Err(msg) = &stored {
        assert!(!msg.contains(secret));
    } else {
        assert_eq!(
            delta_infra::sqlite::app::read_os_credential("delta-r1-probe").unwrap(),
            secret
        );
        delta_infra::sqlite::app::delete_os_credential("delta-r1-probe").unwrap();
    }
    let bytes = std::fs::read(&library.path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(secret));
}

#[test]
fn r1_a_22_backup_restore_and_export_round_trip_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let secret = "probe-secret-r1-9f3a";
    library
        .save_journal("note", "plain note", &[], None, Some("acc"))
        .unwrap();
    let backup = dir.path().join("backup");
    library.backup_to(&backup).unwrap();
    let export = dir.path().join("export");
    library.export_to(&export).unwrap();
    for path in [
        backup.join("manifest.json"),
        backup.join("library.sqlite"),
        export.join("accounts.csv"),
        export.join("events.json"),
        export.join("notes.md"),
    ] {
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(!text.contains(secret), "{}", path.display());
    }
    assert!(std::fs::read_to_string(export.join("notes.md"))
        .unwrap()
        .contains("plain note"));
    let restored = dir.path().join("restored").join("library.sqlite");
    restore_backup(&backup, &restored).unwrap();
    let opened = Library::open(&restored).unwrap();
    assert_eq!(opened.ledger_revision().unwrap(), 0);
    assert_eq!(opened.search_journal("plain", 5, None).unwrap().len(), 1);
    let manifest = backup.join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let flipped = flip_hash(&text);
    std::fs::write(&manifest, flipped).unwrap();
    let rejected = dir.path().join("bad").join("library.sqlite");
    assert!(restore_backup(&backup, &rejected).is_err());
    assert!(!rejected.exists());
    assert!(Library::open(&library.path).is_ok());
}

fn flip_hash(manifest: &str) -> String {
    let mut chars: Vec<char> = manifest.chars().collect();
    if let Some(pos) = manifest.find("\"sha256\": \"") {
        let i = pos + "\"sha256\": \"".len();
        chars[i] = if chars[i] == '0' { '1' } else { '0' };
    }
    chars.into_iter().collect()
}

#[test]
fn r1_a_23_delete_requires_the_canonical_path_and_a_closed_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.sqlite");
    let library = Library::create(&path, "USD").unwrap();
    drop(library);
    assert!(delete_isolated(&path, "cancel", false).is_err());
    assert!(path.exists());
    assert!(delete_isolated(&path, &path.display().to_string(), true).is_err());
    assert!(path.exists());
    let canon = path.canonicalize().unwrap();
    delete_isolated(&path, &canon.to_string_lossy(), false).unwrap();
    assert!(!path.exists());
}

#[test]
fn r1_a_24_cached_first_screen_and_bar_projection_meet_the_cpu_budget() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let at = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let events: Vec<EconomicEvent> = (0..100_000)
        .map(|i| EconomicEvent {
            id: EventId(format!("e{i}")),
            account_id: AccountId("acc".into()),
            occurred_at: at,
            recorded_at: at,
            seq: i,
            source_ref: None,
            correction_group: None,
            revision: 0,
            reverses: None,
            payload: EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1),
            },
        })
        .collect();
    library.record_events(&events, None).unwrap();
    library.refresh_balance_cache().unwrap();
    let started = Instant::now();
    let lines = library.cached_lines().unwrap();
    let elapsed = started.elapsed();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].quantity, "100000");
    assert!(
        elapsed.as_secs_f64() < 3.0,
        "cached first screen took {elapsed:?}"
    );
    library.ensure_asset("AAA", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAA", "AAA", "USD", "NASDAQ")
        .unwrap();
    library.with(|c| {
        let tx = c.unchecked_transaction().unwrap();
        for i in 0..1000 {
            let day = format!("2026-01-{:02}", (i % 28) + 1);
            tx.execute(
                "INSERT INTO bar (instrument_id, timeframe, session_date, open_at, close_at, open, high, low, close, volume, source, adjustment, is_final, dataset_version)
                 VALUES ('NASDAQ:AAA', '1d', ?1, ?2, ?2, '1', '1', '1', ?3, '1', 'file', 'raw', 1, ?4)",
                rusqlite::params![format!("{day}-{i}"), format!("2026-06-01T00:00:{:02}.000000000Z", i % 60), format!("{i}"), format!("ds-{i}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    });
    let closes = library
        .closes_visible("NASDAQ:AAA", "2026-12-31T00:00:00Z", "raw")
        .unwrap();
    let parsed: Vec<f64> = closes.iter().map(|c| c.parse().unwrap()).collect();
    let started = Instant::now();
    let mut window = 0.0f64;
    let mut sma = Vec::new();
    for (i, close) in parsed.iter().enumerate() {
        window += close;
        if i >= 20 {
            window -= parsed[i - 20];
        }
        if i + 1 >= 20 {
            sma.push(window / 20.0);
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(sma.len(), closes.len() - 19);
    assert!(
        elapsed.as_millis() < 20,
        "1000-bar projection took {elapsed:?}; GPU frame rate was not measured"
    );
}

// ---- RW-01 review regressions (Agent A) -------------------------------------------

fn utc(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

fn deposit(
    account: &str,
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
    amount: rust_decimal::Decimal,
) -> EconomicEvent {
    EconomicEvent {
        id: EventId(id.into()),
        account_id: AccountId(account.into()),
        occurred_at: at,
        recorded_at: at,
        seq: 1,
        source_ref: Some(id.into()),
        correction_group: None,
        revision: 0,
        reverses: None,
        payload: EventPayload::CashDeposit {
            asset: AssetId("USD".into()),
            amount,
        },
    }
}

fn mark(
    account: &str,
    asset: &str,
    value: rust_decimal::Decimal,
    ccy: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> ManualMark {
    ManualMark {
        account_id: AccountId(account.into()),
        asset: AssetId(asset.into()),
        value,
        currency: CurrencyCode::new(ccy),
        as_of: at,
        kind: ManualKind::Asset,
        valid_until: None,
    }
}

/// A newer mark of the same asset replaces the older one; it is not added
/// to it. The mark converts at the query time like every other balance.
#[test]
fn r1_a_09_newer_manual_mark_replaces_the_older_one() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library
        .add_manual_mark(&mark("home", "house", dec!(100), "USD", utc(2026, 1, 2)))
        .unwrap();
    library
        .add_manual_mark(&mark("home", "house", dec!(120), "USD", utc(2026, 6, 1)))
        .unwrap();
    let home = scope(&["home"], 0);
    assert_eq!(
        library.wealth(&home, utc(2026, 7, 1)).unwrap().asset_value,
        dec!(120)
    );
    assert_eq!(
        library.wealth(&home, utc(2026, 3, 1)).unwrap().asset_value,
        dec!(100)
    );

    library
        .upsert_fx(
            "EUR",
            "USD",
            "1.0",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "file",
        )
        .unwrap();
    library
        .upsert_fx(
            "EUR",
            "USD",
            "1.2",
            "2026-06-15T00:00:00Z",
            "2026-06-15T00:00:00Z",
            "file",
        )
        .unwrap();
    library
        .add_manual_mark(&mark("home-eu", "flat", dec!(100), "EUR", utc(2026, 1, 2)))
        .unwrap();
    let eu = scope(&["home-eu"], 0);
    assert_eq!(
        library.wealth(&eu, utc(2026, 7, 1)).unwrap().asset_value,
        dec!(120)
    );
}

/// An observation is compared with the ledger at its own time, and cached
/// first-screen rows are not observations (FR-DATA-05).
#[test]
fn r1_a_06_reconciliation_uses_the_ledger_at_the_observation_time() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library
        .record_events(
            &[
                deposit("acc", "d1", utc(2026, 2, 1), dec!(100)),
                deposit("acc", "d2", utc(2026, 3, 1), dec!(50)),
            ],
            None,
        )
        .unwrap();
    library.refresh_balance_cache().unwrap();
    library
        .record_events(&[deposit("acc", "d3", utc(2026, 4, 1), dec!(25))], None)
        .unwrap();
    library
        .observe_balance("acc", "USD", "100", "2026-02-15T00:00:00Z")
        .unwrap();
    let sc = scope(&["acc"], 0);
    assert!(library.reconcile(&sc).unwrap().is_empty());
    library
        .observe_balance("acc", "USD", "90", "2026-02-20T00:00:00Z")
        .unwrap();
    let diffs = library.reconcile(&sc).unwrap();
    assert_eq!(diffs.len(), 1, "{diffs:?}");
    assert_eq!(diffs[0].ledger_quantity, dec!(100));
    assert_eq!(diffs[0].difference, dec!(-10));
}

/// The receipt must describe what was written: a row that cannot be
/// inserted is not counted as accepted (FR-DATA-01/02).
#[test]
fn r1_a_03_receipt_counts_match_the_events_written() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_account("acc2", "B", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let header = "occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote\n";
    let twice = format!(
        "{header}2026-01-02T00:00:00Z,deposit,USD,10,,,same-ref,,\n2026-01-03T00:00:00Z,deposit,USD,20,,,same-ref,,\n"
    );
    let preview = library.preview_csv("acc", &twice, "map-1").unwrap();
    let valid: Vec<i64> = preview
        .rows
        .iter()
        .filter(|r| r.state == "valid")
        .map(|r| r.row_no)
        .collect();
    let receipt = library
        .commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &valid)
        .unwrap();
    let written = library.recorded_events(None).unwrap().len();
    assert_eq!(receipt.accepted as usize, written, "{receipt:?}");
    assert_eq!(receipt.accepted + receipt.rejected, 2, "{receipt:?}");

    // The same source id on another account is a different import.
    let other = format!("{header}2026-01-04T00:00:00Z,deposit,USD,5,,,same-ref,,\n");
    let second = library.preview_csv("acc2", &other, "map-1").unwrap();
    assert_eq!(second.rows[0].state, "valid");
    let row = second.rows[0].row_no;
    let before = library.recorded_events(None).unwrap().len();
    let result = library
        .commit_preview(&second.batch_id, "map-1", &second.file_hash, &[row])
        .unwrap();
    let added = library.recorded_events(None).unwrap().len() - before;
    assert_eq!(result.accepted as usize, added, "{result:?}");
    assert_eq!(added, 1);
}

/// Correcting the same original twice must apply the second amount or
/// fail; it must not report success and keep the first correction.
#[test]
fn r1_a_04_a_second_correction_of_the_same_event_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library
        .record_events(&[deposit("acc", "dep1", utc(2026, 2, 1), dec!(100))], None)
        .unwrap();
    library.correct_cash_amount("dep1", "80").unwrap();
    library.correct_cash_amount("dep1", "70").unwrap();
    let cash = library
        .account_lines()
        .unwrap()
        .into_iter()
        .find(|l| l.asset == "USD")
        .unwrap();
    assert_eq!(cash.quantity, "70");
    assert_eq!(library.recorded_events(None).unwrap().len(), 3);
}

/// Tool lines and the valuation answer the same `as_of`; later fills do not
/// appear in an earlier summary (FR-AI-01, no future data).
#[test]
fn r1_a_15_portfolio_tool_lines_follow_as_of() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(lib(dir.path()));
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library
        .record_events(
            &[
                deposit("acc", "d1", utc(2026, 1, 10), dec!(100)),
                deposit("acc", "d2", utc(2026, 3, 10), dec!(50)),
            ],
            None,
        )
        .unwrap();
    let host = ProductionHost::new(library.clone());
    let snap = scope(&["acc"], library.ledger_revision().unwrap());
    let session = library.create_session(&library.id).unwrap();
    let run = library
        .create_run(&NewRun {
            session_id: session,
            generation: 1,
            scope_snapshot: snap.clone(),
            tool_schema_hash: "t".into(),
            model_ref: "m".into(),
            budget: budget(),
        })
        .unwrap();
    let ctx = CallContext {
        request_id: "req".into(),
        library_id: library.id.clone(),
        actor_kind: ActorKind::Ai,
        run_id: Some(run.id),
        generation: Some(1),
        scope_ref: snap.scope_ref.clone(),
        deadline: None,
    };
    let env = host
        .execute(
            "get_portfolio_summary",
            &ctx,
            &json!({
                "scope": {
                    "account_ids": ["acc"],
                    "start_at": "2026-01-01T00:00:00Z",
                    "end_at": "2026-12-31T00:00:00Z",
                    "reporting_currency": "USD"
                },
                "as_of": "2026-02-01T00:00:00Z"
            }),
        )
        .unwrap();
    assert_eq!(env.value["known_market_value"], "100");
    let usd = env.value["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["asset"] == "USD")
        .unwrap();
    assert_eq!(usd["quantity"], "100", "{}", env.value);
}

/// A folder holds one library, so deleting one never removes another
/// library's attachments: a second library or a restore into the same
/// folder is refused, and a restore into a new folder survives the delete.
#[test]
fn r1_a_23_deleting_a_library_keeps_another_librarys_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("a.sqlite");
    let a = Library::create(&a_path, "USD").unwrap();
    let (id, _) = a.save_attachment(b"keep me", "text/plain").unwrap();
    assert!(Library::create(&dir.path().join("b.sqlite"), "USD").is_err());
    let backup = dir.path().join("backup");
    a.backup_to(&backup).unwrap();
    assert!(restore_backup(&backup, &dir.path().join("restored.sqlite")).is_err());
    let restored_path = dir.path().join("restored").join("library.sqlite");
    restore_backup(&backup, &restored_path).unwrap();
    drop(a);
    let canon = a_path.canonicalize().unwrap();
    delete_isolated(&a_path, &canon.to_string_lossy(), false).unwrap();
    assert!(!a_path.exists());
    let restored = Library::open(&restored_path).unwrap();
    assert_eq!(restored.read_attachment(&id).unwrap(), b"keep me");
}

/// The manifest must cover the library file; an unlisted file is not
/// restored unverified (R1-A-22 bad backup is rejected).
#[test]
fn r1_a_22_restore_rejects_a_manifest_that_omits_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let backup = dir.path().join("backup");
    library.backup_to(&backup).unwrap();
    std::fs::write(backup.join("library.sqlite"), b"not a database").unwrap();
    std::fs::write(
        backup.join("manifest.json"),
        br#"{"files": [], "note": "edited"}"#,
    )
    .unwrap();
    let dest = dir.path().join("restored").join("library.sqlite");
    assert!(restore_backup(&backup, &dest).is_err());
    assert!(!dest.exists());
}

#[test]
fn r1_a_22_export_names_the_library_book_currency() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::create(&dir.path().join("eur.sqlite"), "EUR").unwrap();
    let export = dir.path().join("export");
    library.export_to(&export).unwrap();
    let notes = std::fs::read_to_string(export.join("notes.md")).unwrap();
    assert!(notes.contains("EUR"), "{notes}");
    assert!(!notes.contains("currency USD"), "{notes}");
}

/// Notes of other accounts must not use up the search window before the
/// scope filter runs (FR-JRN-01 recall).
#[test]
fn r1_a_13_scoped_search_is_not_cut_short_by_other_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("a", "A", "broker").unwrap();
    library.ensure_account("b", "B", "broker").unwrap();
    let early = library
        .save_journal("早", "复盘 早期", &[], None, Some("a"))
        .unwrap();
    for i in 0..510 {
        library
            .save_journal("b", &format!("alpha 复盘 {i}"), &[], None, Some("b"))
            .unwrap();
    }
    let late = library
        .save_journal("late", "alpha late", &[], None, Some("a"))
        .unwrap();
    let scoped = Some(&["a".to_string()][..]);
    let ascii = library.search_journal("alpha", 10, scoped).unwrap();
    assert!(ascii.iter().any(|(id, _, _)| id == &late), "{ascii:?}");
    let cjk = library.search_journal("复盘", 10, scoped).unwrap();
    assert!(cjk.iter().any(|(id, _, _)| id == &early), "{cjk:?}");
}

/// Opening a library from before the latest migration keeps a consistent
/// copy first; a library from a newer build is refused (plan W09).
#[test]
fn r1_a_22_older_library_is_backed_up_before_migration_and_newer_is_refused() {
    use delta_infra::sqlite::migrate::{current_version, MIGRATIONS};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        current_version(&conn).unwrap();
        for (i, (name, sql)) in MIGRATIONS[..3].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration (version, name, applied_at) VALUES (?1, ?2, '2026-01-01T00:00:00Z')",
                rusqlite::params![(i + 1) as i64, name],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO library_meta (id, schema_version, book_currency, created_at) VALUES ('old', 1, 'USD', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }
    let library = Library::open(&path).unwrap();
    let backup = dir
        .path()
        .join("backups")
        .join("old.sqlite.pre-migration-v3.sqlite");
    assert!(backup.exists());
    let saved: i64 = rusqlite::Connection::open(&backup)
        .unwrap()
        .query_row("SELECT MAX(version) FROM schema_migration", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(saved, 3);
    library.with(|c| {
        c.execute(
            "INSERT INTO schema_migration (version, name, applied_at) VALUES (99, 'future', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    });
    drop(library);
    let err = Library::open(&path).err().expect("newer schema is refused");
    assert!(err.to_string().contains("newer"), "{err}");
}

fn ai_ctx(library: &Library, snap: &ScopeSnapshot) -> CallContext {
    let session = library.create_session(&library.id).unwrap();
    let run = library
        .create_run(&NewRun {
            session_id: session,
            generation: 1,
            scope_snapshot: snap.clone(),
            tool_schema_hash: "t".into(),
            model_ref: "m".into(),
            budget: budget(),
        })
        .unwrap();
    CallContext {
        request_id: "req".into(),
        library_id: library.id.clone(),
        actor_kind: ActorKind::Ai,
        run_id: Some(run.id),
        generation: Some(1),
        scope_ref: snap.scope_ref.clone(),
        deadline: None,
    }
}

/// An evidence id names one stored value. Another account subset or a later
/// ledger change gets its own id, and the earlier id still opens the value
/// the report was checked against (F-12 / A-21).
#[test]
fn r1_a_21_evidence_id_keeps_the_value_it_was_issued_for() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(lib(dir.path()));
    library.ensure_account("a", "A", "broker").unwrap();
    library.ensure_account("b", "B", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library
        .record_events(
            &[
                deposit("a", "d1", utc(2026, 1, 10), dec!(100)),
                deposit("b", "d2", utc(2026, 1, 10), dec!(7)),
            ],
            None,
        )
        .unwrap();
    let host = ProductionHost::new(library.clone());
    let snap = scope(&["a", "b"], library.ledger_revision().unwrap());
    let ctx = ai_ctx(&library, &snap);
    let summary = |accounts: &[&str]| {
        host.execute(
            "get_portfolio_summary",
            &ctx,
            &json!({
                "scope": {
                    "account_ids": accounts,
                    "start_at": "2026-01-01T00:00:00Z",
                    "end_at": "2026-12-31T00:00:00Z",
                    "reporting_currency": "USD"
                },
                "as_of": "2026-02-01T00:00:00Z"
            }),
        )
        .unwrap()
    };
    let only_a = summary(&["a"]);
    let only_b = summary(&["b"]);
    assert_eq!(only_a.value["known_market_value"], "100");
    assert_eq!(only_b.value["known_market_value"], "7");
    let id_a = only_a.evidence_refs[0].clone();
    assert_ne!(
        id_a, only_b.evidence_refs[0],
        "two different results share one evidence id"
    );
    library
        .record_events(&[deposit("a", "d3", utc(2026, 1, 20), dec!(5))], None)
        .unwrap();
    let later = summary(&["a"]);
    assert_eq!(later.value["known_market_value"], "105");
    assert_ne!(later.evidence_refs[0], id_a);
    let opened = library.open_evidence(&id_a).unwrap();
    assert!(
        opened.body.contains("\"known_market_value\":\"100\""),
        "earlier evidence now opens another value: {}",
        opened.body
    );
}

/// explain_pnl cites a bounded set of effective events, so its tool output
/// does not grow with the ledger (F-12 / A-21, A-24 scale).
#[test]
fn r1_a_21_pnl_event_evidence_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(lib(dir.path()));
    library.ensure_account("a", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    let events: Vec<EconomicEvent> = (0..60)
        .map(|i| {
            deposit(
                "a",
                &format!("d{i}"),
                utc(2026, 1, 2) + chrono::Duration::hours(i),
                dec!(1),
            )
        })
        .collect();
    library.record_events(&events, None).unwrap();
    library.correct_cash_amount("d59", "2").unwrap();
    let host = ProductionHost::new(library.clone());
    let snap = scope(&["a"], library.ledger_revision().unwrap());
    let ctx = ai_ctx(&library, &snap);
    let env = host
        .execute(
            "explain_pnl",
            &ctx,
            &json!({
                "scope": {
                    "account_ids": ["a"],
                    "start_at": "2026-01-01T00:00:00Z",
                    "end_at": "2026-12-31T00:00:00Z",
                    "reporting_currency": "USD"
                },
                "period_start": "2026-01-01T00:00:00Z",
                "period_end": "2026-12-31T00:00:00Z"
            }),
        )
        .unwrap();
    let cited: Vec<&String> = env
        .evidence_refs
        .iter()
        .filter(|r| r.starts_with("event:"))
        .collect();
    assert!(
        !cited.is_empty() && cited.len() <= 20,
        "{} event refs",
        cited.len()
    );
    assert!(
        !cited.iter().any(|r| r.as_str() == "event:d59"),
        "a superseded original is not evidence"
    );
    assert_eq!(env.value["events_in_period"], "60", "{}", env.value);
    assert_eq!(
        env.value["events_cited"],
        cited.len().to_string(),
        "{}",
        env.value
    );
}

/// A note is evidence only at a pinned revision; a bare id would open
/// whatever the note says now.
#[test]
fn r1_a_21_journal_evidence_requires_a_revision() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    let note = library
        .save_journal("复盘", "原文", &[], None, None)
        .unwrap();
    let pinned = library.journal_evidence_id(&note).unwrap();
    library
        .save_journal("复盘", "改写", &[], Some(&note), None)
        .unwrap();
    assert!(library.open_evidence(&format!("journal:{note}")).is_err());
    assert_eq!(library.open_evidence(&pinned).unwrap().body, "原文");
}

/// A preview taken before another file wrote a matching fill is stale: its
/// row would skip the suspect decision (R1-A-03).
#[test]
fn r1_a_03_preview_is_stale_once_the_ledger_gains_a_matching_fill() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    library.ensure_account("acc", "A", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library.ensure_asset("AAPL", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")
        .unwrap();
    let header = "occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote\n";
    let first = format!("{header}2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-x,NASDAQ:AAPL,USD\n");
    let second = format!("{header}2026-01-03T15:00:00Z,buy,AAPL,2,10,0,src-y,NASDAQ:AAPL,USD\n");
    let old = library.preview_csv("acc", &first, "map-1").unwrap();
    assert_eq!(old.rows[0].state, "valid");
    let other = library.preview_csv("acc", &second, "map-1").unwrap();
    library
        .commit_preview(&other.batch_id, "map-1", &other.file_hash, &[1])
        .unwrap();
    let stale = library.commit_preview(&old.batch_id, "map-1", &old.file_hash, &[1]);
    assert!(stale.is_err(), "old preview committed without a decision");
    assert_eq!(library.recorded_events(None).unwrap().len(), 1);
    // Previewing the file again replaces the stale preview and shows the
    // suspect; a file already committed is refused with a clear reason.
    let again = library.preview_csv("acc", &first, "map-1").unwrap();
    assert_eq!(again.rows[0].state, "duplicate_suspect");
    assert!(library
        .commit_preview(&old.batch_id, "map-1", &old.file_hash, &[1])
        .is_err());
    let err = library.preview_csv("acc", &second, "map-1").unwrap_err();
    assert!(err.to_string().contains("already imported"), "{err}");
}

/// `%` and `_` in a search are literal characters (F-15), the same symbol on two
/// venues stays two instruments, and the watchlist keeps their identities apart.
#[test]
fn r1_a_12_instrument_search_treats_wildcards_literally_and_keeps_venues_apart() {
    let dir = tempfile::tempdir().unwrap();
    let library = lib(dir.path());
    for asset in ["USD", "USDT", "BTC", "A_B", "AXB", "50%"] {
        library.ensure_asset(asset, "crypto").unwrap();
    }
    library
        .ensure_instrument("BINANCE:BTCUSDT", "BTC", "USDT", "BINANCE")
        .unwrap();
    library
        .ensure_instrument("KRAKEN:BTCUSDT", "BTC", "USDT", "KRAKEN")
        .unwrap();
    library
        .ensure_instrument("KRAKEN:BTCUSD", "BTC", "USD", "KRAKEN")
        .unwrap();
    library
        .ensure_instrument("TEST:A_BUSD", "A_B", "USD", "TEST")
        .unwrap();
    library
        .ensure_instrument("TEST:AXBUSD", "AXB", "USD", "TEST")
        .unwrap();
    library
        .ensure_instrument("TEST:HALF50%", "50%", "USD", "TEST")
        .unwrap();

    // `_` is one literal underscore, not "any single character".
    assert_eq!(
        library.search_instruments("A_B").unwrap(),
        vec!["TEST:A_BUSD".to_string()]
    );
    assert_eq!(
        library.search_instruments("_").unwrap(),
        vec!["TEST:A_BUSD".to_string()]
    );
    // `%` is one literal percent sign, not "anything".
    assert_eq!(
        library.search_instruments("50%").unwrap(),
        vec!["TEST:HALF50%".to_string()]
    );
    assert_eq!(
        library.search_instruments("%").unwrap(),
        vec!["TEST:HALF50%".to_string()]
    );
    // A backslash is literal too, and matches nothing here without erroring.
    assert!(library.search_instruments("\\").unwrap().is_empty());
    assert!(library.search_instruments("A\\_B").unwrap().is_empty());
    // Case is ignored and surrounding blanks are trimmed.
    assert_eq!(
        library.search_instruments("  binance:btc ").unwrap(),
        vec!["BINANCE:BTCUSDT".to_string()]
    );
    // A blank query lists everything, in id order.
    assert_eq!(library.search_instruments("").unwrap().len(), 6);

    // Venue and pair come with every hit.
    let hits = library.search_instrument_hits("BTC").unwrap();
    let identities: Vec<(&str, &str, &str, &str)> = hits
        .iter()
        .map(|h| {
            (
                h.id.as_str(),
                h.venue.as_str(),
                h.base.as_str(),
                h.quote.as_str(),
            )
        })
        .collect();
    assert_eq!(
        identities,
        vec![
            ("BINANCE:BTCUSDT", "BINANCE", "BTC", "USDT"),
            ("KRAKEN:BTCUSD", "KRAKEN", "BTC", "USD"),
            ("KRAKEN:BTCUSDT", "KRAKEN", "BTC", "USDT"),
        ]
    );

    // Watching one venue's instrument does not watch its lookalike.
    library.add_watch("KRAKEN:BTCUSDT").unwrap();
    library.add_watch("KRAKEN:BTCUSDT").unwrap();
    assert_eq!(
        library.watchlist().unwrap(),
        vec!["KRAKEN:BTCUSDT".to_string()]
    );
    library.remove_watch("BINANCE:BTCUSDT").unwrap();
    assert_eq!(
        library.watchlist().unwrap().len(),
        1,
        "removing an unwatched id changes nothing"
    );
    library.remove_watch("KRAKEN:BTCUSDT").unwrap();
    assert!(library.watchlist().unwrap().is_empty());
    // Only a known instrument can be watched.
    let err = library.add_watch("NOPE:NOTHING").unwrap_err();
    assert!(err.to_string().contains("unknown instrument"), "{err}");
    assert!(library.watchlist().unwrap().is_empty());
}
