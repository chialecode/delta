//! Application host thread (`AppHost`), scoped query contract (`AnalysisScope`, `ResultEnvelope`,
//! `EvidenceRef`), capability catalog, and deterministic read/write dispatch (`M2-W01`, `S-13`, `S-14`, `M2-A-01`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::{
    evaluate_period_breakdown, evaluate_portfolio, parse_decimal_lossless,
    replay_ledger_events_with_bindings, resolve_fx_rate_s07, to_canonical_text, AdjustmentKind,
    BarRecord, CostLot, DataQualityIssue, EconomicEventType, HoldingProjection, LedgerEventInput,
    LedgerProjectionResult, ManualValuation, PeriodBreakdownReport, QualityStatus, ValuationReport,
};
use delta_data::{
    load_all_ledger_events, load_book_fx_lookup_at_version, load_event_book_fx,
    query_calendar_session, AccountBalanceSnapshotDto, AccountRecord, AttachmentDto,
    ChartContextDto, CommitImportReceipt, ImportPreview, InstrumentSearchHit, LibraryManifest,
    NoteDto, PortfolioRecord, ReconciliationIssueRecord, StatementPreset, StructuredNoteFields,
    SyncCheckpointRecord, TradePlanDto, WatchlistGroupDto,
};
use delta_model::ToolDefinition;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::kline::{ChartMarkerDto, KlineViewport, KlineWindowSliceDto, MarkerKind};
use crate::paths::ResolvedAppPaths;
use crate::process::apply_process_quiet_guards;
use crate::service::{
    AppService, ErrorEnvelope, ExportManifestDto, JobController, JobId, JobSnapshot, SyncRequest,
    SyncSummaryDto,
};

pub const M2_CALCULATION_VERSION: &str = "m2-ledger-1";

// ============================================================================
// 1. Scoped Query Contract Types (`S-13`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ScopeTarget {
    AllAccounts,
    Accounts(Vec<String>),
    Portfolio(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisScope {
    pub target: ScopeTarget,
    pub resolved_accounts: Vec<String>,
    pub start_date: Option<NaiveDate>,
    pub as_of_date: NaiveDate,
    pub report_currency: String,
    pub dataset_version: i64,
    pub ledger_revision: i64,
    pub visible_until: Option<DateTime<Utc>>,
    pub include_notes: bool,
    /// Distinguishes a frozen empty version/member set from the UI's "latest" sentinel.
    #[serde(default)]
    pub versions_frozen: bool,
    #[serde(default)]
    pub journal_revision: String,
}

impl AnalysisScope {
    pub fn all_accounts(as_of_date: NaiveDate, report_currency: impl Into<String>) -> Self {
        Self {
            target: ScopeTarget::AllAccounts,
            resolved_accounts: Vec::new(),
            start_date: None,
            as_of_date,
            report_currency: report_currency.into().to_ascii_uppercase(),
            dataset_version: 0,
            ledger_revision: 0,
            visible_until: None,
            include_notes: true,
            versions_frozen: false,
            journal_revision: String::new(),
        }
    }

    pub fn for_accounts(
        accounts: Vec<String>,
        as_of_date: NaiveDate,
        report_currency: impl Into<String>,
    ) -> Self {
        Self {
            target: ScopeTarget::Accounts(accounts.clone()),
            resolved_accounts: accounts,
            start_date: None,
            as_of_date,
            report_currency: report_currency.into().to_ascii_uppercase(),
            dataset_version: 0,
            ledger_revision: 0,
            visible_until: None,
            include_notes: true,
            versions_frozen: false,
            journal_revision: String::new(),
        }
    }

    pub fn for_portfolio(
        portfolio_id: impl Into<String>,
        as_of_date: NaiveDate,
        report_currency: impl Into<String>,
    ) -> Self {
        Self {
            target: ScopeTarget::Portfolio(portfolio_id.into()),
            resolved_accounts: Vec::new(),
            start_date: None,
            as_of_date,
            report_currency: report_currency.into().to_ascii_uppercase(),
            dataset_version: 0,
            ledger_revision: 0,
            visible_until: None,
            include_notes: true,
            versions_frozen: false,
            journal_revision: String::new(),
        }
    }

    /// Deterministic SHA-256 hex digest of the canonical scope representation.
    pub fn scope_hash(&self) -> String {
        let mut sorted_accounts = self.resolved_accounts.clone();
        sorted_accounts.sort();
        sorted_accounts.dedup();
        let canonical = json!({
            "target": self.target,
            "resolved_accounts": sorted_accounts,
            "start_date": self.start_date.map(|d| d.to_string()),
            "as_of_date": self.as_of_date.to_string(),
            "report_currency": self.report_currency.to_ascii_uppercase(),
            "dataset_version": self.dataset_version,
            "ledger_revision": self.ledger_revision,
            "visible_until": self.visible_until.map(|t| t.to_rfc3339()),
            "include_notes": self.include_notes,
            "versions_frozen": self.versions_frozen,
            "journal_revision": self.journal_revision,
        });
        let bytes = serde_json::to_vec(&canonical).expect("scope DTO must serialize");
        hex::encode(Sha256::digest(&bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub id: String,
    pub revision: i64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultEnvelope<T> {
    pub schema_version: u32,
    pub request_id: String,
    pub result_id: String,
    pub generation: u64,
    pub value: T,
    pub scope: AnalysisScope,
    pub quality: QualityStatus,
    pub missing_inputs: Vec<String>,
    pub evidence_refs: Vec<EvidenceRef>,
    pub warnings: Vec<String>,
    pub calculation_version: String,
}

impl<T: Serialize> ResultEnvelope<T> {
    #[allow(clippy::too_many_arguments)]
    pub fn build_deterministic(
        capability_id: &str,
        request_id: impl Into<String>,
        generation: u64,
        value: T,
        scope: AnalysisScope,
        quality: QualityStatus,
        missing_inputs: Vec<String>,
        mut evidence_refs: Vec<EvidenceRef>,
        warnings: Vec<String>,
    ) -> Self {
        evidence_refs.sort();
        evidence_refs.dedup();
        let calculation_version = M2_CALCULATION_VERSION.to_string();
        let value_json =
            serde_json::to_string(&value).expect("application result DTO must serialize");
        let scope_hash = scope.scope_hash();
        let mut hasher = Sha256::new();
        hasher.update(
            format!("{capability_id}:{calculation_version}:{scope_hash}:{value_json}").as_bytes(),
        );
        let digest_hex = hex::encode(hasher.finalize());
        let result_id = format!("RES:{}:{}", &scope_hash[..12], &digest_hex[..16]);

        Self {
            schema_version: 1,
            request_id: request_id.into(),
            result_id,
            generation,
            value,
            scope,
            quality,
            missing_inputs,
            evidence_refs,
            warnings,
            calculation_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub id: String,
    pub schema_version: u32,
    pub is_read_only: bool,
    pub requires_scope: bool,
    pub description: String,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCatalog {
    pub capabilities: Vec<CapabilityDescriptor>,
}

pub const AI_READONLY_TOOL_IDS: &[&str] = &[
    "scope.describe",
    "report.period_breakdown",
    "ledger.list_transactions",
    "ledger.holdings_lots",
    "reconcile.list_issues",
    "data.quality_summary",
    "journal.search_notes",
];

impl CapabilityCatalog {
    pub fn default_catalog() -> Self {
        let capabilities = vec![
            CapabilityDescriptor {
                id: "scope.describe".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Describe the frozen analysis scope, accounts, dates, report currency, dataset version, and ledger revision.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "report.period_breakdown".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Compute deterministic period P&L and net worth breakdown (opening NAV, net external cash flows, realized/unrealized P&L, dividends, fees, FX impact, drawdown, and manual valuations) for the frozen scope.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "start_date": { "type": "string", "description": "Optional ISO date YYYY-MM-DD within frozen scope" },
                        "end_date": { "type": "string", "description": "Optional ISO date YYYY-MM-DD within frozen scope" }
                    },
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "ledger.list_transactions".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "List committed economic events and transactions within the frozen scope with optional instrument/event_type filtering and pagination.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "instrument_id": { "type": "string" },
                        "event_type": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                        "offset": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "ledger.holdings_lots".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "List current cash balances by currency, asset holdings, and open FIFO cost lots within the frozen scope.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "reconcile.list_issues".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "List balance reconciliation discrepancies between the ledger and snapshots within the frozen scope.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "data.quality_summary".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Summarize dataset quality, missing price/FX/cost inputs, cross-validation issues, and sync checkpoints for the frozen scope.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "journal.search_notes".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Search user notes that have model access enabled within the frozen scope.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Keyword or phrase to search in authorized notes" }
                    },
                    "additionalProperties": false
                }),
            },
            // Non-AI / mutating capabilities in the unified application catalog (`S-13`)
            CapabilityDescriptor {
                id: "overview.metrics".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Query top-level overview metrics and coverage status for the workbench.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "market.kline_window".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: false,
                description: "Query windowed K-line bars, indicators, and markers for an instrument.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "ledger.import_statement".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Preview or commit a broker/exchange statement import batch.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "ledger.amend_event".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Amend a committed economic event via explicit reversal and replacement.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "reconcile.account".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Record manual balance snapshots and compute reconciliation differences without auto-adjusting holdings.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "journal.save_note".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Append a new revision to a journal note and update FTS5 index.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "ops.backup".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Create a consistent SQLite and file backup with SHA-256 manifest.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
            CapabilityDescriptor {
                id: "ops.delete_library".to_string(),
                schema_version: 1,
                is_read_only: false,
                requires_scope: false,
                description: "Preview or execute complete library deletion.".to_string(),
                parameters_schema: json!({"type": "object"}),
            },
        ];
        Self { capabilities }
    }

    /// Projects ONLY the 7 `S-20` read-only tools for `ModelClient` / `AnalysisToolGateway`.
    pub fn ai_readonly_tools() -> Vec<ToolDefinition> {
        let catalog = Self::default_catalog();
        AI_READONLY_TOOL_IDS
            .iter()
            .filter_map(|id| {
                catalog
                    .capabilities
                    .iter()
                    .find(|c| c.id == *id && c.is_read_only)
                    .map(|c| ToolDefinition {
                        name: c.id.clone(),
                        description: c.description.clone(),
                        parameters_schema: c.parameters_schema.clone(),
                    })
            })
            .collect()
    }

    pub fn tool_schema_hash() -> String {
        let tools = Self::ai_readonly_tools();
        let bytes = serde_json::to_vec(&tools).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    }
}

// ============================================================================
// 2. DTOs for Scoped Queries (`M2-W01`, `M2-W02`, `M2-W04`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewMetricsDto {
    pub cash_value: Decimal,
    pub holdings_market_value: Decimal,
    pub net_worth: Decimal,
    pub net_invested: Decimal,
    pub total_pnl: Option<Decimal>,
    pub realized_pnl: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub dividend_income: Decimal,
    pub fee_summary_explanatory: Decimal,
    pub known_partial_net_worth: Decimal,
    pub excluded_items: Vec<String>,
    pub coverage_summary: String,
    pub pending_reconciliation_count: usize,
    pub report_id: String,
    pub is_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionFilterDto {
    #[serde(default)]
    pub event_id: Option<String>,
    pub instrument_id: Option<String>,
    pub event_type: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for TransactionFilterDto {
    fn default() -> Self {
        Self {
            event_id: None,
            instrument_id: None,
            event_type: None,
            limit: 100,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRowDto {
    pub event_id: String,
    pub account_id: String,
    pub event_type: String,
    pub occurred_at: String,
    pub asset_id: String,
    pub instrument_id: Option<String>,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub quote_currency: String,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub cost_known: bool,
    pub revision: i64,
    pub source_ref: Option<String>,
    pub batch_id: Option<String>,
    pub raw_payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldingsAndLotsDto {
    pub cash_balances: Vec<(String, String, Decimal, Decimal)>,
    pub holdings: Vec<HoldingProjection>,
    pub open_lots: Vec<CostLot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataQualitySummaryDto {
    pub dataset_version: i64,
    pub quality: String,
    pub issues: Vec<DataQualityIssue>,
    pub checkpoints: Vec<SyncCheckpointRecord>,
    pub missing_inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedReportStatusDto {
    pub report_id: String,
    pub report_type: String,
    pub saved_ledger_revision: i64,
    pub current_ledger_revision: i64,
    pub is_stale: bool,
    pub recompute_available: bool,
    pub payload_json: String,
}

// ============================================================================
// 3. Scoped Query Implementation on `AppService`
// ============================================================================

struct ScopedPointEvaluation {
    filtered_events: Vec<LedgerEventInput>,
    projection: LedgerProjectionResult,
    valuation: ValuationReport,
    fx_map: HashMap<(String, String, NaiveDate), Decimal>,
}

impl AppService {
    pub(crate) fn journal_revision(&self) -> Result<String, ErrorEnvelope> {
        let mut hash = Sha256::new();
        for sql in [
            "SELECT id,current_revision,allow_model_access FROM note ORDER BY id",
            "SELECT note_id,target_type,target_id,target_revision,relation FROM note_link ORDER BY note_id,target_type,target_id,id",
        ] {
            let mut statement = self.store().conn().prepare(sql).map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            let columns = statement.column_count();
            let mut rows = statement.query([]).map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            while let Some(row) = rows.next().map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))? {
                for index in 0..columns {
                    let value = row.get_ref(index).map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
                    match value {
                        rusqlite::types::ValueRef::Text(bytes) => { hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes); }
                        rusqlite::types::ValueRef::Integer(value) => hash.update(value.to_le_bytes()),
                        _ => hash.update([0]),
                    }
                }
            }
        }
        Ok(hex::encode(hash.finalize()))
    }
    /// Validate `dataset_version`, `ledger_revision`, and resolve `ScopeTarget` into a
    /// canonical, sorted, deduplicated account set (`S-13`, `M2-A-01`).
    pub fn resolve_and_validate_scope(
        &self,
        scope: &AnalysisScope,
    ) -> Result<AnalysisScope, ErrorEnvelope> {
        if scope.start_date.is_some_and(|d| d > scope.as_of_date) {
            return Err(ErrorEnvelope::new(
                "INVALID_ARGUMENT",
                "start date exceeds the valuation date",
                false,
            ));
        }
        if scope.dataset_version < 0 {
            return Err(ErrorEnvelope::new(
                "UNPUBLISHED_DATASET",
                "dataset version is unpublished or invalid (negative)",
                false,
            ));
        }

        let effective_dataset_version = if scope.dataset_version > 0 {
            let published: bool = self
                .store()
                .conn()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM dataset_publication WHERE version_num = ?1)",
                    params![scope.dataset_version],
                    |r| r.get(0),
                )
                .map_err(|e| ErrorEnvelope::new("QUERY_VERSION_FAILED", e.to_string(), false))?;
            if !published {
                return Err(ErrorEnvelope::new(
                    "UNPUBLISHED_DATASET",
                    format!(
                        "dataset version {} is unpublished or does not exist",
                        scope.dataset_version
                    ),
                    false,
                ));
            }
            scope.dataset_version
        } else if scope.versions_frozen {
            0
        } else {
            self.store()
                .conn()
                .query_row(
                    "SELECT COALESCE(MAX(version_num), 0) FROM dataset_publication",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| ErrorEnvelope::new("QUERY_VERSION_FAILED", e.to_string(), false))?
        };

        let (current_ledger_rev, default_rep_ccy): (i64, String) = self
            .store()
            .conn()
            .query_row(
                "SELECT ledger_revision, report_currency FROM library LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_LIBRARY_FAILED", e.to_string(), false))?;

        if scope.ledger_revision < 0
            || ((scope.versions_frozen || scope.ledger_revision > 0)
                && scope.ledger_revision != current_ledger_rev)
        {
            return Err(ErrorEnvelope::new(
                "REVISION_CONFLICT",
                format!(
                    "ledger revision conflict: requested {}, current is {}",
                    scope.ledger_revision, current_ledger_rev
                ),
                false,
            ));
        }

        let known_accounts = self.collect_known_accounts()?;
        let target = if scope.versions_frozen {
            ScopeTarget::Accounts(scope.resolved_accounts.clone())
        } else {
            scope.target.clone()
        };
        let resolved_accounts = match &target {
            ScopeTarget::AllAccounts => known_accounts.into_iter().collect::<Vec<_>>(),
            ScopeTarget::Accounts(list) => {
                if list.is_empty() && !scope.versions_frozen {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        "unknown or unauthorized account in scope (empty account list)",
                        false,
                    ));
                }
                let mut set = BTreeSet::new();
                for acc in list {
                    let trimmed = acc.trim();
                    if trimmed.is_empty() || !known_accounts.contains(trimmed) {
                        return Err(ErrorEnvelope::new(
                            "SCOPE_DENIED",
                            format!("unknown or unauthorized account '{trimmed}' in scope"),
                            false,
                        ));
                    }
                    set.insert(trimmed.to_string());
                }
                set.into_iter().collect::<Vec<_>>()
            }
            ScopeTarget::Portfolio(pid) => {
                let exists: bool = self
                    .store()
                    .conn()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM portfolio WHERE id = ?1)",
                        params![pid],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                if !exists {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        format!("unknown portfolio '{pid}' in scope"),
                        false,
                    ));
                }
                let accounts = self
                    .store()
                    .resolve_portfolio_accounts(pid)
                    .map_err(|e| ErrorEnvelope::new("SCOPE_DENIED", e.to_string(), false))?;
                if accounts.is_empty() {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        format!("portfolio '{pid}' has no authorized member accounts"),
                        false,
                    ));
                }
                accounts
            }
        };

        let report_currency = if scope.report_currency.trim().is_empty() {
            default_rep_ccy
        } else {
            scope.report_currency.trim().to_ascii_uppercase()
        };

        let journal_revision = self.journal_revision()?;
        if scope.versions_frozen
            && !scope.journal_revision.is_empty()
            && scope.journal_revision != journal_revision
        {
            return Err(ErrorEnvelope::new(
                "JOURNAL_REVISION_CONFLICT",
                "journal content or permissions changed; start a new analysis snapshot",
                false,
            ));
        }
        Ok(AnalysisScope {
            target: scope.target.clone(),
            resolved_accounts,
            start_date: scope.start_date,
            as_of_date: scope.as_of_date,
            report_currency,
            dataset_version: effective_dataset_version,
            ledger_revision: current_ledger_rev,
            visible_until: scope.visible_until,
            include_notes: scope.include_notes,
            versions_frozen: true,
            journal_revision,
        })
    }

    fn collect_known_accounts(&self) -> Result<BTreeSet<String>, ErrorEnvelope> {
        let mut set = BTreeSet::new();
        for sql in [
            "SELECT id FROM account WHERE status = 'active'",
            "SELECT DISTINCT account_id FROM economic_event WHERE active_in_correction_group = 1",
            "SELECT DISTINCT account_id FROM manual_valuation",
            "SELECT DISTINCT account_id FROM reconciliation_issue",
        ] {
            let mut stmt =
                self.store().conn().prepare(sql).map_err(|e| {
                    ErrorEnvelope::new("QUERY_ACCOUNTS_FAILED", e.to_string(), false)
                })?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| ErrorEnvelope::new("QUERY_ACCOUNTS_FAILED", e.to_string(), false))?;
            for row in rows {
                let row = row.map_err(|e| {
                    ErrorEnvelope::new("QUERY_ACCOUNTS_FAILED", e.to_string(), false)
                })?;
                if !row.trim().is_empty() {
                    set.insert(row);
                }
            }
        }
        Ok(set)
    }

    fn load_fx_map_at_version(
        &self,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
    ) -> Result<HashMap<(String, String, NaiveDate), Decimal>, ErrorEnvelope> {
        let max_v = dataset_version;
        let mut stmt = self
            .store()
            .conn()
            .prepare(
                "SELECT base_currency, quote_currency, rate, effective_date, effective_at
                 FROM fx_rate
                 WHERE dataset_version <= ?1
                 ORDER BY dataset_version ASC, effective_date ASC,
                          CASE WHEN base_currency='USDT' AND source='coinbase' THEN 2
                               WHEN base_currency='USDC' AND source='bitstamp' THEN 2
                               WHEN source='frankfurter' THEN 2
                               ELSE 1 END ASC,
                          source DESC",
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_FX_FAILED", e.to_string(), false))?;
        let rows = stmt
            .query_map(params![max_v], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_FX_FAILED", e.to_string(), false))?;

        let mut fx_map = HashMap::new();
        for row in rows {
            let (b, q, rate_s, ed_s, ea_s) =
                row.map_err(|e| ErrorEnvelope::new("QUERY_FX_FAILED", e.to_string(), false))?;
            if let Some(cut) = visible_until {
                if let Ok(ea) = DateTime::parse_from_rfc3339(&ea_s) {
                    if ea.with_timezone(&Utc) > cut {
                        continue;
                    }
                }
            }
            let Ok(rate) = parse_decimal_lossless(&rate_s) else {
                continue;
            };
            let Ok(effective_date) = NaiveDate::parse_from_str(&ed_s, "%Y-%m-%d") else {
                continue;
            };
            fx_map.insert(
                (
                    b.to_ascii_uppercase(),
                    q.to_ascii_uppercase(),
                    effective_date,
                ),
                rate,
            );
        }
        Ok(fx_map)
    }

    fn evaluate_scoped_point(
        &self,
        resolved: &AnalysisScope,
        target_date: NaiveDate,
    ) -> Result<ScopedPointEvaluation, ErrorEnvelope> {
        let book_currency: String = self
            .store()
            .conn()
            .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(|e| ErrorEnvelope::new("QUERY_LIBRARY_FAILED", e.to_string(), false))?;

        let all_events = load_all_ledger_events(self.store())
            .map_err(|e| ErrorEnvelope::new("LOAD_LEDGER_FAILED", e.to_string(), false))?;
        let filtered_events: Vec<LedgerEventInput> = all_events
            .into_iter()
            .filter(|e| {
                e.active_in_correction_group
                    && e.occurred_at.date_naive() <= target_date
                    && resolved
                        .visible_until
                        .is_none_or(|cut| e.occurred_at <= cut)
            })
            .collect();

        let dsv_for_lookup = resolved.dataset_version;
        let book_fx = load_book_fx_lookup_at_version(self.store(), &book_currency, dsv_for_lookup)
            .map_err(|e| ErrorEnvelope::new("LOAD_BOOK_FX_FAILED", e.to_string(), false))?;
        let bindings = load_event_book_fx(self.store())
            .map_err(|e| ErrorEnvelope::new("LOAD_EVENT_FX_FAILED", e.to_string(), false))?;

        let projection = replay_ledger_events_with_bindings(
            &book_currency,
            &filtered_events,
            &book_fx,
            &bindings,
            resolved.ledger_revision,
        )
        .map_err(|e| ErrorEnvelope::new("LEDGER_PROJECTION_FAILED", e.to_string(), false))?;

        let mut market_prices: HashMap<(String, NaiveDate), Decimal> = HashMap::new();
        let mut asset_quote_ccys: HashMap<String, String> = HashMap::new();

        let reg = self
            .store()
            .load_instrument_registry()
            .map_err(|e| ErrorEnvelope::new("REGISTRY_FAILED", e.to_string(), false))?;
        let mut instruments: Vec<_> = reg.instruments.iter().collect();
        instruments.sort_by_key(|(id, _)| *id);

        let day_cutoff = {
            target_date
                .succ_opt()
                .unwrap_or(target_date)
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
        };
        let cutoff = resolved
            .visible_until
            .map_or(day_cutoff, |cut| cut.min(day_cutoff));

        for (inst_id, inst) in instruments {
            asset_quote_ccys.insert(inst_id.clone(), inst.quote_asset_id.clone());
            asset_quote_ccys.insert(inst.base_asset_id.clone(), inst.quote_asset_id.clone());

            let mut bars = self
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
            bars.retain(|b| b.session_date <= target_date && b.close_at <= cutoff);
            bars.sort_by(|a, b| {
                a.session_date
                    .cmp(&b.session_date)
                    .then_with(|| a.source.cmp(&b.source))
            });
            bars.dedup_by_key(|b| b.session_date);

            for b in &bars {
                market_prices.insert((inst_id.clone(), b.session_date), b.close);
                market_prices.insert((inst.base_asset_id.clone(), b.session_date), b.close);
            }

            if let Some(last_bar) = bars.iter().rfind(|b| b.session_date <= target_date) {
                let venue = if inst.venue == "NASDAQ" {
                    "XNAS"
                } else if inst.venue == "NYSE" {
                    "XNYS"
                } else {
                    "CRYPTO_24_7"
                };
                let mut d = last_bar.session_date;
                let mut fresh = true;
                while d < target_date {
                    d = d.succ_opt().unwrap_or(d);
                    if query_calendar_session(venue, d).kind
                        != delta_core::CalendarSessionKind::Closed
                    {
                        fresh = false;
                        break;
                    }
                }
                if fresh {
                    market_prices.insert((inst_id.clone(), target_date), last_bar.close);
                    market_prices.insert((inst.base_asset_id.clone(), target_date), last_bar.close);
                }
            }
        }

        let fx_map =
            self.load_fx_map_at_version(resolved.dataset_version, resolved.visible_until)?;
        let liabilities = HashMap::new();
        let valuation = evaluate_portfolio(
            &projection,
            &resolved.resolved_accounts,
            &resolved.report_currency,
            target_date,
            &market_prices,
            &asset_quote_ccys,
            &fx_map,
            &liabilities,
        );

        Ok(ScopedPointEvaluation {
            filtered_events,
            projection,
            valuation,
            fx_map,
        })
    }

    pub fn query_scope_describe(
        &self,
        scope: &AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Value>, ErrorEnvelope> {
        let resolved = self.resolve_and_validate_scope(scope)?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
        let eval = self.evaluate_scoped_point(&resolved, resolved.as_of_date)?;
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

        let mut manual_net = Decimal::ZERO;
        let mut evidence_refs = Vec::new();
        for mv in latest_manual.values() {
            let stable = is_stable(&mv.currency) || is_stable(&resolved.report_currency);
            match resolve_fx_rate_s07(
                &mv.currency,
                &resolved.report_currency,
                resolved.as_of_date,
                stable,
                &eval.fx_map,
            ) {
                Some(found) => {
                    let converted = (mv.value.abs() * found.rate).normalize();
                    if mv.is_liability {
                        manual_net -= converted;
                    } else {
                        manual_net += converted;
                    }
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

        let known_partial_net_worth = (eval.valuation.net_worth + manual_net).normalize();
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
                .map(|u| (u + manual_net).normalize());
            let tot = eval
                .valuation
                .total_pnl
                .map(|t| (t + manual_net).normalize());
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
            self.store()
                .conn()
                .execute(
                    "INSERT OR REPLACE INTO analysis_report (
                    id, report_type, scope_json, ledger_revision, dataset_version,
                    calculation_version, payload_json, status, created_at
                 ) VALUES (?1, 'overview_metrics', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        report_id,
                        scope_json,
                        resolved.ledger_revision,
                        resolved.dataset_version,
                        M2_CALCULATION_VERSION,
                        payload_json,
                        quality.as_str(),
                        Utc::now().to_rfc3339(),
                    ],
                )
                .map_err(|e| ErrorEnvelope::new("REPORT_SAVE_FAILED", e.to_string(), true))?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
        let end_date = resolved.as_of_date;
        let end_eval = self.evaluate_scoped_point(&resolved, end_date)?;

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

        let start_eval = self.evaluate_scoped_point(&resolved, opening_eval_date)?;
        let manual_vals = self
            .store()
            .list_manual_valuations(Some(&resolved.resolved_accounts))
            .map_err(|e| ErrorEnvelope::new("QUERY_VALUATIONS_FAILED", e.to_string(), false))?;

        // Build comparable NAV series for drawdown when inputs are complete
        let mut comparable_nav = Vec::new();
        if start_eval.valuation.missing_inputs.is_empty()
            && end_eval.valuation.missing_inputs.is_empty()
        {
            let mut dates = BTreeSet::new();
            dates.insert(start_date);
            for ev in &end_eval.filtered_events {
                let d = ev.occurred_at.date_naive();
                if scope_set.contains(&ev.account_id) && d >= start_date && d <= end_date {
                    dates.insert(d);
                }
            }
            dates.insert(end_date);

            let mut prev_nav: Option<Decimal> = None;
            let mut prev_cap = Decimal::ZERO;
            let mut norm = Decimal::ONE;
            for d in dates {
                let pt = if d == end_date {
                    &end_eval.valuation
                } else {
                    &self.evaluate_scoped_point(&resolved, d)?.valuation
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
                        norm = (norm * ((nav - flow) / p_nav)).normalize();
                        comparable_nav.push(norm);
                    }
                } else if cap > Decimal::ZERO {
                    norm = (nav / cap).normalize();
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
            &end_eval.fx_map,
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
            self.store()
                .conn()
                .execute(
                    "INSERT OR REPLACE INTO analysis_report (
                    id, report_type, scope_json, ledger_revision, dataset_version,
                    calculation_version, payload_json, status, created_at
                 ) VALUES (?1, 'period_breakdown', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        report_id,
                        scope_json,
                        resolved.ledger_revision,
                        resolved.dataset_version,
                        M2_CALCULATION_VERSION,
                        payload_json,
                        report.quality.as_str(),
                        Utc::now().to_rfc3339(),
                    ],
                )
                .map_err(|e| ErrorEnvelope::new("REPORT_SAVE_FAILED", e.to_string(), true))?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
        let eval = self.evaluate_scoped_point(&resolved, resolved.as_of_date)?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
        let eval = self.evaluate_scoped_point(&resolved, resolved.as_of_date)?;

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
        let resolved = self.resolve_and_validate_scope(scope)?;
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
            // Verify that any linked economic_event belongs to an account within `resolved.resolved_accounts`
            let mut has_unauthorized_link = false;
            let mut has_scoped_event = false;
            let mut has_period_anchor = false;
            for link in &note.links {
                if link.target_type == "event" {
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
                    let ctx = self
                        .store()
                        .get_chart_context(&link.target_id)
                        .map_err(|e| {
                            ErrorEnvelope::new("QUERY_NOTE_SCOPE_FAILED", e.to_string(), false)
                        })?;
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
        let resolved = self.resolve_and_validate_scope(scope)?;
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
        let raw_bars = self
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
                        {
                            let ctx =
                                self.store()
                                    .get_chart_context(&link.target_id)
                                    .map_err(|e| {
                                        ErrorEnvelope::new(
                                            "CHART_CONTEXT_FAILED",
                                            e.to_string(),
                                            false,
                                        )
                                    })?;
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

// ============================================================================
// 4. `AppHost` Single-Writer Owner Thread (`S-14`, `M2-K01`, `M2-A-01`)
// ============================================================================

type BoxedHostTask = Box<dyn FnOnce(&mut AppService, u64) + Send + 'static>;

enum HostMessage {
    Execute {
        generation: u64,
        task: BoxedHostTask,
    },
    Shutdown,
}

struct AppHostInner {
    sender: mpsc::Sender<HostMessage>,
    active_generation: Arc<AtomicU64>,
    last_job_polls: Mutex<HashMap<u64, (Instant, JobSnapshot)>>,
    join_handle: Mutex<Option<JoinHandle<()>>>,
    jobs: Arc<JobController>,
}

impl Drop for AppHostInner {
    fn drop(&mut self) {
        let _ = self.sender.send(HostMessage::Shutdown);
        if let Ok(mut guard) = self.join_handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Thread-safe handle to the single-writer `AppService` owner thread (`S-14`).
/// UI and background tasks send typed closures/requests over standard library channels
/// so the UI thread never blocks on SQLite or network I/O, and stale generations are
/// rejected before execution.
#[derive(Clone)]
pub struct AppHost {
    inner: Arc<AppHostInner>,
}

impl AppHost {
    pub fn spawn(
        paths: ResolvedAppPaths,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<Self, ErrorEnvelope> {
        let svc = AppService::open(paths, book_currency, report_currency, timezone, true)?;
        Ok(Self::from_service(svc))
    }

    pub fn from_service(mut service: AppService) -> Self {
        let jobs = service.job_controller();
        let _ = service.store_mut().mark_interrupted_runs_on_startup();
        let active_generation = Arc::new(AtomicU64::new(1));
        let active_gen_worker = Arc::clone(&active_generation);
        let (sender, receiver) = mpsc::channel::<HostMessage>();

        let handle = thread::Builder::new()
            .name("delta-app-host".to_string())
            .spawn(move || {
                apply_process_quiet_guards();
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        HostMessage::Execute { generation, task } => {
                            let current = active_gen_worker.load(Ordering::SeqCst);
                            task(&mut service, current.max(generation));
                        }
                        HostMessage::Shutdown => break,
                    }
                }
            })
            .expect("failed to spawn delta-app-host thread");

        Self {
            inner: Arc::new(AppHostInner {
                sender,
                active_generation,
                last_job_polls: Mutex::new(HashMap::new()),
                join_handle: Mutex::new(Some(handle)),
                jobs,
            }),
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.inner.active_generation.load(Ordering::SeqCst)
    }

    pub fn advance_generation(&self) -> u64 {
        self.inner.active_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Waits for resource release on the caller's background worker before closing SQLite.
    pub fn shutdown(&self) -> Result<(), ErrorEnvelope> {
        self.inner
            .jobs
            .cancel_all_and_wait(Duration::from_secs(10))?;
        self.advance_generation();
        self.inner
            .sender
            .send(HostMessage::Shutdown)
            .map_err(|_| ErrorEnvelope::new("HOST_CLOSED", "宿主已关闭", false))?;
        if let Some(handle) = self.inner.join_handle.lock().unwrap().take() {
            handle
                .join()
                .map_err(|_| ErrorEnvelope::new("HOST_SHUTDOWN_FAILED", "宿主关闭失败", false))?;
        }
        Ok(())
    }

    pub fn execute_with_gen<F, R>(&self, generation: u64, f: F) -> Result<R, ErrorEnvelope>
    where
        F: FnOnce(&mut AppService) -> Result<R, ErrorEnvelope> + Send + 'static,
        R: Send + 'static,
    {
        if generation != self.current_generation() {
            return Err(ErrorEnvelope::new(
                "STALE_GENERATION",
                "stale request generation rejected",
                false,
            ));
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<R, ErrorEnvelope>>(1);
        let active_ref = Arc::clone(&self.inner.active_generation);
        let task: BoxedHostTask = Box::new(move |svc: &mut AppService, _gen_now: u64| {
            let active_now = active_ref.load(Ordering::SeqCst);
            let res = if generation != active_now {
                Err(ErrorEnvelope::new(
                    "STALE_GENERATION",
                    "stale request generation rejected",
                    false,
                ))
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(svc))).unwrap_or_else(
                    |_| {
                        Err(ErrorEnvelope::new(
                            "HOST_TASK_FAILED",
                            "application task failed",
                            false,
                        ))
                    },
                )
            };
            let _ = reply_tx.send(res);
        });

        self.inner
            .sender
            .send(HostMessage::Execute { generation, task })
            .map_err(|_| {
                ErrorEnvelope::new("HOST_CLOSED", "AppHost owner thread has shut down", false)
            })?;

        reply_rx
            .recv_timeout(Duration::from_secs(180))
            .map_err(|_| {
                ErrorEnvelope::new(
                    "HOST_DISCONNECTED",
                    "AppHost owner thread dropped reply channel",
                    false,
                )
            })?
    }

    #[allow(non_snake_case)]
    pub fn execute_With_gen<F, R>(&self, generation: u64, f: F) -> Result<R, ErrorEnvelope>
    where
        F: FnOnce(&mut AppService) -> Result<R, ErrorEnvelope> + Send + 'static,
        R: Send + 'static,
    {
        self.execute_with_gen(generation, f)
    }

    pub fn execute<F, R>(&self, f: F) -> Result<R, ErrorEnvelope>
    where
        F: FnOnce(&mut AppService) -> Result<R, ErrorEnvelope> + Send + 'static,
        R: Send + 'static,
    {
        self.execute_with_gen(self.current_generation(), f)
    }

    // ------------------------------------------------------------------------
    // Typed Scoped Queries (`S-13`, `S-14`, `M2-A-01`)
    // ------------------------------------------------------------------------

    pub fn query_scope_describe(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Value>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_scope_describe(&scope, &req, generation)
        })
    }

    pub fn query_overview(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<OverviewMetricsDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_overview(&scope, &req, generation)
        })
    }

    pub fn query_period_breakdown(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<PeriodBreakdownReport>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_period_breakdown(&scope, &req, generation)
        })
    }

    pub fn query_transactions(
        &self,
        scope: AnalysisScope,
        filter: TransactionFilterDto,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<TransactionRowDto>>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_transactions(&scope, &filter, &req, generation)
        })
    }

    pub fn query_holdings_lots(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<HoldingsAndLotsDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_holdings_lots(&scope, &req, generation)
        })
    }

    pub fn get_scoped_transaction(
        &self,
        scope: AnalysisScope,
        id: &str,
        generation: u64,
    ) -> Result<TransactionRowDto, ErrorEnvelope> {
        let id = id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.get_scoped_transaction(&scope, &id, generation)
        })
    }

    pub fn query_reconciliation_issues(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<ReconciliationIssueRecord>>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_reconciliation_issues(&scope, &req, generation)
        })
    }

    pub fn query_data_quality(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<DataQualitySummaryDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_data_quality(&scope, &req, generation)
        })
    }

    pub fn query_notes(
        &self,
        scope: AnalysisScope,
        query: &str,
        only_model_accessible: bool,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<NoteDto>>, ErrorEnvelope> {
        let q = query.to_string();
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_notes(&scope, &q, only_model_accessible, &req, generation)
        })
    }

    // ------------------------------------------------------------------------
    // Workbench Action Helpers (Accounts, Portfolios, Imports, Sync, Notes, Ops)
    // ------------------------------------------------------------------------

    pub fn upsert_account(
        &self,
        id: &str,
        name: &str,
        account_type: &str,
        provider: &str,
        external_ref: Option<&str>,
        status: &str,
    ) -> Result<AccountRecord, ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        let account_type = account_type.to_string();
        let provider = provider.to_string();
        let external_ref = external_ref.map(str::to_string);
        let status = status.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_account(
                    &id,
                    &name,
                    &account_type,
                    &provider,
                    external_ref.as_deref(),
                    &status,
                )
                .map_err(|e| ErrorEnvelope::new("ACCOUNT_UPSERT_FAILED", e.to_string(), false))
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_accounts()
                .map_err(|e| ErrorEnvelope::new("ACCOUNT_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_portfolio(
        &self,
        id: &str,
        name: &str,
        member_accounts: Vec<String>,
        child_portfolios: Vec<String>,
    ) -> Result<PortfolioRecord, ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_portfolio(&id, &name, &member_accounts, &child_portfolios)
                .map_err(|e| ErrorEnvelope::new("PORTFOLIO_UPSERT_FAILED", e.to_string(), false))
        })
    }

    pub fn list_portfolios(&self) -> Result<Vec<PortfolioRecord>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_portfolios()
                .map_err(|e| ErrorEnvelope::new("PORTFOLIO_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_manual_valuation(&self, mv: ManualValuation) -> Result<(), ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_manual_valuation(&mv)
                .map_err(|e| ErrorEnvelope::new("MANUAL_VALUATION_FAILED", e.to_string(), false))
        })
    }

    pub fn list_manual_valuations(
        &self,
        account_scope: Option<Vec<String>>,
    ) -> Result<Vec<ManualValuation>, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store()
                .list_manual_valuations(account_scope.as_deref())
                .map_err(|e| ErrorEnvelope::new("MANUAL_VALUATION_FAILED", e.to_string(), false))
        })
    }

    pub fn get_saved_report_status(
        &self,
        report_id: &str,
    ) -> Result<SavedReportStatusDto, ErrorEnvelope> {
        let id = report_id.to_string();
        self.execute(move |svc| svc.get_saved_report_status(&id))
    }

    pub fn import_statement(
        &self,
        account_id: &str,
        preset: StatementPreset,
        file_path: PathBuf,
        commit: bool,
    ) -> Result<(ImportPreview, Option<CommitImportReceipt>), ErrorEnvelope> {
        let acc = account_id.to_string();
        self.execute(move |svc| svc.import_statement(&acc, preset, &file_path, commit))
    }

    pub fn resolve_import_row(&self, row_id: &str, include: bool) -> Result<i64, ErrorEnvelope> {
        let rid = row_id.to_string();
        self.execute(move |svc| svc.resolve_import_row(&rid, include))
    }

    pub fn list_held_import_rows(&self) -> Result<Vec<String>, ErrorEnvelope> {
        self.execute(|svc| {
            let mut statement = svc.store().conn().prepare(
                "SELECT id FROM import_row WHERE validation_state = 'suspected_duplicate' ORDER BY id LIMIT 1000"
            ).map_err(|error| ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false))?;
            let rows = statement.query_map([], |row| row.get(0))
                .map_err(|error| ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false));
            rows
        })
    }

    pub fn amend_event(
        &self,
        original_event_id: &str,
        new_quantity: Decimal,
        new_price: Option<Decimal>,
        new_fee: Decimal,
    ) -> Result<i64, ErrorEnvelope> {
        let id = original_event_id.to_string();
        self.execute(move |svc| svc.amend_event(&id, new_quantity, new_price, new_fee))
    }

    pub fn reconcile_account(
        &self,
        account_id: &str,
        snapshots: Vec<AccountBalanceSnapshotDto>,
    ) -> Result<Vec<ReconciliationIssueRecord>, ErrorEnvelope> {
        let acc = account_id.to_string();
        self.execute(move |svc| svc.reconcile_account(&acc, &snapshots))
    }

    pub fn start_market_sync(
        &self,
        request: SyncRequest,
        timeout: Duration,
    ) -> Result<JobId, ErrorEnvelope> {
        self.execute(move |svc| Ok(svc.start_market_sync(request, timeout)))
    }

    /// Poll a background job with `<= 10 Hz` (100 ms) progress throttling (`S-14`).
    pub fn poll_job_throttled(&self, job_id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        {
            let guard = self.inner.last_job_polls.lock().unwrap();
            if let Some((last_instant, cached_snap)) = guard.get(&job_id.0) {
                if last_instant.elapsed() < Duration::from_millis(100) {
                    return Ok(cached_snap.clone());
                }
            }
        }
        let snap = self.inner.jobs.snapshot(job_id)?;
        self.inner
            .last_job_polls
            .lock()
            .unwrap()
            .insert(job_id.0, (Instant::now(), snap.clone()));
        Ok(snap)
    }

    pub fn cancel_job(&self, job_id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let snap = self.inner.jobs.cancel_job(job_id)?;
        self.inner
            .last_job_polls
            .lock()
            .unwrap()
            .insert(job_id.0, (Instant::now(), snap.clone()));
        Ok(snap)
    }

    pub fn wait_job(
        &self,
        job_id: JobId,
        max_wait: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        self.inner.jobs.wait_job(job_id, max_wait)
    }

    pub fn wait_job_finished(
        &self,
        job_id: JobId,
        timeout: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        self.inner.jobs.wait_finished(job_id, timeout)
    }

    pub fn search_instruments(
        &self,
        query: &str,
    ) -> Result<Vec<InstrumentSearchHit>, ErrorEnvelope> {
        let q = query.to_string();
        self.execute(move |svc| {
            svc.store()
                .search_instruments(&q)
                .map_err(|e| ErrorEnvelope::new("SEARCH_INSTRUMENTS_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_watchlist_group(
        &self,
        id: &str,
        name: &str,
        sort_order: i64,
    ) -> Result<(), ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_watchlist_group(&id, &name, sort_order)
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    pub fn add_watchlist_item(
        &self,
        group_id: &str,
        instrument_id: &str,
    ) -> Result<(), ErrorEnvelope> {
        let gid = group_id.to_string();
        let iid = instrument_id.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_watchlist_item(&gid, &iid)
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    pub fn list_watchlist_groups(&self) -> Result<Vec<WatchlistGroupDto>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_watchlist_groups()
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_kline_window_slice(
        &self,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
        viewport: KlineViewport,
        max_pixel_columns: usize,
        generation: u64,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        let iid = instrument_id.to_string();
        let tf = timeframe.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.load_kline_window_slice(
                &iid,
                &tf,
                adjustment,
                dataset_version,
                visible_until,
                &viewport,
                max_pixel_columns,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_scoped_kline_window_slice(
        &self,
        scope: AnalysisScope,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        viewport: KlineViewport,
        max_pixel_columns: usize,
        generation: u64,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        let instrument = instrument_id.to_string();
        let timeframe = timeframe.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.load_scoped_kline_window_slice(
                &scope,
                &instrument,
                &timeframe,
                adjustment,
                &viewport,
                max_pixel_columns,
            )
        })
    }

    pub fn save_chart_context(&self, ctx: ChartContextDto) -> Result<String, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .save_chart_context(&ctx)
                .map_err(|e| ErrorEnvelope::new("CHART_CONTEXT_FAILED", e.to_string(), false))
        })
    }

    pub fn get_chart_context(&self, id: &str) -> Result<ChartContextDto, ErrorEnvelope> {
        let id = id.to_string();
        self.execute(move |svc| {
            svc.store()
                .get_chart_context(&id)
                .map_err(|e| ErrorEnvelope::new("CHART_CONTEXT_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_trade_plan(&self, plan: TradePlanDto) -> Result<(), ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_trade_plan(&plan)
                .map_err(|e| ErrorEnvelope::new("TRADE_PLAN_FAILED", e.to_string(), false))
        })
    }

    pub fn link_plan_or_note_fill(
        &self,
        plan_id: Option<&str>,
        note_id: Option<&str>,
        event_id: &str,
        allocated_quantity: Decimal,
        relation: &str,
    ) -> Result<(), ErrorEnvelope> {
        let pid = plan_id.map(str::to_string);
        let nid = note_id.map(str::to_string);
        let eid = event_id.to_string();
        let rel = relation.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .link_plan_or_note_fill(
                    pid.as_deref(),
                    nid.as_deref(),
                    &eid,
                    allocated_quantity,
                    &rel,
                )
                .map_err(|e| ErrorEnvelope::new("LINK_FILL_FAILED", e.to_string(), false))
        })
    }

    pub fn add_note_link(
        &self,
        note_id: &str,
        target_type: &str,
        target_id: &str,
        target_revision: i64,
        allocated_quantity: Option<Decimal>,
        relation: &str,
    ) -> Result<String, ErrorEnvelope> {
        let nid = note_id.to_string();
        let ttype = target_type.to_string();
        let tid = target_id.to_string();
        let rel = relation.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_note_link(
                    &nid,
                    &ttype,
                    &tid,
                    target_revision,
                    allocated_quantity,
                    &rel,
                )
                .map_err(|e| ErrorEnvelope::new("NOTE_LINK_FAILED", e.to_string(), false))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_note_revision(
        &self,
        note_id: &str,
        title: &str,
        template_kind: &str,
        allow_model_access: bool,
        body_markdown: &str,
        structured: StructuredNoteFields,
        tags: Vec<String>,
    ) -> Result<i64, ErrorEnvelope> {
        let nid = note_id.to_string();
        let title = title.to_string();
        let tk = template_kind.to_string();
        let body = body_markdown.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .save_note_revision(
                    &nid,
                    &title,
                    &tk,
                    allow_model_access,
                    &body,
                    &structured,
                    &tags,
                )
                .map_err(|e| ErrorEnvelope::new("NOTE_SAVE_FAILED", e.to_string(), true))
        })
    }

    pub fn get_note(&self, note_id: &str) -> Result<NoteDto, ErrorEnvelope> {
        let nid = note_id.to_string();
        self.execute(move |svc| {
            svc.store()
                .get_note(&nid)
                .map_err(|e| ErrorEnvelope::new("NOTE_NOT_FOUND", e.to_string(), false))
        })
    }

    pub fn get_scoped_note(
        &self,
        scope: AnalysisScope,
        note_id: &str,
        generation: u64,
    ) -> Result<NoteDto, ErrorEnvelope> {
        let note_id = note_id.to_string();
        self.execute_with_gen(generation, move |svc| svc.get_scoped_note(&scope, &note_id))
    }

    pub fn list_notes(&self, only_model_accessible: bool) -> Result<Vec<NoteDto>, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store()
                .list_notes(only_model_accessible)
                .map_err(|e| ErrorEnvelope::new("NOTE_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn add_note_attachment(
        &self,
        note_id: &str,
        file_name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<AttachmentDto, ErrorEnvelope> {
        let nid = note_id.to_string();
        let fname = file_name.to_string();
        let mime = mime_type.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_note_attachment(&nid, &fname, &mime, &bytes)
                .map_err(|e| ErrorEnvelope::new("ATTACHMENT_FAILED", e.to_string(), false))
        })
    }

    pub fn create_backup(&self, backup_dir: PathBuf) -> Result<LibraryManifest, ErrorEnvelope> {
        self.execute(move |svc| svc.create_backup(&backup_dir))
    }

    pub fn export_library(&self, export_dir: PathBuf) -> Result<ExportManifestDto, ErrorEnvelope> {
        self.execute(move |svc| svc.export_library(&export_dir))
    }

    pub fn sync_from_fixtures(
        &self,
        fixtures_root: PathBuf,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        self.execute(move |svc| svc.sync_from_fixtures(&fixtures_root))
    }
}

/// Seed the canonical AC-02 minimal ledger + market data fixture into `AppService`:
/// - Account `ACC-1`: opening USD `2000`, buy `10` `NASDAQ:AAPL` @ `100` (fee `1`),
///   sell `4` `NASDAQ:AAPL` @ `110` (fee `1`), ending `2026-01-15` close `105`.
/// - Produces cash `1438`, holdings market value `630`, NAV `2068`, realized PnL `38.6`,
///   unrealized PnL `29.4`, total PnL `68`.
pub fn seed_ac02_fixture_in_service(svc: &mut AppService) -> Result<i64, ErrorEnvelope> {
    svc.store_mut()
        .upsert_account(
            "ACC-1",
            "Primary US Equity",
            "broker",
            "futu",
            Some("88881234"),
            "active",
        )
        .map_err(|e| ErrorEnvelope::new("SEED_ACCOUNT_FAILED", e.to_string(), false))?;

    let csv_path = svc.paths().data_dir.join("imports").join("ac02_seed.csv");
    let csv_body = concat!(
        "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
        "ACC-1,opening_balance,2026-01-02 14:30:00,UTC,USD,buy,2000,1,USD,0,USD,AC02-OPEN\n",
        "ACC-1,buy,2026-01-05 15:00:00,UTC,NASDAQ:AAPL,buy,10,100,USD,1,USD,AC02-BUY\n",
        "ACC-1,sell,2026-01-10 15:30:00,UTC,NASDAQ:AAPL,sell,4,110,USD,1,USD,AC02-SELL\n",
    );
    std::fs::write(&csv_path, csv_body)
        .map_err(|e| ErrorEnvelope::new("SEED_CSV_WRITE", e.to_string(), false))?;

    let dsv = svc
        .store_mut()
        .create_dataset_version(
            "ac02-fixture",
            "public_open",
            r#"{"equities":["NASDAQ:AAPL"]}"#,
            "exact",
            "exact",
        )
        .map_err(|e| ErrorEnvelope::new("SEED_DSV_FAILED", e.to_string(), false))?;
    let v = dsv.version_num;

    let d_buy = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
    let d_sell = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
    let d_asof = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

    let mut bars = Vec::new();
    for (d, close_str) in [(d_buy, "100"), (d_sell, "110"), (d_asof, "105")] {
        let close = parse_decimal_lossless(close_str).unwrap();
        bars.push(BarRecord {
            id: format!("BAR:AC02:{d}"),
            instrument_id: "NASDAQ:AAPL".to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date: d,
            open_at: Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap()),
            open: close,
            high: close + Decimal::ONE,
            low: close - Decimal::ONE,
            close,
            volume: Decimal::from(10_000),
            source: "ac02-fixture".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: v,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }

    svc.store_mut()
        .commit_bars_and_checkpoint(&bars, v, None)
        .map_err(|e| ErrorEnvelope::new("SEED_BARS_FAILED", e.to_string(), false))?;
    svc.store_mut()
        .publish_dataset_version(v)
        .map_err(|e| ErrorEnvelope::new("SEED_PUBLISH_FAILED", e.to_string(), false))?;

    svc.import_statement("ACC-1", StatementPreset::GenericCsv, &csv_path, true)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AnalysisToolGateway;
    use crate::paths::{resolve_app_paths, BuildChannel};

    #[test]
    fn test_m2_a01_scoped_query_consistency_and_stale_generation_rejection() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a01_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);

        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".to_string()], as_of, "USD");
        scope.dataset_version = dsv;
        scope.ledger_revision = 2;

        let gen1 = host.current_generation();
        // 1. Run the same scoped query twice through AppHost
        let env1 = host
            .query_period_breakdown(scope.clone(), "req-a01-same", gen1)
            .expect("first AppHost query");
        let env2 = host
            .query_period_breakdown(scope.clone(), "req-a01-same", gen1)
            .expect("second AppHost query");

        // 2. And once through the AI tool gateway
        let env3: ResultEnvelope<PeriodBreakdownReport> = host
            .execute_with_gen(gen1, {
                let s = scope.clone();
                move |svc| {
                    let mut gw = AnalysisToolGateway::new("run-a01-test", gen1, s.clone())?;
                    let receipt = gw.execute_tool(
                        svc,
                        "call-a01-1",
                        "report.period_breakdown",
                        &json!({}),
                        "req-a01-same",
                    )?;
                    serde_json::from_value(receipt.envelope_json)
                        .map_err(|e| ErrorEnvelope::new("DESERIALIZE_FAILED", e.to_string(), false))
                }
            })
            .expect("third query via AnalysisToolGateway");

        // Field-for-field identical check (values, quality, evidence_refs, versions, result_id)
        assert_eq!(env1, env2);
        assert_eq!(env1, env3);
        assert_eq!(
            env1.value.ending_net_worth,
            Some(parse_decimal_lossless("2068").unwrap())
        );
        assert_eq!(
            env1.value.investment_pnl,
            Some(parse_decimal_lossless("68").unwrap())
        );
        assert_eq!(
            env1.value.realized_pnl,
            Some(parse_decimal_lossless("38.6").unwrap())
        );
        assert_eq!(
            env1.value.unrealized_pnl,
            Some(parse_decimal_lossless("29.4").unwrap())
        );
        assert!(!env1.evidence_refs.is_empty());

        // Also check OverviewMetricsDto matches AC-02 (cash 1438, NAV 2068, total PnL 68)
        let ov = host
            .query_overview(scope.clone(), "req-a01-ov", gen1)
            .expect("overview query");
        assert_eq!(ov.value.cash_value, parse_decimal_lossless("1438").unwrap());
        assert_eq!(ov.value.net_worth, parse_decimal_lossless("2068").unwrap());
        assert_eq!(
            ov.value.total_pnl,
            Some(parse_decimal_lossless("68").unwrap())
        );

        // 3. Unknown scope rejection
        let bad_scope =
            AnalysisScope::for_accounts(vec!["ACC-NONEXISTENT-999".to_string()], as_of, "USD");
        let err_unknown = host
            .query_period_breakdown(bad_scope, "req-bad-scope", gen1)
            .expect_err("unknown account scope must be rejected");
        assert_eq!(err_unknown.code, "SCOPE_DENIED");

        // 4. Unpublished dataset_version rejection
        let mut unpub_scope = scope.clone();
        unpub_scope.dataset_version = 9999;
        let err_unpub = host
            .query_period_breakdown(unpub_scope, "req-unpub", gen1)
            .expect_err("unpublished dataset version must be rejected");
        assert_eq!(err_unpub.code, "UNPUBLISHED_DATASET");

        // 5. Stale generation rejection
        let gen2 = host.advance_generation();
        assert_eq!(gen2, 2);
        let err_stale = host
            .query_period_breakdown(scope, "req-stale-gen", gen1)
            .expect_err("stale generation must be rejected");
        assert_eq!(err_stale.code, "STALE_GENERATION");

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_m2_a04_accounts_portfolios_and_scoped_period_pnl_breakdown() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a04_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        svc.store_mut()
            .upsert_account("ACC-A", "Account A", "broker", "futu", None, "active")
            .unwrap();
        svc.store_mut()
            .upsert_account("ACC-B", "Account B", "broker", "ibkr", None, "active")
            .unwrap();

        // Overlapping portfolio hierarchy: PF-CHILD1(ACC-A), PF-CHILD2(ACC-A, ACC-B), PF-ROOT(children: PF-CHILD1, PF-CHILD2)
        svc.store_mut()
            .upsert_portfolio("PF-CHILD1", "Child 1", &["ACC-A".to_string()], &[])
            .unwrap();
        svc.store_mut()
            .upsert_portfolio(
                "PF-CHILD2",
                "Child 2",
                &["ACC-A".to_string(), "ACC-B".to_string()],
                &[],
            )
            .unwrap();
        svc.store_mut()
            .upsert_portfolio(
                "PF-ROOT",
                "Root Combined",
                &["ACC-A".to_string()],
                &["PF-CHILD1".to_string(), "PF-CHILD2".to_string()],
            )
            .unwrap();

        // Import events:
        // Day 1 (2026-01-01): ACC-A opening 1002 USD
        // Day 2 (2026-01-02): Internal transfer 1000 USD from ACC-A to ACC-B with fee 2 USD on ACC-A (AC-04)
        // Day 3 (2026-01-03): External deposit 500 USD into ACC-B with unchanged market (AC-07 inflow)
        let csv_a = svc.paths().data_dir.join("imports").join("a04_acca.csv");
        let csv_a_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id,counterparty_account\n",
            "ACC-A,opening_balance,2026-01-01 10:00:00,UTC,USD,buy,1002,1,USD,0,USD,A04-OPEN,\n",
            "ACC-A,transfer_out,2026-01-02 10:00:00,UTC,USD,sell,1000,1,USD,2,USD,TG-A04,ACC-B\n",
        );
        std::fs::write(&csv_a, csv_a_body).unwrap();
        svc.import_statement("ACC-A", StatementPreset::GenericCsv, &csv_a, true)
            .unwrap();

        let csv_b = svc.paths().data_dir.join("imports").join("a04_accb.csv");
        let csv_b_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id,counterparty_account\n",
            "ACC-B,transfer_in,2026-01-02 10:05:00,UTC,USD,buy,1000,1,USD,0,USD,TG-A04,ACC-A\n",
            "ACC-B,deposit,2026-01-03 10:00:00,UTC,USD,buy,500,1,USD,0,USD,A04-DEP,\n",
        );
        std::fs::write(&csv_b, csv_b_body).unwrap();
        svc.import_statement("ACC-B", StatementPreset::GenericCsv, &csv_b, true)
            .unwrap();

        let host = AppHost::from_service(svc);
        let gen = host.current_generation();
        let d_start = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let d_end = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();

        // 1. Portfolio view (`PF-ROOT` resolving to `["ACC-A", "ACC-B"]` with no double counting):
        // Opening NAV (as of 2026-01-01 close) = 1002, Day 2 internal transfer of 1000 has fee 2 -> NAV 1000, Day 3 external inflow +500 -> Ending NAV 1500.
        // Net external cash flow = +500; Period investment P&L = 1500 - 1002 - 500 = -2 (pure fee P&L, internal transfer & AC-07 inflow create 0 return!).
        let mut pf_scope = AnalysisScope::for_portfolio("PF-ROOT", d_end, "USD");
        pf_scope.start_date = Some(d_start);
        let pf_bd = host
            .query_period_breakdown(pf_scope, "req-a04-pf", gen)
            .unwrap();
        assert_eq!(
            pf_bd.scope.resolved_accounts,
            vec!["ACC-A".to_string(), "ACC-B".to_string()]
        );
        assert_eq!(
            pf_bd.value.opening_net_worth,
            Some(parse_decimal_lossless("1002").unwrap())
        );
        assert_eq!(
            pf_bd.value.ending_net_worth,
            Some(parse_decimal_lossless("1500").unwrap())
        );
        assert_eq!(
            pf_bd.value.net_external_cash_flow,
            Some(parse_decimal_lossless("500").unwrap())
        );
        assert_eq!(
            pf_bd.value.investment_pnl,
            Some(parse_decimal_lossless("-2").unwrap())
        );
        assert_eq!(
            pf_bd.value.explanatory_total_fees,
            parse_decimal_lossless("2").unwrap()
        );
        assert!(!pf_bd.value.drawdown_status.is_empty());

        // 2. Single account `ACC-B` view on Day 3 (AC-07 inflow): opening (as of 2026-01-02 close) = 1000, external deposit +500, ending 1500 -> period P&L = 0!
        let mut acc_b_scope = AnalysisScope::for_accounts(vec!["ACC-B".to_string()], d_end, "USD");
        acc_b_scope.start_date = Some(NaiveDate::from_ymd_opt(2026, 1, 3).unwrap());
        let acc_b_bd = host
            .query_period_breakdown(acc_b_scope, "req-a04-accb", gen)
            .unwrap();
        assert_eq!(
            acc_b_bd.value.opening_net_worth,
            Some(parse_decimal_lossless("1000").unwrap())
        );
        assert_eq!(
            acc_b_bd.value.ending_net_worth,
            Some(parse_decimal_lossless("1500").unwrap())
        );
        assert_eq!(
            acc_b_bd.value.net_external_cash_flow,
            Some(parse_decimal_lossless("500").unwrap())
        );
        assert_eq!(acc_b_bd.value.investment_pnl, Some(Decimal::ZERO));

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_m2_a05_missing_price_fx_cost_manual_valuations_and_stale_reports() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a05_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();

        // Add an asset with unknown opening cost and missing market price (`NYSE:UNPRICED`)
        let csv_path = svc.paths().data_dir.join("imports").join("a05_unknown.csv");
        let csv_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,cost_known,source_id\n",
            "ACC-1,opening_balance,2026-01-12 10:00:00,UTC,NYSE:UNPRICED,buy,5,,USD,0,USD,false,A05-UNK-OPEN\n",
        );
        std::fs::write(&csv_path, csv_body).unwrap();
        svc.import_statement("ACC-1", StatementPreset::GenericCsv, &csv_path, true)
            .unwrap();

        // Add a manual valuation (`Private Equity Fund`, `500 USD`, method `quarterly_appraisal`)
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        svc.store_mut()
            .upsert_manual_valuation(&ManualValuation {
                id: "MV-A05-1".to_string(),
                account_id: "ACC-1".to_string(),
                asset_or_liability_id: "PRIVATE:FUND-A".to_string(),
                is_liability: false,
                value: parse_decimal_lossless("500").unwrap(),
                currency: "USD".to_string(),
                as_of,
                method: "quarterly_appraisal".to_string(),
            })
            .unwrap();

        let host = AppHost::from_service(svc);
        let gen = host.current_generation();
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".to_string()], as_of, "USD");
        scope.dataset_version = dsv;

        // 1. Query overview and period breakdown:
        // Known-part valuation shown (`1438` cash + `630` AAPL + `500` manual valuation = `2568`),
        // excluded items list `NYSE:UNPRICED`, affected total/unrealized PnL is `None` (never fabricated as `0`)!
        let ov = host
            .query_overview(scope.clone(), "req-a05-ov", gen)
            .unwrap();
        assert_eq!(ov.quality, QualityStatus::Partial);
        assert_eq!(ov.value.total_pnl, None);
        assert_eq!(ov.value.unrealized_pnl, None);
        assert_eq!(
            ov.value.known_partial_net_worth,
            parse_decimal_lossless("2568").unwrap()
        );
        assert!(!ov.value.excluded_items.is_empty());

        let bd = host
            .query_period_breakdown(scope.clone(), "req-a05-bd", gen)
            .unwrap();
        assert_eq!(bd.quality, QualityStatus::Partial);
        assert_eq!(bd.value.investment_pnl, None);
        assert_eq!(bd.value.unrealized_pnl, None);
        assert_eq!(bd.value.manual_valuations.len(), 1);
        assert_eq!(
            bd.value.manual_valuations[0].value,
            parse_decimal_lossless("500").unwrap()
        );
        assert_eq!(bd.value.manual_valuations[0].method, "quarterly_appraisal");
        assert_eq!(bd.value.manual_valuations[0].as_of, as_of);

        // 2. Amend a booked trade (`AC02-SELL`), then reopen the earlier saved report -> flagged `is_stale = true` with `recompute_available = true`!
        let saved_report_id = ov.value.report_id.clone();
        let status_before = host.get_saved_report_status(&saved_report_id).unwrap();
        assert!(!status_before.is_stale);

        let sell_event_id = host
            .execute(|svc| {
                let events = svc.load_all_ledger_events()?;
                Ok(events
                    .into_iter()
                    .find(|e| e.event_type == EconomicEventType::Sell)
                    .unwrap()
                    .event_id)
            })
            .unwrap();
        host.amend_event(
            &sell_event_id,
            parse_decimal_lossless("3").unwrap(),
            Some(parse_decimal_lossless("110").unwrap()),
            Decimal::ONE,
        )
        .unwrap();

        let status_after = host.get_saved_report_status(&saved_report_id).unwrap();
        assert!(status_after.is_stale);
        assert!(status_after.recompute_available);

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
