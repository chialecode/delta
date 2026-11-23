use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::Value;

use crate::protocol::{
    build_request_body, map_http_status_error, parse_retry_after_ms, sanitize_error_text,
    InputItem, ModelCapabilities, ModelConnection, ModelError, ModelErrorCode, ModelEvent,
    ModelEventKind, ModelProtocol, ModelRequest, ModelStreamResult,
};
use crate::sse::{
    process_chat_completions_sse, process_responses_sse, StreamAccumulator, TransportStepError,
};

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CACHED_CLIENTS: usize = 16;

type AttemptFailure = Box<(ModelError, bool, u64)>;

/// Pure-Rust model client supporting `openai-responses` and `openai-chat-completions`.
pub struct ModelClient {
    runtime: tokio::runtime::Runtime,
    http_clients: Mutex<BTreeMap<u64, reqwest::Client>>,
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
        Ok(Self {
            runtime,
            http_clients: Mutex::new(BTreeMap::new()),
        })
    }

    fn http_client_for_connect_ms(
        &self,
        connect_ms: u64,
        api_key: Option<&str>,
    ) -> Result<reqwest::Client, ModelError> {
        let key = connect_ms.max(1);
        let mut cache = self
            .http_clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = cache.get(&key) {
            return Ok(existing.clone());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(key))
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
        if cache.len() >= MAX_CACHED_CLIENTS {
            cache.clear();
        }
        cache.insert(key, client.clone());
        Ok(client)
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
        let overall_deadline = overall_start + Duration::from_millis(total_budget_ms);
        let max_retries = conn.retry_policy.max_retries_before_visible_output;
        let client = self.http_client_for_connect_ms(conn.timeouts.connect_ms, api_key)?;

        let emit_failed = |seq: u64, err: &ModelError, on_event: &mut F| {
            on_event(&ModelEvent {
                request_id: req.request_id.clone(),
                run_id: req.run_id.clone(),
                generation: req.generation,
                sequence: seq,
                kind: ModelEventKind::Failed(err.clone()),
            });
        };

        let mut attempt = 0usize;
        loop {
            if cancel.load(Ordering::SeqCst) {
                let err = ModelError::new(
                    ModelErrorCode::Cancelled,
                    false,
                    "model request cancelled before dispatch",
                );
                emit_failed(1, &err, on_event);
                return Err(err);
            }

            let now = Instant::now();
            if now >= overall_deadline {
                let err = ModelError::new(
                    ModelErrorCode::Timeout,
                    false,
                    "model request exceeded total timeout budget",
                );
                emit_failed(1, &err, on_event);
                return Err(err);
            }

            let attempt_res = self
                .execute_single_attempt(
                    &client,
                    &endpoint_url,
                    conn,
                    req,
                    &body_value,
                    api_key,
                    Arc::clone(&cancel),
                    overall_deadline,
                    on_event,
                )
                .await;

            match attempt_res {
                Ok(result) => return Ok(result),
                Err(boxed) => {
                    let (err, visible_output_emitted, next_seq) = *boxed;
                    let can_retry = !visible_output_emitted
                        && err.retryable
                        && attempt < max_retries
                        && !cancel.load(Ordering::SeqCst);

                    if !can_retry {
                        emit_failed(next_seq, &err, on_event);
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
                        emit_failed(next_seq, &err, on_event);
                        return Err(err);
                    }

                    if let Err(cancel_err) =
                        sleep_with_cancel(Duration::from_millis(backoff_ms), &cancel).await
                    {
                        emit_failed(next_seq, &cancel_err, on_event);
                        return Err(cancel_err);
                    }
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
        overall_deadline: Instant,
        on_event: &mut F,
    ) -> Result<ModelStreamResult, AttemptFailure>
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

        let remaining_ms = overall_deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        if remaining_ms == 0 {
            return Err(Box::new((
                ModelError::new(
                    ModelErrorCode::Timeout,
                    false,
                    "model request exceeded total timeout budget",
                ),
                false,
                1,
            )));
        }

        let send_step_limit = Duration::from_millis(
            conn.timeouts
                .connect_ms
                .max(conn.timeouts.idle_ms)
                .min(remaining_ms)
                .max(1),
        );
        let send_deadline = Instant::now() + send_step_limit;
        let effective_send_deadline = send_deadline.min(overall_deadline);

        let response =
            match await_with_cancel(req_builder.send(), effective_send_deadline, &cancel).await {
                AwaitOutcome::Ready(Ok(resp)) => resp,
                AwaitOutcome::Ready(Err(e)) => {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(Box::new((
                            ModelError::new(
                                ModelErrorCode::Cancelled,
                                false,
                                "model request cancelled during send",
                            ),
                            false,
                            1,
                        )));
                    }
                    let is_timeout = e.is_timeout();
                    return Err(Box::new((
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
                        1,
                    )));
                }
                AwaitOutcome::Cancelled => {
                    return Err(Box::new((
                        ModelError::new(
                            ModelErrorCode::Cancelled,
                            false,
                            "model request cancelled during send",
                        ),
                        false,
                        1,
                    )));
                }
                AwaitOutcome::TimedOut => {
                    let budget_exhausted = Instant::now() >= overall_deadline;
                    return Err(Box::new((
                        ModelError::new(
                            ModelErrorCode::Timeout,
                            !budget_exhausted,
                            "timed out waiting for HTTP response headers",
                        ),
                        false,
                        1,
                    )));
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
            return Err(Box::new((err, false, 1)));
        }

        let mut acc = StreamAccumulator::new(req, conn);
        if header_req_id.is_some() {
            acc.provider_request_id = header_req_id;
        }

        let idle_timeout = Duration::from_millis(conn.timeouts.idle_ms.max(1));
        let max_record_bytes = conn.budget.max_sse_record_bytes;
        let cancel_for_stream = Arc::clone(&cancel);

        struct UnfoldState<S> {
            stream: S,
            cancel_flag: Arc<AtomicBool>,
            idle_dur: Duration,
            overall_deadline: Instant,
            max_bytes: usize,
            current_record_bytes: usize,
            consecutive_line_breaks: u8,
            prev_was_cr: bool,
            halted: bool,
        }

        let inner_bytes = Box::pin(response.bytes_stream());
        let guarded_bytes = futures_util::stream::unfold(
            UnfoldState {
                stream: inner_bytes,
                cancel_flag: cancel_for_stream,
                idle_dur: idle_timeout,
                overall_deadline,
                max_bytes: max_record_bytes,
                current_record_bytes: 0,
                consecutive_line_breaks: 0,
                prev_was_cr: false,
                halted: false,
            },
            |mut st| async move {
                if st.halted {
                    return None;
                }
                if st.cancel_flag.load(Ordering::SeqCst) {
                    st.halted = true;
                    return Some((Err(TransportStepError::Cancelled), st));
                }

                let now = Instant::now();
                if now >= st.overall_deadline {
                    st.halted = true;
                    return Some((
                        Err(TransportStepError::Timeout {
                            total_budget_exceeded: true,
                        }),
                        st,
                    ));
                }
                let step_deadline = (now + st.idle_dur).min(st.overall_deadline);
                let outcome =
                    await_with_cancel(st.stream.next(), step_deadline, &st.cancel_flag).await;

                match outcome {
                    AwaitOutcome::Cancelled => {
                        st.halted = true;
                        Some((Err(TransportStepError::Cancelled), st))
                    }
                    AwaitOutcome::TimedOut => {
                        let total_budget_exceeded = Instant::now() >= st.overall_deadline;
                        st.halted = true;
                        Some((
                            Err(TransportStepError::Timeout {
                                total_budget_exceeded,
                            }),
                            st,
                        ))
                    }
                    AwaitOutcome::Ready(None) => None,
                    AwaitOutcome::Ready(Some(Err(e))) => {
                        st.halted = true;
                        Some((Err(TransportStepError::Network(e.to_string())), st))
                    }
                    AwaitOutcome::Ready(Some(Ok(chunk))) => {
                        if st.cancel_flag.load(Ordering::SeqCst) {
                            st.halted = true;
                            return Some((Err(TransportStepError::Cancelled), st));
                        }

                        // Pre-buffering check for oversized SSE records before eventsource_stream
                        // accumulates unbounded lines in memory. Supports LF, CRLF, and CR-only
                        // line endings per the WHATWG SSE specification.
                        let raw_limit = st.max_bytes.saturating_add(256);
                        for &b in chunk.iter() {
                            st.current_record_bytes = st.current_record_bytes.saturating_add(1);
                            if b == b'\n' {
                                if !st.prev_was_cr {
                                    st.consecutive_line_breaks =
                                        st.consecutive_line_breaks.saturating_add(1);
                                }
                                st.prev_was_cr = false;
                            } else if b == b'\r' {
                                st.consecutive_line_breaks =
                                    st.consecutive_line_breaks.saturating_add(1);
                                st.prev_was_cr = true;
                            } else {
                                st.consecutive_line_breaks = 0;
                                st.prev_was_cr = false;
                            }
                            if st.current_record_bytes > raw_limit {
                                let size = st.current_record_bytes;
                                st.halted = true;
                                return Some((Err(TransportStepError::RecordTooLarge(size)), st));
                            }
                            if st.consecutive_line_breaks >= 2 {
                                st.current_record_bytes = 0;
                                st.consecutive_line_breaks = 0;
                                st.prev_was_cr = false;
                            }
                        }

                        Some((Ok::<Bytes, TransportStepError>(chunk), st))
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
                    api_key,
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
                    api_key,
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
                let next_seq = acc.next_sequence;
                Err(Box::new((acc.decorate_error(err), vis, next_seq)))
            }
        }
    }
}

enum AwaitOutcome<T> {
    Ready(T),
    Cancelled,
    TimedOut,
}

async fn await_with_cancel<F, T>(
    fut: F,
    deadline: Instant,
    cancel: &Arc<AtomicBool>,
) -> AwaitOutcome<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        if cancel.load(Ordering::SeqCst) {
            return AwaitOutcome::Cancelled;
        }
        let now = Instant::now();
        if now >= deadline {
            return AwaitOutcome::TimedOut;
        }
        let step = deadline
            .saturating_duration_since(now)
            .min(CANCEL_POLL_INTERVAL);
        tokio::select! {
            biased;
            value = &mut fut => return AwaitOutcome::Ready(value),
            _ = tokio::time::sleep(step) => {}
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
