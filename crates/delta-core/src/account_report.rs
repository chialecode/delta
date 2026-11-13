//! `m5-account-report-1` (`S-53`): practice-account analysis across training runs.
//!
//! Inputs are the frozen per-run reports (closed trade groups, fees, plan execution) and the
//! append-only practice-account entries. Every amount is a `Decimal`; ratios are rounded to
//! 8 decimal places so results can be checked by hand.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::sim_report::{
    ClosedTradeGroup, ExpectancySummary, RatioMetric, WinRateStatus, WinRateSummary,
};

pub const ACCOUNT_REPORT_VERSION: &str = "m5-account-report-1";
/// Fewer closed trade groups than this are flagged "样本不足" (sample insufficient).
pub const ACCOUNT_REPORT_MIN_GROUPS: usize = 20;
const RATIO_DP: u32 = 8;

/// One practice-funded run as seen by the account analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRunInput {
    pub run_id: String,
    pub market_category: String,
    pub timeframe: String,
    /// Allocation taken from the account at start (positive).
    pub allocation: Decimal,
    /// Settlement credited at finish/abandon; `None` while the run is still open.
    pub settlement: Option<Decimal>,
    /// Equity at the last visible close (used for open runs).
    pub mark_equity: Decimal,
    pub closed_groups: Vec<ClosedTradeGroup>,
    pub total_fees: Decimal,
    pub planned_closed_group_count: usize,
    pub plan_compliant_count: usize,
    pub undo_count: u64,
}

/// One account entry in sequence order (`kind` as stored: deposit, withdrawal, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountEntryInput {
    pub seq: i64,
    pub kind: String,
    pub amount: Decimal,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountEquityPoint {
    /// Entry sequence number; `None` for the final mark-to-market point of an open run.
    pub seq: Option<i64>,
    pub kind: String,
    pub equity: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountGroupMetrics {
    pub win_rate: WinRateSummary,
    pub gross_profit: Decimal,
    pub gross_loss_abs: Decimal,
    pub payoff_ratio: RatioMetric,
    pub profit_factor: RatioMetric,
    pub largest_win: Option<Decimal>,
    pub largest_loss: Option<Decimal>,
    /// Largest single win ÷ |largest single loss| ("极值盈亏比").
    pub extreme_payoff_ratio: RatioMetric,
    pub expectancy: ExpectancySummary,
    pub average_holding_bars: Option<Decimal>,
    pub sample_insufficient: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBreakdown {
    pub key: String,
    pub run_count: usize,
    pub pnl: Decimal,
    pub metrics: AccountGroupMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountReport {
    pub report_version: String,
    pub quote_currency: String,
    pub balance: Decimal,
    pub equity: Decimal,
    pub net_deposits: Decimal,
    pub cumulative_pnl: Decimal,
    pub equity_curve: Vec<AccountEquityPoint>,
    pub run_count: usize,
    pub ended_run_count: usize,
    pub winning_run_count: usize,
    pub run_win_rate: Option<Decimal>,
    pub groups: AccountGroupMetrics,
    pub total_fees: Decimal,
    pub plan_execution_rate: Option<Decimal>,
    pub undo_count: u64,
    pub by_market_category: Vec<AccountBreakdown>,
    pub by_timeframe: Vec<AccountBreakdown>,
}

fn ratio(num: Decimal, den: Decimal) -> Decimal {
    (num / den).round_dp(RATIO_DP)
}

pub fn compute_group_metrics(groups: &[&ClosedTradeGroup]) -> AccountGroupMetrics {
    let n = groups.len();
    let wins: Vec<Decimal> = groups
        .iter()
        .map(|g| g.realized_pnl)
        .filter(|p| *p > Decimal::ZERO)
        .collect();
    let losses: Vec<Decimal> = groups
        .iter()
        .map(|g| g.realized_pnl)
        .filter(|p| *p < Decimal::ZERO)
        .collect();
    let zero = n - wins.len() - losses.len();
    let gross_profit: Decimal = wins.iter().copied().sum();
    let gross_loss_abs: Decimal = -losses.iter().copied().sum::<Decimal>();
    let status = if n == 0 {
        WinRateStatus::NoClosedTrades
    } else {
        WinRateStatus::Computed
    };
    let win_rate = (n > 0).then(|| ratio(Decimal::from(wins.len()), Decimal::from(n)));
    let special = |w: bool, l: bool| {
        if n == 0 {
            Some(RatioMetric::NoClosedTrades)
        } else if !w {
            Some(RatioMetric::NoWins)
        } else if !l {
            Some(RatioMetric::NoLosses)
        } else {
            None
        }
    };
    let (has_w, has_l) = (!wins.is_empty(), !losses.is_empty());
    let payoff_ratio = special(has_w, has_l).unwrap_or_else(|| {
        let avg_w = gross_profit / Decimal::from(wins.len());
        let avg_l = gross_loss_abs / Decimal::from(losses.len());
        RatioMetric::Computed(ratio(avg_w, avg_l))
    });
    let profit_factor = special(has_w, has_l)
        .unwrap_or_else(|| RatioMetric::Computed(ratio(gross_profit, gross_loss_abs)));
    let largest_win = wins.iter().copied().max();
    let largest_loss = losses.iter().copied().min();
    let extreme_payoff_ratio = special(has_w, has_l).unwrap_or_else(|| {
        RatioMetric::Computed(ratio(largest_win.unwrap(), -largest_loss.unwrap()))
    });
    let total: Decimal = groups.iter().map(|g| g.realized_pnl).sum();
    let expectancy = ExpectancySummary {
        status,
        sample_count: n,
        value: (n > 0).then(|| ratio(total, Decimal::from(n))),
    };
    let holding: usize = groups
        .iter()
        .map(|g| g.exit_bar_index.saturating_sub(g.entry_bar_index))
        .sum();
    AccountGroupMetrics {
        win_rate: WinRateSummary {
            status,
            closed_trade_count: n,
            winning_count: wins.len(),
            losing_count: losses.len(),
            zero_pnl_count: zero,
            win_rate,
            win_rate_pct: win_rate.map(|w| (w * Decimal::from(100)).round_dp(4)),
        },
        gross_profit,
        gross_loss_abs,
        payoff_ratio,
        profit_factor,
        largest_win,
        largest_loss,
        extreme_payoff_ratio,
        expectancy,
        average_holding_bars: (n > 0).then(|| ratio(Decimal::from(holding), Decimal::from(n))),
        sample_insufficient: n < ACCOUNT_REPORT_MIN_GROUPS,
    }
}

fn run_pnl(r: &AccountRunInput) -> Decimal {
    r.settlement.unwrap_or(r.mark_equity) - r.allocation
}

fn breakdown(
    runs: &[AccountRunInput],
    key: impl Fn(&AccountRunInput) -> String,
) -> Vec<AccountBreakdown> {
    let mut keys: Vec<String> = runs.iter().map(&key).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|k| {
            let members: Vec<&AccountRunInput> = runs.iter().filter(|r| key(r) == k).collect();
            let groups: Vec<&ClosedTradeGroup> = members
                .iter()
                .flat_map(|r| r.closed_groups.iter())
                .collect();
            AccountBreakdown {
                key: k,
                run_count: members.len(),
                pnl: members.iter().map(|r| run_pnl(r)).sum(),
                metrics: compute_group_metrics(&groups),
            }
        })
        .collect()
}

pub fn compute_account_report(
    quote_currency: &str,
    entries: &[AccountEntryInput],
    runs: &[AccountRunInput],
) -> AccountReport {
    // Equity = balance + allocations held by runs that have not settled yet.
    let mut balance = Decimal::ZERO;
    let mut outstanding: std::collections::BTreeMap<String, Decimal> = Default::default();
    let mut curve = Vec::with_capacity(entries.len() + 1);
    let mut net_deposits = Decimal::ZERO;
    for e in entries {
        balance += e.amount;
        match (e.kind.as_str(), &e.run_id) {
            ("deposit" | "withdrawal", _) => net_deposits += e.amount,
            ("run_allocation", Some(id)) => {
                *outstanding.entry(id.clone()).or_default() -= e.amount;
            }
            ("run_settlement", Some(id)) => {
                outstanding.remove(id);
            }
            ("delete_reversal", Some(id)) => {
                outstanding.remove(id);
            }
            _ => {}
        }
        curve.push(AccountEquityPoint {
            seq: Some(e.seq),
            kind: e.kind.clone(),
            equity: balance + outstanding.values().copied().sum::<Decimal>(),
        });
    }
    let open_mark: Decimal = runs
        .iter()
        .filter(|r| r.settlement.is_none() && outstanding.contains_key(&r.run_id))
        .map(|r| r.mark_equity)
        .sum();
    let equity = balance + open_mark;
    if runs.iter().any(|r| r.settlement.is_none()) {
        curve.push(AccountEquityPoint {
            seq: None,
            kind: "mark_to_market".to_string(),
            equity,
        });
    }
    let ended: Vec<&AccountRunInput> = runs.iter().filter(|r| r.settlement.is_some()).collect();
    let winning = ended.iter().filter(|r| run_pnl(r) > Decimal::ZERO).count();
    let groups: Vec<&ClosedTradeGroup> = runs.iter().flat_map(|r| r.closed_groups.iter()).collect();
    let planned: usize = runs.iter().map(|r| r.planned_closed_group_count).sum();
    let compliant: usize = runs.iter().map(|r| r.plan_compliant_count).sum();
    AccountReport {
        report_version: ACCOUNT_REPORT_VERSION.to_string(),
        quote_currency: quote_currency.to_string(),
        balance,
        equity,
        net_deposits,
        cumulative_pnl: runs.iter().map(run_pnl).sum(),
        equity_curve: curve,
        run_count: runs.len(),
        ended_run_count: ended.len(),
        winning_run_count: winning,
        run_win_rate: (!ended.is_empty())
            .then(|| ratio(Decimal::from(winning), Decimal::from(ended.len()))),
        groups: compute_group_metrics(&groups),
        total_fees: runs.iter().map(|r| r.total_fees).sum(),
        plan_execution_rate: (planned > 0)
            .then(|| ratio(Decimal::from(compliant), Decimal::from(planned))),
        undo_count: runs.iter().map(|r| r.undo_count).sum(),
        by_market_category: breakdown(runs, |r| r.market_category.clone()),
        by_timeframe: breakdown(runs, |r| r.timeframe.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_report::PlanCompliance;
    use chrono::{NaiveDate, TimeZone, Utc};

    fn g(pnl: i64, entry: usize, exit: usize) -> ClosedTradeGroup {
        let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let z = Decimal::ZERO;
        ClosedTradeGroup {
            group_id: format!("G{pnl}{entry}"),
            instrument_id: "X".into(),
            entry_bar_index: entry,
            exit_bar_index: exit,
            entry_session_date: d,
            exit_session_date: d,
            entry_at: t,
            exit_at: t,
            entry_fill_ids: vec![],
            exit_fill_ids: vec![],
            total_quantity: Decimal::ONE,
            weighted_avg_entry_price: z,
            weighted_avg_exit_price: z,
            gross_pnl: Decimal::from(pnl),
            total_fees_quote: z,
            realized_pnl: Decimal::from(pnl),
            min_path_price: z,
            max_path_price: z,
            mae_per_unit: z,
            mae_ratio: z,
            mae_pct: z,
            mfe_per_unit: z,
            mfe_ratio: z,
            mfe_pct: z,
            has_plan: false,
            plan_compliance: PlanCompliance::Unplanned,
        }
    }

    fn e(seq: i64, kind: &str, amount: i64, run: Option<&str>) -> AccountEntryInput {
        AccountEntryInput {
            seq,
            kind: kind.into(),
            amount: Decimal::from(amount),
            run_id: run.map(str::to_string),
        }
    }

    #[test]
    fn account_report_is_hand_checkable() {
        let entries = vec![
            e(1, "deposit", 100_000, None),
            e(2, "run_allocation", -100_000, Some("R1")),
            e(3, "run_settlement", 103_000, Some("R1")),
            e(4, "withdrawal", -3_000, None),
            e(5, "run_allocation", -50_000, Some("R2")),
            e(6, "run_settlement", 49_000, Some("R2")),
            e(7, "run_allocation", -99_000, Some("R3")),
        ];
        let run = |id: &str, tf: &str, alloc: i64, settle: Option<i64>, mark: i64, groups| {
            AccountRunInput {
                run_id: id.into(),
                market_category: "crypto_spot".into(),
                timeframe: tf.into(),
                allocation: Decimal::from(alloc),
                settlement: settle.map(Decimal::from),
                mark_equity: Decimal::from(mark),
                closed_groups: groups,
                total_fees: Decimal::from(10),
                planned_closed_group_count: 2,
                plan_compliant_count: 1,
                undo_count: 1,
            }
        };
        let runs = vec![
            run(
                "R1",
                "1d",
                100_000,
                Some(103_000),
                103_000,
                vec![g(4_000, 1, 5), g(-1_000, 6, 8)],
            ),
            run(
                "R2",
                "4h",
                50_000,
                Some(49_000),
                49_000,
                vec![g(-1_000, 0, 2)],
            ),
            run("R3", "1d", 99_000, None, 99_500, vec![g(500, 3, 4)]),
        ];
        let r = compute_account_report("USDT", &entries, &runs);
        assert_eq!(r.report_version, "m5-account-report-1");
        // Balance = sum of entries = 100000 -100000 +103000 -3000 -50000 +49000 -99000 = 0.
        assert_eq!(r.balance, Decimal::ZERO);
        // Equity = balance + open run mark 99500.
        assert_eq!(r.equity, Decimal::from(99_500));
        assert_eq!(r.net_deposits, Decimal::from(97_000));
        // PnL = +3000 -1000 +500.
        assert_eq!(r.cumulative_pnl, Decimal::from(2_500));
        assert_eq!(r.equity_curve.len(), 8);
        assert_eq!(
            r.equity_curve[1].equity,
            Decimal::from(100_000),
            "allocation keeps equity"
        );
        assert_eq!(r.equity_curve[2].equity, Decimal::from(103_000));
        assert_eq!(r.equity_curve[6].equity, Decimal::from(99_000));
        assert_eq!(r.equity_curve[7].equity, Decimal::from(99_500));
        assert_eq!(r.ended_run_count, 2);
        assert_eq!(r.run_win_rate, Some(Decimal::new(5, 1)));
        // Groups: +4000, -1000, -1000, +500.
        assert_eq!(r.groups.win_rate.win_rate, Some(Decimal::new(5, 1)));
        assert_eq!(
            r.groups.profit_factor,
            RatioMetric::Computed(Decimal::new(225, 2))
        );
        // Avg win 2250 / avg loss 1000.
        assert_eq!(
            r.groups.payoff_ratio,
            RatioMetric::Computed(Decimal::new(225, 2))
        );
        assert_eq!(
            r.groups.extreme_payoff_ratio,
            RatioMetric::Computed(Decimal::from(4))
        );
        assert_eq!(r.groups.expectancy.value, Some(Decimal::from(625)));
        // Holding bars: 4 + 2 + 2 + 1 = 9 / 4.
        assert_eq!(r.groups.average_holding_bars, Some(Decimal::new(225, 2)));
        assert!(r.groups.sample_insufficient);
        assert_eq!(r.total_fees, Decimal::from(30));
        assert_eq!(r.plan_execution_rate, Some(Decimal::new(5, 1)));
        assert_eq!(r.undo_count, 3);
        assert_eq!(r.by_timeframe.len(), 2);
        let d1 = r.by_timeframe.iter().find(|b| b.key == "1d").unwrap();
        assert_eq!(d1.pnl, Decimal::from(3_500));
        assert_eq!(d1.metrics.win_rate.closed_trade_count, 3);
        // No trades: special states, never divide by zero.
        let empty = compute_account_report("USD", &[e(1, "deposit", 100_000, None)], &[]);
        assert_eq!(empty.groups.profit_factor, RatioMetric::NoClosedTrades);
        assert_eq!(empty.run_win_rate, None);
        let no_loss = compute_group_metrics(&[&g(10, 0, 1)]);
        assert_eq!(no_loss.profit_factor, RatioMetric::NoLosses);
    }
}
