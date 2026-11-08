//! Pure-Rust model client supporting `openai-responses` and `openai-chat-completions`
//! over SSE without business database dependencies (S-12, S-19, M2-K03, M2-A-13).
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

        let base_trimmed = self.base_url.trim().trim_end_matches('/');
        let joined = format!("{}{}", base_trimmed, self.protocol.endpoint_path());
        let joined_url = reqwest::Url::parse(&joined).map_err(|_| {
            ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                "failed to join protocol endpoint path onto base_url",
            )
        })?;

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

#[derive(Debug, Clone)]
enum TransportStepError {
    Cancelled,
    Timeout,
    RecordTooLarge(usize),
    Network(String),
}

impl fmt::Display for TransportStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("stream cancelled"),
            Self::Timeout => f.write_str("stream timed out waiting for bytes"),
            Self::RecordTooLarge(sz) => {
                write!(f, "SSE record exceeded byte ceiling ({} bytes)", sz)
            }
            Self::Network(msg) => write!(f, "network error: {}", msg),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ToolCallBuilder {
    call_id: String,
    name: String,
    arguments: String,
    finalized: bool,
}

struct StreamAccumulator<'a> {
    req: &'a ModelRequest,
    conn: &'a ModelConnection,
    next_sequence: u64,
    events: Vec<ModelEvent>,
    accumulated_text: String,
    ready_tool_calls: Vec<(String, String, Value)>,
    completed_tool_calls_raw: Vec<(String, String, String)>,
    seen_tool_calls: BTreeMap<String, (String, Value)>,
    reasoning_items: Vec<String>,
    usage: Option<TokenUsage>,
    provider_request_id: Option<String>,
    started_emitted: bool,
    visible_output_emitted: bool,
    completed: bool,
    ignored_event_count: usize,
}

impl<'a> StreamAccumulator<'a> {
    fn new(req: &'a ModelRequest, conn: &'a ModelConnection) -> Self {
        Self {
            req,
            conn,
            next_sequence: 1,
            events: Vec::new(),
            accumulated_text: String::new(),
            ready_tool_calls: Vec::new(),
            completed_tool_calls_raw: Vec::new(),
            seen_tool_calls: BTreeMap::new(),
            reasoning_items: Vec::new(),
            usage: None,
            provider_request_id: None,
            started_emitted: false,
            visible_output_emitted: false,
            completed: false,
            ignored_event_count: 0,
        }
    }

    fn emit<F>(&mut self, kind: ModelEventKind, on_event: &mut F)
    where
        F: FnMut(&ModelEvent),
    {
        let ev = ModelEvent {
            request_id: self.req.request_id.clone(),
            run_id: self.req.run_id.clone(),
            generation: self.req.generation,
            sequence: self.next_sequence,
            kind,
        };
        self.next_sequence += 1;
        on_event(&ev);
        self.events.push(ev);
    }

    fn ensure_started<F>(&mut self, provider_req_id: Option<String>, on_event: &mut F)
    where
        F: FnMut(&ModelEvent),
    {
        if self.provider_request_id.is_none() && provider_req_id.is_some() {
            self.provider_request_id = provider_req_id.clone();
        }
        if !self.started_emitted {
            self.started_emitted = true;
            let id = self.provider_request_id.clone();
            self.emit(
                ModelEventKind::Started {
                    provider_request_id: id,
                },
                on_event,
            );
        }
    }

    fn emit_text_delta<F>(&mut self, delta: &str, on_event: &mut F)
    where
        F: FnMut(&ModelEvent),
    {
        if delta.is_empty() {
            return;
        }
        self.ensure_started(None, on_event);
        self.visible_output_emitted = true;
        self.accumulated_text.push_str(delta);
        self.emit(
            ModelEventKind::TextDelta {
                delta: delta.to_string(),
            },
            on_event,
        );
    }

    fn emit_tool_delta<F>(
        &mut self,
        call_id: &str,
        name: Option<String>,
        arguments_delta: &str,
        on_event: &mut F,
    ) where
        F: FnMut(&ModelEvent),
    {
        if arguments_delta.is_empty() {
            return;
        }
        self.ensure_started(None, on_event);
        self.visible_output_emitted = true;
        self.emit(
            ModelEventKind::ToolCallDelta {
                call_id: call_id.to_string(),
                name,
                arguments_delta: arguments_delta.to_string(),
            },
            on_event,
        );
    }

    fn finalize_tool_call<F>(
        &mut self,
        call_id: &str,
        name: &str,
        arguments_json: &str,
        on_event: &mut F,
    ) -> Result<(), ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        let call_id_trimmed = call_id.trim();
        let wire_name = name.trim();
        let logical_name = self
            .req
            .tool_definitions
            .iter()
            .find(|tool| wire_tool_name(&tool.name) == wire_name)
            .map(|tool| tool.name.as_str())
            .unwrap_or(wire_name);
        let name_trimmed = logical_name;
        if call_id_trimmed.is_empty() || name_trimmed.is_empty() {
            return Err(self.decorate_error(ModelError::new(
                ModelErrorCode::InvalidToolCall,
                false,
                "tool call is missing a valid call_id or function name",
            )));
        }

        let parsed_args: Value = serde_json::from_str(arguments_json).map_err(|_| {
            self.decorate_error(ModelError::new(
                ModelErrorCode::InvalidToolCall,
                false,
                format!(
                    "tool call '{}' for '{}' has incomplete or invalid JSON arguments",
                    call_id_trimmed, name_trimmed
                ),
            ))
        })?;

        if !parsed_args.is_object() {
            return Err(self.decorate_error(ModelError::new(
                ModelErrorCode::InvalidToolCall,
                false,
                format!(
                    "tool call '{}' for '{}' arguments must be a JSON object",
                    call_id_trimmed, name_trimmed
                ),
            )));
        }

        if let Some((existing_name, existing_args)) = self.seen_tool_calls.get(call_id_trimmed) {
            if existing_name == name_trimmed && existing_args == &parsed_args {
                // Identical duplicate call_id + arguments: deduplicate silently per contract.
                return Ok(());
            }
            return Err(self.decorate_error(ModelError::new(
                ModelErrorCode::DuplicateToolCallMismatch,
                false,
                format!(
                    "duplicate tool call_id '{}' with conflicting function name or arguments",
                    call_id_trimmed
                ),
            )));
        }

        if self.ready_tool_calls.len() >= self.conn.budget.max_tool_calls_per_run {
            return Err(self.decorate_error(ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                format!(
                    "tool calls in response exceeded max_tool_calls_per_run ({})",
                    self.conn.budget.max_tool_calls_per_run
                ),
            )));
        }

        let canonical_json =
            serde_json::to_string(&parsed_args).unwrap_or_else(|_| arguments_json.to_string());
        self.seen_tool_calls.insert(
            call_id_trimmed.to_string(),
            (name_trimmed.to_string(), parsed_args.clone()),
        );
        self.ready_tool_calls.push((
            call_id_trimmed.to_string(),
            name_trimmed.to_string(),
            parsed_args,
        ));
        self.completed_tool_calls_raw.push((
            call_id_trimmed.to_string(),
            name_trimmed.to_string(),
            canonical_json.clone(),
        ));
        self.visible_output_emitted = true;
        self.emit(
            ModelEventKind::ToolCallReady {
                call_id: call_id_trimmed.to_string(),
                name: name_trimmed.to_string(),
                arguments_json: canonical_json,
            },
            on_event,
        );
        Ok(())
    }

    fn record_usage<F>(&mut self, usage: TokenUsage, on_event: &mut F)
    where
        F: FnMut(&ModelEvent),
    {
        if usage.is_empty() {
            return;
        }
        match &mut self.usage {
            Some(existing) => existing.merge(&usage),
            None => self.usage = Some(usage.clone()),
        }
        self.emit(ModelEventKind::Usage(usage), on_event);
    }

    fn decorate_error(&self, mut err: ModelError) -> ModelError {
        if err.provider_request_id.is_none() {
            err.provider_request_id = self.provider_request_id.clone();
        }
        err.partial_text = self.accumulated_text.clone();
        err.completed_tool_calls = self.completed_tool_calls_raw.clone();
        if self.visible_output_emitted {
            err.retryable = false;
        }
        err
    }

    fn into_result(self) -> ModelStreamResult {
        ModelStreamResult {
            events: self.events,
            accumulated_text: self.accumulated_text,
            ready_tool_calls: self.ready_tool_calls,
            reasoning_items: self.reasoning_items,
            usage: self.usage,
            completed: self.completed,
        }
    }
}

/// Pure-Rust model client supporting `openai-responses` and `openai-chat-completions`.
pub struct ModelClient {
    runtime: tokio::runtime::Runtime,
}

impl Default for ModelClient {
    fn default() -> Self {
        Self::new().expect("failed to initialize Tokio runtime for ModelClient")
    }
}

impl ModelClient {
    pub fn new() -> Result<Self, ModelError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| {
                ModelError::new(
                    ModelErrorCode::NetworkError,
                    false,
                    format!("failed to create Tokio runtime: {}", e),
                )
            })?;
        Ok(Self { runtime })
    }

    /// Validates that all input items (especially `ReasoningOpaque`) belong to the current
    /// connection, protocol, model, and scope hash.
    pub fn validate_request(
        conn: &ModelConnection,
        req: &ModelRequest,
    ) -> Result<String, ModelError> {
        let endpoint_url = conn.resolved_endpoint_url()?;
        if req.connection_id != conn.id {
            return Err(ModelError::new(
                ModelErrorCode::InvalidEndpoint,
                false,
                format!(
                    "request connection_id '{}' does not match connection '{}'",
                    req.connection_id, conn.id
                ),
            ));
        }
        for item in &req.input_items {
            if let InputItem::ReasoningOpaque {
                provider_id,
                protocol,
                model_id,
                scope_hash,
                ..
            } = item
            {
                if provider_id != &conn.id
                    || protocol != &conn.protocol
                    || model_id != &conn.model_id
                    || scope_hash != &req.scope_hash
                {
                    return Err(ModelError::new(
                        ModelErrorCode::CrossScopeOpaqueRejected,
                        false,
                        "rejected cross-provider, cross-protocol, cross-model, or cross-scope opaque reasoning item",
                    ));
                }
            }
        }
        Ok(endpoint_url)
    }

    /// Streams a model request to completion with bounded retries before visible output.
    pub fn stream_request(
        &self,
        conn: &ModelConnection,
        req: &ModelRequest,
        api_key: Option<&str>,
        cancel: Arc<AtomicBool>,
    ) -> Result<ModelStreamResult, ModelError> {
        self.stream_request_with_callback(conn, req, api_key, cancel, |_| {})
    }

    /// Streams a model request while invoking `on_event` for each sequenced `ModelEvent`.
    pub fn stream_request_with_callback<F>(
        &self,
        conn: &ModelConnection,
        req: &ModelRequest,
        api_key: Option<&str>,
        cancel: Arc<AtomicBool>,
        mut on_event: F,
    ) -> Result<ModelStreamResult, ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        self.runtime
            .block_on(self.stream_request_async(conn, req, api_key, cancel, &mut on_event))
    }

    /// Checks connectivity and returns verified capabilities for the configured endpoint.
    pub fn check_connectivity(
        &self,
        conn: &ModelConnection,
        api_key: Option<&str>,
    ) -> Result<ModelCapabilities, ModelError> {
        conn.validate_endpoint()?;
        let probe_req = ModelRequest {
            request_id: "connectivity-probe".to_string(),
            run_id: "connectivity-probe-run".to_string(),
            generation: 1,
            connection_id: conn.id.clone(),
            scope_hash: "connectivity-probe-scope".to_string(),
            instructions: "Reply with OK.".to_string(),
            input_items: vec![InputItem::UserMessage {
                content: "ping".to_string(),
            }],
            tool_definitions: Vec::new(),
            output_budget: 16,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = self.stream_request(conn, &probe_req, api_key, cancel)?;
        Ok(conn.capabilities)
    }

    async fn stream_request_async<F>(
        &self,
        conn: &ModelConnection,
        req: &ModelRequest,
        api_key: Option<&str>,
        cancel: Arc<AtomicBool>,
        on_event: &mut F,
    ) -> Result<ModelStreamResult, ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        let endpoint_url = Self::validate_request(conn, req)?;
        let body_value = build_request_body(conn, req);
        let overall_start = Instant::now();
        let total_budget_ms = conn
            .timeouts
            .total_ms
            .min(conn.budget.max_run_duration_ms)
            .max(1);
        let max_retries = conn.retry_policy.max_retries_before_visible_output;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(conn.timeouts.connect_ms.max(1)))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                ModelError::new(
                    ModelErrorCode::NetworkError,
                    false,
                    format!(
                        "failed to build HTTP client: {}",
                        sanitize_error_text(&e.to_string(), api_key)
                    ),
                )
            })?;

        let mut attempt = 0usize;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(ModelError::new(
                    ModelErrorCode::Cancelled,
                    false,
                    "model request cancelled before dispatch",
                ));
            }

            let elapsed_ms = overall_start.elapsed().as_millis() as u64;
            if elapsed_ms >= total_budget_ms {
                return Err(ModelError::new(
                    ModelErrorCode::Timeout,
                    false,
                    "model request exceeded total timeout budget",
                ));
            }
            let remaining_ms = total_budget_ms - elapsed_ms;

            let attempt_res = self
                .execute_single_attempt(
                    &client,
                    &endpoint_url,
                    conn,
                    req,
                    &body_value,
                    api_key,
                    Arc::clone(&cancel),
                    remaining_ms,
                    on_event,
                )
                .await;

            match attempt_res {
                Ok(result) => return Ok(result),
                Err((err, visible_output_emitted)) => {
                    let can_retry = !visible_output_emitted
                        && err.retryable
                        && attempt < max_retries
                        && !cancel.load(Ordering::SeqCst);

                    if !can_retry {
                        on_event(&ModelEvent {
                            request_id: req.request_id.clone(),
                            run_id: req.run_id.clone(),
                            generation: req.generation,
                            sequence: u64::MAX,
                            kind: ModelEventKind::Failed(err.clone()),
                        });
                        return Err(err);
                    }

                    attempt += 1;
                    let backoff_ms = err.retry_after_ms.unwrap_or_else(|| {
                        conn.retry_policy
                            .base_backoff_ms
                            .saturating_mul(1u64 << (attempt.saturating_sub(1).min(4)))
                    });

                    let elapsed_now = overall_start.elapsed().as_millis() as u64;
                    if elapsed_now.saturating_add(backoff_ms) >= total_budget_ms {
                        return Err(err);
                    }

                    sleep_with_cancel(Duration::from_millis(backoff_ms), &cancel).await?;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_single_attempt<F>(
        &self,
        client: &reqwest::Client,
        endpoint_url: &str,
        conn: &ModelConnection,
        req: &ModelRequest,
        body_value: &Value,
        api_key: Option<&str>,
        cancel: Arc<AtomicBool>,
        remaining_ms: u64,
        on_event: &mut F,
    ) -> Result<ModelStreamResult, (ModelError, bool)>
    where
        F: FnMut(&ModelEvent),
    {
        let mut req_builder = client
            .post(endpoint_url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("X-Client-Request-Id", &req.request_id)
            .json(body_value);

        if let Some(key) = api_key {
            if !key.trim().is_empty() {
                req_builder = req_builder.bearer_auth(key.trim());
            }
        }

        let send_timeout = Duration::from_millis(
            conn.timeouts
                .connect_ms
                .max(conn.timeouts.idle_ms)
                .min(remaining_ms)
                .max(1),
        );

        let response = match tokio::time::timeout(send_timeout, req_builder.send()).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                if cancel.load(Ordering::SeqCst) {
                    return Err((
                        ModelError::new(
                            ModelErrorCode::Cancelled,
                            false,
                            "model request cancelled during send",
                        ),
                        false,
                    ));
                }
                let is_timeout = e.is_timeout();
                return Err((
                    ModelError::new(
                        if is_timeout {
                            ModelErrorCode::Timeout
                        } else {
                            ModelErrorCode::NetworkError
                        },
                        true,
                        format!(
                            "HTTP transport error: {}",
                            sanitize_error_text(&e.to_string(), api_key)
                        ),
                    ),
                    false,
                ));
            }
            Err(_) => {
                return Err((
                    ModelError::new(
                        ModelErrorCode::Timeout,
                        true,
                        "timed out waiting for HTTP response headers",
                    ),
                    false,
                ));
            }
        };

        let status = response.status().as_u16();
        let header_req_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        if status != 200 {
            let err = map_http_status_error(status, header_req_id, retry_after_ms);
            return Err((err, false));
        }

        let mut acc = StreamAccumulator::new(req, conn);
        if header_req_id.is_some() {
            acc.provider_request_id = header_req_id;
        }

        let idle_timeout = Duration::from_millis(conn.timeouts.idle_ms.min(remaining_ms).max(1));
        let max_record_bytes = conn.budget.max_sse_record_bytes;
        let cancel_for_stream = Arc::clone(&cancel);

        let inner_bytes = Box::pin(response.bytes_stream());
        let guarded_bytes = futures_util::stream::unfold(
            (
                inner_bytes,
                cancel_for_stream,
                idle_timeout,
                max_record_bytes,
                0usize,
                0u8,
                false,
            ),
            |(
                mut stream,
                cancel_flag,
                idle_dur,
                max_bytes,
                mut current_record_bytes,
                mut consecutive_newlines,
                halted,
            )| async move {
                if halted {
                    return None;
                }
                if cancel_flag.load(Ordering::SeqCst) {
                    return Some((
                        Err(TransportStepError::Cancelled),
                        (
                            stream,
                            cancel_flag,
                            idle_dur,
                            max_bytes,
                            current_record_bytes,
                            consecutive_newlines,
                            true,
                        ),
                    ));
                }

                let next_chunk = tokio::time::timeout(idle_dur, stream.next()).await;
                match next_chunk {
                    Err(_) => Some((
                        Err(TransportStepError::Timeout),
                        (
                            stream,
                            cancel_flag,
                            idle_dur,
                            max_bytes,
                            current_record_bytes,
                            consecutive_newlines,
                            true,
                        ),
                    )),
                    Ok(None) => None,
                    Ok(Some(Err(e))) => Some((
                        Err(TransportStepError::Network(e.to_string())),
                        (
                            stream,
                            cancel_flag,
                            idle_dur,
                            max_bytes,
                            current_record_bytes,
                            consecutive_newlines,
                            true,
                        ),
                    )),
                    Ok(Some(Ok(chunk))) => {
                        if cancel_flag.load(Ordering::SeqCst) {
                            return Some((
                                Err(TransportStepError::Cancelled),
                                (
                                    stream,
                                    cancel_flag,
                                    idle_dur,
                                    max_bytes,
                                    current_record_bytes,
                                    consecutive_newlines,
                                    true,
                                ),
                            ));
                        }

                        // Pre-buffering check for oversized SSE records before eventsource_stream
                        // accumulates unbounded lines in memory.
                        let raw_limit = max_bytes.saturating_add(64);
                        for &b in chunk.iter() {
                            if b == b'\r' {
                                current_record_bytes = current_record_bytes.saturating_add(1);
                            } else if b == b'\n' {
                                consecutive_newlines = consecutive_newlines.saturating_add(1);
                                current_record_bytes = current_record_bytes.saturating_add(1);
                                if current_record_bytes > raw_limit {
                                    return Some((
                                        Err(TransportStepError::RecordTooLarge(
                                            current_record_bytes,
                                        )),
                                        (
                                            stream,
                                            cancel_flag,
                                            idle_dur,
                                            max_bytes,
                                            current_record_bytes,
                                            consecutive_newlines,
                                            true,
                                        ),
                                    ));
                                }
                                if consecutive_newlines >= 2 {
                                    current_record_bytes = 0;
                                    consecutive_newlines = 0;
                                }
                            } else {
                                consecutive_newlines = 0;
                                current_record_bytes = current_record_bytes.saturating_add(1);
                                if current_record_bytes > raw_limit {
                                    return Some((
                                        Err(TransportStepError::RecordTooLarge(
                                            current_record_bytes,
                                        )),
                                        (
                                            stream,
                                            cancel_flag,
                                            idle_dur,
                                            max_bytes,
                                            current_record_bytes,
                                            consecutive_newlines,
                                            true,
                                        ),
                                    ));
                                }
                            }
                        }

                        Some((
                            Ok::<Bytes, TransportStepError>(chunk),
                            (
                                stream,
                                cancel_flag,
                                idle_dur,
                                max_bytes,
                                current_record_bytes,
                                consecutive_newlines,
                                false,
                            ),
                        ))
                    }
                }
            },
        );

        let mut sse_stream = Box::pin(guarded_bytes.eventsource());

        let parse_res = match conn.protocol {
            ModelProtocol::OpenAiResponses => {
                process_responses_sse(
                    &mut sse_stream,
                    &mut acc,
                    max_record_bytes,
                    &cancel,
                    on_event,
                )
                .await
            }
            ModelProtocol::OpenAiChatCompletions => {
                process_chat_completions_sse(
                    &mut sse_stream,
                    &mut acc,
                    max_record_bytes,
                    &cancel,
                    on_event,
                )
                .await
            }
        };

        match parse_res {
            Ok(()) => Ok(acc.into_result()),
            Err(err) => {
                let vis = acc.visible_output_emitted;
                Err((acc.decorate_error(err), vis))
            }
        }
    }
}

async fn process_responses_sse<S, F>(
    sse_stream: &mut S,
    acc: &mut StreamAccumulator<'_>,
    max_record_bytes: usize,
    cancel: &Arc<AtomicBool>,
    on_event: &mut F,
) -> Result<(), ModelError>
where
    S: futures_util::Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<TransportStepError>,
            >,
        > + Unpin,
    F: FnMut(&ModelEvent),
{
    let mut item_id_to_call_id: BTreeMap<String, String> = BTreeMap::new();
    let mut tool_builders: BTreeMap<String, ToolCallBuilder> = BTreeMap::new();

    while let Some(next_item) = sse_stream.next().await {
        if acc.events.len() >= 16_384 {
            return Err(ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                "stream event budget exceeded",
            ));
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::new(
                ModelErrorCode::Cancelled,
                false,
                "model stream cancelled by caller",
            ));
        }

        let ev = match next_item {
            Ok(ev) => ev,
            Err(eventsource_stream::EventStreamError::Transport(step_err)) => {
                return Err(map_transport_step_error(
                    step_err,
                    acc.visible_output_emitted,
                ));
            }
            Err(other) => {
                return Err(ModelError::new(
                    ModelErrorCode::ProtocolError,
                    false,
                    format!(
                        "SSE framing/UTF-8 error in openai-responses stream: {}",
                        other
                    ),
                ));
            }
        };

        if ev.data.len() > max_record_bytes {
            return Err(ModelError::new(
                ModelErrorCode::RecordTooLarge,
                false,
                format!(
                    "SSE event data size ({} bytes) exceeded budget ({} bytes)",
                    ev.data.len(),
                    max_record_bytes
                ),
            ));
        }

        let data_trimmed = ev.data.trim();
        if data_trimmed.is_empty() || data_trimmed == "[DONE]" {
            continue;
        }

        let payload: Value = serde_json::from_str(data_trimmed).map_err(|_| {
            ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                "invalid JSON in openai-responses SSE event",
            )
        })?;

        let event_type = if !ev.event.is_empty() && ev.event != "message" {
            ev.event.as_str()
        } else {
            payload.get("type").and_then(Value::as_str).unwrap_or("")
        };

        match event_type {
            "response.created" | "response.in_progress" => {
                let req_id = payload
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .or_else(|| payload.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                acc.ensure_started(req_id, on_event);
            }
            "response.output_text.delta" => {
                let delta = payload.get("delta").and_then(Value::as_str).unwrap_or("");
                acc.emit_text_delta(delta, on_event);
            }
            "response.output_item.added" => {
                acc.ensure_started(None, on_event);
                let item = payload.get("item").unwrap_or(&payload);
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                if item_type == "function_call" {
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(item_id.as_str())
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !item_id.is_empty() && !call_id.is_empty() {
                        item_id_to_call_id.insert(item_id, call_id.clone());
                    }
                    if !call_id.is_empty() {
                        let entry = tool_builders.entry(call_id.clone()).or_default();
                        entry.call_id = call_id;
                        if !name.is_empty() {
                            entry.name = name;
                        }
                        if let Some(initial_args) = item.get("arguments").and_then(Value::as_str) {
                            if !initial_args.is_empty() {
                                entry.arguments.push_str(initial_args);
                            }
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let raw_call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("item_id").and_then(Value::as_str))
                    .unwrap_or("");
                let resolved_call_id = item_id_to_call_id
                    .get(raw_call_id)
                    .cloned()
                    .unwrap_or_else(|| raw_call_id.to_string());
                let delta = payload.get("delta").and_then(Value::as_str).unwrap_or("");
                let maybe_name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let entry = tool_builders.entry(resolved_call_id.clone()).or_default();
                if entry.call_id.is_empty() {
                    entry.call_id = resolved_call_id.clone();
                }
                if let Some(ref n) = maybe_name {
                    if !n.is_empty() {
                        entry.name = n.clone();
                    }
                }
                entry.arguments.push_str(delta);
                let name_for_event = if entry.name.is_empty() {
                    None
                } else {
                    Some(entry.name.clone())
                };
                acc.emit_tool_delta(&resolved_call_id, name_for_event, delta, on_event);
            }
            "response.function_call_arguments.done" => {
                let raw_call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("item_id").and_then(Value::as_str))
                    .unwrap_or("");
                let resolved_call_id = item_id_to_call_id
                    .get(raw_call_id)
                    .cloned()
                    .unwrap_or_else(|| raw_call_id.to_string());
                let explicit_name = payload.get("name").and_then(Value::as_str).unwrap_or("");
                let explicit_args = payload.get("arguments").and_then(Value::as_str);

                let entry = tool_builders.entry(resolved_call_id.clone()).or_default();
                if entry.call_id.is_empty() {
                    entry.call_id = resolved_call_id.clone();
                }
                if !explicit_name.is_empty() {
                    entry.name = explicit_name.to_string();
                }
                let final_args = match explicit_args {
                    Some(a) if entry.arguments.is_empty() => a.to_string(),
                    _ => entry.arguments.clone(),
                };
                let final_name = entry.name.clone();
                entry.finalized = true;
                acc.finalize_tool_call(&resolved_call_id, &final_name, &final_args, on_event)?;
            }
            "response.output_item.done" => {
                let item = payload.get("item").unwrap_or(&payload);
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                if item_type == "function_call" {
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| item_id_to_call_id.get(item_id).map(String::as_str))
                        .unwrap_or(item_id)
                        .to_string();
                    let name_from_item = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let args_from_item =
                        item.get("arguments").and_then(Value::as_str).unwrap_or("");

                    let already_finalized = tool_builders
                        .get(&call_id)
                        .map(|b| b.finalized)
                        .unwrap_or(false);

                    let final_name = if !name_from_item.is_empty() {
                        name_from_item.to_string()
                    } else {
                        tool_builders
                            .get(&call_id)
                            .map(|b| b.name.clone())
                            .unwrap_or_default()
                    };
                    let final_args = if !args_from_item.is_empty() {
                        args_from_item.to_string()
                    } else {
                        tool_builders
                            .get(&call_id)
                            .map(|b| b.arguments.clone())
                            .unwrap_or_default()
                    };

                    if !already_finalized || !args_from_item.is_empty() {
                        if let Some(b) = tool_builders.get_mut(&call_id) {
                            b.finalized = true;
                        }
                        acc.finalize_tool_call(&call_id, &final_name, &final_args, on_event)?;
                    }
                } else if item_type == "reasoning" {
                    let payload = item.to_string();
                    if !acc.reasoning_items.contains(&payload) {
                        acc.reasoning_items.push(payload.clone());
                        acc.emit(ModelEventKind::ReasoningItem { payload }, on_event);
                    }
                }
            }
            "response.completed" => {
                if payload
                    .get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "completed")
                {
                    return Err(ModelError::new(
                        ModelErrorCode::ProtocolError,
                        false,
                        "inconsistent response terminal status",
                    ));
                }
                acc.ensure_started(None, on_event);
                for builder in tool_builders.values() {
                    if !builder.finalized {
                        acc.finalize_tool_call(
                            &builder.call_id,
                            &builder.name,
                            &builder.arguments,
                            on_event,
                        )?;
                    }
                }
                if let Some(usage_val) = payload
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .or_else(|| payload.get("usage"))
                {
                    let usage = parse_token_usage(usage_val);
                    acc.record_usage(usage, on_event);
                }
                let finish_reason = payload
                    .get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_string();
                acc.completed = true;
                acc.emit(ModelEventKind::Completed { finish_reason }, on_event);
                return Ok(());
            }
            "response.incomplete" => {
                let reason = payload
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("incomplete");
                return Err(ModelError::new(
                    ModelErrorCode::StreamTruncated,
                    false,
                    format!("openai-responses terminated as incomplete ({})", reason),
                ));
            }
            "response.failed" | "error" => {
                let msg = payload
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .or_else(|| {
                        payload
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .and_then(|e| e.get("message"))
                    })
                    .and_then(Value::as_str)
                    .unwrap_or("provider reported response.failed");
                return Err(ModelError::new(
                    ModelErrorCode::ProtocolError,
                    false,
                    format!("openai-responses failed: {}", msg),
                ));
            }
            _ => {
                acc.ignored_event_count += 1;
            }
        }
    }

    if !acc.completed {
        return Err(ModelError::new(
            ModelErrorCode::StreamTruncated,
            !acc.visible_output_emitted,
            "openai-responses SSE stream ended before response.completed",
        ));
    }

    Ok(())
}

async fn process_chat_completions_sse<S, F>(
    sse_stream: &mut S,
    acc: &mut StreamAccumulator<'_>,
    max_record_bytes: usize,
    cancel: &Arc<AtomicBool>,
    on_event: &mut F,
) -> Result<(), ModelError>
where
    S: futures_util::Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<TransportStepError>,
            >,
        > + Unpin,
    F: FnMut(&ModelEvent),
{
    let mut tool_builders: BTreeMap<usize, ToolCallBuilder> = BTreeMap::new();
    let mut finish_reason: Option<String> = None;

    while let Some(next_item) = sse_stream.next().await {
        if acc.events.len() >= 16_384 {
            return Err(ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                "stream event budget exceeded",
            ));
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::new(
                ModelErrorCode::Cancelled,
                false,
                "model stream cancelled by caller",
            ));
        }

        let ev = match next_item {
            Ok(ev) => ev,
            Err(eventsource_stream::EventStreamError::Transport(step_err)) => {
                return Err(map_transport_step_error(
                    step_err,
                    acc.visible_output_emitted,
                ));
            }
            Err(other) => {
                return Err(ModelError::new(
                    ModelErrorCode::ProtocolError,
                    false,
                    format!(
                        "SSE framing/UTF-8 error in openai-chat-completions stream: {}",
                        other
                    ),
                ));
            }
        };

        if ev.data.len() > max_record_bytes {
            return Err(ModelError::new(
                ModelErrorCode::RecordTooLarge,
                false,
                format!(
                    "SSE event data size ({} bytes) exceeded budget ({} bytes)",
                    ev.data.len(),
                    max_record_bytes
                ),
            ));
        }

        let data_trimmed = ev.data.trim();
        if data_trimmed.is_empty() {
            continue;
        }

        if data_trimmed == "[DONE]" {
            let reason = finish_reason.clone().ok_or_else(|| {
                ModelError::new(
                    ModelErrorCode::StreamTruncated,
                    !acc.visible_output_emitted,
                    "received [DONE] without a preceding valid finish_reason",
                )
            })?;

            match reason.as_str() {
                "stop" | "tool_calls" => {
                    for builder in tool_builders.values_mut() {
                        if !builder.finalized {
                            builder.finalized = true;
                            acc.finalize_tool_call(
                                &builder.call_id,
                                &builder.name,
                                &builder.arguments,
                                on_event,
                            )?;
                        }
                    }
                    acc.completed = true;
                    acc.emit(
                        ModelEventKind::Completed {
                            finish_reason: reason,
                        },
                        on_event,
                    );
                    return Ok(());
                }
                other => {
                    return Err(ModelError::new(
                        ModelErrorCode::StreamTruncated,
                        false,
                        format!(
                            "chat completion stream truncated with finish_reason '{}'",
                            other
                        ),
                    ));
                }
            }
        }

        let chunk: Value = serde_json::from_str(data_trimmed).map_err(|_| {
            ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                "invalid JSON in openai-chat-completions SSE chunk",
            )
        })?;

        if let Some(err_obj) = chunk.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error in chat completion stream");
            return Err(ModelError::new(
                ModelErrorCode::ProtocolError,
                false,
                format!("openai-chat-completions error: {}", msg),
            ));
        }

        let chunk_id = chunk.get("id").and_then(Value::as_str).map(str::to_string);
        acc.ensure_started(chunk_id, on_event);

        if let Some(usage_val) = chunk.get("usage") {
            if !usage_val.is_null() {
                let usage = parse_token_usage(usage_val);
                acc.record_usage(usage, on_event);
            }
        }

        if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        acc.emit_text_delta(text, on_event);
                    }
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                    {
                        if !reasoning.is_empty() {
                            acc.reasoning_items.push(reasoning.to_string());
                            acc.emit(
                                ModelEventKind::ReasoningItem {
                                    payload: reasoning.to_string(),
                                },
                                on_event,
                            );
                        }
                    }
                    if let Some(tc_list) = delta.get("tool_calls").and_then(Value::as_array) {
                        for tc in tc_list {
                            let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            let entry = tool_builders.entry(idx).or_default();
                            if let Some(id_str) = tc.get("id").and_then(Value::as_str) {
                                if !id_str.is_empty() {
                                    entry.call_id = id_str.to_string();
                                }
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name_piece) = func.get("name").and_then(Value::as_str) {
                                    if !name_piece.is_empty() {
                                        if entry.name.is_empty() {
                                            entry.name = name_piece.to_string();
                                        } else if entry.name != name_piece {
                                            entry.name.push_str(name_piece);
                                        }
                                    }
                                }
                                if let Some(arg_delta) =
                                    func.get("arguments").and_then(Value::as_str)
                                {
                                    entry.arguments.push_str(arg_delta);
                                    let call_id_now = entry.call_id.clone();
                                    let name_now = if entry.name.is_empty() {
                                        None
                                    } else {
                                        Some(entry.name.clone())
                                    };
                                    acc.emit_tool_delta(
                                        &call_id_now,
                                        name_now,
                                        arg_delta,
                                        on_event,
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(reason_str) = choice.get("finish_reason").and_then(Value::as_str) {
                    if reason_str == "length" || reason_str == "content_filter" {
                        return Err(ModelError::new(
                            ModelErrorCode::StreamTruncated,
                            false,
                            format!(
                                "chat completion choice terminated early with finish_reason '{}'",
                                reason_str
                            ),
                        ));
                    }
                    if reason_str == "tool_calls" || reason_str == "stop" {
                        for builder in tool_builders.values_mut() {
                            if !builder.finalized {
                                builder.finalized = true;
                                acc.finalize_tool_call(
                                    &builder.call_id,
                                    &builder.name,
                                    &builder.arguments,
                                    on_event,
                                )?;
                            }
                        }
                    }
                    finish_reason = Some(reason_str.to_string());
                }
            }
        }
    }

    if !acc.completed {
        return Err(ModelError::new(
            ModelErrorCode::StreamTruncated,
            !acc.visible_output_emitted,
            "openai-chat-completions SSE stream ended before [DONE]",
        ));
    }

    Ok(())
}

fn wire_tool_name(name: &str) -> String {
    name.replace('.', "__")
}

fn build_request_body(conn: &ModelConnection, req: &ModelRequest) -> Value {
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

            json!({
                "model": conn.model_id,
                "instructions": req.instructions,
                "input": input_vec,
                "tools": tools_vec,
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "max_output_tokens": max_tokens
            })
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
                        if let Some(calls) = messages
                            .last_mut()
                            .and_then(|message| message.get_mut("tool_calls"))
                            .and_then(Value::as_array_mut)
                        {
                            calls.push(call);
                        } else {
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

            json!({
                "model": conn.model_id,
                "messages": messages,
                "tools": tools_vec,
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_completion_tokens": max_tokens
            })
        }
    }
}

fn parse_token_usage(usage_val: &Value) -> TokenUsage {
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

fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(ms_str) = trimmed.strip_suffix("ms") {
        return ms_str.trim().parse::<u64>().ok();
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    if let Ok(seconds_f) = trimmed.parse::<f64>() {
        if seconds_f >= 0.0 {
            return Some((seconds_f * 1000.0) as u64);
        }
    }
    None
}

fn map_http_status_error(
    status: u16,
    provider_request_id: Option<String>,
    retry_after_ms: Option<u64>,
) -> ModelError {
    let (code, retryable, msg) = match status {
        401 => (
            ModelErrorCode::Unauthorized,
            false,
            "HTTP 401 unauthorized: check model API key in keyring".to_string(),
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

fn map_transport_step_error(
    step_err: TransportStepError,
    visible_output_emitted: bool,
) -> ModelError {
    match step_err {
        TransportStepError::Cancelled => ModelError::new(
            ModelErrorCode::Cancelled,
            false,
            "model stream cancelled by caller",
        ),
        TransportStepError::Timeout => ModelError::new(
            ModelErrorCode::Timeout,
            !visible_output_emitted,
            "model stream timed out waiting for next SSE frame",
        ),
        TransportStepError::RecordTooLarge(sz) => ModelError::new(
            ModelErrorCode::RecordTooLarge,
            false,
            format!("single SSE record exceeded budget ({} bytes)", sz),
        ),
        TransportStepError::Network(msg) => {
            if visible_output_emitted {
                ModelError::new(
                    ModelErrorCode::StreamTruncated,
                    false,
                    format!("stream disconnected after partial output: {}", msg),
                )
            } else {
                ModelError::new(
                    ModelErrorCode::NetworkError,
                    true,
                    format!("stream transport error before output: {}", msg),
                )
            }
        }
    }
}

async fn sleep_with_cancel(duration: Duration, cancel: &Arc<AtomicBool>) -> Result<(), ModelError> {
    let start = Instant::now();
    let step = Duration::from_millis(5);
    while start.elapsed() < duration {
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::new(
                ModelErrorCode::Cancelled,
                false,
                "model request cancelled during retry backoff",
            ));
        }
        let rem = duration.saturating_sub(start.elapsed());
        tokio::time::sleep(rem.min(step)).await;
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(ModelError::new(
            ModelErrorCode::Cancelled,
            false,
            "model request cancelled during retry backoff",
        ));
    }
    Ok(())
}

fn sanitize_error_text(raw: &str, api_key: Option<&str>) -> String {
    let mut out = raw.to_string();
    if let Some(k) = api_key {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            out = out.replace(trimmed, "[REDACTED]");
        }
    }
    out
}

/// A single scripted TCP write chunk for [`ModelLoopbackServer`].
#[derive(Debug, Clone)]
pub struct ScriptedStreamChunk {
    pub bytes: Vec<u8>,
    pub delay_ms: u64,
}

impl ScriptedStreamChunk {
    pub fn immediate(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            delay_ms: 0,
        }
    }

    pub fn delayed(delay_ms: u64, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            delay_ms,
        }
    }
}

/// Scripted HTTP response served by [`ModelLoopbackServer`].
#[derive(Debug, Clone)]
pub struct ScriptedModelResponse {
    pub status: u16,
    pub content_type: String,
    pub extra_headers: Vec<(String, String)>,
    pub chunks: Vec<ScriptedStreamChunk>,
    pub hold_open_ms_after_chunks: u64,
}

impl ScriptedModelResponse {
    /// Creates an SSE 200 OK response that writes the given byte chunks in order,
    /// flushing the TCP socket after each chunk.
    pub fn sse_chunks(chunks: Vec<ScriptedStreamChunk>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream; charset=utf-8".to_string(),
            extra_headers: vec![("x-request-id".to_string(), "req-loopback-sse-1".to_string())],
            chunks,
            hold_open_ms_after_chunks: 0,
        }
    }

    /// Creates an SSE 200 OK response from raw byte slices without artificial delay.
    pub fn sse_raw_slices(slices: &[&[u8]]) -> Self {
        Self::sse_chunks(
            slices
                .iter()
                .map(|s| ScriptedStreamChunk::immediate(s.to_vec()))
                .collect(),
        )
    }

    /// Creates a non-streaming HTTP status response (e.g. 401, 403, 429, 500) with optional headers.
    pub fn status_with_headers(
        status: u16,
        headers: Vec<(String, String)>,
        body: impl Into<String>,
    ) -> Self {
        let body_bytes = body.into().into_bytes();
        Self {
            status,
            content_type: "application/json; charset=utf-8".to_string(),
            extra_headers: headers,
            chunks: vec![ScriptedStreamChunk::immediate(body_bytes)],
            hold_open_ms_after_chunks: 0,
        }
    }
}

/// Loopback HTTP/SSE test server strictly bound to `127.0.0.1:0` (S-10, D-25, M2-K03, M2-A-13).
pub struct ModelLoopbackServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    routes: Arc<Mutex<BTreeMap<String, VecDeque<ScriptedModelResponse>>>>,
    request_counts: Arc<Mutex<BTreeMap<String, usize>>>,
    captured_bodies: Arc<Mutex<Vec<(String, String)>>>,
    active_streams: Arc<AtomicUsize>,
    accept_handle: Option<JoinHandle<()>>,
}

impl ModelLoopbackServer {
    pub fn start() -> Result<Self, String> {
        let bind_addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(bind_addr).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let local_addr = listener.local_addr().map_err(|e| e.to_string())?;
        assert!(
            local_addr.ip().is_loopback(),
            "ModelLoopbackServer must strictly bind to loopback 127.0.0.1"
        );

        let base_url = format!("http://127.0.0.1:{}", local_addr.port());
        let stop = Arc::new(AtomicBool::new(false));
        let routes: Arc<Mutex<BTreeMap<String, VecDeque<ScriptedModelResponse>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let request_counts: Arc<Mutex<BTreeMap<String, usize>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let captured_bodies: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let active_streams = Arc::new(AtomicUsize::new(0));

        let stop_clone = Arc::clone(&stop);
        let routes_clone = Arc::clone(&routes);
        let counts_clone = Arc::clone(&request_counts);
        let bodies_clone = Arc::clone(&captured_bodies);
        let active_clone = Arc::clone(&active_streams);

        let accept_handle = thread::spawn(move || {
            let mut worker_handles: Vec<JoinHandle<()>> = Vec::new();
            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop_conn = Arc::clone(&stop_clone);
                        let routes_conn = Arc::clone(&routes_clone);
                        let counts_conn = Arc::clone(&counts_clone);
                        let bodies_conn = Arc::clone(&bodies_clone);
                        let active_conn = Arc::clone(&active_clone);
                        worker_handles.push(thread::spawn(move || {
                            handle_loopback_connection(
                                stream,
                                stop_conn,
                                routes_conn,
                                counts_conn,
                                bodies_conn,
                                active_conn,
                            );
                        }));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for h in worker_handles {
                let _ = h.join();
            }
        });

        Ok(Self {
            base_url,
            stop,
            routes,
            request_counts,
            captured_bodies,
            active_streams,
            accept_handle: Some(accept_handle),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Enqueues a sequence of scripted responses for `path` (e.g. `"/responses"` or `"/chat/completions"`).
    pub fn enqueue_route(&self, path: impl Into<String>, responses: Vec<ScriptedModelResponse>) {
        let mut guard = self.routes.lock().unwrap();
        let queue = guard.entry(path.into()).or_default();
        queue.clear();
        for r in responses {
            queue.push_back(r);
        }
    }

    /// Returns the number of HTTP requests received for `path`.
    pub fn request_count(&self, path: &str) -> usize {
        self.request_counts
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// Returns all captured `(path, body_json)` pairs received by the loopback server.
    pub fn captured_requests(&self) -> Vec<(String, String)> {
        self.captured_bodies.lock().unwrap().clone()
    }

    /// Returns the number of currently active streaming connections on the server.
    pub fn active_stream_count(&self) -> usize {
        self.active_streams.load(Ordering::SeqCst)
    }
}

impl Drop for ModelLoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_loopback_connection(
    mut stream: TcpStream,
    stop: Arc<AtomicBool>,
    routes: Arc<Mutex<BTreeMap<String, VecDeque<ScriptedModelResponse>>>>,
    request_counts: Arc<Mutex<BTreeMap<String, usize>>>,
    captured_bodies: Arc<Mutex<Vec<(String, String)>>>,
    active_streams: Arc<AtomicUsize>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let (path, body_str) = match read_http_request(&mut stream) {
        Some(pair) => pair,
        None => return,
    };

    {
        let mut counts = request_counts.lock().unwrap();
        *counts.entry(path.clone()).or_insert(0) += 1;
    }
    {
        let mut bodies = captured_bodies.lock().unwrap();
        bodies.push((path.clone(), body_str));
    }

    let scripted = {
        let mut map = routes.lock().unwrap();
        if let Some(q) = map.get_mut(&path) {
            if q.len() > 1 {
                q.pop_front()
            } else {
                q.front().cloned()
            }
        } else {
            None
        }
    }
    .unwrap_or_else(|| {
        ScriptedModelResponse::status_with_headers(
            404,
            Vec::new(),
            r#"{"error":{"message":"unscripted loopback route"}}"#,
        )
    });

    let status_reason = match scripted.status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Response",
    };

    let mut header_block = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        scripted.status, status_reason, scripted.content_type
    );
    for (k, v) in &scripted.extra_headers {
        header_block.push_str(&format!("{}: {}\r\n", k, v));
    }
    header_block.push_str("\r\n");

    active_streams.fetch_add(1, Ordering::SeqCst);
    if stream.write_all(header_block.as_bytes()).is_ok() {
        let _ = stream.flush();
        for chunk in &scripted.chunks {
            if !sleep_server_loop(chunk.delay_ms, &stop) {
                break;
            }
            if stream.write_all(&chunk.bytes).is_err() {
                break;
            }
            if stream.flush().is_err() {
                break;
            }
        }
        if scripted.hold_open_ms_after_chunks > 0 {
            let start = Instant::now();
            let total = Duration::from_millis(scripted.hold_open_ms_after_chunks);
            while start.elapsed() < total && !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(15));
                // Probe with an SSE comment line so we immediately detect when client drops socket.
                if stream.write_all(b": keep-alive\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    active_streams.fetch_sub(1, Ordering::SeqCst);
}

fn sleep_server_loop(delay_ms: u64, stop: &Arc<AtomicBool>) -> bool {
    if delay_ms == 0 {
        return !stop.load(Ordering::Relaxed);
    }
    let start = Instant::now();
    let target = Duration::from_millis(delay_ms);
    while start.elapsed() < target {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let rem = target.saturating_sub(start.elapsed());
        thread::sleep(rem.min(Duration::from_millis(5)));
    }
    !stop.load(Ordering::Relaxed)
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::with_capacity(4096);
    let mut temp = [0u8; 2048];
    let mut header_end_idx = None;

    loop {
        match stream.read(&mut temp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&temp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    header_end_idx = Some(pos + 4);
                    break;
                }
                if buf.len() > 256 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let body_start = header_end_idx?;
    let header_str = String::from_utf8_lossy(&buf[..body_start]).to_string();
    let first_line = header_str.lines().next().unwrap_or("");
    let raw_target = first_line.split_whitespace().nth(1).unwrap_or("/");
    let path_only = raw_target.split('?').next().unwrap_or("/").to_string();

    let content_length = header_str
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    while buf.len().saturating_sub(body_start) < content_length {
        match stream.read(&mut temp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&temp[..n]),
            Err(_) => break,
        }
    }

    let body_slice = if buf.len() >= body_start {
        &buf[body_start..buf.len().min(body_start + content_length)]
    } else {
        &[]
    };
    let body_str = String::from_utf8_lossy(body_slice).to_string();
    Some((path_only, body_str))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Executes the full Spike `M2-K03` and Stage Case `M2-A-13` loopback verification matrix
/// across both `openai-responses` and `openai-chat-completions` protocols and returns a
/// structured JSON evidence report for `verify-stage --stage M2`.
pub fn run_m2_k03_and_a13_verification() -> Result<Value, String> {
    let server = ModelLoopbackServer::start()?;
    let client = ModelClient::new().map_err(|e| e.to_string())?;

    let responses_report = verify_protocol_suite(&client, &server, ModelProtocol::OpenAiResponses)?;
    let chat_report =
        verify_protocol_suite(&client, &server, ModelProtocol::OpenAiChatCompletions)?;

    // Verify unknown protocol rejection without fallback.
    let unknown_err = ModelProtocol::parse("anthropic-messages-v1")
        .expect_err("unknown protocol must be rejected");
    if unknown_err.code != ModelErrorCode::UnsupportedProtocol {
        return Err(format!(
            "expected UnsupportedProtocol for unknown protocol, got {:?}",
            unknown_err.code
        ));
    }

    // Verify cross-scope / cross-protocol opaque item rejection.
    let conn_resp = ModelConnection::loopback(
        "conn-resp",
        ModelProtocol::OpenAiResponses,
        server.base_url(),
        "gpt-5",
    );
    let mut cross_req = sample_request("conn-resp", "scope-a");
    cross_req.input_items.push(InputItem::ReasoningOpaque {
        provider_id: "conn-resp".to_string(),
        protocol: ModelProtocol::OpenAiChatCompletions,
        model_id: "gpt-5".to_string(),
        scope_hash: "scope-a".to_string(),
        payload: "enc_blob".to_string(),
    });
    let opaque_err = client
        .stream_request(
            &conn_resp,
            &cross_req,
            Some("test-key"),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("cross-protocol opaque item must be rejected");
    if opaque_err.code != ModelErrorCode::CrossScopeOpaqueRejected {
        return Err(format!(
            "expected CrossScopeOpaqueRejected, got {:?}",
            opaque_err.code
        ));
    }

    Ok(json!({
        "case_id": "M2-A-13",
        "spike_id": "M2-K03",
        "status": "passed",
        "sse_parser": "eventsource-stream 0.2.3 + reqwest 0.13.5",
        "loopback_bind": "127.0.0.1:0",
        "unknown_protocol_rejected": true,
        "cross_scope_opaque_rejected": true,
        "protocols": {
            "openai-responses": responses_report,
            "openai-chat-completions": chat_report
        }
    }))
}

fn sample_request(connection_id: &str, scope_hash: &str) -> ModelRequest {
    ModelRequest {
        request_id: "req-m2-a13".to_string(),
        run_id: "run-m2-a13".to_string(),
        generation: 1,
        connection_id: connection_id.to_string(),
        scope_hash: scope_hash.to_string(),
        instructions: "Only use host-verified ledger tools.".to_string(),
        input_items: vec![InputItem::UserMessage {
            content: "Explain AC-02 P&L.".to_string(),
        }],
        tool_definitions: vec![ToolDefinition {
            name: "report.period_breakdown".to_string(),
            description: "Query scoped period P&L breakdown".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "account_id": { "type": "string" },
                    "currency": { "type": "string" }
                },
                "required": ["account_id", "currency"]
            }),
        }],
        output_budget: 512,
    }
}

fn build_success_chunks_for_protocol(protocol: ModelProtocol) -> Vec<ScriptedStreamChunk> {
    // Split multi-byte UTF-8 character '资' ([0xE8, 0xB5, 0x84]) across two TCP writes
    // and split JSON tool call arguments across 3 SSE chunks, with SSE comment & multi-line data.
    match protocol {
        ModelProtocol::OpenAiResponses => {
            let frame_created = b": keep-alive comment\n\nevent: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_k03_1\"}}\n\n";
            // Frame with "资产净值 2068 USD" where '资' is [0xE8, 0xB5, 0x84]
            let mut text_part1 = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\ndata: \"delta\":\"".to_vec();
            text_part1.extend_from_slice(&[0xE8, 0xB5]); // first 2 bytes of '资'
            let mut text_part2 = vec![0x84]; // 3rd byte of '资'
            text_part2.extend_from_slice("产净值 2068 USD\"}\n\n".as_bytes());

            let tool_added = b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"call_id\":\"call_ac02_1\",\"type\":\"function_call\",\"name\":\"report.period_breakdown\"}}\n\n";
            let tool_delta_1 = b"event: response.function_call_arguments.delta\ndata: {\"item_id\":\"fc_1\",\"delta\":\"{\\\"account_id\\\":\\\"acc\"}\n\n";
            let tool_delta_2 = b"event: response.function_call_arguments.delta\ndata: {\"item_id\":\"fc_1\",\"delta\":\"-main\\\",\\\"currency\\\":\"}\n\n";
            let tool_delta_3 = b"event: response.function_call_arguments.delta\ndata: {\"item_id\":\"fc_1\",\"delta\":\"\\\"USD\\\"}\"}\n\n";
            let tool_done = b"event: response.function_call_arguments.done\ndata: {\"item_id\":\"fc_1\",\"call_id\":\"call_ac02_1\",\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"account_id\\\":\\\"acc-main\\\",\\\"currency\\\":\\\"USD\\\"}\"}\n\n";
            let completed = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_k03_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":42,\"output_tokens\":18,\"total_tokens\":60}}}\n\n";

            vec![
                ScriptedStreamChunk::immediate(frame_created.to_vec()),
                ScriptedStreamChunk::immediate(text_part1.to_vec()),
                ScriptedStreamChunk::delayed(5, text_part2.to_vec()),
                ScriptedStreamChunk::immediate(tool_added.to_vec()),
                ScriptedStreamChunk::immediate(tool_delta_1.to_vec()),
                ScriptedStreamChunk::delayed(3, tool_delta_2.to_vec()),
                ScriptedStreamChunk::delayed(3, tool_delta_3.to_vec()),
                ScriptedStreamChunk::immediate(tool_done.to_vec()),
                ScriptedStreamChunk::immediate(completed.to_vec()),
            ]
        }
        ModelProtocol::OpenAiChatCompletions => {
            let comment = b": keep-alive\n\n";
            let mut text_part1 = b"data: {\"id\":\"chatcmpl_k03_1\",\"choices\":[{\"index\":0,\ndata: \"delta\":{\"content\":\"".to_vec();
            text_part1.extend_from_slice(&[0xE8, 0xB5]); // first 2 bytes of '资'
            let mut text_part2 = vec![0x84]; // 3rd byte of '资'
            text_part2.extend_from_slice("产净值 2068 USD\"}}]}\n\n".as_bytes());

            let tc_chunk_1 = b"data: {\"id\":\"chatcmpl_k03_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_ac02_1\",\"type\":\"function\",\"function\":{\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"account_id\\\":\\\"acc\"}}]}}]}\n\n";
            let tc_chunk_2 = b"data: {\"id\":\"chatcmpl_k03_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"-main\\\",\\\"currency\\\":\"}}]}}]}\n\n";
            let tc_chunk_3 = b"data: {\"id\":\"chatcmpl_k03_1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"USD\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":18,\"total_tokens\":60}}\n\n";
            let done_frame = b"data: [DONE]\n\n";

            vec![
                ScriptedStreamChunk::immediate(comment.to_vec()),
                ScriptedStreamChunk::immediate(text_part1.to_vec()),
                ScriptedStreamChunk::delayed(5, text_part2.to_vec()),
                ScriptedStreamChunk::immediate(tc_chunk_1.to_vec()),
                ScriptedStreamChunk::delayed(3, tc_chunk_2.to_vec()),
                ScriptedStreamChunk::delayed(3, tc_chunk_3.to_vec()),
                ScriptedStreamChunk::immediate(done_frame.to_vec()),
            ]
        }
    }
}

fn build_truncated_chunks_for_protocol(protocol: ModelProtocol) -> Vec<ScriptedStreamChunk> {
    match protocol {
        ModelProtocol::OpenAiResponses => vec![
            ScriptedStreamChunk::immediate(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_trunc_1\"}}\n\n"
                    .to_vec(),
            ),
            ScriptedStreamChunk::immediate(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Partial NAV 2068\"}\n\n"
                    .to_vec(),
            ),
            // EOF without response.completed!
        ],
        ModelProtocol::OpenAiChatCompletions => vec![
            ScriptedStreamChunk::immediate(
                b"data: {\"id\":\"chatcmpl_trunc_1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Partial NAV 2068\"}}]}\n\n"
                    .to_vec(),
            ),
            // EOF without finish_reason or [DONE]!
        ],
    }
}

fn verify_protocol_suite(
    client: &ModelClient,
    server: &ModelLoopbackServer,
    protocol: ModelProtocol,
) -> Result<Value, String> {
    let sub_path = format!("/suite/{}", protocol.as_str());
    let base_url = format!("{}{}", server.base_url(), sub_path);
    let route_path = format!("{}{}", sub_path, protocol.endpoint_path());
    let mut conn = ModelConnection::loopback(
        format!("conn-{}", protocol.as_str()),
        protocol,
        &base_url,
        "gpt-5-mini",
    );
    conn.retry_policy.base_backoff_ms = 10;

    // 1. Streaming UTF-8 across chunk boundary + multi-line data + split tool call + terminal state
    server.enqueue_route(
        &route_path,
        vec![ScriptedModelResponse::sse_chunks(
            build_success_chunks_for_protocol(protocol),
        )],
    );
    let req = sample_request(&conn.id, "scope-ac02");
    let res = client
        .stream_request(
            &conn,
            &req,
            Some("sk-loopback-secret"),
            Arc::new(AtomicBool::new(false)),
        )
        .map_err(|e| format!("{} success case failed: {}", protocol, e))?;

    if res.accumulated_text != "资产净值 2068 USD" {
        return Err(format!(
            "{} UTF-8 text mismatch: got '{}'",
            protocol, res.accumulated_text
        ));
    }
    if !res.completed {
        return Err(format!("{} expected completed=true", protocol));
    }
    if res.ready_tool_calls.len() != 1 {
        return Err(format!(
            "{} expected 1 ready tool call, got {}",
            protocol,
            res.ready_tool_calls.len()
        ));
    }
    let (call_id, tool_name, tool_args) = &res.ready_tool_calls[0];
    if call_id != "call_ac02_1"
        || tool_name != "report.period_breakdown"
        || tool_args.get("account_id").and_then(Value::as_str) != Some("acc-main")
    {
        return Err(format!(
            "{} tool call pairing mismatch: {:?}",
            protocol, res.ready_tool_calls
        ));
    }
    let usage = res
        .usage
        .as_ref()
        .ok_or_else(|| format!("{} missing token usage", protocol))?;
    if usage.input_tokens != Some(42) || usage.output_tokens != Some(18) {
        return Err(format!("{} token usage mismatch: {:?}", protocol, usage));
    }

    // 2. Truncated stream after visible output: must fail with StreamTruncated, preserve partial_text,
    // and MUST NOT replay/retry the request!
    let count_before_trunc = server.request_count(&route_path);
    server.enqueue_route(
        &route_path,
        vec![
            ScriptedModelResponse::sse_chunks(build_truncated_chunks_for_protocol(protocol)),
            ScriptedModelResponse::sse_chunks(build_success_chunks_for_protocol(protocol)),
        ],
    );
    let trunc_err = client
        .stream_request(
            &conn,
            &req,
            Some("sk-loopback-secret"),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("truncated stream must fail");
    let count_after_trunc = server.request_count(&route_path);
    // Clear the unused backup response by popping it via a dummy or overwriting queue:
    server.routes.lock().unwrap().remove(&route_path);

    if trunc_err.code != ModelErrorCode::StreamTruncated {
        return Err(format!(
            "{} expected StreamTruncated, got {:?}",
            protocol, trunc_err.code
        ));
    }
    if trunc_err.partial_text != "Partial NAV 2068" {
        return Err(format!(
            "{} expected partial_text 'Partial NAV 2068', got '{}'",
            protocol, trunc_err.partial_text
        ));
    }
    if count_after_trunc - count_before_trunc != 1 {
        return Err(format!(
            "{} truncated stream after visible output must not retry, attempts={}",
            protocol,
            count_after_trunc - count_before_trunc
        ));
    }

    // 3. 401 and 403 must NOT retry (1 request each)
    for status in [401u16, 403u16] {
        let before = server.request_count(&route_path);
        server.enqueue_route(
            &route_path,
            vec![ScriptedModelResponse::status_with_headers(
                status,
                Vec::new(),
                r#"{"error":{"message":"auth denied"}}"#,
            )],
        );
        let auth_err = client
            .stream_request(
                &conn,
                &req,
                Some("sk-loopback-secret"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("401/403 must fail");
        let after = server.request_count(&route_path);
        server.routes.lock().unwrap().remove(&route_path);
        if after - before != 1 || auth_err.retryable {
            return Err(format!(
                "{} HTTP {} must not retry (attempts={}, retryable={})",
                protocol,
                status,
                after - before,
                auth_err.retryable
            ));
        }
    }

    // 4. 429 with Retry-After before visible output: retries and succeeds
    let before_429 = server.request_count(&route_path);
    server.enqueue_route(
        &route_path,
        vec![
            ScriptedModelResponse::status_with_headers(
                429,
                vec![("Retry-After".to_string(), "10ms".to_string())],
                r#"{"error":{"message":"rate limit"}}"#,
            ),
            ScriptedModelResponse::sse_chunks(build_success_chunks_for_protocol(protocol)),
        ],
    );
    let retry_ok = client
        .stream_request(
            &conn,
            &req,
            Some("sk-loopback-secret"),
            Arc::new(AtomicBool::new(false)),
        )
        .map_err(|e| format!("{} 429 retry failed: {}", protocol, e))?;
    let after_429 = server.request_count(&route_path);
    server.routes.lock().unwrap().remove(&route_path);
    if after_429 - before_429 != 2 || !retry_ok.completed {
        return Err(format!(
            "{} expected 2 attempts on 429->200, got {}",
            protocol,
            after_429 - before_429
        ));
    }

    // 5. Cancel during active stream releases resources promptly
    let mut slow_resp =
        ScriptedModelResponse::sse_chunks(build_truncated_chunks_for_protocol(protocol));
    slow_resp.hold_open_ms_after_chunks = 3_000;
    server.enqueue_route(&route_path, vec![slow_resp]);

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_trigger = Arc::clone(&cancel_flag);
    let cancel_start = Instant::now();
    let cancel_err = client
        .stream_request_with_callback(
            &conn,
            &req,
            Some("sk-loopback-secret"),
            cancel_flag,
            move |ev| {
                if matches!(ev.kind, ModelEventKind::TextDelta { .. }) {
                    cancel_trigger.store(true, Ordering::SeqCst);
                }
            },
        )
        .expect_err("cancelled stream must return Cancelled error");
    let cancel_elapsed_ms = cancel_start.elapsed().as_millis() as u64;
    server.routes.lock().unwrap().remove(&route_path);

    if cancel_err.code != ModelErrorCode::Cancelled {
        return Err(format!(
            "{} expected Cancelled error code, got {:?}",
            protocol, cancel_err.code
        ));
    }

    Ok(json!({
        "protocol": protocol.as_str(),
        "utf8_cross_chunk_streaming": "passed",
        "multiline_sse_and_comments": "passed",
        "split_tool_call_pairing": "passed",
        "terminal_event_validation": "passed",
        "truncated_stream_no_replay": "passed",
        "auth_401_403_no_retry": "passed",
        "rate_limit_429_retry_after": "passed",
        "cancel_during_stream": "passed",
        "cancel_latency_ms": cancel_elapsed_ms
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_m2_k03_and_a13_full_loopback_verification() {
        let report =
            run_m2_k03_and_a13_verification().expect("M2-K03 and M2-A-13 verification must pass");
        assert_eq!(report["status"], "passed");
        assert_eq!(
            report["protocols"]["openai-responses"]["utf8_cross_chunk_streaming"],
            "passed"
        );
        assert_eq!(
            report["protocols"]["openai-chat-completions"]["utf8_cross_chunk_streaming"],
            "passed"
        );
    }

    #[test]
    fn test_endpoint_validation_rejects_embedded_credentials_and_remote_http() {
        let mut conn = ModelConnection::loopback(
            "conn-1",
            ModelProtocol::OpenAiResponses,
            "http://127.0.0.1:8080/v1",
            "gpt-5",
        );
        assert!(conn.validate_endpoint().is_ok());

        conn.base_url = format!("{}//{}:{}@api.openai.com/v1", "https:", "user", "secret");
        let err = conn.validate_endpoint().unwrap_err();
        assert_eq!(err.code, ModelErrorCode::InvalidEndpoint);

        conn.base_url = "http://api.openai.com/v1".to_string();
        let err = conn.validate_endpoint().unwrap_err();
        assert_eq!(err.code, ModelErrorCode::InvalidEndpoint);

        conn.base_url = "https://api.openai.com/v1?api_key=sk-123".to_string();
        let err = conn.validate_endpoint().unwrap_err();
        assert_eq!(err.code, ModelErrorCode::InvalidEndpoint);
    }

    #[test]
    fn test_oversized_sse_record_rejected_with_record_too_large() {
        let server = ModelLoopbackServer::start().unwrap();
        let client = ModelClient::new().unwrap();

        let mut conn = ModelConnection::loopback(
            "conn-budget",
            ModelProtocol::OpenAiResponses,
            server.base_url(),
            "gpt-5",
        );
        conn.budget.max_sse_record_bytes = 128;

        let huge_line = format!(
            "event: response.output_text.delta\ndata: {{\"delta\":\"{}\"}}\n\n",
            "A".repeat(512)
        );
        server.enqueue_route(
            "/responses",
            vec![ScriptedModelResponse::sse_raw_slices(&[
                huge_line.as_bytes()
            ])],
        );

        let req = sample_request("conn-budget", "scope-1");
        let err = client
            .stream_request(
                &conn,
                &req,
                Some("sk-test"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("oversized SSE record must be rejected");
        assert_eq!(err.code, ModelErrorCode::RecordTooLarge);
    }

    #[test]
    fn test_duplicate_tool_call_id_deduplication_and_mismatch_rejection() {
        let server = ModelLoopbackServer::start().unwrap();
        let client = ModelClient::new().unwrap();
        let conn = ModelConnection::loopback(
            "conn-dup",
            ModelProtocol::OpenAiResponses,
            server.base_url(),
            "gpt-5",
        );
        let req = sample_request("conn-dup", "scope-1");

        // Same call_id + identical arguments -> deduplicated to 1 tool call
        let same_dup_sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
            "event: response.function_call_arguments.done\ndata: {\"call_id\":\"call_1\",\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"account_id\\\":\\\"a1\\\",\\\"currency\\\":\\\"USD\\\"}\"}\n\n",
            "event: response.function_call_arguments.done\ndata: {\"call_id\":\"call_1\",\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"currency\\\":\\\"USD\\\",\\\"account_id\\\":\\\"a1\\\"}\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n"
        );
        server.enqueue_route(
            "/responses",
            vec![ScriptedModelResponse::sse_raw_slices(&[
                same_dup_sse.as_bytes()
            ])],
        );
        let ok_res = client
            .stream_request(
                &conn,
                &req,
                Some("sk-test"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect("identical duplicate call_id should deduplicate");
        assert_eq!(ok_res.ready_tool_calls.len(), 1);

        // Same call_id + conflicting arguments -> rejected with DuplicateToolCallMismatch
        server.routes.lock().unwrap().remove("/responses");
        let mismatch_sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\"}}\n\n",
            "event: response.function_call_arguments.done\ndata: {\"call_id\":\"call_1\",\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"account_id\\\":\\\"a1\\\",\\\"currency\\\":\\\"USD\\\"}\"}\n\n",
            "event: response.function_call_arguments.done\ndata: {\"call_id\":\"call_1\",\"name\":\"report.period_breakdown\",\"arguments\":\"{\\\"account_id\\\":\\\"a2\\\",\\\"currency\\\":\\\"USD\\\"}\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"status\":\"completed\"}}\n\n"
        );
        server.enqueue_route(
            "/responses",
            vec![ScriptedModelResponse::sse_raw_slices(&[
                mismatch_sse.as_bytes()
            ])],
        );
        let mismatch_err = client
            .stream_request(
                &conn,
                &req,
                Some("sk-test"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("conflicting duplicate call_id must fail");
        assert_eq!(mismatch_err.code, ModelErrorCode::DuplicateToolCallMismatch);
    }

    #[test]
    fn test_chat_completions_length_finish_reason_marks_truncated() {
        let server = ModelLoopbackServer::start().unwrap();
        let client = ModelClient::new().unwrap();
        let conn = ModelConnection::loopback(
            "conn-chat-len",
            ModelProtocol::OpenAiChatCompletions,
            server.base_url(),
            "gpt-5-mini",
        );
        let req = sample_request("conn-chat-len", "scope-1");

        let length_sse = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Cut off\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        server.enqueue_route(
            "/chat/completions",
            vec![ScriptedModelResponse::sse_raw_slices(&[
                length_sse.as_bytes()
            ])],
        );
        let err = client
            .stream_request(
                &conn,
                &req,
                Some("sk-test"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("finish_reason=length must be treated as StreamTruncated");
        assert_eq!(err.code, ModelErrorCode::StreamTruncated);
        assert_eq!(err.partial_text, "Cut off");
    }
}
