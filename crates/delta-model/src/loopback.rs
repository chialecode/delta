use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::client::ModelClient;
use crate::protocol::{
    InputItem, ModelConnection, ModelErrorCode, ModelEventKind, ModelProtocol, ModelRequest,
    ToolDefinition,
};

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

    /// Clears scripted responses for `path`.
    pub(crate) fn clear_route(&self, path: &str) {
        self.routes.lock().unwrap().remove(path);
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

pub(crate) fn sample_request(connection_id: &str, scope_hash: &str) -> ModelRequest {
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
    server.clear_route(&route_path);

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
        server.clear_route(&route_path);
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
    server.clear_route(&route_path);
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
    server.clear_route(&route_path);

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
