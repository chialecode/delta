//! Read-only analysis tool gateway (AC-19/23/25).
//!
//! Only the six whitelisted business tools are registered. The gateway
//! independently re-validates name, schema, scope binding, generation and
//! budget before executing against the same application services the UI uses.

use crate::ai::session::RunBudget;
use crate::contracts::{AppError, CallContext, ResultEnvelope};
use delta_core::{ScopeFault, ScopeSnapshot};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The S1 read-only tool whitelist (ai-design.md §3).
pub const TOOL_NAMES: [&str; 6] = [
    "get_data_quality",
    "get_portfolio_summary",
    "explain_pnl",
    "query_trades",
    "search_journal",
    "get_chart_window",
];

/// Tool description + JSON parameters schema, projected to the model.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// Deterministic tool catalog hash binds runs to a known schema set.
pub fn tool_catalog() -> Vec<ToolSpec> {
    let scope_schema = json!({
        "type": "object",
        "properties": {
            "account_ids": {"type": "array", "items": {"type": "string"}},
            "start_at": {"type": "string"},
            "end_at": {"type": "string"},
            "reporting_currency": {"type": "string"}
        },
        "required": ["account_ids", "start_at", "end_at", "reporting_currency"],
        "additionalProperties": false
    });
    vec![
        ToolSpec {
            name: "get_data_quality",
            description:
                "List missing costs, market data, FX rates and reconciliation issues for the scope.",
            parameters: json!({
                "type": "object",
                "properties": {"scope": scope_schema},
                "required": ["scope"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "get_portfolio_summary",
            description: "Net worth, positions and valuation quality as of a date.",
            parameters: json!({
                "type": "object",
                "properties": {"scope": scope_schema, "as_of": {"type": "string"}},
                "required": ["scope", "as_of"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "explain_pnl",
            description: "Period PnL decomposition: flows, realized, unrealized, fees, income.",
            parameters: json!({
                "type": "object",
                "properties": {"scope": scope_schema, "period_start": {"type": "string"}, "period_end": {"type": "string"}},
                "required": ["scope", "period_start", "period_end"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "query_trades",
            description: "Paged fills within the scope with allowed filters.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "scope": scope_schema,
                    "instrument": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                    "cursor": {"type": ["string", "null"]}
                },
                "required": ["scope", "limit"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "search_journal",
            description: "Search authorized journal entries by keywords/tags within the scope.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50}
                },
                "required": ["query", "limit"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "get_chart_window",
            description: "Bounded OHLCV window for one instrument with source and quality.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "instrument": {"type": "string"},
                    "start_at": {"type": "string"},
                    "end_at": {"type": "string"},
                    "timeframe": {"type": "string", "enum": ["1d"]}
                },
                "required": ["instrument", "start_at", "end_at", "timeframe"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn tool_schema_hash() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for t in tool_catalog() {
        t.name.hash(&mut h);
        t.parameters.to_string().hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Host-side execution of validated tool calls. Implementations must enforce
/// scope themselves too — the gateway binds scope_ref to the run snapshot.
pub trait AnalysisHost: Send + Sync {
    fn execute(
        &self,
        tool: &str,
        ctx: &CallContext,
        args: &Value,
    ) -> Result<ResultEnvelope<Value>, AppError>;
}

fn hash_args(args: &Value) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    args.to_string().hash(&mut h);
    format!("{:016x}", h.finish())
}

struct Receipt {
    args_hash: String,
    evidence_refs: Vec<String>,
    value: Value,
    quality: String,
}

/// Per-run gateway instance: validates then executes read-only tools.
pub struct ToolGateway {
    run_id: String,
    generation: u64,
    scope: ScopeSnapshot,
    budget: RunBudget,
    calls: AtomicU64,
    receipts: Mutex<HashMap<String, Receipt>>,
}

impl ToolGateway {
    pub fn new(run_id: &str, generation: u64, scope: ScopeSnapshot, budget: RunBudget) -> Self {
        Self {
            run_id: run_id.into(),
            generation,
            scope,
            budget,
            calls: AtomicU64::new(0),
            receipts: Mutex::new(HashMap::new()),
        }
    }

    pub fn scope(&self) -> &ScopeSnapshot {
        &self.scope
    }

    pub fn calls_used(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Validate (whitelist, generation, schema, budget, dedup) and execute.
    pub fn execute(
        &self,
        host: &dyn AnalysisHost,
        ctx: &CallContext,
        call_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<ResultEnvelope<Value>, AppError> {
        // 1. Whitelist.
        if !TOOL_NAMES.contains(&tool) {
            return Err(AppError::UnsupportedCapability(format!(
                "tool {tool:?} is not registered"
            )));
        }
        // 2. Run/generation binding: the model cannot escalate by guessing ids.
        if ctx.run_id.as_deref() != Some(self.run_id.as_str())
            || ctx.generation != Some(self.generation)
        {
            return Err(AppError::StaleGeneration(format!(
                "run {} generation {}",
                self.run_id, self.generation
            )));
        }
        if ctx.scope_ref != self.scope.scope_ref {
            return Err(AppError::ScopeDenied);
        }
        // 3. Budget.
        let used = self.calls.fetch_add(1, Ordering::SeqCst);
        if used >= self.budget.max_tool_calls {
            return Err(AppError::InvalidArgument(format!(
                "tool call budget exhausted ({})",
                self.budget.max_tool_calls
            )));
        }
        // 4. Schema validation (independent of the model's claim).
        validate_args(tool, args)?;
        // 4b. Frozen scope: model-supplied accounts, period and currency must
        // sit inside the run snapshot. The host repeats this check.
        match self.scope.check_tool_args(tool, args) {
            Ok(()) => {}
            Err(ScopeFault::Denied(_)) => return Err(AppError::ScopeDenied),
            Err(ScopeFault::Invalid(msg)) => return Err(AppError::InvalidArgument(msg)),
        }
        // 5. Dedup: same (run, call) with same args replays the receipt; with
        // different args it is rejected (context-state-management §3).
        let args_hash = hash_args(args);
        {
            let receipts = self.receipts.lock().expect("receipts lock");
            if let Some(existing) = receipts.get(call_id) {
                if existing.args_hash == args_hash {
                    let mut envelope: ResultEnvelope<Value> =
                        ResultEnvelope::new(&ctx.request_id, existing.value.clone());
                    envelope.evidence_refs = existing.evidence_refs.clone();
                    envelope.quality = existing.quality.clone();
                    return Ok(envelope);
                }
                return Err(AppError::RevisionConflict(format!(
                    "call id {call_id} reused with different arguments"
                )));
            }
        }
        // 6. Execute against the shared host (read-only).
        let envelope = host.execute(tool, ctx, args)?;
        let mut receipts = self.receipts.lock().expect("receipts lock");
        receipts.insert(
            call_id.to_string(),
            Receipt {
                args_hash,
                evidence_refs: envelope.evidence_refs.clone(),
                value: envelope.value.clone(),
                quality: envelope.quality.clone(),
            },
        );
        Ok(envelope)
    }

    /// All receipts of this run (used for reference validation).
    pub fn receipt_evidence(&self) -> Vec<String> {
        let receipts = self.receipts.lock().expect("receipts lock");
        receipts
            .values()
            .flat_map(|r| r.evidence_refs.iter().cloned())
            .collect()
    }

    /// Decimal strings that appeared in tool results. Report claims must
    /// match one of these; unknown is not coerced to zero.
    pub fn receipt_numbers(&self) -> Vec<String> {
        let receipts = self.receipts.lock().expect("receipts lock");
        let mut out = Vec::new();
        for r in receipts.values() {
            collect_decimals(&r.value, &mut out);
        }
        out
    }

    pub fn any_insufficient(&self) -> bool {
        let receipts = self.receipts.lock().expect("receipts lock");
        receipts.values().any(|r| r.quality != "complete")
    }
}

fn collect_decimals(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if let Ok(d) = s.parse::<rust_decimal::Decimal>() {
                out.push(d.normalize().to_string());
            }
        }
        Value::Array(items) => items.iter().for_each(|c| collect_decimals(c, out)),
        Value::Object(map) => map.values().for_each(|c| collect_decimals(c, out)),
        _ => {}
    }
}

/// Structural validation of tool arguments against the catalog schemas
/// (required fields, types, bounds, enums, no additional properties),
/// applied recursively so nested objects such as `scope` are checked too.
fn validate_args(tool: &str, args: &Value) -> Result<(), AppError> {
    let catalog = tool_catalog();
    let spec = catalog
        .iter()
        .find(|t| t.name == tool)
        .expect("whitelisted tool has a spec");
    if !args.is_object() {
        return Err(AppError::InvalidArgument(
            "arguments must be an object".into(),
        ));
    }
    check_value(tool, "arguments", args, &spec.parameters)
}

fn invalid(tool: &str, path: &str, what: impl std::fmt::Display) -> AppError {
    AppError::InvalidArgument(format!("argument {path:?} of {tool} {what}"))
}

/// Validate `value` against the JSON-schema subset used by the catalog:
/// `type` (single or union), `properties`/`required`/`additionalProperties`,
/// `items`, `minimum`/`maximum` and `enum`. Unknown keywords are rejected
/// at catalog level by `catalog_uses_supported_keywords` in tests.
fn check_value(tool: &str, path: &str, value: &Value, schema: &Value) -> Result<(), AppError> {
    if let Some(kind) = schema.get("type") {
        let allowed: Vec<&str> = match kind {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(|t| t.as_str()).collect(),
            _ => Vec::new(),
        };
        if !allowed.iter().any(|t| type_matches(t, value)) {
            return Err(invalid(
                tool,
                path,
                format!("must be {}", allowed.join(" or ")),
            ));
        }
    }
    if let Some(enum_values) = schema.get("enum").and_then(|v| v.as_array()) {
        if !enum_values.contains(value) {
            return Err(invalid(tool, path, "is not an allowed value"));
        }
    }
    if let Some(n) = value.as_f64().filter(|_| value.is_number()) {
        if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
            if n < min {
                return Err(invalid(tool, path, format!("is below minimum {min}")));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(|v| v.as_f64()) {
            if n > max {
                return Err(invalid(tool, path, format!("is above maximum {max}")));
            }
        }
    }
    if let Some(obj) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            for key in required.iter().filter_map(|k| k.as_str()) {
                if !obj.contains_key(key) {
                    return Err(invalid(tool, &format!("{path}.{key}"), "is required"));
                }
            }
        }
        let props = schema.get("properties").and_then(|p| p.as_object());
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (key, v) in obj {
            let child = format!("{path}.{key}");
            match props.and_then(|p| p.get(key)) {
                Some(pspec) => check_value(tool, &child, v, pspec)?,
                None if closed => return Err(invalid(tool, &child, "is not allowed")),
                None => {}
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            check_value(tool, &format!("{path}[{i}]"), item, item_schema)?;
        }
    }
    Ok(())
}

fn type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Value {
        json!({
            "account_ids": ["a"],
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-02-01T00:00:00Z",
            "reporting_currency": "USD"
        })
    }

    #[test]
    fn r1_a_15_valid_arguments_pass() {
        validate_args(
            "query_trades",
            &json!({"scope": scope(), "limit": 10, "cursor": null}),
        )
        .unwrap();
        validate_args(
            "query_trades",
            &json!({"scope": scope(), "limit": 10, "cursor": "c1"}),
        )
        .unwrap();
    }

    #[test]
    fn r1_a_15_bounds_types_and_nested_scope_are_enforced() {
        let cases = [
            json!({"scope": scope(), "limit": 9999}),
            json!({"scope": scope(), "limit": 0}),
            json!({"scope": scope(), "limit": "10"}),
            json!({"scope": scope(), "limit": 1.5}),
            json!({"scope": scope(), "limit": 10, "cursor": 3}),
            json!({"scope": "a", "limit": 10}),
            json!({"scope": {"account_ids": ["a"]}, "limit": 10}),
            json!({"scope": {"account_ids": [1], "start_at": "x", "end_at": "y", "reporting_currency": "USD"}, "limit": 10}),
            json!({"scope": {"account_ids": ["a"], "start_at": "x", "end_at": "y", "reporting_currency": "USD", "all": true}, "limit": 10}),
            json!({"scope": scope(), "limit": 10, "extra": 1}),
        ];
        for args in cases {
            let err = validate_args("query_trades", &args).unwrap_err();
            assert!(matches!(err, AppError::InvalidArgument(_)), "{args}");
        }
        let err = validate_args(
            "get_chart_window",
            &json!({"instrument": "X", "start_at": "a", "end_at": "b", "timeframe": "1m"}),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidArgument(_)));
        let err = validate_args(
            "search_journal",
            &json!({"query": "q", "limit": 5, "tags": ["ok", 2]}),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidArgument(_)));
    }

    #[test]
    fn catalog_uses_supported_keywords() {
        const KNOWN: [&str; 8] = [
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "minimum",
            "maximum",
            "enum",
        ];
        fn walk(v: &Value) {
            if let Some(o) = v.as_object() {
                for (k, child) in o {
                    assert!(KNOWN.contains(&k.as_str()), "unsupported keyword {k}");
                    match k.as_str() {
                        "properties" => child.as_object().unwrap().values().for_each(walk),
                        "items" => walk(child),
                        _ => {}
                    }
                }
            }
        }
        for t in tool_catalog() {
            walk(&t.parameters);
        }
    }
}
