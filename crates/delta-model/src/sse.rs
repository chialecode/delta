use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::Value;

use crate::protocol::{
    parse_token_usage, sanitize_error_text, wire_tool_name, ModelConnection, ModelError,
    ModelErrorCode, ModelEvent, ModelEventKind, ModelRequest, ModelStreamResult, TokenUsage,
};

pub(crate) const MAX_SSE_FRAMES: usize = 65_536;
pub(crate) const MAX_ACCUMULATED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) enum TransportStepError {
    Cancelled,
    Timeout { total_budget_exceeded: bool },
    RecordTooLarge(usize),
    Network(String),
}

impl fmt::Display for TransportStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("stream cancelled"),
            Self::Timeout {
                total_budget_exceeded: true,
            } => f.write_str("stream exceeded total timeout budget"),
            Self::Timeout {
                total_budget_exceeded: false,
            } => f.write_str("stream timed out waiting for bytes"),
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

pub(crate) struct StreamAccumulator<'a> {
    req: &'a ModelRequest,
    conn: &'a ModelConnection,
    pub(crate) next_sequence: u64,
    pub(crate) events: Vec<ModelEvent>,
    pub(crate) accumulated_text: String,
    ready_tool_calls: Vec<(String, String, Value)>,
    completed_tool_calls_raw: Vec<(String, String, String)>,
    seen_tool_calls: BTreeMap<String, (String, Value)>,
    pub(crate) reasoning_items: Vec<String>,
    usage: Option<TokenUsage>,
    pub(crate) provider_request_id: Option<String>,
    started_emitted: bool,
    pub(crate) visible_output_emitted: bool,
    pub(crate) completed: bool,
    ignored_event_count: usize,
    accumulated_bytes: usize,
}

impl<'a> StreamAccumulator<'a> {
    pub(crate) fn new(req: &'a ModelRequest, conn: &'a ModelConnection) -> Self {
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
            accumulated_bytes: 0,
        }
    }

    fn account_bytes(&mut self, bytes: usize) -> Result<(), ModelError> {
        self.accumulated_bytes = self.accumulated_bytes.saturating_add(bytes);
        if self.accumulated_bytes > MAX_ACCUMULATED_BYTES {
            return Err(ModelError::new(
                ModelErrorCode::RecordTooLarge,
                false,
                format!(
                    "accumulated stream payload exceeded budget ({} bytes)",
                    MAX_ACCUMULATED_BYTES
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn emit<F>(&mut self, kind: ModelEventKind, on_event: &mut F)
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

    fn emit_text_delta<F>(&mut self, delta: &str, on_event: &mut F) -> Result<(), ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        if delta.is_empty() {
            return Ok(());
        }
        self.account_bytes(delta.len())?;
        self.ensure_started(None, on_event);
        self.visible_output_emitted = true;
        self.accumulated_text.push_str(delta);
        self.emit(
            ModelEventKind::TextDelta {
                delta: delta.to_string(),
            },
            on_event,
        );
        Ok(())
    }

    fn emit_reasoning_item<F>(
        &mut self,
        payload: String,
        on_event: &mut F,
    ) -> Result<(), ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        if payload.is_empty() {
            return Ok(());
        }
        self.account_bytes(payload.len())?;
        self.ensure_started(None, on_event);
        self.visible_output_emitted = true;
        self.reasoning_items.push(payload.clone());
        self.emit(ModelEventKind::ReasoningItem { payload }, on_event);
        Ok(())
    }

    fn emit_tool_delta<F>(
        &mut self,
        call_id: &str,
        name: Option<String>,
        arguments_delta: &str,
        on_event: &mut F,
    ) -> Result<(), ModelError>
    where
        F: FnMut(&ModelEvent),
    {
        if arguments_delta.is_empty() {
            return Ok(());
        }
        self.account_bytes(arguments_delta.len())?;
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
        Ok(())
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
        self.account_bytes(canonical_json.len())?;
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

    pub(crate) fn decorate_error(&self, mut err: ModelError) -> ModelError {
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

    pub(crate) fn into_result(self) -> ModelStreamResult {
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

pub(crate) async fn process_responses_sse<S, F>(
    sse_stream: &mut S,
    acc: &mut StreamAccumulator<'_>,
    max_record_bytes: usize,
    api_key: Option<&str>,
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
    let mut frame_count = 0usize;

    while let Some(next_item) = sse_stream.next().await {
        frame_count += 1;
        if acc.events.len() >= 16_384 || frame_count > MAX_SSE_FRAMES {
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
                    api_key,
                ));
            }
            Err(other) => {
                return Err(ModelError::new(
                    ModelErrorCode::ProtocolError,
                    false,
                    format!(
                        "SSE framing/UTF-8 error in openai-responses stream: {}",
                        sanitize_error_text(&other.to_string(), api_key)
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
                acc.emit_text_delta(delta, on_event)?;
            }
            "response.output_text.done" => {
                if acc.accumulated_text.is_empty() {
                    if let Some(text) = payload.get("text").and_then(Value::as_str) {
                        acc.emit_text_delta(text, on_event)?;
                    }
                }
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
                acc.emit_tool_delta(&resolved_call_id, name_for_event, delta, on_event)?;
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
                        acc.emit_reasoning_item(payload, on_event)?;
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
                    format!(
                        "openai-responses terminated as incomplete ({})",
                        sanitize_error_text(reason, api_key)
                    ),
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
                    format!(
                        "openai-responses failed: {}",
                        sanitize_error_text(msg, api_key)
                    ),
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

pub(crate) async fn process_chat_completions_sse<S, F>(
    sse_stream: &mut S,
    acc: &mut StreamAccumulator<'_>,
    max_record_bytes: usize,
    api_key: Option<&str>,
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
    let mut frame_count = 0usize;

    while let Some(next_item) = sse_stream.next().await {
        frame_count += 1;
        if acc.events.len() >= 16_384 || frame_count > MAX_SSE_FRAMES {
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
                    api_key,
                ));
            }
            Err(other) => {
                return Err(ModelError::new(
                    ModelErrorCode::ProtocolError,
                    false,
                    format!(
                        "SSE framing/UTF-8 error in openai-chat-completions stream: {}",
                        sanitize_error_text(&other.to_string(), api_key)
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
                            sanitize_error_text(other, api_key)
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
                format!(
                    "openai-chat-completions error: {}",
                    sanitize_error_text(msg, api_key)
                ),
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
                        acc.emit_text_delta(text, on_event)?;
                    }
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                    {
                        if !reasoning.is_empty() {
                            acc.emit_reasoning_item(reasoning.to_string(), on_event)?;
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
                                    )?;
                                }
                            }
                        }
                    }
                }

                if let Some(reason_str) = choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    if reason_str == "length" || reason_str == "content_filter" {
                        return Err(ModelError::new(
                            ModelErrorCode::StreamTruncated,
                            false,
                            format!(
                                "chat completion choice terminated early with finish_reason '{}'",
                                sanitize_error_text(reason_str, api_key)
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

pub(crate) fn map_transport_step_error(
    step_err: TransportStepError,
    visible_output_emitted: bool,
    api_key: Option<&str>,
) -> ModelError {
    match step_err {
        TransportStepError::Cancelled => ModelError::new(
            ModelErrorCode::Cancelled,
            false,
            "model stream cancelled by caller",
        ),
        TransportStepError::Timeout {
            total_budget_exceeded,
        } => ModelError::new(
            ModelErrorCode::Timeout,
            !visible_output_emitted && !total_budget_exceeded,
            if total_budget_exceeded {
                "model stream exceeded total timeout budget"
            } else {
                "model stream timed out waiting for next SSE frame"
            },
        ),
        TransportStepError::RecordTooLarge(sz) => ModelError::new(
            ModelErrorCode::RecordTooLarge,
            false,
            format!("single SSE record exceeded budget ({} bytes)", sz),
        ),
        TransportStepError::Network(msg) => {
            let safe = sanitize_error_text(&msg, api_key);
            if visible_output_emitted {
                ModelError::new(
                    ModelErrorCode::StreamTruncated,
                    false,
                    format!("stream disconnected after partial output: {}", safe),
                )
            } else {
                ModelError::new(
                    ModelErrorCode::NetworkError,
                    true,
                    format!("stream transport error before output: {}", safe),
                )
            }
        }
    }
}
