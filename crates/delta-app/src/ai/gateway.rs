use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use delta_core::{parse_decimal_lossless, to_canonical_text};
use delta_data::AiToolCallRecord;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::host::{
    AnalysisScope, CapabilityCatalog, EvidenceRef, ScopeTarget, TransactionFilterDto,
    AI_READONLY_TOOL_IDS, AI_TRAINING_READONLY_TOOL_IDS,
};
use crate::service::{AppService, ErrorEnvelope};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionReceipt {
    pub call_id: String,
    pub tool_name: String,
    pub result_id: String,
    pub envelope_json: Value,
    pub evidence_refs: Vec<EvidenceRef>,
}

// ============================================================================
// `AnalysisToolGateway` (`S-20`, `M2-A-01`, `M2-A-14`)
// ============================================================================

pub struct AnalysisToolGateway {
    pub run_id: String,
    pub generation: u64,
    pub frozen_scope: AnalysisScope,
    pub valid_evidence_map: BTreeMap<String, EvidenceRef>,
    pub authorized_numbers: BTreeSet<String>,
    pub authorized_currencies: BTreeSet<String>,
    pub executed_calls: usize,
    pub(super) max_detail_rows: usize,
}

impl AnalysisToolGateway {
    pub fn new(
        run_id: impl Into<String>,
        generation: u64,
        frozen_scope: AnalysisScope,
    ) -> Result<Self, ErrorEnvelope> {
        let mut authorized_currencies = BTreeSet::new();
        let rep = frozen_scope.report_currency.trim().to_ascii_uppercase();
        if !rep.is_empty() {
            authorized_currencies.insert(rep);
        }
        Ok(Self {
            run_id: run_id.into(),
            generation,
            frozen_scope,
            valid_evidence_map: BTreeMap::new(),
            authorized_numbers: BTreeSet::new(),
            authorized_currencies,
            executed_calls: 0,
            max_detail_rows: 100,
        })
    }

    /// Validate and execute a single read-only tool call against `AppService` under `self.frozen_scope`.
    /// - Rejects any tool not in the scope's read-only whitelist with `UNSUPPORTED_CAPABILITY` / `SCOPE_DENIED`.
    /// - Rejects any argument attempting to pass an `account_id` or `accounts` outside `frozen_scope` with `SCOPE_DENIED`.
    /// - Records the tool call idempotently in `ai_tool_call` (`S-21`).
    /// - Harvests all returned `EvidenceRef`s, canonical Decimal numbers, and structured currencies for post-run verification (`S-20`).
    pub fn execute_tool(
        &mut self,
        svc: &mut AppService,
        call_id: &str,
        tool_name: &str,
        args: &Value,
        request_id: &str,
    ) -> Result<ToolExecutionReceipt, ErrorEnvelope> {
        self.execute_tool_from_snapshot(svc, None, call_id, tool_name, args, request_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_tool_from_snapshot(
        &mut self,
        svc: &mut AppService,
        snapshot: Option<&AppService>,
        call_id: &str,
        tool_name: &str,
        args: &Value,
        request_id: &str,
    ) -> Result<ToolExecutionReceipt, ErrorEnvelope> {
        let trimmed_tool = tool_name.trim();
        let is_account_tool = AI_READONLY_TOOL_IDS.contains(&trimmed_tool);
        let is_training_tool = AI_TRAINING_READONLY_TOOL_IDS.contains(&trimmed_tool);
        if !is_account_tool && !is_training_tool {
            return Err(ErrorEnvelope::new(
                "UNSUPPORTED_CAPABILITY",
                format!("tool '{trimmed_tool}' not in read-only whitelist"),
                false,
            ));
        }

        // Resolve and validate frozen scope on host (enforcing finished-only for `ScopeTarget::TrainingRun`)
        if !self.frozen_scope.versions_frozen
            || matches!(self.frozen_scope.target, ScopeTarget::TrainingRun(_))
            || self.frozen_scope.resolved_accounts.is_empty()
        {
            self.frozen_scope = svc.resolve_and_validate_scope(&self.frozen_scope)?;
            let rep = self
                .frozen_scope
                .report_currency
                .trim()
                .to_ascii_uppercase();
            if !rep.is_empty() {
                self.authorized_currencies.insert(rep);
            }
        }

        if matches!(self.frozen_scope.target, ScopeTarget::TrainingRun(_)) {
            if !is_training_tool {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    format!(
                        "tool '{trimmed_tool}' is forbidden inside simulated training scope (real-account ledger tools are isolated)"
                    ),
                    false,
                ));
            }
        } else if !is_account_tool {
            return Err(ErrorEnvelope::new(
                "SCOPE_DENIED",
                format!("tool '{trimmed_tool}' is only permitted for ScopeTarget::TrainingRun"),
                false,
            ));
        }

        // Enforce strict scope boundary on tool arguments: model or prompt-injected text can NEVER widen scope
        self.validate_tool_arguments_scope(args)?;
        validate_tool_schema(trimmed_tool, args)?;

        let canonical_args = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
        let mut hasher = Sha256::new();
        hasher.update(format!("{trimmed_tool}:{canonical_args}").as_bytes());
        let args_hash = hex::encode(hasher.finalize());

        // Check idempotent `(run_id, call_id)` before executing
        let existing: Option<(String, Option<String>, String)> = svc
            .store()
            .conn()
            .query_row(
                "SELECT args_hash, result_json, status FROM ai_tool_call WHERE run_id = ?1 AND call_id = ?2",
                params![self.run_id, call_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| ErrorEnvelope::new("TOOL_CALL_LOOKUP_FAILED", e.to_string(), false))?;

        if let Some((prev_hash, prev_json_str, status)) = existing {
            if prev_hash != args_hash {
                return Err(ErrorEnvelope::new(
                    "DUPLICATE_TOOL_CALL_MISMATCH",
                    format!("duplicate tool call_id '{call_id}' with mismatched arguments"),
                    false,
                ));
            }
            if status != "succeeded" {
                return Err(ErrorEnvelope::new(
                    "TOOL_PREVIOUSLY_FAILED",
                    "the earlier tool call did not succeed",
                    false,
                ));
            }
            let prev_json_str = prev_json_str.ok_or_else(|| {
                ErrorEnvelope::new(
                    "INTERRUPTED_TOOL_CALL",
                    "an incomplete tool call cannot be replayed as succeeded",
                    false,
                )
            })?;
            let env_val: Value = serde_json::from_str(&prev_json_str)
                .map_err(|e| ErrorEnvelope::new("TOOL_RECEIPT_CORRUPT", e.to_string(), false))?;
            let receipt = self.ingest_envelope_value(call_id, trimmed_tool, env_val)?;
            return Ok(receipt);
        }

        if self.executed_calls >= 12 {
            return Err(ErrorEnvelope::new(
                "BUDGET_EXCEEDED",
                "run tool budget exhausted",
                false,
            ));
        }
        let source = snapshot.unwrap_or(&*svc);
        if snapshot.is_some()
            && svc.scope_journal_revision(&self.frozen_scope.target)?
                != self.frozen_scope.journal_revision
        {
            return Err(ErrorEnvelope::new(
                "SCOPE_DENIED",
                "journal content or permissions changed during analysis",
                false,
            ));
        }
        let mut envelope_json: Value = match trimmed_tool {
            "scope.describe" => {
                let env =
                    source.query_scope_describe(&self.frozen_scope, request_id, self.generation)?;
                serde_json::to_value(env).unwrap()
            }
            "training.run_summary" => {
                let env = source.query_training_ai_run_summary(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "training.orders_and_fills" => {
                let env = source.query_training_ai_orders_and_fills(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "training.report" => {
                let env = source.query_training_ai_report(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "training.notes" => {
                let env = source.query_training_ai_notes(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "training.revealed_bars" => {
                let env = source.query_training_ai_revealed_bars(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "report.period_breakdown" => {
                let mut scope = self.frozen_scope.clone();
                for (key, is_start) in [("start_date", true), ("end_date", false)] {
                    if let Some(date) = args.get(key).and_then(Value::as_str) {
                        let date =
                            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| {
                                ErrorEnvelope::new("INVALID_ARGUMENT", "invalid period date", false)
                            })?;
                        if date > self.frozen_scope.as_of_date
                            || self
                                .frozen_scope
                                .start_date
                                .is_some_and(|start| date < start)
                        {
                            return Err(ErrorEnvelope::new(
                                "SCOPE_DENIED",
                                "period exceeds frozen scope",
                                false,
                            ));
                        }
                        if is_start {
                            scope.start_date = Some(date);
                        } else {
                            scope.as_of_date = date;
                        }
                    }
                }
                let env = source.query_period_breakdown(&scope, request_id, self.generation)?;
                serde_json::to_value(env).unwrap()
            }
            "ledger.list_transactions" => {
                let instrument_id = args
                    .get("instrument_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let event_type = args
                    .get("event_type")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| (n as usize).clamp(1, self.max_detail_rows))
                    .unwrap_or(self.max_detail_rows);
                let offset = args
                    .get("offset")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(0);
                let filter = TransactionFilterDto {
                    event_id: None,
                    instrument_id,
                    event_type,
                    limit,
                    offset,
                };
                let mut env = source.query_transactions(
                    &self.frozen_scope,
                    &filter,
                    request_id,
                    self.generation,
                )?;
                for row in &mut env.value {
                    row.raw_payload = None;
                }
                serde_json::to_value(env).unwrap()
            }
            "ledger.holdings_lots" => {
                let env =
                    source.query_holdings_lots(&self.frozen_scope, request_id, self.generation)?;
                serde_json::to_value(env).unwrap()
            }
            "reconcile.list_issues" => {
                let env = source.query_reconciliation_issues(
                    &self.frozen_scope,
                    request_id,
                    self.generation,
                )?;
                serde_json::to_value(env).unwrap()
            }
            "data.quality_summary" => {
                let env =
                    source.query_data_quality(&self.frozen_scope, request_id, self.generation)?;
                serde_json::to_value(env).unwrap()
            }
            "journal.search_notes" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                // Always enforce `only_model_accessible = true` and `frozen_scope.include_notes`
                let mut env = source.query_notes(
                    &self.frozen_scope,
                    query,
                    true,
                    request_id,
                    self.generation,
                )?;
                for note in &mut env.value {
                    note.attachments.clear();
                }
                serde_json::to_value(env).unwrap()
            }
            _ => unreachable!(),
        };

        limit_tool_details(trimmed_tool, &mut envelope_json, self.max_detail_rows)?;

        if snapshot.is_some() {
            persist_snapshot_report_objects(svc, source, &envelope_json)?;
        }
        let result_json_str =
            serde_json::to_string(&envelope_json).unwrap_or_else(|_| "{}".to_string());
        let tc_record = AiToolCallRecord {
            id: format!("TC:{}:{}", self.run_id, call_id),
            run_id: self.run_id.clone(),
            call_id: call_id.to_string(),
            args_hash,
            tool_name: trimmed_tool.to_string(),
            args_json: canonical_args,
            result_json: Some(result_json_str),
            status: "succeeded".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        svc.store_mut()
            .record_ai_tool_call(&tc_record)
            .map_err(|e| ErrorEnvelope::new("RECORD_TOOL_CALL_FAILED", e.to_string(), false))?;

        self.executed_calls += 1;
        self.ingest_envelope_value(call_id, trimmed_tool, envelope_json)
    }

    fn validate_tool_arguments_scope(&self, args: &Value) -> Result<(), ErrorEnvelope> {
        let Some(obj) = args.as_object() else {
            return Ok(());
        };
        let allowed: BTreeSet<&str> = self
            .frozen_scope
            .resolved_accounts
            .iter()
            .map(String::as_str)
            .collect();

        for key in ["account_id", "account", "portfolio_id", "scope"] {
            if let Some(val) = obj.get(key) {
                if let Some(acc_str) = val.as_str() {
                    if !allowed.contains(acc_str.trim()) {
                        return Err(ErrorEnvelope::new(
                            "SCOPE_DENIED",
                            format!(
                                "tool argument '{key}={acc_str}' attempts to widen frozen scope"
                            ),
                            false,
                        ));
                    }
                } else {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        format!("tool argument '{key}' attempts to override frozen scope"),
                        false,
                    ));
                }
            }
        }

        if let Some(arr) = obj.get("accounts").and_then(Value::as_array) {
            for item in arr {
                let Some(acc_str) = item.as_str() else {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        "invalid account item in tool arguments",
                        false,
                    ));
                };
                if !allowed.contains(acc_str.trim()) {
                    return Err(ErrorEnvelope::new(
                        "SCOPE_DENIED",
                        format!("tool argument account '{acc_str}' is outside frozen scope"),
                        false,
                    ));
                }
            }
        }

        if let Some(val) = obj.get("run_id") {
            let matches_training_run = match (&self.frozen_scope.target, val.as_str()) {
                (ScopeTarget::TrainingRun(expected), Some(actual)) => {
                    actual.trim() == expected.as_str()
                }
                _ => false,
            };
            if !matches_training_run {
                return Err(ErrorEnvelope::new(
                    "SCOPE_DENIED",
                    "tool argument 'run_id' attempts to cross frozen training run scope",
                    false,
                ));
            }
        }
        Ok(())
    }

    pub(super) fn ingest_envelope_value(
        &mut self,
        call_id: &str,
        tool_name: &str,
        envelope_json: Value,
    ) -> Result<ToolExecutionReceipt, ErrorEnvelope> {
        let result_id = envelope_json
            .get("result_id")
            .and_then(Value::as_str)
            .unwrap_or("RES:UNKNOWN")
            .to_string();

        self.valid_evidence_map.insert(
            result_id.clone(),
            EvidenceRef {
                kind: "analysis_result".to_string(),
                id: result_id.clone(),
                revision: self.frozen_scope.ledger_revision,
                label: format!("Tool result {tool_name} ({result_id})"),
            },
        );

        let mut evidence_refs = Vec::new();
        if let Some(ev_arr) = envelope_json.get("evidence_refs").and_then(Value::as_array) {
            for item in ev_arr {
                if let Ok(ev_ref) = serde_json::from_value::<EvidenceRef>(item.clone()) {
                    self.valid_evidence_map
                        .insert(ev_ref.id.clone(), ev_ref.clone());
                    evidence_refs.push(ev_ref);
                }
            }
        }

        // Harvest numbers and structured currency codes from `value` (excluding untrusted
        // free-text note markdown so prompt-injected numbers/currencies in notes cannot self-authorize!)
        if let Some(val_node) = envelope_json.get("value") {
            if tool_name == "journal.search_notes" {
                // Only harvest note revision integers, never free-text numbers inside note bodies!
                if let Some(notes_arr) = val_node.as_array() {
                    for note_obj in notes_arr {
                        if let Some(rev) = note_obj.get("current_revision").and_then(Value::as_i64)
                        {
                            self.authorized_numbers.insert(rev.to_string());
                        }
                    }
                }
            } else if tool_name == "training.notes" {
                // Never harvest free-text numbers from `rationale` or `emotion_tag` so prompt-injected numbers cannot self-authorize (`M4-A-14`)!
            } else {
                harvest_json_numbers(val_node, &mut self.authorized_numbers);
                harvest_json_currencies(val_node, &mut self.authorized_currencies);
            }
        }

        Ok(ToolExecutionReceipt {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            result_id,
            envelope_json,
            evidence_refs,
        })
    }
}

pub(super) fn limit_tool_details(
    capability: &str,
    envelope: &mut Value,
    limit: usize,
) -> Result<(), ErrorEnvelope> {
    let mut remaining = limit;
    let mut truncated = false;
    if let Some(value) = envelope.get_mut("value") {
        let arrays: Vec<&mut Value> = match value {
            Value::Array(_) => vec![value],
            Value::Object(map) => map
                .iter_mut()
                .filter(|(key, value)| {
                    matches!(
                        key.as_str(),
                        "cash_balances"
                            | "holdings"
                            | "open_lots"
                            | "issues"
                            | "checkpoints"
                            | "orders"
                            | "fills"
                            | "post_finish_bars"
                    ) && value.is_array()
                })
                .map(|(_, value)| value)
                .collect(),
            _ => vec![],
        };
        for value in arrays {
            if let Some(rows) = value.as_array_mut() {
                let retained = rows.len().min(remaining);
                truncated |= rows.len() > retained;
                rows.truncate(retained);
                remaining -= retained;
            }
        }
    }
    if truncated {
        envelope["quality"] = json!("partial");
        if let Some(warnings) = envelope["warnings"].as_array_mut() {
            warnings.push(json!(format!(
                "工具明细受预算限制，仅返回至多 {limit} 行；汇总计算口径不变"
            )));
        }
        let mut retained_ids = BTreeSet::new();
        collect_object_ids(&envelope["value"], &mut retained_ids);
        if let Some(references) = envelope["evidence_refs"].as_array_mut() {
            references.retain(|reference| {
                matches!(
                    reference["kind"].as_str(),
                    Some(
                        "analysis_report"
                            | "dataset_version"
                            | "ledger_revision"
                            | "fee_profile_version"
                    )
                ) || reference["id"]
                    .as_str()
                    .is_some_and(|id| retained_ids.contains(id))
            });
        }
        let scope: AnalysisScope = serde_json::from_value(envelope["scope"].clone())
            .map_err(|e| ErrorEnvelope::new("TOOL_RECEIPT_CORRUPT", e.to_string(), false))?;
        let scope_hash = scope.scope_hash();
        let value_json = serde_json::to_string(&envelope["value"])
            .map_err(|e| ErrorEnvelope::new("TOOL_RECEIPT_CORRUPT", e.to_string(), false))?;
        let version = envelope["calculation_version"].as_str().ok_or_else(|| {
            ErrorEnvelope::new("TOOL_RECEIPT_CORRUPT", "missing calculation version", false)
        })?;
        let digest = hex::encode(Sha256::digest(
            format!("{capability}:{version}:{scope_hash}:{value_json}").as_bytes(),
        ));
        envelope["result_id"] = json!(format!("RES:{}:{}", &scope_hash[..12], &digest[..16]));
    }
    Ok(())
}

fn collect_object_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if (key == "id" || key.ends_with("_id")) && value.is_string() {
                    out.insert(value.as_str().unwrap().to_string());
                }
                collect_object_ids(value, out);
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_object_ids(value, out);
            }
        }
        _ => {}
    }
}

fn persist_snapshot_report_objects(
    target: &AppService,
    source: &AppService,
    envelope: &Value,
) -> Result<(), ErrorEnvelope> {
    for reference in envelope["evidence_refs"].as_array().into_iter().flatten() {
        if reference["kind"] != "analysis_report" {
            continue;
        }
        let id = reference["id"].as_str().ok_or_else(|| {
            ErrorEnvelope::new("TOOL_RECEIPT_CORRUPT", "report reference has no ID", false)
        })?;
        let record: (String, String, i64, i64, String, String, String, String) = source
            .store()
            .conn()
            .query_row(
                "SELECT report_type,scope_json,ledger_revision,dataset_version,calculation_version,payload_json,status,created_at FROM analysis_report WHERE id=?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .map_err(|e| ErrorEnvelope::new("TOOL_RECEIPT_SAVE_FAILED", e.to_string(), true))?;
        target
            .store()
            .conn()
            .execute(
                "INSERT OR REPLACE INTO analysis_report(id,report_type,scope_json,ledger_revision,dataset_version,calculation_version,payload_json,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    id, record.0, record.1, record.2, record.3, record.4, record.5, record.6,
                    record.7
                ],
            )
            .map_err(|e| ErrorEnvelope::new("TOOL_RECEIPT_SAVE_FAILED", e.to_string(), true))?;
    }
    Ok(())
}

pub(super) fn harvest_json_numbers(node: &Value, out: &mut BTreeSet<String>) {
    match node {
        Value::Number(n) => {
            if let Ok(dec) = parse_decimal_lossless(&n.to_string()) {
                out.insert(to_canonical_text(dec));
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty()
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                && trimmed.chars().any(|c| c.is_ascii_digit())
                && !looks_like_iso_date(trimmed)
            {
                if let Ok(dec) = parse_decimal_lossless(trimmed) {
                    out.insert(to_canonical_text(dec));
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                harvest_json_numbers(v, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                // Skip dates/timestamps/IDs/hashes/sequence counters
                if matches!(
                    k.as_str(),
                    "id" | "event_id"
                        | "report_id"
                        | "result_id"
                        | "request_id"
                        | "run_id"
                        | "parent_run_id"
                        | "order_id"
                        | "parent_order_id"
                        | "oco_peer_order_id"
                        | "linked_order_id"
                        | "reservation_id"
                        | "fill_id"
                        | "note_id"
                        | "archived_journal_note_id"
                        | "group_id"
                        | "parent_group_id"
                        | "fee_profile_version_id"
                        | "fee_profile_id"
                        | "state_hash"
                        | "latest_state_hash"
                        | "initial_or_checkpoint_hash"
                        | "candidate_pool_hash"
                        | "bar_index"
                        | "display_bar_index"
                        | "entry_bar_index"
                        | "exit_bar_index"
                        | "submitted_bar_index"
                        | "step_index"
                        | "current_step_index"
                        | "order_seq"
                        | "fill_seq"
                        | "seq"
                        | "latest_seq"
                        | "started_from_seq"
                        | "redraw_count"
                        | "redraw_index"
                        | "seed"
                        | "priority"
                        | "self_evaluation"
                        | "ledger_revision"
                        | "revision"
                        | "generation"
                        | "schema_version"
                        | "dataset_version"
                        | "occurred_at"
                        | "created_at"
                        | "updated_at"
                        | "saved_at"
                        | "finished_at"
                        | "submitted_at"
                        | "bar_close_at"
                        | "visible_until"
                        | "entry_at"
                        | "exit_at"
                        | "open_at"
                        | "close_at"
                        | "session_date"
                        | "entry_session_date"
                        | "exit_session_date"
                        | "bar_session_date"
                        | "as_of"
                        | "as_of_date"
                        | "start_date"
                        | "end_date"
                        | "scope_hash"
                ) {
                    continue;
                }
                harvest_json_numbers(v, out);
            }
        }
        _ => {}
    }
}

fn harvest_json_currencies(node: &Value, out: &mut BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "currency"
                        | "quote_currency"
                        | "fee_currency"
                        | "report_currency"
                        | "book_currency"
                        | "base_currency"
                ) {
                    if let Some(code) = v.as_str() {
                        insert_currency_code(code, out);
                    }
                } else if k == "cash_balances" {
                    if let Some(entries) = v.as_array() {
                        for entry in entries {
                            if let Some(code) = entry
                                .as_array()
                                .and_then(|tuple| tuple.get(1))
                                .and_then(Value::as_str)
                            {
                                insert_currency_code(code, out);
                            }
                        }
                    }
                } else {
                    harvest_json_currencies(v, out);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                harvest_json_currencies(item, out);
            }
        }
        _ => {}
    }
}

fn insert_currency_code(raw: &str, out: &mut BTreeSet<String>) {
    let trimmed = raw.trim();
    if (2..=10).contains(&trimmed.len()) && trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
        out.insert(trimmed.to_ascii_uppercase());
    }
}

fn validate_tool_schema(tool: &str, args: &Value) -> Result<(), ErrorEnvelope> {
    let invalid = || {
        ErrorEnvelope::new(
            "INVALID_ARGUMENT",
            "tool arguments do not match the registered schema",
            false,
        )
    };
    let object = args.as_object().ok_or_else(invalid)?;
    let catalog = CapabilityCatalog::default_catalog();
    let schema = &catalog
        .capabilities
        .iter()
        .find(|c| c.id == tool)
        .ok_or_else(invalid)?
        .parameters_schema;
    let properties = schema["properties"].as_object().ok_or_else(invalid)?;
    for (key, value) in object {
        let rule = properties.get(key).ok_or_else(invalid)?;
        match rule["type"].as_str() {
            Some("string")
                if value.is_string() && value.as_str().is_some_and(|s| s.len() <= 4096) => {}
            Some("integer")
                if value.as_u64().is_some_and(|n| {
                    rule["minimum"].as_u64().is_none_or(|min| n >= min)
                        && rule["maximum"].as_u64().is_none_or(|max| n <= max)
                }) => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

pub(super) fn looks_like_iso_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}
