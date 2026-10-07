mod fake;
use fake::*;

use delta_app::ai::runtime::{ModelClient, ModelEvent, Protocol, StopReason};
use delta_app::contracts::CancelToken;
use futures::StreamExt;
use std::sync::atomic::Ordering;

// ---- R1-A-16 tests -----------------------------------------------------------

#[tokio::test]
async fn r1_a_16_responses_text_stream_fragmented_utf8_and_terminal() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(RESP_TEXT_STREAM)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    // Fragmented deltas reassemble into the full text.
    assert_eq!(text_of(&events), "资产净值 2068");
    // Unknown vendor event ignored; usage from completed; terminal present.
    assert!(matches!(stop_reason_of(&events), Some(StopReason::End)));
    let usage = events.iter().any(|e| {
        matches!(
            e,
            ModelEvent::Usage {
                input_tokens: Some(10),
                output_tokens: Some(5)
            }
        )
    });
    assert!(usage, "usage must surface");
    assert_eq!(
        ep.requests.load(Ordering::SeqCst),
        1,
        "no retries for success"
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_responses_tool_call_pairs_and_complete_json_only() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(RESP_TOOL_STREAM)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let ready = events.iter().find_map(|e| match e {
        ModelEvent::ToolCallReady {
            call_id,
            name,
            arguments,
        } => Some((call_id.clone(), name.clone(), arguments.clone())),
        _ => None,
    });
    let (call_id, name, arguments) = ready.expect("tool call must become ready");
    assert_eq!(call_id, "call_1");
    assert_eq!(name, "get_portfolio_summary");
    // Arguments are complete JSON even though they arrived in fragments.
    let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(parsed["scope"]["account_ids"][0], "a");
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_responses_failed_terminal_is_failure_not_success() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(RESP_FAILED_STREAM)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert_eq!(text_of(&events), "partial", "partial text preserved");
    assert!(failed_of(&events).unwrap().contains("upstream failure"));
    assert!(
        stop_reason_of(&events).is_none(),
        "failed stream has no success terminal"
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_chat_text_stream_with_usage_and_done() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(CHAT_TEXT_STREAM)]).await;
    let client = client_for(chat_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiChatCompletions)).await;
    assert_eq!(text_of(&events), "你好，世界");
    assert!(matches!(stop_reason_of(&events), Some(StopReason::End)));
    let usage = events.iter().any(|e| {
        matches!(
            e,
            ModelEvent::Usage {
                input_tokens: Some(7),
                output_tokens: Some(3)
            }
        )
    });
    assert!(usage);
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_chat_tool_fragments_assembled_by_index() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(CHAT_TOOL_STREAM)]).await;
    let client = client_for(chat_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiChatCompletions)).await;
    let ready = events.iter().find_map(|e| match e {
        ModelEvent::ToolCallReady {
            call_id,
            name,
            arguments,
        } => Some((call_id.clone(), name.clone(), arguments.clone())),
        _ => None,
    });
    let (call_id, name, arguments) = ready.expect("chat tool call ready");
    assert_eq!(call_id, "call_9");
    assert_eq!(name, "query_trades");
    let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(parsed["limit"], 10);
    assert!(matches!(stop_reason_of(&events), Some(StopReason::ToolUse)));
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_missing_terminal_state_fails() {
    // Responses: EOF without response.completed/failed.
    let ep = spawn_endpoint(vec![ScriptedResponse {
        drop_after_bytes: None,
        delay_ms: 0,
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
        body_delay_ms: 0,
        pace_ms: 0,
    }])
    .await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(failed_of(&events)
        .unwrap()
        .contains("without a Responses terminal event"));
    ep.shutdown().await;

    // Chat: EOF without [DONE].
    let ep2 = spawn_endpoint(vec![ScriptedResponse {
        drop_after_bytes: None,
        delay_ms: 0,
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n".into(),
        body_delay_ms: 0,
        pace_ms: 0,
    }])
    .await;
    let client2 = client_for(chat_connection(&ep2.url));
    let events2 = collect(&client2, sample_request(Protocol::OpenaiChatCompletions)).await;
    assert!(failed_of(&events2).unwrap().contains("finish_reason"));
    ep2.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_auth_failure_never_retries() {
    let ep = spawn_endpoint(vec![ScriptedResponse::status(401)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert_eq!(ep.requests.load(Ordering::SeqCst), 1, "401 must not retry");
    assert!(failed_of(&events).unwrap().contains("authentication"));
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_rate_limit_retries_within_bounds_with_retry_after() {
    let ep = spawn_endpoint(vec![
        ScriptedResponse {
            status: 429,
            headers: vec![("retry-after".into(), "0".into())],
            ..ScriptedResponse::status(429)
        },
        ScriptedResponse::sse(RESP_TEXT_STREAM),
    ])
    .await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(matches!(stop_reason_of(&events), Some(StopReason::End)));
    assert_eq!(
        ep.requests.load(Ordering::SeqCst),
        2,
        "one retry then success"
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_server_errors_retry_max_two_then_fail() {
    let ep = spawn_endpoint(vec![
        ScriptedResponse::status(500),
        ScriptedResponse::status(500),
        ScriptedResponse::status(500),
        ScriptedResponse::status(500),
    ])
    .await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    // 1 initial + 2 retries = 3 requests, then failure.
    assert_eq!(ep.requests.load(Ordering::SeqCst), 3);
    assert!(failed_of(&events).is_some());
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_partial_stream_drop_is_not_replayed() {
    // Stream visible text then drop; the client must not auto-retry/replay.
    let ep = spawn_endpoint(vec![ScriptedResponse {
        drop_after_bytes: Some(60),
        ..ScriptedResponse::sse(RESP_TEXT_STREAM)
    }])
    .await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert_eq!(
        ep.requests.load(Ordering::SeqCst),
        1,
        "no replay after visible stream"
    );
    // Ends in failure (missing terminal), never silent success.
    assert!(failed_of(&events).is_some());
    assert!(stop_reason_of(&events).is_none());
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_cancel_stops_stream_without_new_requests() {
    let ep = spawn_endpoint(vec![ScriptedResponse {
        delay_ms: 8000,
        ..ScriptedResponse::sse(RESP_TEXT_STREAM)
    }])
    .await;
    let client = client_for(responses_connection(&ep.url));
    let cancel = CancelToken::new();
    let mut stream = client
        .stream(sample_request(Protocol::OpenaiResponses), cancel.clone())
        .unwrap();
    cancel.cancel();
    // Stream terminates promptly after cancellation.
    let t0 = std::time::Instant::now();
    while let Some(_ev) = stream.next().await {}
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "cancel must not wait for the body"
    );
    // Cancellation before connect may prevent the request entirely; at most
    // one request is ever made and no retry happens.
    assert!(ep.requests.load(Ordering::SeqCst) <= 1);
    ep.shutdown().await;
}

/// A token cancelled before the call (a revoked or cancelled run) sends
/// nothing: the cancel branch wins before the connect phase starts.
#[tokio::test]
async fn r1_a_16_a_cancelled_token_sends_no_request() {
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(RESP_TEXT_STREAM)]).await;
    let client = client_for(responses_connection(&ep.url));
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut stream = client
        .stream(sample_request(Protocol::OpenaiResponses), cancel)
        .unwrap();
    let mut events = 0;
    while stream.next().await.is_some() {
        events += 1;
    }
    assert_eq!(events, 0, "a cancelled stream yields no events");
    // Give a stray connect time to land before checking.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(ep.requests.load(Ordering::SeqCst), 0);
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_oversized_stream_is_rejected() {
    // A single SSE record exceeding 1 MiB must fail, not truncate silently.
    let big_delta = "x".repeat(2 * 1024 * 1024);
    let body = format!(
        "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\"}}\n\n",
        big_delta
    );
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(&body)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(failed_of(&events).is_some(), "oversized stream must fail");
    assert!(stop_reason_of(&events).is_none());
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_long_stream_of_small_records_is_accepted() {
    // The 1 MiB cap is per SSE record, not per stream: ~2 MiB of ordinary
    // deltas must complete normally.
    let piece = "y".repeat(1000);
    let mut body = String::new();
    for _ in 0..2048 {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{piece}\"}}\n\n"
        ));
    }
    body.push_str("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{}}}\n\n");
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(&body)]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert_eq!(failed_of(&events), None);
    assert_eq!(stop_reason_of(&events), Some(StopReason::End));
    assert_eq!(text_of(&events).len(), 2048 * 1000);
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_transport_failure_message_has_no_url() {
    // Bind then drop a listener so the port refuses connections.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/secret-path");
    let client = client_for(responses_connection(&url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let msg = failed_of(&events).expect("connection must fail");
    assert!(
        !msg.contains("127.0.0.1") && !msg.contains("secret-path"),
        "{msg}"
    );
}

#[tokio::test]
async fn r1_a_16_redirect_is_not_followed_and_leaks_no_location() {
    let mut redirect = ScriptedResponse::status(302);
    redirect
        .headers
        .push(("location".into(), "http://evil.example/steal-key".into()));
    let ep = spawn_endpoint(vec![redirect]).await;
    let client = client_for(responses_connection(&ep.url));
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let msg = failed_of(&events).expect("redirect must fail closed");
    assert!(!msg.contains("evil.example"), "{msg}");
    assert!(!msg.contains("steal-key"), "{msg}");
    assert_eq!(ep.requests.load(Ordering::SeqCst), 1);
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_timeout_is_not_retried() {
    let ep = spawn_endpoint(vec![ScriptedResponse {
        delay_ms: 3000,
        ..ScriptedResponse::sse(RESP_TEXT_STREAM)
    }])
    .await;
    let mut conn = responses_connection(&ep.url);
    conn.request_timeout_secs = 1;
    let client = client_for(conn);
    let started = std::time::Instant::now();
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(2500));
    let msg = failed_of(&events).expect("timeout must fail");
    assert!(!msg.contains("127.0.0.1"), "{msg}");
    assert_eq!(
        ep.requests.load(Ordering::SeqCst),
        1,
        "timeout is not a connect retry"
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_16_backpressure_delivers_every_event() {
    let mut body = String::new();
    for i in 0..200 {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{i}\"}}\n\n"
        ));
    }
    body.push_str(
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"bp\",\"usage\":{}}}\n\n",
    );
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(&body)]).await;
    let client = client_for(responses_connection(&ep.url));
    let cancel = CancelToken::new();
    let mut stream = client
        .stream(sample_request(Protocol::OpenaiResponses), cancel)
        .unwrap();
    // Let the producer fill the bounded queue and block instead of dropping.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut deltas = 0usize;
    while let Some(ev) = stream.next().await {
        match ev {
            ModelEvent::TextDelta { .. } => deltas += 1,
            ModelEvent::Completed { stop_reason } => {
                assert_eq!(stop_reason, StopReason::End);
                break;
            }
            ModelEvent::Failed { error } => {
                panic!("backpressure must not fail the stream: {error:?}")
            }
            _ => {}
        }
    }
    assert_eq!(deltas, 200);
    ep.shutdown().await;
}

/// A stream that keeps sending outlasts the idle budget and still finishes
/// inside the total budget. The idle timer must reset on each read.
#[tokio::test]
async fn r1_a_16_active_long_stream_is_not_killed_by_the_idle_timeout() {
    let mut body = String::new();
    for i in 0..8 {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{i}\"}}\n\n"
        ));
    }
    body.push_str(
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"long\",\"usage\":{}}}\n\n",
    );
    let ep = spawn_endpoint(vec![ScriptedResponse {
        pace_ms: 300,
        ..ScriptedResponse::sse(&body)
    }])
    .await;
    let mut conn = responses_connection(&ep.url);
    conn.connect_timeout_secs = 2;
    conn.idle_timeout_secs = 1;
    conn.request_timeout_secs = 15;
    let client = client_for(conn);
    let started = std::time::Instant::now();
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(
        started.elapsed() > std::time::Duration::from_millis(1500),
        "the stream has to actually take longer than the idle budget"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(12),
        "total timeout must not fire on an active stream"
    );
    assert_eq!(text_of(&events), "01234567");
    assert!(matches!(stop_reason_of(&events), Some(StopReason::End)));
    assert_eq!(ep.requests.load(Ordering::SeqCst), 1);
    ep.shutdown().await;
}

/// Silence after the headers is an idle timeout, not the total deadline,
/// and it is not retried as a connect failure.
#[tokio::test]
async fn r1_a_16_idle_timeout_stops_a_stalled_read() {
    let ep = spawn_endpoint(vec![ScriptedResponse {
        body_delay_ms: 4000,
        ..ScriptedResponse::sse(RESP_TEXT_STREAM)
    }])
    .await;
    let mut conn = responses_connection(&ep.url);
    conn.connect_timeout_secs = 2;
    conn.idle_timeout_secs = 1;
    conn.request_timeout_secs = 20;
    let client = client_for(conn);
    let started = std::time::Instant::now();
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "idle timeout must not wait for the total budget, elapsed {elapsed:?}"
    );
    assert!(elapsed > std::time::Duration::from_millis(500));
    let msg = failed_of(&events).expect("stalled read must fail");
    assert!(!msg.contains("127.0.0.1"), "{msg}");
    assert_eq!(ep.requests.load(Ordering::SeqCst), 1);
    ep.shutdown().await;
}

// The connect-budget and proxy-policy tests live in `model_proxy.rs`: they
// need local sockets, a hanging resolver and child processes, and none of them
// may depend on this machine's network or proxy.
