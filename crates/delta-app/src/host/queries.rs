use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};

use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{
    evaluate_period_breakdown, evaluate_portfolio, parse_decimal_lossless,
    replay_ledger_events_with_bindings, resolve_fx_rate_s07, to_canonical_text, AdjustmentKind,
    BarRecord, DataQualityIssue, EconomicEventType, LedgerEventInput, LedgerProjectionResult,
    ManualValuation, PeriodBreakdownReport, QualityStatus, ValuationReport,
};
use delta_data::{
    load_all_ledger_events, load_book_fx_lookup_at_version, load_event_book_fx,
    query_calendar_session, NoteDto, ReconciliationIssueRecord, SyncCheckpointRecord,
};
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use super::scope::{
    AnalysisScope, DataQualitySummaryDto, EvidenceRef, HoldingsAndLotsDto, OverviewMetricsDto,
    ResultEnvelope, SavedReportStatusDto, ScopeTarget, TransactionFilterDto, TransactionRowDto,
    M2_CALCULATION_VERSION,
};
use crate::kline::{ChartMarkerDto, KlineViewport, KlineWindowSliceDto, MarkerKind};
use crate::service::{venue_to_calendar, AppService, ErrorEnvelope};

struct ScopedPointEvaluation {
    filtered_events: Vec<LedgerEventInput>,
    projection: LedgerProjectionResult,
    valuation: ValuationReport,
}

struct PreloadedInstrumentBars {
    instrument_id: String,
    base_asset_id: String,
    calendar_venue: &'static str,
    bars: Vec<BarRecord>,
}

/// Preloaded immutable market, ledger, and FX snapshot for one scoped query (`APP-Q02`, `APP-B05`).
/// Allows evaluating multiple dates in `query_period_breakdown` entirely in memory without N+1 SQLite reads.
struct ScopedValuationContext {
    book_currency: String,
    report_currency: String,
    ledger_revision: i64,
    resolved_accounts: Vec<String>,
    visible_until: Option<DateTime<Utc>>,
    active_events: Vec<LedgerEventInput>,
    book_fx: HashMap<(String, NaiveDate), Decimal>,
    bindings: HashMap<(String, String), Decimal>,
    instruments: Vec<PreloadedInstrumentBars>,
    asset_quote_ccys: HashMap<String, String>,
    fx_map: HashMap<(String, String, NaiveDate), Decimal>,
}

impl ScopedValuationContext {
    fn load(
        service: &AppService,
        resolved: &AnalysisScope,
        max_date: NaiveDate,
    ) -> Result<Self, ErrorEnvelope> {
        let book_currency: String = service
            .store()
            .conn()
            .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_LIBRARY_FAILED", e.to_string(), false))?;

        let all_events = load_all_ledger_events(service.store())
            .map_err(|e| ErrorEnvelope::new("LOAD_LEDGER_FAILED", e.to_string(), false))?;
        let active_events: Vec<LedgerEventInput> = all_events
            .into_iter()
            .filter(|e| {
                e.active_in_correction_group
                    && e.occurred_at.date_naive() <= max_date
                    && resolved
                        .visible_until
                        .is_none_or(|cut| e.occurred_at <= cut)
            })
            .collect();

        let dsv_for_lookup = resolved.dataset_version;
        let book_fx =
            load_book_fx_lookup_at_version(service.store(), &book_currency, dsv_for_lookup)
                .map_err(|e| ErrorEnvelope::new("LOAD_BOOK_FX_FAILED", e.to_string(), false))?;
        let bindings = load_event_book_fx(service.store())
            .map_err(|e| ErrorEnvelope::new("LOAD_EVENT_FX_FAILED", e.to_string(), false))?;

        let mut asset_quote_ccys: HashMap<String, String> = HashMap::new();
        let reg = service
            .store()
            .load_instrument_registry()
            .map_err(|e| ErrorEnvelope::new("REGISTRY_FAILED", e.to_string(), false))?;
        let mut reg_instruments: Vec<_> = reg.instruments.iter().collect();
        reg_instruments.sort_by_key(|(id, _)| *id);

        let max_day_cutoff = max_date
            .succ_opt()
            .unwrap_or(max_date)
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let max_cutoff = resolved
            .visible_until
            .map_or(max_day_cutoff, |cut| cut.min(max_day_cutoff));

        let mut instruments = Vec::with_capacity(reg_instruments.len());
        for (inst_id, inst) in reg_instruments {
            asset_quote_ccys.insert(inst_id.clone(), inst.quote_asset_id.clone());
            asset_quote_ccys.insert(inst.base_asset_id.clone(), inst.quote_asset_id.clone());

            let mut bars = service
                .store()
                .query_bars_at_version(
                    inst_id,
                    "1d",
                    AdjustmentKind::Raw,
                    None,
                    dsv_for_lookup,
                    resolved.visible_until,
                )
                .map_err(|e| ErrorEnvelope::new("QUERY_BARS_FAILED", e.to_string(), false))?;
            bars.retain(|b| b.session_date <= max_date && b.close_at <= max_cutoff);
            bars.sort_by(|a, b| {
                a.session_date
                    .cmp(&b.session_date)
                    .then_with(|| a.source.cmp(&b.source))
            });
            bars.dedup_by_key(|b| b.session_date);

            instruments.push(PreloadedInstrumentBars {
                instrument_id: inst_id.clone(),
                base_asset_id: inst.base_asset_id.clone(),
                calendar_venue: venue_to_calendar(&inst.venue),
                bars,
            });
        }

        // Reuse `AppService::load_fx_rates` so unpublished dataset FX rows are excluded and
        // stored FX decimals/timestamps are strictly validated (`APP-B05`).
        let fx_records = service.load_fx_rates(resolved.dataset_version)?;
        let mut fx_map = HashMap::new();
        for rec in fx_records {
            if resolved
                .visible_until
                .is_some_and(|cut| rec.effective_at > cut)
            {
                continue;
            }
            fx_map.insert(
                (
                    rec.base_currency.to_ascii_uppercase(),
                    rec.quote_currency.to_ascii_uppercase(),
                    rec.effective_date,
                ),
                rec.rate,
            );
        }

        Ok(Self {
            book_currency,
            report_currency: resolved.report_currency.clone(),
            ledger_revision: resolved.ledger_revision,
            resolved_accounts: resolved.resolved_accounts.clone(),
            visible_until: resolved.visible_until,
            active_events,
            book_fx,
            bindings,
            instruments,
            asset_quote_ccys,
            fx_map,
        })
    }

    fn evaluate_at(&self, target_date: NaiveDate) -> Result<ScopedPointEvaluation, ErrorEnvelope> {
        let filtered_events: Vec<LedgerEventInput> = self
            .active_events
            .iter()
            .filter(|e| e.occurred_at.date_naive() <= target_date)
            .cloned()
            .collect();

        let projection = replay_ledger_events_with_bindings(
            &self.book_currency,
            &filtered_events,
            &self.book_fx,
            &self.bindings,
            self.ledger_revision,
        )
        .map_err(|e| ErrorEnvelope::new("LEDGER_PROJECTION_FAILED", e.to_string(), false))?;

        let day_cutoff = target_date
            .succ_opt()
            .unwrap_or(target_date)
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let cutoff = self
            .visible_until
            .map_or(day_cutoff, |cut| cut.min(day_cutoff));

        let mut market_prices: HashMap<(String, NaiveDate), Decimal> = HashMap::new();
        for inst in &self.instruments {
            let mut last_bar: Option<&BarRecord> = None;
            for b in &inst.bars {
                if b.session_date > target_date {
                    break;
                }
                if b.close_at > cutoff {
                    continue;
                }
                market_prices.insert((inst.instrument_id.clone(), b.session_date), b.close);
                market_prices.insert((inst.base_asset_id.clone(), b.session_date), b.close);
                last_bar = Some(b);
            }
            if let Some(last) = last_bar {
                if last.session_date < target_date {
                    let mut d = last.session_date;
                    let mut fresh = true;
                    while d < target_date {
                        let Some(next) = d.succ_opt() else {
                            fresh = false;
                            break;
                        };
                        d = next;
                        if query_calendar_session(inst.calendar_venue, d).kind
                            != delta_core::CalendarSessionKind::Closed
                        {
                            fresh = false;
                            break;
                        }
                    }
                    if fresh {
                        market_prices.insert((inst.instrument_id.clone(), target_date), last.close);
                        market_prices.insert((inst.base_asset_id.clone(), target_date), last.close);
                    }
                }
            }
        }

        let liabilities = HashMap::new();
        let valuation = catch_unwind(AssertUnwindSafe(|| {
            evaluate_portfolio(
                &projection,
                &self.resolved_accounts,
                &self.report_currency,
                target_date,
                &market_prices,
                &self.asset_quote_ccys,
                &self.fx_map,
                &liabilities,
            )
        }))
        .map_err(|_| {
            ErrorEnvelope::new(
                "VALUATION_OVERFLOW",
                "valuation exceeds supported Decimal range",
                false,
            )
        })?;

        Ok(ScopedPointEvaluation {
            filtered_events,
            projection,
            valuation,
        })
    }
}

impl AppService {
    pub fn query_scope_describe(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Value>, ErrorEnvelope> {
        let resolved = self.resolve_and_validate_scope(scope)?;
        if let ScopeTarget::TrainingRun(ref run_id) = resolved.target {
            let summary = self.get_training_run_summary(run_id)?;
            let val = json!({
                "target": resolved.target,
                "simulated_training": true,
                "simulated_training_badge": crate::training::SIMULATED_TRAINING_BADGE,
                "run_id": run_id,
                "display_symbol": summary.display_symbol,
                "current_bar_label": summary.current_bar_label,
                "hidden_mode": summary.hidden_mode,
                "revealed": summary.revealed,
                "resolved_accounts": resolved.resolved_accounts,
                "report_currency": resolved.report_currency,
                "dataset_version": resolved.dataset_version,
                "ledger_revision": resolved.ledger_revision,
                "include_notes": resolved.include_notes,
                "scope_hash": resolved.scope_hash(),
            });
            let evidence = vec![
                EvidenceRef {
                    kind: "fee_profile_version".to_string(),
                    id: summary.fee_profile_version_id.clone(),
                    revision: 1,
                    label: format!("模拟费用配置 {}", summary.fee_profile_name),
                },
                EvidenceRef {
                    kind: "dataset_version".to_string(),
                    id: format!("DSV-{:06}", resolved.dataset_version),
                    revision: resolved.dataset_version,
                    label: format!("Dataset v{}", resolved.dataset_version),
                },
            ];
            return Ok(ResultEnvelope::build_deterministic(
                "scope.describe",
                req_id,
                generation,
                val,
                resolved,
                QualityStatus::Exact,
                Vec::new(),
                evidence,
                vec!["数据来自模拟训练，与真实账户账本隔离".to_string()],
            ));
        }
        let val = json!({
            "target": resolved.target,
            "resolved_accounts": resolved.resolved_accounts,
            "start_date": resolved.start_date.map(|d| d.to_string()),
            "as_of_date": resolved.as_of_date.to_string(),
            "report_currency": resolved.report_currency,
            "dataset_version": resolved.dataset_version,
            "ledger_revision": resolved.ledger_revision,
            "visible_until": resolved.visible_until.map(|t| t.to_rfc3339()),
            "include_notes": resolved.include_notes,
            "scope_hash": resolved.scope_hash(),
        });
        let evidence = vec![EvidenceRef {
            kind: "dataset_version".to_string(),
            id: format!("DSV-{:06}", resolved.dataset_version),
            revision: resolved.dataset_version,
            label: format!(
                "Dataset v{} / Ledger r{}",
                resolved.dataset_version, resolved.ledger_revision
            ),
        }];
        Ok(ResultEnvelope::build_deterministic(
            "scope.describe",
            req_id,
            generation,
            val,
            resolved,
            QualityStatus::Exact,
            Vec::new(),
            evidence,
            Vec::new(),
        ))
    }

    pub fn query_overview(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<OverviewMetricsDto>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let ctx = ScopedValuationContext::load(self, &resolved, resolved.as_of_date)?;
        let eval = ctx.evaluate_at(resolved.as_of_date)?;
        let manual_vals = self
            .store()
            .list_manual_valuations(Some(&resolved.resolved_accounts))
            .map_err(|e| ErrorEnvelope::new("QUERY_VALUATIONS_FAILED", e.to_string(), false))?;

        let is_stable = |ccy: &str| matches!(ccy.to_ascii_uppercase().as_str(), "USDT" | "USDC");
        let mut missing_inputs = eval.valuation.missing_inputs.clone();
        let mut quality = eval.valuation.quality;
        let mut warnings = eval.valuation.warnings.clone();

        let mut latest_manual: BTreeMap<(String, String), &ManualValuation> = BTreeMap::new();
        for mv in &manual_vals {
            if mv.as_of <= resolved.as_of_date {
                let key = (mv.account_id.clone(), mv.asset_or_liability_id.clone());
                if latest_manual
                    .get(&key)
                    .is_none_or(|prev| mv.as_of >= prev.as_of)
                {
                    latest_manual.insert(key, mv);
                }
            }
        }

        let overflow = || {
            ErrorEnvelope::new(
                "VALUATION_OVERFLOW",
                "manual valuation exceeds supported Decimal range",
                false,
            )
        };
        let mut manual_net = Decimal::ZERO;
        let mut evidence_refs = Vec::new();
        for mv in latest_manual.values() {
            let stable = is_stable(&mv.currency) || is_stable(&resolved.report_currency);
            match resolve_fx_rate_s07(
                &mv.currency,
                &resolved.report_currency,
                resolved.as_of_date,
                stable,
                &ctx.fx_map,
            ) {
                Some(found) => {
                    let converted = mv
                        .value
                        .abs()
                        .checked_mul(found.rate)
                        .ok_or_else(overflow)?
                        .normalize();
                    manual_net = if mv.is_liability {
                        manual_net.checked_sub(converted).ok_or_else(overflow)?
                    } else {
                        manual_net.checked_add(converted).ok_or_else(overflow)?
                    };
                    evidence_refs.push(EvidenceRef {
                        kind: "manual_valuation".to_string(),
                        id: mv.id.clone(),
                        revision: 1,
                        label: format!(
                            "{} ({}, as of {})",
                            mv.asset_or_liability_id, mv.method, mv.as_of
                        ),
                    });
                }
                None => {
                    quality = QualityStatus::Partial;
                    let item = format!(
                        "manual_valuation_fx:{}:{}/{}@{}",
                        mv.asset_or_liability_id,
                        mv.currency,
                        resolved.report_currency,
                        resolved.as_of_date
                    );
                    if !missing_inputs.contains(&item) {
                        missing_inputs.push(item);
                    }
                }
            }
        }

        let known_partial_net_worth = eval
            .valuation
            .net_worth
            .checked_add(manual_net)
            .ok_or_else(overflow)?
            .normalize();
        let net_worth = known_partial_net_worth;
        let excluded_items = missing_inputs.clone();

        let (total_pnl, realized_pnl, unrealized_pnl) = if !missing_inputs.is_empty() {
            if warnings.is_empty() {
                warnings.push(
                    "incomplete valuation inputs or unknown cost basis: PnL is not fabricated as zero"
                        .to_string(),
                );
            }
            (None, None, None)
        } else {
            let unrea = eval
                .valuation
                .unrealized_pnl
                .map(|u| {
                    u.checked_add(manual_net)
                        .map(|v| v.normalize())
                        .ok_or_else(overflow)
                })
                .transpose()?;
            let tot = eval
                .valuation
                .total_pnl
                .map(|t| {
                    t.checked_add(manual_net)
                        .map(|v| v.normalize())
                        .ok_or_else(overflow)
                })
                .transpose()?;
            (tot, eval.valuation.realized_pnl, unrea)
        };

        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();
        for ev in &eval.filtered_events {
            if scope_set.contains(&ev.account_id) {
                evidence_refs.push(EvidenceRef {
                    kind: "economic_event".to_string(),
                    id: ev.event_id.clone(),
                    revision: ev.revision,
                    label: format!(
                        "{} {} {}",
                        ev.event_type.as_str(),
                        to_canonical_text(ev.quantity),
                        ev.asset_id
                    ),
                });
            }
        }

        let mut pending_reconciliation_count = 0usize;
        if let Ok(mut stmt) = self
            .store()
            .conn()
            .prepare("SELECT account_id, resolution_event_id FROM reconciliation_issue")
        {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }) {
                for (acct, res_ev) in rows.flatten() {
                    if scope_set.contains(&acct) && res_ev.is_none() {
                        pending_reconciliation_count += 1;
                    }
                }
            }
        }

        let scope_hash = resolved.scope_hash();
        let report_id = format!(
            "REP:OVERVIEW:{}:{}:{}",
            resolved.ledger_revision,
            resolved.dataset_version,
            &scope_hash[..16]
        );
        evidence_refs.push(EvidenceRef {
            kind: "analysis_report".to_string(),
            id: report_id.clone(),
            revision: resolved.ledger_revision,
            label: format!("Overview Report @ {}", resolved.as_of_date),
        });

        let coverage_summary = if missing_inputs.is_empty() {
            format!(
                "complete (dataset v{}, ledger r{})",
                resolved.dataset_version, resolved.ledger_revision
            )
        } else {
            format!(
                "partial ({} missing inputs: {})",
                missing_inputs.len(),
                missing_inputs.join(", ")
            )
        };

        let dto = OverviewMetricsDto {
            cash_value: eval.valuation.cash_value,
            holdings_market_value: eval.valuation.holdings_market_value,
            net_worth,
            net_invested: eval.valuation.net_external_cash_flow,
            total_pnl,
            realized_pnl,
            unrealized_pnl,
            dividend_income: eval.valuation.dividend_income,
            fee_summary_explanatory: eval.valuation.fee_summary_explanatory,
            known_partial_net_worth,
            excluded_items,
            coverage_summary,
            pending_reconciliation_count,
            report_id: report_id.clone(),
            is_stale: false,
        };

        // Persist in `analysis_report` so later amendments can mark it stale (`M2-A-05`).
        if let Ok(payload_json) = serde_json::to_string(&dto) {
            let scope_json = serde_json::to_string(&resolved).unwrap_or_default();
            self.persist_analysis_report(
                &report_id,
                "overview_metrics",
                &scope_json,
                resolved.ledger_revision,
                resolved.dataset_version,
                M2_CALCULATION_VERSION,
                &payload_json,
                quality.as_str(),
                true,
                "REPORT_SAVE_FAILED",
            )?;
        }

        Ok(ResultEnvelope::build_deterministic(
            "overview.metrics",
            req_id,
            generation,
            dto,
            resolved,
            quality,
            missing_inputs,
            evidence_refs,
            warnings,
        ))
    }

    pub fn query_period_breakdown(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<PeriodBreakdownReport>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let end_date = resolved.as_of_date;
        let ctx = ScopedValuationContext::load(self, &resolved, end_date)?;
        let end_eval = ctx.evaluate_at(end_date)?;

        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();
        let earliest_event_date = end_eval
            .filtered_events
            .iter()
            .filter(|e| scope_set.contains(&e.account_id))
            .map(|e| e.occurred_at.date_naive())
            .min();

        let start_date = resolved.start_date.unwrap_or_else(|| {
            earliest_event_date
                .and_then(|d| d.pred_opt())
                .unwrap_or(end_date)
        });
        let opening_eval_date = if resolved.start_date.is_some() {
            start_date.pred_opt().unwrap_or(start_date)
        } else {
            start_date
        };

        let start_eval = ctx.evaluate_at(opening_eval_date)?;
        let manual_vals = self
            .store()
            .list_manual_valuations(Some(&resolved.resolved_accounts))
            .map_err(|e| ErrorEnvelope::new("QUERY_VALUATIONS_FAILED", e.to_string(), false))?;

        // Build comparable NAV series for drawdown when inputs are complete (`APP-B15`, `APP-Q02`).
        // Sample start/end, event dates, and every available market bar / FX date for instruments
        // and currencies exposed in the scope so non-trade-day price drops are captured accurately
        // without querying SQLite per day or breaking sparse fixtures.
        let mut comparable_nav = Vec::new();
        if start_eval.valuation.missing_inputs.is_empty()
            && end_eval.valuation.missing_inputs.is_empty()
        {
            let mut dates = BTreeSet::new();
            dates.insert(start_date);
            let mut exposed_assets: BTreeSet<String> = BTreeSet::new();
            let mut exposed_currencies: BTreeSet<String> = BTreeSet::new();
            for ((acct, asset), h) in &start_eval.projection.holdings {
                if scope_set.contains(acct) && !h.quantity.is_zero() {
                    exposed_assets.insert(asset.clone());
                }
            }
            for ((acct, ccy), qty) in &start_eval.projection.cash_balances {
                if scope_set.contains(acct)
                    && !qty.is_zero()
                    && !ccy.eq_ignore_ascii_case(&resolved.report_currency)
                {
                    exposed_currencies.insert(ccy.to_ascii_uppercase());
                }
            }
            for ev in &end_eval.filtered_events {
                if !scope_set.contains(&ev.account_id) {
                    continue;
                }
                let d = ev.occurred_at.date_naive();
                if d >= start_date && d <= end_date {
                    dates.insert(d);
                }
                if let Some(inst) = &ev.instrument_id {
                    exposed_assets.insert(inst.clone());
                }
                exposed_assets.insert(ev.asset_id.clone());
                if !ev
                    .quote_currency
                    .eq_ignore_ascii_case(&resolved.report_currency)
                {
                    exposed_currencies.insert(ev.quote_currency.to_ascii_uppercase());
                }
            }
            for inst in &ctx.instruments {
                if exposed_assets.contains(&inst.instrument_id)
                    || exposed_assets.contains(&inst.base_asset_id)
                {
                    for bar in &inst.bars {
                        if bar.session_date >= start_date && bar.session_date <= end_date {
                            dates.insert(bar.session_date);
                        }
                    }
                }
            }
            if !exposed_currencies.is_empty() {
                for (base, quote, fx_date) in ctx.fx_map.keys() {
                    if *fx_date >= start_date
                        && *fx_date <= end_date
                        && (exposed_currencies.contains(base) || exposed_currencies.contains(quote))
                    {
                        dates.insert(*fx_date);
                    }
                }
            }
            dates.insert(end_date);

            let mut prev_nav: Option<Decimal> = None;
            let mut prev_cap = Decimal::ZERO;
            let mut norm = Decimal::ONE;
            for d in dates {
                let evaluated_pt;
                let pt = if d == end_date {
                    &end_eval.valuation
                } else {
                    evaluated_pt = ctx.evaluate_at(d)?;
                    &evaluated_pt.valuation
                };
                if !pt.missing_inputs.is_empty() {
                    comparable_nav.clear();
                    break;
                }
                let nav = pt.net_worth;
                let cap = pt.net_external_cash_flow;
                let flow = cap - prev_cap;
                if let Some(p_nav) = prev_nav {
                    if p_nav > Decimal::ZERO {
                        let Some(ratio) = (nav - flow).checked_div(p_nav) else {
                            comparable_nav.clear();
                            break;
                        };
                        let Some(next_norm) = norm.checked_mul(ratio) else {
                            comparable_nav.clear();
                            break;
                        };
                        norm = next_norm.normalize();
                        comparable_nav.push(norm);
                    }
                } else if cap > Decimal::ZERO {
                    let Some(initial) = nav.checked_div(cap) else {
                        comparable_nav.clear();
                        break;
                    };
                    norm = initial.normalize();
                    comparable_nav.push(norm);
                } else if nav > Decimal::ZERO {
                    comparable_nav.push(Decimal::ONE);
                }
                prev_nav = Some(nav);
                prev_cap = cap;
            }
        }

        let report = evaluate_period_breakdown(
            &resolved.resolved_accounts,
            start_date,
            end_date,
            &resolved.report_currency,
            &start_eval.valuation,
            &end_eval.valuation,
            &start_eval.projection,
            &end_eval.projection,
            &ctx.fx_map,
            &manual_vals,
            &comparable_nav,
        );

        let mut evidence_refs = Vec::new();
        for ev in &end_eval.filtered_events {
            if scope_set.contains(&ev.account_id) {
                evidence_refs.push(EvidenceRef {
                    kind: "economic_event".to_string(),
                    id: ev.event_id.clone(),
                    revision: ev.revision,
                    label: format!(
                        "{} {} {}",
                        ev.event_type.as_str(),
                        to_canonical_text(ev.quantity),
                        ev.asset_id
                    ),
                });
            }
        }
        for mv in &report.manual_valuations {
            evidence_refs.push(EvidenceRef {
                kind: "manual_valuation".to_string(),
                id: mv.id.clone(),
                revision: 1,
                label: format!(
                    "{} ({}, as of {})",
                    mv.asset_or_liability_id, mv.method, mv.as_of
                ),
            });
        }

        let scope_hash = resolved.scope_hash();
        let report_id = format!(
            "REP:BREAKDOWN:{}:{}:{}",
            resolved.ledger_revision,
            resolved.dataset_version,
            &scope_hash[..16]
        );
        evidence_refs.push(EvidenceRef {
            kind: "analysis_report".to_string(),
            id: report_id.clone(),
            revision: resolved.ledger_revision,
            label: format!("Period Breakdown {}..{}", start_date, end_date),
        });

        if let Ok(payload_json) = serde_json::to_string(&report) {
            let scope_json = serde_json::to_string(&resolved).unwrap_or_default();
            self.persist_analysis_report(
                &report_id,
                "period_breakdown",
                &scope_json,
                resolved.ledger_revision,
                resolved.dataset_version,
                M2_CALCULATION_VERSION,
                &payload_json,
                report.quality.as_str(),
                true,
                "REPORT_SAVE_FAILED",
            )?;
        }

        let quality = report.quality;
        let missing = report.missing_inputs.clone();
        let warnings = report.warnings.clone();

        Ok(ResultEnvelope::build_deterministic(
            "report.period_breakdown",
            req_id,
            generation,
            report,
            resolved,
            quality,
            missing,
            evidence_refs,
            warnings,
        ))
    }

    pub fn query_transactions(
        &self,
        scope: &AnalysisScope,
        filter: &TransactionFilterDto,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<TransactionRowDto>>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();

        let mut stmt = self
            .store()
            .conn()
            .prepare(
                "SELECT e.id, e.account_id, e.type, e.occurred_at, e.asset_id, e.instrument_id,
                        e.quantity, e.price, e.quote_currency, e.fee_amount, e.fee_currency,
                        e.cost_known, e.revision, e.source_ref, r.batch_id, r.raw_payload
                 FROM economic_event e
                 LEFT JOIN import_row r ON r.id = (SELECT MIN(id) FROM import_row WHERE event_id = e.id)
                 WHERE e.active_in_correction_group = 1
                 ORDER BY e.occurred_at ASC, e.sequence_no ASC, e.id ASC",
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_TRANSACTIONS_FAILED", e.to_string(), false))?;

        let raw_rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, i64>(11)? != 0,
                    r.get::<_, i64>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, Option<String>>(15)?,
                ))
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_TRANSACTIONS_FAILED", e.to_string(), false))?;

        let mut matched = Vec::new();
        for row_res in raw_rows {
            let (
                event_id,
                account_id,
                event_type,
                occurred_at_str,
                asset_id,
                instrument_id,
                qty_s,
                price_s,
                quote_currency,
                fee_s,
                fee_currency,
                cost_known,
                revision,
                source_ref,
                batch_id,
                raw_payload,
            ) = row_res.map_err(|e| {
                ErrorEnvelope::new("QUERY_TRANSACTIONS_FAILED", e.to_string(), false)
            })?;
            if filter.event_id.as_ref().is_some_and(|id| id != &event_id) {
                continue;
            }

            if !scope_set.contains(&account_id) {
                continue;
            }
            {
                let dt = DateTime::parse_from_rfc3339(&occurred_at_str).map_err(|_| {
                    ErrorEnvelope::new("CORRUPT_LEDGER", "invalid event timestamp", false)
                })?;
                let utc_dt = dt.with_timezone(&Utc);
                let d = utc_dt.date_naive();
                if d > resolved.as_of_date {
                    continue;
                }
                if let Some(start) = resolved.start_date {
                    if d < start {
                        continue;
                    }
                }
                if let Some(cut) = resolved.visible_until {
                    if utc_dt > cut {
                        continue;
                    }
                }
            }

            if let Some(ref inst_filter) = filter.instrument_id {
                let needle = inst_filter.trim();
                if !needle.is_empty() {
                    let inst_match = instrument_id
                        .as_ref()
                        .is_some_and(|i| i.eq_ignore_ascii_case(needle));
                    let asset_match = asset_id.eq_ignore_ascii_case(needle);
                    if !inst_match && !asset_match {
                        continue;
                    }
                }
            }

            if let Some(ref type_filter) = filter.event_type {
                let needle = type_filter.trim();
                if !needle.is_empty() && !event_type.eq_ignore_ascii_case(needle) {
                    continue;
                }
            }

            let corrupt = || ErrorEnvelope::new("CORRUPT_LEDGER", "invalid event decimal", false);
            let quantity = parse_decimal_lossless(&qty_s).map_err(|_| corrupt())?;
            let price = price_s
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(parse_decimal_lossless)
                .transpose()
                .map_err(|_| corrupt())?;
            let fee_amount = parse_decimal_lossless(&fee_s).map_err(|_| corrupt())?;

            matched.push(TransactionRowDto {
                event_id,
                account_id,
                event_type,
                occurred_at: occurred_at_str,
                asset_id,
                instrument_id,
                quantity,
                price,
                quote_currency,
                fee_amount,
                fee_currency,
                cost_known,
                revision,
                source_ref,
                batch_id,
                raw_payload,
            });
        }

        let effective_limit = if filter.limit == 0 { 100 } else { filter.limit };
        let sliced: Vec<TransactionRowDto> = matched
            .into_iter()
            .skip(filter.offset)
            .take(effective_limit)
            .collect();

        let evidence_refs: Vec<EvidenceRef> = sliced
            .iter()
            .map(|r| EvidenceRef {
                kind: "economic_event".to_string(),
                id: r.event_id.clone(),
                revision: r.revision,
                label: format!(
                    "{} {} {} ({})",
                    r.event_type,
                    to_canonical_text(r.quantity),
                    r.asset_id,
                    r.account_id
                ),
            })
            .collect();

        Ok(ResultEnvelope::build_deterministic(
            "ledger.list_transactions",
            req_id,
            generation,
            sliced,
            resolved,
            QualityStatus::Exact,
            Vec::new(),
            evidence_refs,
            Vec::new(),
        ))
    }

    pub fn get_scoped_transaction(
        &self,
        scope: &AnalysisScope,
        id: &str,
        generation: u64,
    ) -> Result<TransactionRowDto, ErrorEnvelope> {
        let filter = TransactionFilterDto {
            event_id: Some(id.to_string()),
            limit: 1,
            ..TransactionFilterDto::default()
        };
        self.query_transactions(scope, &filter, "transaction-detail", generation)?
            .value
            .into_iter()
            .next()
            .ok_or_else(|| {
                ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    "transaction is missing or outside the selected scope",
                    false,
                )
            })
    }

    pub fn query_holdings_lots(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<HoldingsAndLotsDto>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let ctx = ScopedValuationContext::load(self, &resolved, resolved.as_of_date)?;
        let eval = ctx.evaluate_at(resolved.as_of_date)?;
        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();

        let mut cash_balances = Vec::new();
        for ((acct, ccy), qty) in &eval.projection.cash_balances {
            if !scope_set.contains(acct) || qty.is_zero() {
                continue;
            }
            let cost_book = eval
                .projection
                .cash_carrying_costs
                .get(&(acct.clone(), ccy.clone()))
                .copied()
                .unwrap_or(*qty);
            cash_balances.push((acct.clone(), ccy.clone(), *qty, cost_book));
        }

        let mut holdings = Vec::new();
        for ((acct, _), h) in &eval.projection.holdings {
            if scope_set.contains(acct) && !h.quantity.is_zero() {
                holdings.push(h.clone());
            }
        }

        let mut open_lots = Vec::new();
        let mut evidence_refs = Vec::new();
        for lot in &eval.projection.open_lots {
            if scope_set.contains(&lot.account_id) && !lot.remaining_quantity.is_zero() {
                evidence_refs.push(EvidenceRef {
                    kind: "cost_lot".to_string(),
                    id: lot.id.clone(),
                    revision: lot.ledger_revision,
                    label: format!(
                        "Lot {} ({} {}, cost {})",
                        lot.id,
                        to_canonical_text(lot.remaining_quantity),
                        lot.asset_id,
                        to_canonical_text(lot.remaining_cost_book)
                    ),
                });
                evidence_refs.push(EvidenceRef {
                    kind: "economic_event".to_string(),
                    id: lot.source_fill_id.clone(),
                    revision: 1,
                    label: format!("Source Fill {}", lot.source_fill_id),
                });
                open_lots.push(lot.clone());
            }
        }

        let dto = HoldingsAndLotsDto {
            cash_balances,
            holdings,
            open_lots,
        };

        Ok(ResultEnvelope::build_deterministic(
            "ledger.holdings_lots",
            req_id,
            generation,
            dto,
            resolved,
            eval.valuation.quality,
            eval.valuation.missing_inputs,
            evidence_refs,
            eval.valuation.warnings,
        ))
    }

    pub fn query_reconciliation_issues(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<ReconciliationIssueRecord>>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();

        let mut stmt = self
            .store()
            .conn()
            .prepare(
                "SELECT id, account_id, asset_id, as_of, expected, observed, difference, reason, resolution_event_id
                 FROM reconciliation_issue
                 ORDER BY as_of DESC, id ASC",
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_RECON_FAILED", e.to_string(), false))?;

        let rows = stmt
            .query_map([], |r| {
                Ok(ReconciliationIssueRecord {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    asset_id: r.get(2)?,
                    as_of: r.get(3)?,
                    expected: r.get(4)?,
                    observed: r.get(5)?,
                    difference: r.get(6)?,
                    reason: r.get(7)?,
                    resolution_event_id: r.get(8)?,
                })
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_RECON_FAILED", e.to_string(), false))?;

        let mut issues = Vec::new();
        let mut evidence_refs = Vec::new();
        for row in rows {
            let rec =
                row.map_err(|e| ErrorEnvelope::new("QUERY_RECON_FAILED", e.to_string(), false))?;
            if scope_set.contains(&rec.account_id) {
                evidence_refs.push(EvidenceRef {
                    kind: "reconciliation_issue".to_string(),
                    id: rec.id.clone(),
                    revision: 1,
                    label: format!(
                        "{} {} diff={}",
                        rec.account_id, rec.asset_id, rec.difference
                    ),
                });
                issues.push(rec);
            }
        }

        Ok(ResultEnvelope::build_deterministic(
            "reconcile.list_issues",
            req_id,
            generation,
            issues,
            resolved,
            QualityStatus::Exact,
            Vec::new(),
            evidence_refs,
            Vec::new(),
        ))
    }

    pub fn query_data_quality(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<DataQualitySummaryDto>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        let ctx = ScopedValuationContext::load(self, &resolved, resolved.as_of_date)?;
        let eval = ctx.evaluate_at(resolved.as_of_date)?;

        let all_issues = self
            .store()
            .list_quality_issues()
            .map_err(|e| ErrorEnvelope::new("QUERY_QUALITY_FAILED", e.to_string(), false))?;
        let max_v = if resolved.dataset_version <= 0 {
            i64::MAX
        } else {
            resolved.dataset_version
        };
        let issues: Vec<DataQualityIssue> = all_issues
            .into_iter()
            .filter(|i| i.dataset_version <= max_v)
            .collect();

        let mut cp_stmt = self
            .store()
            .conn()
            .prepare(
                "SELECT provider, resource_kind, resource_key, connection_id, cursor,
                        last_committed_at, calendar_version, base_url
                 FROM sync_checkpoint
                 ORDER BY provider ASC, resource_kind ASC, resource_key ASC",
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_CHECKPOINT_FAILED", e.to_string(), false))?;
        let checkpoints: Vec<SyncCheckpointRecord> = cp_stmt
            .query_map([], |r| {
                Ok(SyncCheckpointRecord {
                    provider: r.get(0)?,
                    resource_kind: r.get(1)?,
                    resource_key: r.get(2)?,
                    connection_id: r.get(3)?,
                    cursor: r.get(4)?,
                    last_committed_at: r.get(5)?,
                    calendar_version: r.get(6)?,
                    base_url: r.get(7)?,
                })
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_CHECKPOINT_FAILED", e.to_string(), false))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorEnvelope::new("QUERY_CHECKPOINT_FAILED", e.to_string(), false))?;

        let quality = eval.valuation.quality;
        let missing_inputs = eval.valuation.missing_inputs.clone();
        let dto = DataQualitySummaryDto {
            dataset_version: resolved.dataset_version,
            quality: quality.as_str().to_string(),
            issues,
            checkpoints,
            missing_inputs: missing_inputs.clone(),
        };

        let evidence_refs = vec![EvidenceRef {
            kind: "dataset_version".to_string(),
            id: format!("DSV-{:06}", resolved.dataset_version),
            revision: resolved.dataset_version,
            label: format!("Dataset Version {}", resolved.dataset_version),
        }];

        Ok(ResultEnvelope::build_deterministic(
            "data.quality_summary",
            req_id,
            generation,
            dto,
            resolved,
            quality,
            missing_inputs,
            evidence_refs,
            eval.valuation.warnings,
        ))
    }

    pub fn query_notes(
        &self,
        scope: &AnalysisScope,
        query: &str,
        only_model_accessible: bool,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<NoteDto>>, ErrorEnvelope> {
        self.query_notes_with_candidate(
            scope,
            query,
            only_model_accessible,
            req_id,
            generation,
            None,
        )
    }

    pub fn get_scoped_note(
        &self,
        scope: &AnalysisScope,
        note_id: &str,
    ) -> Result<NoteDto, ErrorEnvelope> {
        let note = self
            .store()
            .get_note(note_id)
            .map_err(|e| ErrorEnvelope::new("NOTE_NOT_FOUND", e.to_string(), false))?;
        self.query_notes_with_candidate(scope, "", false, "get-scoped-note", 0, Some(note))?
            .value
            .into_iter()
            .next()
            .ok_or_else(|| {
                ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    "笔记不在当前账户、期间或可见截止范围内",
                    false,
                )
            })
    }

    fn query_notes_with_candidate(
        &self,
        scope: &AnalysisScope,
        query: &str,
        only_model_accessible: bool,
        req_id: &str,
        generation: u64,
        candidate: Option<NoteDto>,
    ) -> Result<ResultEnvelope<Vec<NoteDto>>, ErrorEnvelope> {
        let resolved = self.require_real_account_scope(scope)?;
        if only_model_accessible && !resolved.include_notes {
            return Ok(ResultEnvelope::build_deterministic(
                "journal.search_notes",
                req_id,
                generation,
                Vec::new(),
                resolved,
                QualityStatus::Exact,
                Vec::new(),
                Vec::new(),
                vec!["notes excluded by scope.include_notes=false".to_string()],
            ));
        }

        let mut truncated = false;
        let raw_notes: Box<dyn Iterator<Item = Result<NoteDto, ErrorEnvelope>> + '_> =
            if let Some(note) = candidate {
                Box::new(std::iter::once(Ok(note)))
            } else {
                let mut ids = self
                    .store()
                    .search_note_candidate_ids(query, only_model_accessible, 10_001)
                    .map_err(|e| ErrorEnvelope::new("SEARCH_NOTES_FAILED", e.to_string(), false))?;
                truncated = ids.len() > 10_000;
                ids.truncate(10_000);
                Box::new(ids.into_iter().map(|id| {
                    self.store().get_note(&id).map_err(|e| {
                        ErrorEnvelope::new("SEARCH_NOTES_FAILED", e.to_string(), false)
                    })
                }))
            };

        let scope_set: BTreeSet<String> = resolved.resolved_accounts.iter().cloned().collect();
        let mut authorized_notes = Vec::new();
        let mut evidence_refs = Vec::new();

        for note in raw_notes {
            let note = note?;
            if only_model_accessible && !note.allow_model_access {
                continue;
            }
            let created = DateTime::parse_from_rfc3339(&note.saved_at)
                .map_err(|_| ErrorEnvelope::new("CORRUPT_NOTE", "invalid note timestamp", false))?;
            if let Some(cut) = resolved.visible_until {
                if created.with_timezone(&Utc) > cut {
                    continue;
                }
            }
            // Verify that any linked economic_event (whether target_type is "event" or
            // "economic_event") belongs to an account within `resolved.resolved_accounts` (`APP-B01`).
            let mut has_unauthorized_link = false;
            let mut has_scoped_event = false;
            let mut has_period_anchor = false;
            for link in &note.links {
                if link.target_type == "event" || link.target_type == "economic_event" {
                    has_period_anchor = true;
                    let event_target: Option<(String, String)> = self
                        .store()
                        .conn()
                        .query_row(
                            "SELECT account_id, occurred_at FROM economic_event WHERE (id = ?1 OR correction_group_id = ?1) AND active_in_correction_group = 1 LIMIT 1",
                            params![link.target_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()
                        .map_err(|e| ErrorEnvelope::new("QUERY_NOTE_SCOPE_FAILED", e.to_string(), false))?;
                    let Some((acct, occurred)) = event_target else {
                        has_unauthorized_link = true;
                        break;
                    };
                    let when = DateTime::parse_from_rfc3339(&occurred)
                        .map_err(|_| {
                            ErrorEnvelope::new(
                                "CORRUPT_LEDGER",
                                "invalid linked event timestamp",
                                false,
                            )
                        })?
                        .with_timezone(&Utc);
                    if !scope_set.contains(&acct)
                        || when.date_naive() > resolved.as_of_date
                        || resolved
                            .start_date
                            .is_some_and(|start| when.date_naive() < start)
                        || resolved.visible_until.is_some_and(|cut| when > cut)
                    {
                        has_unauthorized_link = true;
                        break;
                    }
                    has_scoped_event = true;
                } else if link.target_type == "account" {
                    if !scope_set.contains(&link.target_id) {
                        has_unauthorized_link = true;
                        break;
                    }
                    has_scoped_event = true;
                } else if link.target_type == "review_date" {
                    has_period_anchor = true;
                    let date =
                        NaiveDate::parse_from_str(&link.target_id, "%Y-%m-%d").map_err(|_| {
                            ErrorEnvelope::new("CORRUPT_NOTE", "invalid note review date", false)
                        })?;
                    if date > resolved.as_of_date
                        || resolved.start_date.is_some_and(|start| date < start)
                    {
                        has_unauthorized_link = true;
                        break;
                    }
                } else if link.target_type == "plan"
                    && !matches!(resolved.target, ScopeTarget::AllAccounts)
                {
                    // Plans have no account owner. A scoped note must also link an authorized fill.
                    continue;
                } else if link.target_type == "chart_context" {
                    has_period_anchor = true;
                    let ctx = match self.store().get_chart_context(&link.target_id) {
                        Ok(ctx) => ctx,
                        Err(delta_data::StorageError::Sqlite(
                            rusqlite::Error::QueryReturnedNoRows,
                        )) => {
                            has_unauthorized_link = true;
                            break;
                        }
                        Err(e) => {
                            return Err(ErrorEnvelope::new(
                                "QUERY_NOTE_SCOPE_FAILED",
                                e.to_string(),
                                false,
                            ));
                        }
                    };
                    let anchor = ctx.anchor_date.unwrap_or(ctx.visible_end_date);
                    if anchor > resolved.as_of_date
                        || resolved.start_date.is_some_and(|start| anchor < start)
                        || resolved
                            .visible_until
                            .is_some_and(|cut| anchor > cut.date_naive())
                    {
                        has_unauthorized_link = true;
                        break;
                    }
                }
            }
            if !has_period_anchor
                && (created.with_timezone(&Utc).date_naive() > resolved.as_of_date
                    || resolved
                        .start_date
                        .is_some_and(|start| created.with_timezone(&Utc).date_naive() < start))
            {
                continue;
            }
            if has_unauthorized_link
                || (!matches!(resolved.target, ScopeTarget::AllAccounts) && !has_scoped_event)
            {
                continue;
            }

            if authorized_notes.len() == 100 {
                truncated = true;
                break;
            }

            evidence_refs.push(EvidenceRef {
                kind: "note".to_string(),
                id: note.id.clone(),
                revision: note.current_revision,
                label: note.title.clone(),
            });
            authorized_notes.push(note);
        }

        Ok(ResultEnvelope::build_deterministic(
            "journal.search_notes",
            req_id,
            generation,
            authorized_notes,
            resolved,
            if truncated {
                QualityStatus::Partial
            } else {
                QualityStatus::Exact
            },
            Vec::new(),
            evidence_refs,
            if truncated {
                vec!["笔记结果有界显示前 100 篇，候选扫描上限为 10000；请缩小搜索范围".into()]
            } else {
                Vec::new()
            },
        ))
    }

    /// Inspect a saved report by `report_id` and check whether a newer `ledger_revision` or
    /// explicit `'stale'` status has invalidated it (`M2-A-05`).
    pub fn get_saved_report_status(
        &self,
        report_id: &str,
    ) -> Result<SavedReportStatusDto, ErrorEnvelope> {
        let current_ledger_revision: i64 = self
            .store()
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_LIBRARY_FAILED", e.to_string(), false))?;

        let (report_type, saved_rev, status, payload_json): (String, i64, String, String) = self
            .store()
            .conn()
            .query_row(
                "SELECT report_type, ledger_revision, status, payload_json
                 FROM analysis_report WHERE id = ?1",
                params![report_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| ErrorEnvelope::new("REPORT_NOT_FOUND", e.to_string(), false))?;

        let is_stale = status == "stale" || saved_rev < current_ledger_revision;
        Ok(SavedReportStatusDto {
            report_id: report_id.to_string(),
            report_type,
            saved_ledger_revision: saved_rev,
            current_ledger_revision,
            is_stale,
            recompute_available: is_stale,
            payload_json,
        })
    }

    /// Load a windowed K-line slice (`KlineWindowSliceDto`) for `instrument_id` at `dataset_version`
    /// with trade fill and note markers (`M2-W05`, `M2-A-10`, `M2-A-11`).
    #[allow(clippy::too_many_arguments)]
    pub fn load_kline_window_slice(
        &self,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
        viewport: &KlineViewport,
        max_pixel_columns: usize,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        let as_of = visible_until
            .map(|cut| cut.date_naive())
            .unwrap_or_else(|| Utc::now().date_naive());
        let mut scope = AnalysisScope::all_accounts(as_of, "USD");
        scope.dataset_version = dataset_version;
        scope.visible_until = visible_until;
        self.load_scoped_kline_window_slice(
            &scope,
            instrument_id,
            timeframe,
            adjustment,
            viewport,
            max_pixel_columns,
        )
    }

    pub fn load_scoped_kline_window_slice(
        &self,
        scope: &AnalysisScope,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        viewport: &KlineViewport,
        max_pixel_columns: usize,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        if timeframe != "1d" {
            return Err(ErrorEnvelope::new(
                "UNSUPPORTED_CAPABILITY",
                "only daily bars are available",
                false,
            ));
        }
        let resolved = self.require_real_account_scope(scope)?;
        let scope_accounts: BTreeSet<_> = resolved.resolved_accounts.iter().cloned().collect();
        let day_end = resolved
            .as_of_date
            .succ_opt()
            .unwrap_or(resolved.as_of_date)
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let visible_until = Some(
            resolved
                .visible_until
                .map_or(day_end, |cut| cut.min(day_end)),
        );
        let reg = self
            .store()
            .load_instrument_registry()
            .map_err(|e| ErrorEnvelope::new("REGISTRY_FAILED", e.to_string(), false))?;
        let venue = reg
            .instruments
            .get(instrument_id)
            .map(|i| i.venue.clone())
            .unwrap_or_else(|| {
                instrument_id
                    .split(':')
                    .next()
                    .unwrap_or("NASDAQ")
                    .to_string()
            });

        let query_v = resolved.dataset_version;
        let mut raw_bars = self
            .store()
            .query_bars_at_version(
                instrument_id,
                timeframe,
                AdjustmentKind::Raw,
                None,
                query_v,
                visible_until,
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_BARS_FAILED", e.to_string(), false))?;
        // Deduplicate multi-source sessions deterministically while preserving any unclosed tail
        // record (`is_final = false`) so candles and indicators are never double-counted (`APP-B04`).
        raw_bars.sort_by(|a, b| {
            a.session_date
                .cmp(&b.session_date)
                .then_with(|| b.is_final.cmp(&a.is_final))
                .then_with(|| a.source.cmp(&b.source))
        });
        raw_bars.dedup_by_key(|b| (b.session_date, b.is_final));

        let mut actions = self
            .store()
            .query_corporate_actions(instrument_id, Some(query_v))
            .map_err(|e| ErrorEnvelope::new("QUERY_ACTIONS_FAILED", e.to_string(), false))?;
        actions.retain(|action| {
            action.ex_date <= resolved.as_of_date
                && action
                    .announced_at
                    .is_none_or(|when| visible_until.is_none_or(|cut| when <= cut))
        });

        let adjusted_bars = if adjustment == AdjustmentKind::Raw {
            raw_bars
        } else {
            delta_core::derive_adjusted_bars(&raw_bars, &actions, adjustment)
                .map_err(|e| ErrorEnvelope::new("ADJUSTMENT_FAILED", e.to_string(), false))?
        };

        let final_bars = delta_core::filter_visible_final_bars(&adjusted_bars, visible_until);
        let date_to_idx: HashMap<NaiveDate, (usize, Decimal)> = final_bars
            .iter()
            .enumerate()
            .map(|(i, b)| (b.session_date, (i, b.close)))
            .collect();

        let mut markers = Vec::new();
        {
            let events = load_all_ledger_events(self.store())
                .map_err(|e| ErrorEnvelope::new("CORRUPT_LEDGER", e.to_string(), false))?;
            for ev in events {
                if !ev.active_in_correction_group
                    || !scope_accounts.contains(&ev.account_id)
                    || resolved
                        .start_date
                        .is_some_and(|start| ev.occurred_at.date_naive() < start)
                    || visible_until.is_some_and(|cut| ev.occurred_at > cut)
                {
                    continue;
                }
                let matches_inst = ev
                    .instrument_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(instrument_id))
                    || ev.asset_id.eq_ignore_ascii_case(instrument_id);
                if !matches_inst {
                    continue;
                }
                let kind = match ev.event_type {
                    EconomicEventType::Buy => MarkerKind::BuyFill,
                    EconomicEventType::Sell => MarkerKind::SellFill,
                    _ => continue,
                };
                let d = ev.occurred_at.date_naive();
                if let Some(&(bar_index, fallback_close)) = date_to_idx.get(&d) {
                    let price = ev.price.unwrap_or(fallback_close);
                    markers.push(ChartMarkerDto {
                        id: format!("MK:EV:{}", ev.event_id),
                        kind,
                        session_date: d,
                        bar_index,
                        price,
                        label: format!(
                            "{} {}",
                            ev.event_type.as_str(),
                            to_canonical_text(ev.quantity)
                        ),
                        target_id: ev.event_id,
                        target_revision: ev.revision,
                    });
                }
            }
        }

        {
            let notes = self
                .query_notes(&resolved, "", false, "chart-markers", 0)?
                .value;
            for note in notes {
                for link in &note.links {
                    if link.target_type == "chart_context" {
                        let ctx = match self.store().get_chart_context(&link.target_id) {
                            Ok(ctx) => ctx,
                            Err(delta_data::StorageError::Sqlite(
                                rusqlite::Error::QueryReturnedNoRows,
                            )) => continue,
                            Err(e) => {
                                return Err(ErrorEnvelope::new(
                                    "CHART_CONTEXT_FAILED",
                                    e.to_string(),
                                    false,
                                ));
                            }
                        };
                        if ctx.instrument_id.eq_ignore_ascii_case(instrument_id) {
                            let d = ctx.anchor_date.unwrap_or(ctx.visible_end_date);
                            if let Some(&(bar_index, close_price)) = date_to_idx.get(&d) {
                                markers.push(ChartMarkerDto {
                                    id: format!("MK:NOTE:{}", note.id),
                                    kind: MarkerKind::Note,
                                    session_date: d,
                                    bar_index,
                                    price: close_price,
                                    label: note.title.clone(),
                                    target_id: note.id.clone(),
                                    target_revision: note.current_revision,
                                });
                            }
                        }
                    }
                }
            }
        }

        let mut viewport = viewport.clone();
        if viewport.total_bars == 0 || viewport.requested_anchor_date.is_some() {
            let count = final_bars.len();
            let config = viewport.indicator_config.clone();
            let window_len = viewport
                .requested_window
                .map(|(start, end)| {
                    final_bars
                        .iter()
                        .filter(|b| b.session_date >= start && b.session_date <= end)
                        .count()
                })
                .unwrap_or_else(|| viewport.visible_len().max(100))
                .max(1);
            let anchor = viewport.requested_anchor_date;
            viewport = KlineViewport::new(count, window_len);
            viewport.indicator_config = config;
            if let Some(date) = anchor {
                let bars: Vec<_> = final_bars.iter().map(|bar| (*bar).clone()).collect();
                viewport.jump_to_date(&bars, date);
            }
        }
        Ok(viewport.build_window_slice(
            instrument_id,
            &venue,
            timeframe,
            adjustment,
            &adjusted_bars,
            visible_until,
            &markers,
            &[],
            max_pixel_columns,
        ))
    }
}
