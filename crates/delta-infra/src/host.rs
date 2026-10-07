//! Production analysis host. UI pages and AI tools call the same library
//! queries; the host repeats the frozen-scope check before reading.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use delta_app::ai::gateway::AnalysisHost;
use delta_app::ai::session::SessionStore;
use delta_app::contracts::{AppError, CallContext, ResultEnvelope};
use delta_core::ids::AccountId;
use delta_core::money::CurrencyCode;
use delta_core::scope::{ScopeFault, ScopeSnapshot};
use delta_core::valuation::{PriceSource, Quality};
use delta_core::EventPayload;
use rusqlite::params;
use serde_json::{json, Value};

use crate::error::InfraError;
use crate::sqlite::store::{canonical_ts, Library};
use sha2::{Digest, Sha256};

pub struct ProductionHost {
    library: Arc<Library>,
}

impl ProductionHost {
    pub fn new(library: Arc<Library>) -> Self {
        Self { library }
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    fn frozen(&self, ctx: &CallContext) -> Result<ScopeSnapshot, AppError> {
        let run_id = ctx.run_id.as_deref().ok_or(AppError::ScopeDenied)?;
        let run = self
            .library
            .get_run(run_id)
            .map_err(|e| AppError::Storage(e.to_string()))?
            .ok_or(AppError::ScopeDenied)?;
        if run.scope_snapshot.scope_ref != ctx.scope_ref {
            return Err(AppError::ScopeDenied);
        }
        Ok(run.scope_snapshot)
    }

    fn active_scope(frozen: &ScopeSnapshot, args: &Value) -> Result<ScopeSnapshot, AppError> {
        let Some(scope) = args.get("scope") else {
            return Ok(frozen.clone());
        };
        let ids = scope
            .get("account_ids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| AppError::InvalidArgument("scope.account_ids is required".into()))?;
        let mut account_ids = Vec::new();
        for id in ids {
            let raw = id
                .as_str()
                .ok_or_else(|| AppError::InvalidArgument("account id must be a string".into()))?;
            account_ids.push(AccountId::new(raw));
        }
        let mut next = frozen.clone();
        next.account_ids = account_ids;
        next.start_at = parse_ts(scope, "start_at")?;
        next.end_at = parse_ts(scope, "end_at")?;
        next.reporting_currency = CurrencyCode::new(
            scope
                .get("reporting_currency")
                .and_then(|v| v.as_str())
                .unwrap_or(frozen.reporting_currency.as_str()),
        );
        Ok(next)
    }
}

impl AnalysisHost for ProductionHost {
    fn execute(
        &self,
        tool: &str,
        ctx: &CallContext,
        args: &Value,
    ) -> Result<ResultEnvelope<Value>, AppError> {
        let frozen = self.frozen(ctx)?;
        match frozen.check_tool_args(tool, args) {
            Ok(()) => {}
            Err(ScopeFault::Denied(_)) => return Err(AppError::ScopeDenied),
            Err(ScopeFault::Invalid(msg)) => return Err(AppError::InvalidArgument(msg)),
        }
        let scope = Self::active_scope(&frozen, args)?;
        let mut warnings = Vec::new();
        if let Ok(rev) = self.library.ledger_revision() {
            if rev != frozen.ledger_revision {
                warnings.push(
                    "ledger revision changed since this scope was frozen; the report may be stale"
                        .to_string(),
                );
            }
        }
        let (value, quality, missing, evidence) = match tool {
            "get_data_quality" => self.data_quality(&scope, &ctx.scope_ref)?,
            "get_portfolio_summary" => {
                let as_of = parse_ts(args, "as_of")?;
                self.portfolio(&scope, as_of, &ctx.scope_ref)?
            }
            "explain_pnl" => self.pnl(&scope, &ctx.scope_ref)?,
            "query_trades" => self.trades(&scope, args, &ctx.scope_ref)?,
            "search_journal" => self.journal(&frozen, args, &ctx.scope_ref)?,
            "get_chart_window" => self.chart(args, &ctx.scope_ref)?,
            other => {
                return Err(AppError::UnsupportedCapability(format!(
                    "tool {other:?} is not registered"
                )))
            }
        };
        let mut envelope = ResultEnvelope::new(&ctx.request_id, value).with_quality(&quality);
        envelope.missing_inputs = missing;
        envelope.evidence_refs = evidence;
        envelope.warnings = warnings;
        envelope.scope_snapshot = serde_json::to_value(&scope).unwrap_or(Value::Null);
        Ok(envelope)
    }
}

impl ProductionHost {
    fn remember(
        &self,
        scope_ref: &str,
        tool: &str,
        discriminator: &str,
        value: &Value,
        quality: &str,
    ) -> Result<String, AppError> {
        let id = tool_evidence_id(tool, scope_ref, discriminator, value, quality);
        self.library
            .store_analysis_result(&id, scope_ref, tool, &value.to_string(), quality)
            .map_err(storage)?;
        Ok(id)
    }

    fn data_quality(
        &self,
        scope: &ScopeSnapshot,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let engine = self.library.rebuild_engine(None).map_err(storage)?;
        let mut missing = Vec::new();
        let prices = self.library.price_source();
        for (account, ledger) in &engine.accounts {
            if !scope.covers_account(account) {
                continue;
            }
            for item in &ledger.pending {
                missing.push(format!("pending {} on {}", item.reason, account.0));
            }
            for (asset, holding) in &ledger.holdings {
                if holding.quantity() == rust_decimal::Decimal::ZERO {
                    continue;
                }
                if prices.price(asset, scope.end_at).is_none() {
                    missing.push(format!("price for {asset}"));
                }
            }
        }
        let diffs = self.library.reconcile(scope).map_err(storage)?;
        for diff in &diffs {
            missing.push(format!(
                "balance diff {} {} {}",
                diff.account_id.0, diff.asset.0, diff.difference
            ));
        }
        let quality = if missing.is_empty() {
            "complete"
        } else {
            "partial"
        };
        let value = json!({
            "pending_or_missing": missing.len().to_string(),
            "issues": missing.clone(),
        });
        let quality = quality.to_string();
        let id = self.remember(scope_ref, "get_data_quality", "quality", &value, &quality)?;
        Ok((value, quality, missing, vec![id]))
    }

    fn portfolio(
        &self,
        scope: &ScopeSnapshot,
        as_of: DateTime<Utc>,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let summary = self
            .library
            .portfolio_value(scope, as_of)
            .map_err(storage)?;
        // Same cut-off as the valuation: no fill after `as_of` appears.
        let lines = self
            .library
            .account_lines_at(Some(as_of))
            .map_err(storage)?;
        let rows: Vec<Value> = lines
            .into_iter()
            .filter(|line| scope.covers_account(&AccountId::new(&line.account)))
            .map(|line| {
                json!({
                    "account": line.account,
                    "asset": line.asset,
                    "quantity": line.quantity,
                    "cost": line.cost,
                })
            })
            .collect();
        let quality = quality_name(summary.quality);
        let value = json!({
            "known_market_value": summary.known_market_value.normalize().to_string(),
            "lines": rows,
        });
        let quality = quality.to_string();
        let id = self.remember(
            scope_ref,
            "get_portfolio_summary",
            &canonical_ts(as_of),
            &value,
            &quality,
        )?;
        Ok((value, quality, summary.missing_inputs, vec![id]))
    }

    fn pnl(
        &self,
        scope: &ScopeSnapshot,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let report = self.library.explain(scope).map_err(storage)?;
        // Cite the latest effective events only: the tool output must not
        // grow with the ledger, and a superseded original is not evidence.
        let ids: Vec<String> = scope.account_ids.iter().map(|a| a.0.clone()).collect();
        let recorded = self.library.recorded_events(Some(&ids)).map_err(storage)?;
        let mut in_period: Vec<_> = delta_core::pnl::effective(&recorded)
            .into_iter()
            .filter(|e| e.occurred_at >= scope.start_at && e.occurred_at < scope.end_at)
            .collect();
        in_period.sort_by_key(|e| std::cmp::Reverse((e.occurred_at, e.seq)));
        let cited: Vec<String> = in_period
            .iter()
            .take(MAX_EVENT_EVIDENCE)
            .map(|e| format!("event:{}", e.id.0))
            .collect();
        let value = json!({
            "total_pnl": report.total_pnl.map(|d| d.normalize().to_string()),
            "realized_pnl": report.realized_pnl.map(|d| d.normalize().to_string()),
            "unrealized_pnl": report.unrealized_pnl.map(|d| d.normalize().to_string()),
            "fees": report.fees_valued.map(|d| d.normalize().to_string()),
            "income": report.income.map(|d| d.normalize().to_string()),
            "events_in_period": in_period.len().to_string(),
            "events_cited": cited.len().to_string(),
        });
        let quality = quality_name(report.quality).to_string();
        let disc = format!(
            "{}|{}",
            canonical_ts(scope.start_at),
            canonical_ts(scope.end_at)
        );
        let id = self.remember(scope_ref, "explain_pnl", &disc, &value, &quality)?;
        let mut evidence = vec![id];
        evidence.extend(cited);
        Ok((value, quality, report.missing_inputs, evidence))
    }

    fn trades(
        &self,
        scope: &ScopeSnapshot,
        args: &Value,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let instrument = args.get("instrument").and_then(|v| v.as_str());
        let ids: Vec<String> = scope.account_ids.iter().map(|a| a.0.clone()).collect();
        let events = self.library.recorded_events(Some(&ids)).map_err(storage)?;
        let mut rows = Vec::new();
        let mut evidence = Vec::new();
        for event in events {
            if event.occurred_at < scope.start_at || event.occurred_at >= scope.end_at {
                continue;
            }
            let (name, qty, price) = match &event.payload {
                EventPayload::Buy {
                    instrument,
                    quantity,
                    price,
                    ..
                }
                | EventPayload::Sell {
                    instrument,
                    quantity,
                    price,
                    ..
                } => (
                    instrument.0.clone(),
                    quantity.normalize().to_string(),
                    price.normalize().to_string(),
                ),
                _ => continue,
            };
            if instrument.is_some_and(|want| want != name) {
                continue;
            }
            evidence.push(format!("event:{}", event.id.0));
            rows.push(json!({
                "event_id": event.id.0,
                "account": event.account_id.0,
                "instrument": name,
                "quantity": qty,
                "price": price,
            }));
            if rows.len() >= limit {
                break;
            }
        }
        let value = json!({ "trades": rows });
        let disc = format!("{limit}:{}", instrument.unwrap_or(""));
        let id = self.remember(scope_ref, "query_trades", &disc, &value, "complete")?;
        evidence.push(id);
        Ok((value, "complete".into(), Vec::new(), evidence))
    }

    fn journal(
        &self,
        frozen: &ScopeSnapshot,
        args: &Value,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let ids: Vec<String> = frozen.account_ids.iter().map(|a| a.0.clone()).collect();
        let hits = self
            .library
            .search_journal(query, limit, Some(&ids))
            .map_err(storage)?;
        let rows: Vec<Value> = hits
            .iter()
            .map(|(id, title, body)| json!({"id": id, "title": title, "body": body}))
            .collect();
        let mut evidence = Vec::new();
        for (id, _, _) in &hits {
            evidence.push(self.library.journal_evidence_id(id).map_err(storage)?);
        }
        let value = json!({ "hits": rows });
        let id = self.remember(scope_ref, "search_journal", query, &value, "complete")?;
        evidence.push(id);
        Ok((value, "complete".into(), Vec::new(), evidence))
    }

    fn chart(
        &self,
        args: &Value,
        scope_ref: &str,
    ) -> Result<(Value, String, Vec<String>, Vec<String>), AppError> {
        let instrument = args
            .get("instrument")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::InvalidArgument("instrument is required".into()))?;
        let start = parse_ts(args, "start_at")?;
        let end = parse_ts(args, "end_at")?;
        let start_s = crate::sqlite::store::canonical_ts(start);
        let end_s = crate::sqlite::store::canonical_ts(end);
        let bars = self
            .library
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT session_date, open, high, low, close, volume, source, adjustment
                 FROM bar WHERE instrument_id = ?1 AND adjustment = 'raw'
                 AND open_at >= ?2 AND close_at <= ?3 ORDER BY close_at",
                )?;
                let rows = stmt.query_map(params![instrument, start_s, end_s], |r| {
                    Ok(json!({
                        "session_date": r.get::<_, String>(0)?,
                        "open": r.get::<_, String>(1)?,
                        "high": r.get::<_, String>(2)?,
                        "low": r.get::<_, String>(3)?,
                        "close": r.get::<_, String>(4)?,
                        "volume": r.get::<_, String>(5)?,
                        "source": r.get::<_, String>(6)?,
                        "adjustment": r.get::<_, String>(7)?,
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(InfraError::Sqlite)
            })
            .map_err(storage)?;
        let quality = if bars.is_empty() {
            "unavailable"
        } else {
            "complete"
        };
        let missing = if bars.is_empty() {
            vec![format!("no bars for {instrument} in the requested window")]
        } else {
            Vec::new()
        };
        let value = json!({ "bars": bars });
        let quality = quality.to_string();
        let disc = format!("{instrument}|{start_s}|{end_s}");
        let id = self.remember(scope_ref, "get_chart_window", &disc, &value, &quality)?;
        Ok((value, quality, missing, vec![id]))
    }
}

/// Events cited by one explain_pnl result; the result itself is the receipt.
const MAX_EVENT_EVIDENCE: usize = 20;

/// Content-addressed id for one tool result. The same tool, scope,
/// discriminator, value and quality give the same id; a different result
/// never reuses one, so a report's evidence keeps opening the value the
/// report was checked against.
pub fn tool_evidence_id(
    tool: &str,
    scope_ref: &str,
    discriminator: &str,
    value: &Value,
    quality: &str,
) -> String {
    let raw = format!("{tool}|{scope_ref}|{discriminator}|{quality}|{value}");
    let digest = Sha256::digest(raw.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("res:{tool}:{hex}")
}

fn quality_name(q: Quality) -> &'static str {
    match q {
        Quality::Complete => "complete",
        Quality::Partial => "partial",
        Quality::Unavailable => "unavailable",
    }
}

fn parse_ts(value: &Value, field: &str) -> Result<DateTime<Utc>, AppError> {
    let raw = value
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::InvalidArgument(format!("{field} is required")))?;
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| AppError::InvalidArgument(format!("{field} is not RFC3339")))
}

fn storage(err: InfraError) -> AppError {
    match err {
        InfraError::Rejected(msg) | InfraError::InvalidPath(msg) => AppError::InvalidArgument(msg),
        InfraError::NotFound(msg) => AppError::MissingInput(msg),
        other => AppError::Storage(other.to_string()),
    }
}
