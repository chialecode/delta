use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{CostLot, DataQualityIssue, HoldingProjection, QualityStatus};
use delta_data::SyncCheckpointRecord;
use delta_model::ToolDefinition;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::service::{AppService, ErrorEnvelope};

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
    TrainingRun(String),
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

    pub fn for_training_run(run_id: impl Into<String>) -> Self {
        Self {
            target: ScopeTarget::TrainingRun(run_id.into()),
            resolved_accounts: Vec::new(),
            start_date: None,
            as_of_date: NaiveDate::from_ymd_opt(1, 1, 1).expect("valid synthetic date"),
            report_currency: String::new(),
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

pub const AI_TRAINING_READONLY_TOOL_IDS: &[&str] = &[
    "scope.describe",
    "training.run_summary",
    "training.orders_and_fills",
    "training.report",
    "training.notes",
    "training.revealed_bars",
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
            // M4 Read-only simulated training review tools (`S-45`, `M4-A-14`)
            CapabilityDescriptor {
                id: "training.run_summary".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Summarize the finished simulated training run, fee profile version, step count, hidden-mode display label, and ending equity.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "training.orders_and_fills".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "List simulated orders and fills in the finished training run with openable SIM-ORD and SIM-FILL evidence IDs.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "training.report".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Read the deterministic simulated training report, closed trade groups, MAE/MFE, Buy-and-Hold benchmark, plan execution rate, and sample limitations.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "training.notes".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Read user training notes recorded during the finished simulated training run as untrusted free-text data.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            CapabilityDescriptor {
                id: "training.revealed_bars".to_string(),
                schema_version: 1,
                is_read_only: true,
                requires_scope: true,
                description: "Read post-finish 60-bar market context when the finished training run is unmasked or explicitly revealed.".to_string(),
                parameters_schema: json!({
                    "type": "object",
                    "properties": {},
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

    /// Projects ONLY the `S-45` read-only simulated training review tools (`ScopeTarget::TrainingRun`).
    pub fn ai_training_readonly_tools() -> Vec<ToolDefinition> {
        let catalog = Self::default_catalog();
        AI_TRAINING_READONLY_TOOL_IDS
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

    pub fn ai_tools_for_scope(scope: &AnalysisScope) -> Vec<ToolDefinition> {
        if matches!(scope.target, ScopeTarget::TrainingRun(_)) {
            Self::ai_training_readonly_tools()
        } else {
            Self::ai_readonly_tools()
        }
    }

    pub fn tool_schema_hash() -> String {
        let tools = Self::ai_readonly_tools();
        let bytes = serde_json::to_vec(&tools).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    }

    pub fn tool_schema_hash_for_scope(scope: &AnalysisScope) -> String {
        let tools = Self::ai_tools_for_scope(scope);
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

impl AppService {
    pub(crate) fn journal_revision(&self) -> Result<String, ErrorEnvelope> {
        let mut hash = Sha256::new();
        for sql in [
            "SELECT id,current_revision,allow_model_access FROM note ORDER BY id",
            "SELECT note_id,target_type,target_id,target_revision,relation FROM note_link ORDER BY note_id,target_type,target_id,id",
        ] {
            let mut statement = self
                .store()
                .conn()
                .prepare(sql)
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            let columns = statement.column_count();
            let mut rows = statement
                .query([])
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            while let Some(row) = rows
                .next()
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?
            {
                for index in 0..columns {
                    let value = row.get_ref(index).map_err(|e| {
                        ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false)
                    })?;
                    match value {
                        rusqlite::types::ValueRef::Text(bytes) => {
                            hash.update((bytes.len() as u64).to_le_bytes());
                            hash.update(bytes);
                        }
                        rusqlite::types::ValueRef::Integer(value) => {
                            hash.update(value.to_le_bytes())
                        }
                        _ => hash.update([0]),
                    }
                }
            }
        }
        Ok(hex::encode(hash.finalize()))
    }

    pub(crate) fn scope_journal_revision(
        &self,
        target: &ScopeTarget,
    ) -> Result<String, ErrorEnvelope> {
        if let ScopeTarget::TrainingRun(ref run_id) = target {
            let mut hash = Sha256::new();
            let trimmed_run_id = run_id.trim();
            let run_meta: Option<(String, i64)> = self
                .store()
                .conn()
                .query_row(
                    "SELECT r.latest_state_hash, s.revealed
                     FROM simulation_run r
                     JOIN training_session s ON s.run_id = r.run_id
                     WHERE r.run_id = ?1",
                    params![trimmed_run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            if let Some((latest_state_hash, revealed)) = run_meta {
                let state_bytes = latest_state_hash.as_bytes();
                hash.update((state_bytes.len() as u64).to_le_bytes());
                hash.update(state_bytes);
                hash.update(revealed.to_le_bytes());
            }
            let mut stmt = self
                .store()
                .conn()
                .prepare(
                    "SELECT note_id, visible_until, rationale, COALESCE(emotion_tag, ''), COALESCE(linked_order_id, ''), COALESCE(archived_journal_note_id, ''), updated_at
                     FROM training_note
                     WHERE run_id = ?1
                     ORDER BY created_at ASC, note_id ASC",
                )
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            let columns = stmt.column_count();
            let mut rows = stmt
                .query(params![trimmed_run_id])
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?;
            while let Some(row) = rows
                .next()
                .map_err(|e| ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false))?
            {
                for index in 0..columns {
                    let value = row.get_ref(index).map_err(|e| {
                        ErrorEnvelope::new("JOURNAL_VERSION_FAILED", e.to_string(), false)
                    })?;
                    match value {
                        rusqlite::types::ValueRef::Text(bytes) => {
                            hash.update((bytes.len() as u64).to_le_bytes());
                            hash.update(bytes);
                        }
                        rusqlite::types::ValueRef::Integer(value) => {
                            hash.update(value.to_le_bytes())
                        }
                        _ => hash.update([0]),
                    }
                }
            }
            return Ok(hex::encode(hash.finalize()));
        }
        self.journal_revision()
    }

    /// Resolve and validate `scope`, rejecting `ScopeTarget::TrainingRun` on real-account ledger,
    /// valuation, and note queries so simulated training runs remain strictly isolated (`APP-B15`).
    pub(super) fn require_real_account_scope(
        &self,
        scope: &AnalysisScope,
    ) -> Result<AnalysisScope, ErrorEnvelope> {
        let resolved = self.resolve_and_validate_scope(scope)?;
        if matches!(resolved.target, ScopeTarget::TrainingRun(_)) {
            return Err(ErrorEnvelope::new(
                "SCOPE_DENIED",
                "real-account query is not permitted for a simulated training run scope",
                false,
            ));
        }
        Ok(resolved)
    }

    /// Validate `dataset_version`, `ledger_revision`, and resolve `ScopeTarget` into a
    /// canonical, sorted, deduplicated account set (`S-13`, `M2-A-01`) or a verified
    /// finished training run (`S-45`, `M4-A-14`).
    pub fn resolve_and_validate_scope(
        &self,
        scope: &AnalysisScope,
    ) -> Result<AnalysisScope, ErrorEnvelope> {
        if let ScopeTarget::TrainingRun(ref raw_run_id) = scope.target {
            let run_id = raw_run_id.trim();
            if run_id.is_empty() {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    "training run id in scope is empty",
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
            let current_ledger_rev: i64 = self
                .store()
                .conn()
                .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                    r.get(0)
                })
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

            let run = self.store().get_simulation_run(run_id).map_err(|_| {
                ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!("unknown simulated training run '{run_id}' in scope"),
                    false,
                )
            })?;
            let session = self.store().get_training_session(run_id).map_err(|_| {
                ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!("missing training session for '{run_id}'"),
                    false,
                )
            })?;
            if !run.status.is_ended() {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!(
                        "simulated training run '{run_id}' is '{}' — AI review is permitted only after the training session is finished",
                        run.status.as_str()
                    ),
                    false,
                ));
            }
            let _ = self.read_verified_sim_state(run_id)?;

            if scope.dataset_version > 0 && scope.dataset_version != run.dataset_version {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!(
                        "scope dataset_version {} does not match frozen training run dataset_version {}",
                        scope.dataset_version, run.dataset_version
                    ),
                    false,
                ));
            }

            let report_currency = run.quote_currency.trim().to_ascii_uppercase();
            if !scope.report_currency.trim().is_empty()
                && !scope
                    .report_currency
                    .trim()
                    .eq_ignore_ascii_case(&report_currency)
            {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!(
                        "scope report_currency '{}' does not match training quote_currency '{report_currency}'",
                        scope.report_currency.trim()
                    ),
                    false,
                ));
            }

            let masked = crate::training::is_run_identity_masked(&run, &session);
            let (start_date, as_of_date, visible_until) = if masked {
                (
                    None,
                    NaiveDate::from_ymd_opt(1, 1, 1).expect("valid synthetic date"),
                    None,
                )
            } else {
                (
                    Some(delta_data::SqliteStore::effective_visible_ex_date(
                        &session.real_instrument_id,
                        session.history_start_at,
                    )),
                    delta_data::SqliteStore::effective_visible_ex_date(
                        &session.real_instrument_id,
                        session.visible_until,
                    ),
                    Some(session.visible_until),
                )
            };

            let target = ScopeTarget::TrainingRun(run_id.to_string());
            let journal_revision = self.scope_journal_revision(&target)?;
            if scope.versions_frozen
                && !scope.journal_revision.is_empty()
                && scope.journal_revision != journal_revision
            {
                return Err(ErrorEnvelope::new(
                    "JOURNAL_REVISION_CONFLICT",
                    "training notes changed during analysis; start a new analysis snapshot",
                    false,
                ));
            }

            return Ok(AnalysisScope {
                target,
                resolved_accounts: Vec::new(),
                start_date,
                as_of_date,
                report_currency,
                dataset_version: run.dataset_version,
                ledger_revision: current_ledger_rev,
                visible_until,
                include_notes: scope.include_notes,
                versions_frozen: true,
                journal_revision,
            });
        }
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
            ScopeTarget::TrainingRun(_) => unreachable!("handled above"),
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
}
