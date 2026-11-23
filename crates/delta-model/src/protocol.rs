use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Explicit wire protocol for a model connection (S-19).
/// Unknown protocols are rejected without automatic fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ModelProtocol {
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-chat-completions")]
    OpenAiChatCompletions,
}

impl ModelProtocol {
    /// Parses a protocol identifier string and rejects any unknown value with
    /// [`ModelErrorCode::UnsupportedProtocol`].
    pub fn parse(raw: &str) -> Result<Self, ModelError> {
        match raw.trim() {
            "openai-responses" => Ok(Self::OpenAiResponses),
            "openai-chat-completions" => Ok(Self::OpenAiChatCompletions),
            other => Err(ModelError::new(
                ModelErrorCode::UnsupportedProtocol,
                false,
                format!(
                    "Unsupported model protocol '{}'; expected 'openai-responses' or 'openai-chat-completions'",
                    other
                ),
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiChatCompletions => "openai-chat-completions",
        }
    }

    pub fn endpoint_path(&self) -> &'static str {
        match self {
            Self::OpenAiResponses => "/responses",
            Self::OpenAiChatCompletions => "/chat/completions",
        }
    }
}

impl fmt::Display for ModelProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelProtocol {
    type Err = ModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Per-model capability flags verified or declared for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_streaming: true,
            supports_tools: true,
            supports_reasoning: true,
        }
    }
}

/// Connection, stream idle, and total request timeout configuration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTimeouts {
    pub connect_ms: u64,
    pub idle_ms: u64,
    pub total_ms: u64,
}

impl Default for ModelTimeouts {
    fn default() -> Self {
        Self {
            connect_ms: 10_000,
            idle_ms: 30_000,
            total_ms: 120_000,
        }
    }
}

/// Retry policy before any application-visible streaming output is emitted (S-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_retries_before_visible_output: usize,
    pub base_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries_before_visible_output: 2,
            base_backoff_ms: 50,
        }
    }
}

/// Hard resource and safety ceilings for model streaming and tool execution (S-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBudget {
    pub max_sse_record_bytes: usize,
    pub max_tool_calls_per_run: usize,
    pub max_run_duration_ms: u64,
    pub max_tool_detail_rows: usize,
    pub max_output_tokens: u32,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self {
            max_sse_record_bytes: 1024 * 1024,
            max_tool_calls_per_run: 12,
            max_run_duration_ms: 120_000,
            max_tool_detail_rows: 100,
            max_output_tokens: 4096,
        }
    }
}

/// Complete model endpoint configuration and runtime guardrails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConnection {
    pub id: String,
    pub protocol: ModelProtocol,
    pub base_url: String,
    pub model_id: String,
    pub credential_ref: Option<String>,
    pub context_window: u32,
    pub capabilities: ModelCapabilities,
    pub timeouts: ModelTimeouts,
    pub retry_policy: RetryPolicy,
    pub budget: ModelBudget,
}

impl ModelConnection {
    /// Creates a loopback test/dev connection with standard S-19 budgets and timeouts.
    pub fn loopback(
        id: impl Into<String>,
        protocol: ModelProtocol,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            protocol,
            base_url: base_url.into(),
            model_id: model_id.into(),
            credential_ref: None,
            context_window: 128_000,
            capabilities: ModelCapabilities::default(),
            timeouts: ModelTimeouts::default(),
            retry_policy: RetryPolicy::default(),
            budget: ModelBudget::default(),
        }
    }

    /// Validates endpoint URL safety, scheme rules, and path joining:
    /// - Rejects empty model IDs or zero context windows.
    /// - Rejects embedded URL credentials (`user:pass@host`) or query secrets.
    /// - Requires `https` for remote endpoints, allowing `http` only for `127.0.0.1` or `localhost`.
    pub fn validate_endpoint(&self) -> Result<(), ModelError> {
        let _ = self.resolved_endpoint_url()?;
        Ok(())
    }

    /// Validates the connection and returns the joined protocol endpoint URL.
    pub fn resolved_endpoint_url(&self) -> Result<String, ModelError> {
        if self.id.trim().is_empty() {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "connection id must not be empty",
            ));
        }
        if self.model_id.trim().is_empty() {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "model_id must not be empty",
            ));
        }
        if self.context_window == 0 {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "context_window must be greater than zero",
            ));
        }

        let parsed = reqwest::Url::parse(self.base_url.trim()).map_err(|_| {
            ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "base_url is not a valid absolute URL",
            )
        })?;

        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "base_url must not contain embedded credentials",
            ));
        }

        if parsed.fragment().is_some() {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "base_url must not contain URL fragments",
            ));
        }

        if let Some(query) = parsed.query() {
            let lower = query.to_ascii_lowercase();
            if lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.contains("key=")
                || lower.contains("token=")
                || lower.contains("secret=")
            {
                return Err(ModelError::new(
                    ModelErrorCode::InvalidEndpoint,
                    false,
                    "base_url must not embed API keys or tokens in query parameters",
                ));
            }
        }

        let host = parsed.host_str().ok_or_else(|| {
            ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "base_url must specify a valid host",
            )
        })?;

        match parsed.scheme() {
            "https" => {}
            "http" if host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") => {}
            "http" => {
                return Err(ModelError::new(
                    ModelErrorCode::InvalidEndpoint,
                    false,
                    format!(
                        "plain HTTP is only allowed for loopback hosts (127.0.0.1 or localhost), got '{}'",
                        host
                    ),
                ));
            }
            other => {
                return Err(ModelError::new(
                    ModelErrorCode::InvalidEndpoint,
                    false,
                    format!("unsupported URL scheme '{}'", other),
                ));
            }
        }

        // Join the protocol path onto the parsed URL path so query parameters (e.g. Azure
        // `?api-version=...`) are preserved instead of being mangled by raw string concatenation.
        let mut joined_url = parsed.clone();
        let base_path = parsed.path().trim_end_matches('/');
        let new_path = format!("{base_path}{}", self.protocol.endpoint_path());
        joined_url.set_path(&new_path);

        if joined_url.host_str() != Some(host) || joined_url.scheme() != parsed.scheme() {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "endpoint path joining altered target host or scheme",
            ));
        }

        Ok(joined_url.to_string())
    }
}

/// Typed input item preserved in conversation history without flattening structured tool turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputItem {
    SystemMessage {
        content: String,
    },
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments_json: String,
    },
    ToolOutput {
        call_id: String,
        output_json: String,
        is_error: bool,
    },
    ReasoningOpaque {
        provider_id: String,
        protocol: ModelProtocol,
        model_id: String,
        scope_hash: String,
        payload: String,
    },
}

/// Read-only function tool definition exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Typed model request bound to a specific run, generation, connection, and scope snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub request_id: String,
    pub run_id: String,
    pub generation: u64,
    pub connection_id: String,
    pub scope_hash: String,
    pub instructions: String,
    pub input_items: Vec<InputItem>,
    pub tool_definitions: Vec<ToolDefinition>,
    pub output_budget: u32,
}

/// Token usage reported by the provider. Missing usage fields remain `None` (never fabricated as 0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none() && self.output_tokens.is_none() && self.total_tokens.is_none()
    }

    pub fn merge(&mut self, newer: &TokenUsage) {
        if newer.input_tokens.is_some() {
            self.input_tokens = newer.input_tokens;
        }
        if newer.output_tokens.is_some() {
            self.output_tokens = newer.output_tokens;
        }
        if newer.total_tokens.is_some() {
            self.total_tokens = newer.total_tokens;
        }
    }
}

/// Categorized error codes for `ModelClient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCode {
    UnsupportedProtocol,
    InvalidEndpoint,
    Unauthorized,
    Forbidden,
    RateLimited,
    ServerError,
    NetworkError,
    StreamTruncated,
    RecordTooLarge,
    InvalidToolCall,
    DuplicateToolCallMismatch,
    Cancelled,
    Timeout,
    ProtocolError,
    CrossScopeOpaqueRejected,
}

/// Structured, redacted error returned by `ModelClient`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelError {
    pub code: ModelErrorCode,
    pub retryable: bool,
    pub safe_message: String,
    pub http_status: Option<u16>,
    pub provider_request_id: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub partial_text: String,
    pub completed_tool_calls: Vec<(String, String, String)>,
}

impl ModelError {
    pub fn new(code: ModelErrorCode, retryable: bool, safe_message: impl Into<String>) -> Self {
        Self {
            code,
            retryable,
            safe_message: safe_message.into(),
            http_status: None,
            provider_request_id: None,
            retry_after_ms: None,
            partial_text: String::new(),
            completed_tool_calls: Vec::new(),
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} (retryable={}, status={:?}): {}",
            self.code, self.retryable, self.http_status, self.safe_message
        )
    }
}

impl std::error::Error for ModelError {}

/// Normalized streaming event payload emitted across both protocols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEventKind {
    Started {
        provider_request_id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCallReady {
        call_id: String,
        name: String,
        arguments_json: String,
    },
    ReasoningItem {
        payload: String,
    },
    Usage(TokenUsage),
    Completed {
        finish_reason: String,
    },
    Failed(ModelError),
}

/// Sequenced streaming event stamped with request, run, and generation identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEvent {
    pub request_id: String,
    pub run_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub kind: ModelEventKind,
}

/// Complete summary returned after a streaming request reaches a terminal protocol state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStreamResult {
    pub events: Vec<ModelEvent>,
    pub accumulated_text: String,
    pub ready_tool_calls: Vec<(String, String, Value)>,
    pub reasoning_items: Vec<String>,
    pub usage: Option<TokenUsage>,
    pub completed: bool,
}

pub(crate) fn wire_tool_name(name: &str) -> String {
    name.replace('.', "__")
}

pub(crate) fn build_request_body(conn: &ModelConnection, req: &ModelRequest) -> Value {
    let max_tokens = if req.output_budget == 0 {
        conn.budget.max_output_tokens
    } else {
        req.output_budget.min(conn.budget.max_output_tokens)
    };

    match conn.protocol {
        ModelProtocol::OpenAiResponses => {
            let mut input_vec = Vec::with_capacity(req.input_items.len());
            for item in &req.input_items {
                match item {
                    InputItem::SystemMessage { content } => {
                        input_vec.push(json!({
                            "type": "message",
                            "role": "system",
                            "content": [{ "type": "input_text", "text": content }]
                        }));
                    }
                    InputItem::UserMessage { content } => {
                        input_vec.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": [{ "type": "input_text", "text": content }]
                        }));
                    }
                    InputItem::AssistantMessage { content } => {
                        input_vec.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": content }]
                        }));
                    }
                    InputItem::ToolCall {
                        call_id,
                        name,
                        arguments_json,
                    } => {
                        input_vec.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": wire_tool_name(name),
                            "arguments": arguments_json
                        }));
                    }
                    InputItem::ToolOutput {
                        call_id,
                        output_json,
                        is_error: _,
                    } => {
                        input_vec.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output_json
                        }));
                    }
                    InputItem::ReasoningOpaque { payload, .. } => {
                        let item = serde_json::from_str::<Value>(payload)
                            .ok()
                            .filter(|item| item["type"].as_str() == Some("reasoning"))
                            .unwrap_or_else(
                                || json!({"type": "reasoning", "encrypted_content": payload}),
                            );
                        input_vec.push(item);
                    }
                }
            }

            let tools_vec: Vec<Value> = req
                .tool_definitions
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": wire_tool_name(&t.name),
                        "description": t.description,
                        "parameters": t.parameters_schema,
                        "strict": false
                    })
                })
                .collect();

            let mut body = Map::new();
            body.insert("model".into(), json!(conn.model_id));
            body.insert("instructions".into(), json!(req.instructions));
            body.insert("input".into(), Value::Array(input_vec));
            if !tools_vec.is_empty() {
                body.insert("tools".into(), Value::Array(tools_vec));
            }
            body.insert("stream".into(), json!(true));
            body.insert("store".into(), json!(false));
            if conn.capabilities.supports_reasoning {
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
            body.insert("max_output_tokens".into(), json!(max_tokens));
            Value::Object(body)
        }
        ModelProtocol::OpenAiChatCompletions => {
            let mut messages: Vec<Value> = Vec::with_capacity(req.input_items.len() + 1);
            if !req.instructions.trim().is_empty() {
                messages.push(json!({
                    "role": "system",
                    "content": req.instructions
                }));
            }
            for item in &req.input_items {
                match item {
                    InputItem::SystemMessage { content } => {
                        messages.push(json!({
                            "role": "system",
                            "content": content
                        }));
                    }
                    InputItem::UserMessage { content } => {
                        messages.push(json!({
                            "role": "user",
                            "content": content
                        }));
                    }
                    InputItem::AssistantMessage { content } => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": content
                        }));
                    }
                    InputItem::ToolCall {
                        call_id,
                        name,
                        arguments_json,
                    } => {
                        let call = json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": wire_tool_name(name),
                                "arguments": arguments_json
                            }
                        });
                        // Attach tool calls onto an immediately preceding assistant message
                        // (whether it already has `tool_calls` or only text `content`) so we never
                        // emit consecutive `role: "assistant"` messages rejected by strict APIs.
                        let merged = messages.last_mut().is_some_and(|message| {
                            let is_assistant =
                                message.get("role").and_then(Value::as_str) == Some("assistant");
                            if !is_assistant {
                                return false;
                            }
                            if let Some(calls) =
                                message.get_mut("tool_calls").and_then(Value::as_array_mut)
                            {
                                calls.push(call.clone());
                                true
                            } else if let Some(obj) = message.as_object_mut() {
                                obj.insert("tool_calls".into(), Value::Array(vec![call.clone()]));
                                true
                            } else {
                                false
                            }
                        });
                        if !merged {
                            messages.push(
                                json!({"role": "assistant", "content": null, "tool_calls": [call]}),
                            );
                        }
                    }
                    InputItem::ToolOutput {
                        call_id,
                        output_json,
                        ..
                    } => {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output_json
                        }));
                    }
                    InputItem::ReasoningOpaque { .. } => {}
                }
            }

            let tools_vec: Vec<Value> = req
                .tool_definitions
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": wire_tool_name(&t.name),
                            "description": t.description,
                            "parameters": t.parameters_schema
                        }
                    })
                })
                .collect();

            let mut body = Map::new();
            body.insert("model".into(), json!(conn.model_id));
            body.insert("messages".into(), Value::Array(messages));
            if !tools_vec.is_empty() {
                body.insert("tools".into(), Value::Array(tools_vec));
            }
            body.insert("stream".into(), json!(true));
            body.insert("stream_options".into(), json!({"include_usage": true}));
            body.insert("max_completion_tokens".into(), json!(max_tokens));
            Value::Object(body)
        }
    }
}

pub(crate) fn parse_token_usage(usage_val: &Value) -> TokenUsage {
    let input_tokens = usage_val
        .get("input_tokens")
        .or_else(|| usage_val.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = usage_val
        .get("output_tokens")
        .or_else(|| usage_val.get("completion_tokens"))
        .and_then(Value::as_u64);
    let total_tokens = usage_val.get("total_tokens").and_then(Value::as_u64);
    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    }
}

pub(crate) fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(ms_str) = trimmed.strip_suffix("ms") {
        return ms_str.trim().parse::<u64>().ok();
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    if let Ok(seconds_f) = trimmed.parse::<f64>() {
        if seconds_f.is_finite() && seconds_f >= 0.0 {
            return Some((seconds_f * 1000.0) as u64);
        }
    }
    None
}

pub(crate) fn map_http_status_error(
    status: u16,
    provider_request_id: Option<String>,
    retry_after_ms: Option<u64>,
) -> ModelError {
    let (code, retryable, msg) = match status {
        401 => (
            ModelErrorCode::Unauthorized,
            false,
            "HTTP 401 unauthorized: check the saved model API key".to_string(),
        ),
        403 => (
            ModelErrorCode::Forbidden,
            false,
            "HTTP 403 forbidden: model access denied".to_string(),
        ),
        408 => (
            ModelErrorCode::Timeout,
            true,
            "HTTP 408 request timeout".to_string(),
        ),
        429 => (
            ModelErrorCode::RateLimited,
            true,
            "HTTP 429 rate limited by model endpoint".to_string(),
        ),
        500 | 502 | 503 | 504 => (
            ModelErrorCode::ServerError,
            true,
            format!("HTTP {} transient upstream server error", status),
        ),
        other => (
            ModelErrorCode::ProtocolError,
            false,
            format!("HTTP {} unexpected status from model endpoint", other),
        ),
    };

    ModelError {
        code,
        retryable,
        safe_message: msg,
        http_status: Some(status),
        provider_request_id,
        retry_after_ms,
        partial_text: String::new(),
        completed_tool_calls: Vec::new(),
    }
}

pub(crate) fn sanitize_error_text(raw: &str, api_key: Option<&str>) -> String {
    let mut redacted = raw.to_string();
    if let Some(k) = api_key {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            redacted = redacted.replace(trimmed, "[REDACTED]");
        }
    }
    let mut out = String::with_capacity(redacted.len());
    for ch in redacted.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(ch),
            '\x07' => out.push_str("\\x07"),
            '\x1b' => out.push_str("\\x1b"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
