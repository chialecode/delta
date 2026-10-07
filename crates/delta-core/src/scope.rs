//! Analysis scope: explicit account set, period, currency and versions.

use crate::ids::AccountId;
use crate::money::CurrencyCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a scope is a portfolio (paired transfers are internal) or a single
/// account (transfers are external flows) — financial-engine §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeView {
    Portfolio,
    SingleAccount,
}

/// Frozen analysis scope. Every report/result references one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisScope {
    pub account_ids: Vec<AccountId>,
    /// `[start, end)` interval.
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub reporting_currency: CurrencyCode,
    pub view: ScopeView,
    /// Ledger revision this scope was frozen against.
    pub ledger_revision: i64,
    pub market_dataset_version: Option<String>,
    pub calculation_version: String,
}

impl AnalysisScope {
    /// Current calculation version of the R1 financial engine.
    pub const CALCULATION_VERSION: &'static str = "ledger-fifo-v1";

    pub fn covers(&self, account: &AccountId) -> bool {
        self.account_ids.iter().any(|a| a == account)
    }
}

/// Why a model-supplied tool argument was rejected against a frozen snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeFault {
    /// Parsed, but outside the frozen accounts, period or currency.
    Denied(String),
    /// Present but not a usable timestamp or id list.
    Invalid(String),
}

/// Host-issued frozen scope for one run. Model-supplied `scope` objects,
/// periods and reporting currencies must fall inside this snapshot; the
/// snapshot is not rebuilt from model text (ai-design / FR-AI-03/07).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeSnapshot {
    pub scope_ref: String,
    pub account_ids: Vec<AccountId>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub reporting_currency: CurrencyCode,
    pub view: ScopeView,
    pub ledger_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_dataset_version: Option<String>,
    pub calculation_version: String,
}

impl ScopeSnapshot {
    pub fn freeze(
        scope_ref: impl Into<String>,
        account_ids: Vec<AccountId>,
        start_at: DateTime<Utc>,
        end_at: DateTime<Utc>,
        reporting_currency: CurrencyCode,
        view: ScopeView,
        ledger_revision: i64,
    ) -> Self {
        Self {
            scope_ref: scope_ref.into(),
            account_ids,
            start_at,
            end_at,
            reporting_currency,
            view,
            ledger_revision,
            market_dataset_version: None,
            calculation_version: AnalysisScope::CALCULATION_VERSION.into(),
        }
    }

    /// Same accounts, period, currency and view. Ledger revision and the
    /// scope reference are not part of the economic scope: a revision bump
    /// keeps context and surfaces a stale-report warning instead.
    pub fn same_economic_scope(&self, other: &Self) -> bool {
        let mut a = self.account_ids.clone();
        let mut b = other.account_ids.clone();
        a.sort();
        b.sort();
        a == b
            && self.start_at == other.start_at
            && self.end_at == other.end_at
            && self.reporting_currency == other.reporting_currency
            && self.view == other.view
    }

    pub fn covers_account(&self, id: &AccountId) -> bool {
        self.account_ids.iter().any(|a| a == id)
    }

    /// `[start, end]` of a model request must sit inside `[start_at, end_at]`.
    pub fn contains_period(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        start >= self.start_at && end <= self.end_at && start <= end
    }

    /// Check model-supplied scope, `as_of`, period and chart window fields.
    pub fn check_tool_args(&self, tool: &str, args: &serde_json::Value) -> Result<(), ScopeFault> {
        if let Some(scope) = args.get("scope") {
            self.check_scope_object(scope)?;
        }
        if let Some(as_of) = args.get("as_of").and_then(|v| v.as_str()) {
            let t = parse_ts("as_of", as_of)?;
            if t < self.start_at || t > self.end_at {
                return Err(ScopeFault::Denied(
                    "as_of is outside the frozen period".into(),
                ));
            }
        }
        if args.get("period_start").is_some() || args.get("period_end").is_some() {
            let start = required_ts(args, "period_start")?;
            let end = required_ts(args, "period_end")?;
            if !self.contains_period(start, end) {
                return Err(ScopeFault::Denied(
                    "requested period is outside the frozen period".into(),
                ));
            }
        }
        if tool == "get_chart_window" {
            let start = required_ts(args, "start_at")?;
            let end = required_ts(args, "end_at")?;
            if !self.contains_period(start, end) {
                return Err(ScopeFault::Denied(
                    "chart window is outside the frozen period".into(),
                ));
            }
        }
        Ok(())
    }

    fn check_scope_object(&self, scope: &serde_json::Value) -> Result<(), ScopeFault> {
        let ids = scope
            .get("account_ids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ScopeFault::Invalid("scope.account_ids is required".into()))?;
        for id in ids {
            let raw = id
                .as_str()
                .ok_or_else(|| ScopeFault::Invalid("account id must be a string".into()))?;
            let account = AccountId::new(raw);
            if !self.covers_account(&account) {
                return Err(ScopeFault::Denied(format!(
                    "account {raw} is outside the frozen scope"
                )));
            }
        }
        let start = required_ts(scope, "start_at")?;
        let end = required_ts(scope, "end_at")?;
        if !self.contains_period(start, end) {
            return Err(ScopeFault::Denied(
                "scope period is outside the frozen period".into(),
            ));
        }
        let ccy = scope
            .get("reporting_currency")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ScopeFault::Invalid("reporting_currency is required".into()))?;
        if CurrencyCode::new(ccy) != self.reporting_currency {
            return Err(ScopeFault::Denied(format!(
                "reporting currency {ccy} is outside the frozen scope"
            )));
        }
        Ok(())
    }
}

fn parse_ts(field: &str, raw: &str) -> Result<DateTime<Utc>, ScopeFault> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| ScopeFault::Invalid(format!("{field} is not RFC3339")))
}

fn required_ts(obj: &serde_json::Value, field: &str) -> Result<DateTime<Utc>, ScopeFault> {
    let raw = obj
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ScopeFault::Invalid(format!("{field} is required")))?;
    parse_ts(field, raw)
}
