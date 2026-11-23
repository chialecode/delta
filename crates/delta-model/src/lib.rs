//! Pure-Rust model client supporting `openai-responses` and `openai-chat-completions`
//! over SSE without business database dependencies (S-12, S-19, M2-K03, M2-A-13).

mod client;
mod loopback;
mod protocol;
mod sse;

pub use client::ModelClient;
pub use loopback::{
    run_m2_k03_and_a13_verification, ModelLoopbackServer, ScriptedModelResponse,
    ScriptedStreamChunk,
};
pub use protocol::{
    InputItem, ModelBudget, ModelCapabilities, ModelConnection, ModelError, ModelErrorCode,
    ModelEvent, ModelEventKind, ModelProtocol, ModelRequest, ModelStreamResult, ModelTimeouts,
    RetryPolicy, TokenUsage, ToolDefinition,
};

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::loopback::sample_request;
    use crate::protocol::build_request_body;

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

        // Preserves query parameters such as Azure `?api-version=...` when appending path.
        conn.base_url =
            "https://azure.example.com/openai/deployments/d1?api-version=2024-06-01".to_string();
        assert_eq!(
            conn.resolved_endpoint_url().unwrap(),
            "https://azure.example.com/openai/deployments/d1/responses?api-version=2024-06-01"
        );

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
        server.clear_route("/responses");
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

    #[test]
    fn test_chat_completions_merges_assistant_text_and_tool_call_and_omits_empty_tools() {
        let mut conn = ModelConnection::loopback(
            "conn-chat-body",
            ModelProtocol::OpenAiChatCompletions,
            "http://127.0.0.1:9999",
            "gpt-5-mini",
        );
        let mut req = sample_request("conn-chat-body", "scope-1");
        req.input_items.push(InputItem::AssistantMessage {
            content: "Checking your report".into(),
        });
        req.input_items.push(InputItem::ToolCall {
            call_id: "call_1".into(),
            name: "report.period_breakdown".into(),
            arguments_json: "{}".into(),
        });
        req.input_items.push(InputItem::ToolCall {
            call_id: "call_2".into(),
            name: "scope.describe".into(),
            arguments_json: "{}".into(),
        });
        let body = build_request_body(&conn, &req);
        let msgs = body["messages"].as_array().unwrap();
        // system + user + ONE combined assistant message with both content and tool_calls.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "Checking your report");
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 2);

        req.tool_definitions.clear();
        let no_tools_chat = build_request_body(&conn, &req);
        assert!(no_tools_chat.get("tools").is_none());

        conn.protocol = ModelProtocol::OpenAiResponses;
        conn.capabilities.supports_reasoning = false;
        let no_tools_resp = build_request_body(&conn, &req);
        assert!(no_tools_resp.get("tools").is_none());
        assert!(no_tools_resp.get("include").is_none());
    }

    #[test]
    fn test_silent_stream_cancels_promptly_and_slow_drip_hits_total_timeout() {
        let server = ModelLoopbackServer::start().unwrap();
        let client = ModelClient::new().unwrap();
        let mut conn = ModelConnection::loopback(
            "conn-cancel-silent",
            ModelProtocol::OpenAiResponses,
            server.base_url(),
            "gpt-5-mini",
        );
        conn.timeouts.idle_ms = 5_000;
        conn.timeouts.total_ms = 5_000;
        conn.retry_policy.max_retries_before_visible_output = 0;

        // Server delays 2 seconds silently without sending any keepalive bytes.
        server.enqueue_route(
            "/responses",
            vec![ScriptedModelResponse::sse_chunks(vec![
                ScriptedStreamChunk::delayed(
                    2_000,
                    b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
            ])],
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_trigger = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            cancel_trigger.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let req = sample_request("conn-cancel-silent", "scope-1");
        let err = client
            .stream_request(&conn, &req, Some("sk-test"), cancel)
            .expect_err("silent stream must cancel promptly");
        assert_eq!(err.code, ModelErrorCode::Cancelled);
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "silent cancel took {:?}",
            started.elapsed()
        );

        // Slow-drip stream sending chunks faster than idle_ms must still obey total_ms.
        server.clear_route("/responses");
        conn.timeouts.idle_ms = 500;
        conn.timeouts.total_ms = 180;
        let chunks = (0..12)
            .map(|_| {
                ScriptedStreamChunk::delayed(
                    40,
                    b"event: response.output_text.delta\ndata: {\"delta\":\"x\"}\n\n",
                )
            })
            .collect();
        server.enqueue_route(
            "/responses",
            vec![ScriptedModelResponse::sse_chunks(chunks)],
        );
        let err = client
            .stream_request(
                &conn,
                &req,
                Some("sk-test"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("slow drip must fail on total timeout budget");
        assert_eq!(err.code, ModelErrorCode::Timeout);
        assert!(!err.retryable);
    }

    #[test]
    fn test_reasoning_partial_stream_does_not_retry_and_error_redacts_secret_and_controls() {
        let server = ModelLoopbackServer::start().unwrap();
        let client = ModelClient::new().unwrap();
        let conn = ModelConnection::loopback(
            "conn-reason",
            ModelProtocol::OpenAiChatCompletions,
            server.base_url(),
            "gpt-5-mini",
        );
        let req = sample_request("conn-reason", "scope-1");

        // Partial reasoning followed by EOF must NOT retry, and Failed event sequence is monotonic.
        let reasoning_only =
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\n";
        server.enqueue_route(
            "/chat/completions",
            vec![
                ScriptedModelResponse::sse_raw_slices(&[reasoning_only.as_bytes()]),
                ScriptedModelResponse::sse_raw_slices(&[
                    b"data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                ]),
            ],
        );
        let mut seen_seqs = Vec::new();
        let before = server.request_count("/chat/completions");
        let err = client
            .stream_request_with_callback(
                &conn,
                &req,
                Some("synthetic-secret-token-xyz"),
                Arc::new(AtomicBool::new(false)),
                |ev| seen_seqs.push(ev.sequence),
            )
            .expect_err("truncated reasoning stream must not retry");
        assert_eq!(err.code, ModelErrorCode::StreamTruncated);
        assert_eq!(server.request_count("/chat/completions") - before, 1);
        assert!(seen_seqs.windows(2).all(|w| w[0] < w[1]));
        assert!(seen_seqs.last().copied().unwrap_or(0) < u64::MAX);

        // Provider error echoing secret + control chars is redacted and sanitized.
        server.clear_route("/chat/completions");
        let evil_err =
            "data: {\"error\":{\"message\":\"bad synthetic-secret-token-xyz \\u0007 \\u001b[31m\"}}\n\n";
        server.enqueue_route(
            "/chat/completions",
            vec![ScriptedModelResponse::sse_raw_slices(
                &[evil_err.as_bytes()],
            )],
        );
        let err = client
            .stream_request(
                &conn,
                &req,
                Some("synthetic-secret-token-xyz"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect_err("provider error must fail");
        assert!(!err.safe_message.contains("synthetic-secret-token-xyz"));
        assert!(err.safe_message.contains("[REDACTED]"));
        assert!(!err.safe_message.contains('\x07'));
        assert!(!err.safe_message.contains('\x1b'));

        // Trailing usage chunk with empty string finish_reason must not clobber earlier stop.
        server.clear_route("/chat/completions");
        let trailing_empty_finish = concat!(
            "data: {\"id\":\"c3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );
        server.enqueue_route(
            "/chat/completions",
            vec![ScriptedModelResponse::sse_raw_slices(&[
                trailing_empty_finish.as_bytes(),
            ])],
        );
        let ok = client
            .stream_request(
                &conn,
                &req,
                Some("synthetic-secret-token-xyz"),
                Arc::new(AtomicBool::new(false)),
            )
            .expect("empty trailing finish_reason must preserve stop");
        assert!(ok.completed);
        assert_eq!(ok.accumulated_text, "ok");
    }
}
