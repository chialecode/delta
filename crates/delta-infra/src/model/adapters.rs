//! Protocol adapters: Responses and Chat Completions encoding/decoding into
//! common typed events (rust-model-client.md §4). Protocol fields never leak
//! across adapters and unknown protocols are rejected, never guessed.

use delta_app::ai::runtime::{
    ModelConnectionConfig, ModelError, ModelEvent, ModelRequest, Protocol, StopReason,
};

/// Per-protocol adapter state for one streamed request.
pub enum AdapterState {
    Responses {
        saw_terminal: bool,
    },
    Chat {
        /// tool index → (call_id, name, accumulated arguments)
        pending_tools: std::collections::HashMap<u64, (String, String, String)>,
        finish_reason: Option<String>,
        saw_done: bool,
    },
}

impl AdapterState {
    pub fn for_protocol(p: Protocol) -> Result<Self, ModelError> {
        match p {
            Protocol::OpenaiResponses => Ok(AdapterState::Responses {
                saw_terminal: false,
            }),
            Protocol::OpenaiChatCompletions => Ok(AdapterState::Chat {
                pending_tools: std::collections::HashMap::new(),
                finish_reason: None,
                saw_done: false,
            }),
        }
    }
}

pub fn encode_request(
    connection: &ModelConnectionConfig,
    req: &ModelRequest,
) -> Result<serde_json::Value, ModelError> {
    match connection.protocol {
        Protocol::OpenaiResponses => encode_responses(req, connection),
        Protocol::OpenaiChatCompletions => encode_chat(req, connection),
    }
}

fn encode_responses(
    req: &ModelRequest,
    connection: &ModelConnectionConfig,
) -> Result<serde_json::Value, ModelError> {
    let mut input: Vec<serde_json::Value> = Vec::new();
    for item in &req.input_items {
        if let Some(encoded) = encode_responses_tool_item(item) {
            input.push(encoded);
            continue;
        }
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = item.get("content");
        match role {
            "system" | "user" => {
                input.push(serde_json::json!({
                    "role": role,
                    "content": [{ "type": "input_text", "text": content.cloned().unwrap_or_default() }],
                }));
            }
            "assistant" => {
                input.push(serde_json::json!({
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": content.cloned().unwrap_or_default() }],
                }));
            }
            _ => {
                return Err(ModelError {
                    code: "PROTOCOL_ERROR".into(),
                    retryable: false,
                    safe_message: "unsupported input item for responses protocol".into(),
                    provider_request_id: None,
                    retry_after_secs: None,
                })
            }
        }
    }
    Ok(serde_json::json!({
        "model": connection.model_id,
        "instructions": req.instructions,
        "input": input,
        "tools": req.tool_definitions,
        "stream": true,
        "store": false,
    }))
}

fn encode_responses_tool_item(item: &serde_json::Value) -> Option<serde_json::Value> {
    match item.get("type").and_then(|t| t.as_str()) {
        Some("function_call") => Some(serde_json::json!({
            "type": "function_call",
            "call_id": item.get("call_id").cloned().unwrap_or_default(),
            "name": item.get("name").cloned().unwrap_or_default(),
            "arguments": item.get("arguments").cloned().unwrap_or_default(),
        })),
        Some("function_call_output") => Some(serde_json::json!({
            "type": "function_call_output",
            "call_id": item.get("call_id").cloned().unwrap_or_default(),
            "output": item.get("output").cloned().unwrap_or_default(),
        })),
        _ => None,
    }
}

fn encode_chat(
    req: &ModelRequest,
    connection: &ModelConnectionConfig,
) -> Result<serde_json::Value, ModelError> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(serde_json::json!({ "role": "system", "content": req.instructions }));
    }
    let mut pending_calls: Vec<serde_json::Value> = Vec::new();
    let flush_calls = |messages: &mut Vec<serde_json::Value>,
                       pending: &mut Vec<serde_json::Value>| {
        if pending.is_empty() {
            return;
        }
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": std::mem::take(pending),
        }));
    };
    for item in &req.input_items {
        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
            pending_calls.push(serde_json::json!({
                "id": item.get("call_id").cloned().unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or_default(),
                    "arguments": item.get("arguments").cloned().unwrap_or_default(),
                }
            }));
            continue;
        }
        if item.get("type").and_then(|t| t.as_str()) == Some("function_call_output") {
            flush_calls(&mut messages, &mut pending_calls);
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or_default(),
                "content": item.get("output").cloned().unwrap_or_default(),
            }));
            continue;
        }
        flush_calls(&mut messages, &mut pending_calls);
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = item.get("content");
        match role {
            "system" | "user" | "assistant" => {
                messages.push(serde_json::json!({
                    "role": role,
                    "content": content.cloned().unwrap_or_default(),
                }));
            }
            _ => {
                return Err(ModelError {
                    code: "PROTOCOL_ERROR".into(),
                    retryable: false,
                    safe_message: "unsupported input item for chat protocol".into(),
                    provider_request_id: None,
                    retry_after_secs: None,
                })
            }
        }
    }
    flush_calls(&mut messages, &mut pending_calls);
    let tools: Vec<serde_json::Value> = req
        .tool_definitions
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.get("name").cloned().unwrap_or_default(),
                    "description": t.get("description").cloned().unwrap_or_default(),
                    "parameters": t.get("parameters").cloned().unwrap_or_default(),
                },
            })
        })
        .collect();
    Ok(serde_json::json!({
        "model": connection.model_id,
        "messages": messages,
        "tools": tools,
        "stream": true,
    }))
}

/// One SSE event through the protocol state machine; may emit several typed
/// events (e.g. usage + terminal).
pub fn handle_event(state: &mut AdapterState, _kind: &str, data: &str) -> Vec<ModelEvent> {
    match state {
        AdapterState::Responses { saw_terminal } => handle_responses_event(data, saw_terminal),
        AdapterState::Chat {
            pending_tools,
            finish_reason,
            saw_done,
        } => handle_chat_event(data, pending_tools, finish_reason, saw_done),
    }
}

/// EOF handling: Responses requires a terminal event; Chat requires [DONE].
pub fn on_eof(state: &mut AdapterState) -> Option<ModelEvent> {
    match state {
        AdapterState::Responses { saw_terminal } => {
            if *saw_terminal {
                None
            } else {
                Some(fail("stream ended without a Responses terminal event"))
            }
        }
        AdapterState::Chat {
            finish_reason,
            saw_done,
            ..
        } => {
            if *saw_done && finish_reason.is_some() {
                None
            } else {
                Some(fail("chat stream ended without finish_reason/[DONE]"))
            }
        }
    }
}

fn fail(msg: &str) -> ModelEvent {
    ModelEvent::Failed {
        error: ModelError {
            code: "PROTOCOL_ERROR".into(),
            retryable: false,
            safe_message: msg.into(),
            provider_request_id: None,
            retry_after_secs: None,
        },
    }
}

fn one(ev: ModelEvent) -> Vec<ModelEvent> {
    vec![ev]
}

// ---- Responses -------------------------------------------------------------

fn handle_responses_event(data: &str, saw_terminal: &mut bool) -> Vec<ModelEvent> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return one(fail(&format!("responses event json: {e}"))),
    };
    let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match t {
        "response.output_text.delta" => one(ModelEvent::TextDelta {
            delta: v
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "response.function_call_arguments.delta" => one(ModelEvent::ToolCallDelta {
            call_id: v
                .get("item_id")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
            name: String::new(),
            args_delta: v
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or_default();
            if item.get("type").and_then(|x| x.as_str()) == Some("function_call") {
                let arguments = item
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                // Arguments must be complete JSON before the call is ready.
                if serde_json::from_str::<serde_json::Value>(&arguments).is_err() {
                    return one(fail("function_call arguments are not complete JSON"));
                }
                return one(ModelEvent::ToolCallReady {
                    call_id: item
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                    arguments,
                });
            }
            Vec::new()
        }
        "response.completed" => {
            *saw_terminal = true;
            let response = v.get("response").cloned().unwrap_or_default();
            let usage = response.get("usage").cloned();
            let mut out = vec![ModelEvent::Usage {
                input_tokens: usage
                    .as_ref()
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|x| x.as_u64()),
                output_tokens: usage
                    .as_ref()
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|x| x.as_u64()),
            }];
            out.push(ModelEvent::Completed {
                stop_reason: StopReason::End,
            });
            out
        }
        "response.incomplete" => {
            *saw_terminal = true;
            one(ModelEvent::Failed {
                error: ModelError {
                    code: "INCOMPLETE".into(),
                    retryable: false,
                    safe_message: "response incomplete (max tokens or interruption)".into(),
                    provider_request_id: None,
                    retry_after_secs: None,
                },
            })
        }
        "response.failed" => {
            *saw_terminal = true;
            let msg = v
                .pointer("/response/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("response failed");
            one(ModelEvent::Failed {
                error: ModelError {
                    code: "PROVIDER_UNAVAILABLE".into(),
                    retryable: false,
                    safe_message: msg.into(),
                    provider_request_id: None,
                    retry_after_secs: None,
                },
            })
        }
        // Unknown non-critical events are ignored and counted upstream.
        _ => Vec::new(),
    }
}

// ---- Chat Completions --------------------------------------------------------

fn handle_chat_event(
    data: &str,
    pending_tools: &mut std::collections::HashMap<u64, (String, String, String)>,
    finish_reason: &mut Option<String>,
    saw_done: &mut bool,
) -> Vec<ModelEvent> {
    let data = data.trim();
    if data == "[DONE]" {
        *saw_done = true;
        return one(match finish_reason.as_deref() {
            Some("stop") | Some("tool_calls") => ModelEvent::Completed {
                stop_reason: if finish_reason.as_deref() == Some("tool_calls") {
                    StopReason::ToolUse
                } else {
                    StopReason::End
                },
            },
            Some("length") => ModelEvent::Completed {
                stop_reason: StopReason::Length,
            },
            Some("content_filter") => ModelEvent::Completed {
                stop_reason: StopReason::ContentFilter,
            },
            _ => fail("chat stream completed without finish_reason"),
        });
    }
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return one(fail(&format!("chat chunk json: {e}"))),
    };
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("provider error");
        return one(fail(msg));
    }
    let mut out: Vec<ModelEvent> = Vec::new();
    // Usage may ride on any chunk (typically the last); missing fields stay
    // unknown — never zero.
    if let Some(usage) = v.get("usage").filter(|u| u.is_object()) {
        out.push(ModelEvent::Usage {
            input_tokens: usage.get("prompt_tokens").and_then(|x| x.as_u64()),
            output_tokens: usage.get("completion_tokens").and_then(|x| x.as_u64()),
        });
    }
    let choice = v.pointer("/choices/0").cloned().unwrap_or_default();
    let mut fr: Option<String> = None;
    if let Some(f) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        *finish_reason = Some(f.to_string());
        fr = Some(f.to_string());
    }
    let delta = choice.get("delta").cloned().unwrap_or_default();
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            out.push(ModelEvent::TextDelta {
                delta: content.to_string(),
            });
            return out;
        }
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            let entry = pending_tools.entry(index).or_default();
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                entry.0 = id.to_string();
            }
            if let Some(name) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                entry.1 = name.to_string();
            }
            if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                entry.2.push_str(args);
            }
            out.push(ModelEvent::ToolCallDelta {
                call_id: entry.0.clone(),
                name: entry.1.clone(),
                args_delta: tc
                    .pointer("/function/arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    // finish_reason "tool_calls": all accumulated calls become ready (each
    // validated for complete JSON before execution).
    if fr.as_deref() == Some("tool_calls") && !pending_tools.is_empty() {
        let mut indexes: Vec<u64> = pending_tools.keys().copied().collect();
        indexes.sort_unstable();
        for idx in indexes {
            let (call_id, name, arguments) = pending_tools.remove(&idx).expect("checked");
            if serde_json::from_str::<serde_json::Value>(&arguments).is_err() {
                return one(fail("chat tool arguments are not complete JSON"));
            }
            out.push(ModelEvent::ToolCallReady {
                call_id,
                name,
                arguments,
            });
        }
    }
    out
}
