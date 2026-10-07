//! AgentRuntime integration tests (R1-A-15/17/18/19/20/21): real Rust client
//! against controlled endpoints, SQLite session store, scope/generation
//! enforcement, compaction atomicity, cancellation and reference validation.

use chrono::TimeZone;
use delta_app::ai::gateway::{AnalysisHost, ToolGateway, TOOL_NAMES};
use delta_app::ai::runtime::{AgentRuntime, ModelConnectionConfig, RunRequest};
use delta_app::ai::session::{NewRun, RunBudget, RunState, SessionStore};
use delta_app::contracts::{ActorKind, AppError, CallContext, CancelToken, ResultEnvelope};
use delta_core::money::CurrencyCode;
use delta_core::{AccountId, ScopeSnapshot, ScopeView};
use delta_infra::host::ProductionHost;
use delta_infra::model::{DeltaModelClient, StaticCredentials};
use delta_infra::sqlite::store::Library;
use serde_json::{json, Value};
use std::sync::Arc;

fn frozen_scope(scope_ref: &str) -> ScopeSnapshot {
    ScopeSnapshot::freeze(
        scope_ref,
        vec![AccountId::new("acc-us")],
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap(),
        CurrencyCode::usd(),
        ScopeView::Portfolio,
        0,
    )
}

mod fake;
use fake::{responses_connection, ScriptedResponse};

// ---- scripted streams for runtime scenarios -----------------------------------

/// A Responses SSE stream that answers with a tool call to
/// get_portfolio_summary, then (second request) a final report.
const RESP_TOOL_THEN_DONE: [(&str, &str); 2] = [
    (
        "tool",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"get_portfolio_summary\",\"arguments\":\"{\\\"scope\\\":{\\\"account_ids\\\":[\\\"acc-us\\\"],\\\"start_at\\\":\\\"2026-01-01T00:00:00Z\\\",\\\"end_at\\\":\\\"2026-01-31T00:00:00Z\\\",\\\"reporting_currency\\\":\\\"USD\\\"},\\\"as_of\\\":\\\"2026-01-31T00:00:00Z\\\"}\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
    ),
    (
        "report",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益 68，证据 res:test-evidence-1。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"usage\":{\"input_tokens\":20,\"output_tokens\":9}}}\n\n",
    ),
];

fn script_vec(entries: &[(&str, &str)]) -> Vec<ScriptedResponse> {
    entries
        .iter()
        .map(|(_, body)| ScriptedResponse::sse(body))
        .collect()
}

// ---- test host ------------------------------------------------------------------

/// Canned analysis host: returns fixed envelopes with evidence ids.
struct TestHost {
    fail_tools: std::sync::Mutex<std::collections::HashSet<String>>,
    quality: String,
    value: String,
}

impl TestHost {
    fn ok() -> Self {
        Self {
            fail_tools: std::sync::Mutex::new(std::collections::HashSet::new()),
            quality: "complete".into(),
            value: "68".into(),
        }
    }
}

impl AnalysisHost for TestHost {
    fn execute(
        &self,
        tool: &str,
        ctx: &CallContext,
        _args: &Value,
    ) -> Result<ResultEnvelope<Value>, AppError> {
        if self.fail_tools.lock().unwrap().contains(tool) {
            return Err(AppError::ProviderUnavailable(
                "tool failure injected".into(),
            ));
        }
        let mut e = ResultEnvelope::new(
            &ctx.request_id,
            json!({ "metric": "total_pnl", "value": self.value }),
        );
        e.quality = self.quality.clone();
        e.evidence_refs = vec!["res:test-evidence-1".into()];
        Ok(e)
    }
}

fn budget_small() -> RunBudget {
    RunBudget {
        max_tool_calls: 12,
        max_model_requests: 20,
        context_window_tokens: 8192,
        max_duration_secs: 120,
    }
}

fn runtime_for(url: &str, library: &Arc<Library>) -> AgentRuntime<DeltaModelClient, Library> {
    runtime_with(responses_connection(url), library, TestHost::ok())
}

fn runtime_with(
    conn: ModelConnectionConfig,
    library: &Arc<Library>,
    host: TestHost,
) -> AgentRuntime<DeltaModelClient, Library> {
    let client =
        DeltaModelClient::new(conn, Arc::new(StaticCredentials { key: "k".into() })).unwrap();
    // Fresh handle for the runtime; SQLite allows multiple connections.
    let store = Library::open(&library.path).expect("reopen library");
    AgentRuntime::new(client, store, Arc::new(host))
}

fn run_request(
    _library: &Arc<Library>,
    session_id: &str,
    conn: ModelConnectionConfig,
) -> RunRequest {
    RunRequest {
        session_id: session_id.into(),
        user_prompt: "解释当前损益".into(),
        scope_snapshot: frozen_scope("scope-1"),
        connection: conn,
        budget: budget_small(),
        system_instructions: "Read-only financial analysis.".into(),
        cancel: CancelToken::new(),
    }
}

// ---- R1-A-15/21: tool loop, receipts, validation, offline behavior --------------

#[tokio::test]
async fn r1_a_21_tool_loop_report_succeeds_with_valid_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(script_vec(&RESP_TOOL_THEN_DONE)).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let outcome = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap();
    assert_eq!(outcome.state, RunState::Succeeded);
    assert_eq!(outcome.tool_calls_used, 1);
    assert!(outcome
        .evidence_refs
        .contains(&"res:test-evidence-1".to_string()));
    // Transcript persisted: user + tool output + assistant.
    let messages = rt.store().messages(&session, None).unwrap();
    let kinds: Vec<String> = messages.iter().map(|m| format!("{:?}", m.kind)).collect();
    assert!(kinds.contains(&"User".to_string()));
    assert!(kinds.contains(&"ToolOutput".to_string()));
    assert!(kinds.contains(&"Assistant".to_string()));
    // Tool call record completed with result ref.
    let calls = rt.store().tool_calls(&outcome.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "ok");
    assert!(calls[0].result_ref.is_some(), "receipt result ref recorded");
    let kinds_again = rt.store().messages(&session, None).unwrap();
    assert!(
        kinds_again
            .iter()
            .any(|m| format!("{:?}", m.kind) == "ToolCall"),
        "tool call message is persisted"
    );
    let bodies = ep.bodies.lock().unwrap().clone();
    assert!(bodies.len() >= 2, "tool turn then report turn");
    assert!(
        bodies[1].contains("\"type\":\"function_call\"")
            || bodies[1].contains("\"type\": \"function_call\""),
        "responses request pairs the call: {}",
        bodies[1]
    );
    assert!(
        bodies[1].contains("function_call_output"),
        "responses request pairs the output: {}",
        bodies[1]
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_21_forged_reference_gets_one_constrained_retry_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    // Report references a fabricated evidence id twice.
    let forged = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"text\\\":\\\"68\\\",\\\"evidence_refs\\\":[\\\"res:forged-99\\\"]}\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rf\",\"usage\":{}}}\n\n";
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(forged),
        ScriptedResponse::sse(forged),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let outcome = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    let err = match outcome {
        Ok(o) => panic!("forged refs must fail, got {o:?}"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("invalid after one correction"));
    // The run must not be marked succeeded.
    let runs_left = rt.store().latest_generation(&session).unwrap();
    assert_eq!(runs_left, 1);
    ep.shutdown().await;
}

// ---- R1-A-17/23: gateway enforces whitelist, scope, generation ------------------

#[test]
fn r1_a_15_gateway_rejects_unknown_tools_and_stale_generation() {
    let gw = ToolGateway::new("run-1", 3, frozen_scope("scope-1"), budget_small());
    let host = TestHost::ok();
    let ctx_ok = CallContext {
        request_id: "r1".into(),
        library_id: "lib".into(),
        actor_kind: delta_app::contracts::ActorKind::Ai,
        run_id: Some("run-1".into()),
        generation: Some(3),
        scope_ref: "scope-1".into(),
        deadline: None,
    };
    // Unknown/shell tool rejected even if the model asks for it.
    for tool in ["run_shell", "read_file", "sql_query"] {
        let err = gw
            .execute(&host, &ctx_ok, "c1", tool, &json!({}))
            .unwrap_err();
        assert_eq!(err.code(), "UNSUPPORTED_CAPABILITY", "{tool}");
    }
    // Stale generation rejected.
    let mut ctx_stale = ctx_ok.clone();
    ctx_stale.generation = Some(2);
    let err = gw
        .execute(&host, &ctx_stale, "c2", "get_portfolio_summary", &json!({}))
        .unwrap_err();
    assert_eq!(err.code(), "STALE_GENERATION");
    // Wrong scope ref rejected.
    let mut ctx_scope = ctx_ok.clone();
    ctx_scope.scope_ref = "scope-evil".into();
    let err = gw
        .execute(&host, &ctx_scope, "c3", "get_portfolio_summary", &json!({}))
        .unwrap_err();
    assert_eq!(err.code(), "SCOPE_DENIED");
    // Schema violation rejected.
    let err = gw
        .execute(
            &host,
            &ctx_ok,
            "c4",
            "query_trades",
            &json!({ "limit": 9999 }),
        )
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    // Whitelist contains exactly the six business tools.
    assert_eq!(TOOL_NAMES.len(), 6);
}

#[tokio::test]
async fn r1_a_17_new_run_gets_new_generation_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(script_vec(&RESP_TOOL_THEN_DONE[..1])).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    // First run (ends after tool round because the script only has one response
    // → second model request fails; run fails, but generation advanced).
    let _ = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    let gen1 = rt.store().latest_generation(&session).unwrap();
    // Second run must use generation 2 with a clean context.
    let ep2 = fake::spawn_endpoint(script_vec(&RESP_TOOL_THEN_DONE)).await;
    let rt2 = runtime_for(&ep2.url, &library);
    let outcome = rt2
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep2.url),
        ))
        .await
        .unwrap();
    let gen2 = rt2.store().latest_generation(&session).unwrap();
    assert_eq!(gen1, 1);
    assert_eq!(gen2, 2);
    assert_eq!(outcome.generation, 2);
    ep.shutdown().await;
    ep2.shutdown().await;
}

// ---- R1-A-18: compaction atomicity and recovery ---------------------------------

#[tokio::test]
async fn r1_a_18_compaction_switches_checkpoint_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    // Script: summary response first (compaction), then tool + report.
    let summary_stream = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Earlier the user discussed positions.\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rs\",\"usage\":{}}}\n\n";
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(summary_stream),
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[1].1),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    // Seed enough complete turns so compaction has compressible history.
    use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
    let seed_run = rt
        .store()
        .create_run(&NewRun {
            session_id: session.clone(),
            generation: 0,
            scope_snapshot: frozen_scope("scope-1"),
            tool_schema_hash: "seed".into(),
            model_ref: "seed".into(),
            budget: RunBudget {
                max_tool_calls: 0,
                max_model_requests: 0,
                context_window_tokens: 0,
                max_duration_secs: 0,
            },
        })
        .unwrap();
    // A finished seed run must not affect the real run's generation.
    rt.store()
        .update_run_state(&seed_run.id, RunState::Failed)
        .unwrap();
    for i in 0..3 {
        rt.store()
            .append_message(&NewMessage {
                session_id: session.clone(),
                run_id: seed_run.id.clone(),
                kind: MK::User,
                payload: serde_json::json!({ "text": format!("历史问题 {i}：{}", "请解释持仓与费用。".repeat(30)) }),
                status: MS::Complete,
                connection_ref: None,
            })
            .unwrap();
        rt.store()
            .append_message(&NewMessage {
                session_id: session.clone(),
                run_id: seed_run.id.clone(),
                kind: MK::Assistant,
                payload: serde_json::json!({ "text": format!("历史回答 {i}：{}", "费用共 2，损益 68。".repeat(30)) }),
                status: MS::Complete,
                connection_ref: None,
            })
            .unwrap();
    }
    // Small window forces compaction before the first model request.
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    let outcome = rt.run_turn(req).await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded);
    // A checkpoint is active and the original history is preserved.
    let cp = rt.store().active_checkpoint(&session).unwrap();
    assert!(cp.is_some(), "compaction must have switched a checkpoint");
    let all = rt.store().messages(&session, None).unwrap();
    let after_cp = rt
        .store()
        .messages(&session, cp.as_ref().map(|c| c.source_range.as_str()))
        .unwrap();
    assert!(all.len() > after_cp.len(), "original history retained");
    // Reopen (host restart): runs marked interrupted; history intact.
    let reopened = Library::open(&library.path).unwrap();
    let marked = reopened.mark_interrupted_runs().unwrap();
    assert_eq!(marked, 0, "succeeded runs are not interrupted");
    assert_eq!(reopened.messages(&session, None).unwrap().len(), all.len());
    // The summary reaches the model as untrusted user-role history; nothing
    // but the run instructions may be system-level (context-state §4).
    let bodies = ep.bodies.lock().unwrap().clone();
    let after_compaction: Value = serde_json::from_str(&bodies[1]).unwrap();
    let input = after_compaction["input"].as_array().unwrap();
    assert!(input.iter().all(|i| i["role"] != "system"), "{input:?}");
    let summary_item = input
        .iter()
        .find(|i| {
            i.to_string()
                .contains("Earlier the user discussed positions.")
        })
        .expect("summary is sent");
    assert_eq!(summary_item["role"], "user");
    assert!(summary_item.to_string().contains("untrusted"));
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_18_cancel_during_compaction_summary_stops_without_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let summary_stream = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"late summary\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rs\",\"usage\":{}}}\n\n";
    let ep = fake::spawn_endpoint(vec![ScriptedResponse {
        delay_ms: 3000,
        ..ScriptedResponse::sse(summary_stream)
    }])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
    let seed_run = rt
        .store()
        .create_run(&NewRun {
            session_id: session.clone(),
            generation: 0,
            scope_snapshot: frozen_scope("scope-1"),
            tool_schema_hash: "seed".into(),
            model_ref: "seed".into(),
            budget: budget_small(),
        })
        .unwrap();
    rt.store()
        .update_run_state(&seed_run.id, RunState::Failed)
        .unwrap();
    for i in 0..3 {
        for (kind, text) in [(MK::User, "历史问题"), (MK::Assistant, "历史回答")] {
            rt.store()
                .append_message(&NewMessage {
                    session_id: session.clone(),
                    run_id: seed_run.id.clone(),
                    kind,
                    payload: json!({ "text": format!("{text} {i}：请解释持仓与费用。") }),
                    status: MS::Complete,
                    connection_ref: None,
                })
                .unwrap();
        }
    }
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 300;
    let cancel = req.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let started = std::time::Instant::now();
    let result = rt.run_turn(req).await;
    assert!(
        matches!(result, Err(AppError::Cancelled)),
        "expected cancelled, got {result:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2500),
        "cancel must interrupt the summary request"
    );
    assert!(rt.store().active_checkpoint(&session).unwrap().is_none());
    ep.shutdown().await;
}

// ---- R1-A-19: cancellation leaves clean state ------------------------------------

#[tokio::test]
async fn r1_a_19_cancel_during_run_marks_cancelled_no_partial_success() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    // Slow stream: delay before body so we cancel mid-flight.
    let ep = fake::spawn_endpoint(vec![ScriptedResponse {
        delay_ms: 3000,
        ..ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1)
    }])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let req = run_request(&library, &session, responses_connection(&ep.url));
    let cancel = req.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let result = rt.run_turn(req).await;
    match result {
        Err(AppError::Cancelled) => {}
        Ok(o) => panic!("cancelled expected, got state {:?}", o.state),
        Err(e) => panic!("expected cancelled, got {e:?}"),
    }
    // The only message persisted is the user prompt; no partial assistant
    // success exists.
    let messages = rt.store().messages(&session, None).unwrap();
    assert!(messages
        .iter()
        .all(|m| m.status != delta_app::ai::session::MessageStatus::Partial));
    ep.shutdown().await;
}

// ---- R1-A-20: isolation probes -----------------------------------------------------

#[tokio::test]
async fn r1_a_20_host_only_exposes_business_queries() {
    // The host implementation is the surface the model can reach; this test
    // pins it to business query envelopes with evidence only.
    let host = TestHost::ok();
    let ctx = CallContext {
        request_id: "r".into(),
        library_id: "lib".into(),
        actor_kind: delta_app::contracts::ActorKind::Ai,
        run_id: Some("run".into()),
        generation: Some(1),
        scope_ref: "scope".into(),
        deadline: None,
    };
    let gw = ToolGateway::new("run", 1, frozen_scope("scope"), budget_small());
    for (i, tool) in TOOL_NAMES.iter().enumerate() {
        let call_id = format!("c-{i}");
        let env = gw
            .execute(&host, &ctx, &call_id, tool, &valid_args(tool))
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(env.evidence_refs.iter().all(|r| r.starts_with("res:")));
    }
    // Duplicate call id with identical args replays the receipt (AC-23).
    let replay = gw
        .execute(
            &host,
            &ctx,
            "c-0",
            TOOL_NAMES[0],
            &valid_args(TOOL_NAMES[0]),
        )
        .unwrap();
    assert_eq!(
        replay.evidence_refs,
        vec!["res:test-evidence-1".to_string()]
    );
}

fn valid_args(tool: &str) -> Value {
    let scope = json!({
        "account_ids": ["acc-us"],
        "start_at": "2026-01-01T00:00:00Z",
        "end_at": "2026-01-31T00:00:00Z",
        "reporting_currency": "USD"
    });
    match tool {
        "get_data_quality" => json!({ "scope": scope }),
        "query_trades" => json!({ "scope": scope, "limit": 20 }),
        "explain_pnl" => {
            json!({ "scope": scope, "period_start": "2026-01-01T00:00:00Z", "period_end": "2026-01-31T00:00:00Z" })
        }
        "get_portfolio_summary" => json!({ "scope": scope, "as_of": "2026-01-31T00:00:00Z" }),
        "search_journal" => json!({ "query": "买入", "limit": 5 }),
        "get_chart_window" => json!({
            "instrument": "NASDAQ:AAPL",
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-01-31T00:00:00Z",
            "timeframe": "1d"
        }),
        _ => json!({}),
    }
}

const INTERRUPTED_TOOL: &str = "\
event: response.output_item.done
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_x\",\"name\":\"get_portfolio_summary\",\"arguments\":\"{\\\"scope\\\":{\\\"account_ids\\\":[\\\"acc-us\\\"],\\\"start_at\\\":\\\"2026-01-01T00:00:00Z\\\",\\\"end_at\\\":\\\"2026-01-31T00:00:00Z\\\",\\\"reporting_currency\\\":\\\"USD\\\"},\\\"as_of\\\":\\\"2026-01-31T00:00:00Z\\\"}\"}}\n
event: response.failed
data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"upstream failure\"}}}\n
";

#[tokio::test]
async fn r1_a_18_interrupted_tool_call_is_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![ScriptedResponse::sse(INTERRUPTED_TOOL)]).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let err = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("upstream") || err.code() == "PROVIDER_UNAVAILABLE");
    let runs = rt.store().runs(&session).unwrap();
    let calls = rt.store().tool_calls(&runs[0].id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, "interrupted");
    let messages = rt.store().messages(&session, None).unwrap();
    assert!(messages.iter().any(|m| {
        format!("{:?}", m.kind) == "ToolCall" && format!("{:?}", m.status) == "Interrupted"
    }));
    assert!(messages
        .iter()
        .all(|m| format!("{:?}", m.kind) != "ToolOutput"));
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_18_chat_request_pairs_tool_calls_with_tool_messages() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let tool = "\
data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"get_portfolio_summary\",\"arguments\":\"{\\\"scope\\\":{\\\"account_ids\\\":[\\\"acc-us\\\"],\\\"start_at\\\":\\\"2026-01-01T00:00:00Z\\\",\\\"end_at\\\":\\\"2026-01-31T00:00:00Z\\\",\\\"reporting_currency\\\":\\\"USD\\\"},\\\"as_of\\\":\\\"2026-01-31T00:00:00Z\\\"}\"}}]},\"finish_reason\":null}]}

data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
    let report = "\
data: {\"id\":\"c3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"总损益 68，证据 res:test-evidence-1。\"},\"finish_reason\":null}]}

data: {\"id\":\"c3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(tool),
        ScriptedResponse::sse(report),
    ])
    .await;
    let rt = runtime_with(fake::chat_connection(&ep.url), &library, TestHost::ok());
    let session = rt.store().create_session(&library.id).unwrap();
    let outcome = rt
        .run_turn(run_request(
            &library,
            &session,
            fake::chat_connection(&ep.url),
        ))
        .await
        .unwrap();
    assert_eq!(outcome.tool_calls_used, 1);
    let bodies = ep.bodies.lock().unwrap().clone();
    assert!(
        bodies[1].contains("\"tool_calls\""),
        "chat assistant message carries tool_calls: {}",
        bodies[1]
    );
    assert!(
        bodies[1].contains("\"role\":\"tool\"") || bodies[1].contains("\"role\": \"tool\""),
        "chat tool output is a tool message: {}",
        bodies[1]
    );
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_17_scope_change_drops_prior_tool_output_from_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(script_vec(&RESP_TOOL_THEN_DONE)).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    rt.run_turn(run_request(
        &library,
        &session,
        responses_connection(&ep.url),
    ))
    .await
    .unwrap();
    ep.shutdown().await;
    let done = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"新范围。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"n\",\"usage\":{}}}\n\n";
    let ep2 = fake::spawn_endpoint(vec![ScriptedResponse::sse(done)]).await;
    let rt2 = runtime_for(&ep2.url, &library);
    let mut req = run_request(&library, &session, responses_connection(&ep2.url));
    req.scope_snapshot = ScopeSnapshot::freeze(
        "scope-2",
        vec![AccountId::new("acc-other")],
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap(),
        CurrencyCode::usd(),
        ScopeView::Portfolio,
        0,
    );
    rt2.run_turn(req).await.unwrap();
    let body = ep2.bodies.lock().unwrap()[0].clone();
    assert!(
        !body.contains("test-evidence-1"),
        "old tool output must not ride into the new scope: {body}"
    );
    assert!(!body.contains("function_call_output"), "{body}");
    let stored = rt2.store().messages(&session, None).unwrap();
    assert!(stored
        .iter()
        .any(|m| format!("{:?}", m.kind) == "ToolOutput"));
    ep2.shutdown().await;
}

#[tokio::test]
async fn r1_a_17_same_economic_scope_keeps_history_when_revision_changes() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let done = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"继续。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"n\",\"usage\":{}}}\n\n";
    let ep = fake::spawn_endpoint(vec![ScriptedResponse::sse(done)]).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
    let seed = rt
        .store()
        .create_run(&NewRun {
            session_id: session.clone(),
            generation: 0,
            scope_snapshot: frozen_scope("scope-1"),
            tool_schema_hash: "seed".into(),
            model_ref: "seed".into(),
            budget: budget_small(),
        })
        .unwrap();
    rt.store()
        .update_run_state(&seed.id, RunState::Failed)
        .unwrap();
    rt.store()
        .append_message(&NewMessage {
            session_id: session.clone(),
            run_id: seed.id,
            kind: MK::User,
            payload: json!({ "text": "历史问题保持在上下文" }),
            status: MS::Complete,
            connection_ref: None,
        })
        .unwrap();
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.scope_snapshot.ledger_revision = 9;
    rt.run_turn(req).await.unwrap();
    let body = ep.bodies.lock().unwrap()[0].clone();
    assert!(body.contains("历史问题保持在上下文"), "{body}");
    ep.shutdown().await;
}

#[test]
fn r1_a_17_gateway_denies_accounts_outside_the_snapshot() {
    let gw = ToolGateway::new("run-1", 3, frozen_scope("scope-1"), budget_small());
    let host = TestHost::ok();
    let ctx = CallContext {
        request_id: "r".into(),
        library_id: "lib".into(),
        actor_kind: delta_app::contracts::ActorKind::Ai,
        run_id: Some("run-1".into()),
        generation: Some(3),
        scope_ref: "scope-1".into(),
        deadline: None,
    };
    let mut args = valid_args("get_portfolio_summary");
    args["scope"]["account_ids"] = json!(["acc-us", "acc-evil"]);
    let err = gw
        .execute(&host, &ctx, "c-wide", "get_portfolio_summary", &args)
        .unwrap_err();
    assert_eq!(err.code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn r1_a_21_numeric_contradiction_and_missing_insufficiency_are_rejected() {
    let forged = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益 99，证据 res:test-evidence-1。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rf\",\"usage\":{}}}\n\n";
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(forged),
        ScriptedResponse::sse(forged),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let err = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("numeric contradiction"), "{err}");
    ep.shutdown().await;

    let partial = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益 68，证据 res:test-evidence-1。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rp\",\"usage\":{}}}\n\n";
    let ep2 = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(partial),
        ScriptedResponse::sse(partial),
    ])
    .await;
    let mut host = TestHost::ok();
    host.quality = "partial".into();
    let rt2 = runtime_with(responses_connection(&ep2.url), &library, host);
    let session2 = rt2.store().create_session(&library.id).unwrap();
    let err = rt2
        .run_turn(run_request(
            &library,
            &session2,
            responses_connection(&ep2.url),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("insufficiency"), "{err}");
    ep2.shutdown().await;
}

#[tokio::test]
async fn r1_a_19_late_callback_after_success_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(script_vec(&RESP_TOOL_THEN_DONE)).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let outcome = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap();
    assert!(!rt.accept_late_callback(&outcome.run_id).unwrap());
    let other = tempfile::tempdir().unwrap();
    let switched = Library::create(&other.path().join("other.sqlite"), "USD").unwrap();
    assert!(switched.get_run(&outcome.run_id).unwrap().is_none());
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_18_compaction_keeps_a_tool_call_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let summary_stream = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Earlier the user discussed positions.\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rs\",\"usage\":{}}}\n\n";
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(summary_stream),
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[1].1),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
    let seed_run = rt
        .store()
        .create_run(&NewRun {
            session_id: session.clone(),
            generation: 0,
            scope_snapshot: frozen_scope("scope-1"),
            tool_schema_hash: "seed".into(),
            model_ref: "seed".into(),
            budget: budget_small(),
        })
        .unwrap();
    rt.store()
        .update_run_state(&seed_run.id, RunState::Failed)
        .unwrap();
    rt.store()
        .append_message(&NewMessage {
            session_id: session.clone(),
            run_id: seed_run.id.clone(),
            kind: MK::ToolCall,
            payload: json!({
                "type": "function_call",
                "call_id": "call-keep",
                "name": "get_portfolio_summary",
                "arguments": "{}"
            }),
            status: MS::Complete,
            connection_ref: None,
        })
        .unwrap();
    rt.store()
        .append_message(&NewMessage {
            session_id: session.clone(),
            run_id: seed_run.id.clone(),
            kind: MK::ToolOutput,
            payload: json!({
                "type": "function_call_output",
                "call_id": "call-keep",
                "output": "output-keep"
            }),
            status: MS::Complete,
            connection_ref: None,
        })
        .unwrap();
    for i in 0..3 {
        for (kind, text) in [(MK::User, "历史问题"), (MK::Assistant, "历史回答")] {
            rt.store()
                .append_message(&NewMessage {
                    session_id: session.clone(),
                    run_id: seed_run.id.clone(),
                    kind,
                    payload: json!({ "text": format!("{text} {i}：{}", "请解释持仓与费用。".repeat(30)) }),
                    status: MS::Complete,
                    connection_ref: None,
                })
                .unwrap();
        }
    }
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    rt.run_turn(req).await.unwrap();
    let body = ep.bodies.lock().unwrap()[0].clone();
    let call_at = body.find("call-keep").expect("summary includes the call");
    let out_at = body
        .find("output-keep")
        .expect("summary includes the output");
    assert!(call_at < out_at, "call and output stay in order");
    ep.shutdown().await;
}

// ---- RW-01 review regressions (Agent A) -------------------------------------------

const PLAIN_DONE: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"新范围。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"n\",\"usage\":{}}}\n\n";

fn other_scope() -> ScopeSnapshot {
    ScopeSnapshot::freeze(
        "scope-2",
        vec![AccountId::new("acc-other")],
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap(),
        CurrencyCode::usd(),
        ScopeView::Portfolio,
        0,
    )
}

/// A→B→B: the third run matches the second run's scope, but the first run's
/// scope-A tool output must still stay out of its context (F-11).
#[tokio::test]
async fn r1_a_17_a_later_run_in_the_new_scope_still_drops_the_old_scope() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[1].1),
        ScriptedResponse::sse(PLAIN_DONE),
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    rt.run_turn(run_request(
        &library,
        &session,
        responses_connection(&ep.url),
    ))
    .await
    .unwrap();
    for _ in 0..2 {
        let mut req = run_request(&library, &session, responses_connection(&ep.url));
        req.scope_snapshot = other_scope();
        rt.run_turn(req).await.unwrap();
    }
    let bodies = ep.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    for body in &bodies[2..] {
        assert!(!body.contains("test-evidence-1"), "{body}");
        assert!(!body.contains("function_call_output"), "{body}");
    }
    assert!(
        bodies[3].contains("新范围"),
        "same-scope history stays: {}",
        bodies[3]
    );
    ep.shutdown().await;
}

/// An interrupted call has no output. Sending it again would be an orphan
/// `function_call` that real endpoints reject (F-09).
#[tokio::test]
async fn r1_a_18_interrupted_call_is_not_resent_without_an_output() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(INTERRUPTED_TOOL),
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    rt.run_turn(run_request(
        &library,
        &session,
        responses_connection(&ep.url),
    ))
    .await
    .unwrap_err();
    rt.run_turn(run_request(
        &library,
        &session,
        responses_connection(&ep.url),
    ))
    .await
    .unwrap();
    let body = ep.bodies.lock().unwrap()[1].clone();
    assert!(!body.contains("call_x"), "orphan call was sent: {body}");
    ep.shutdown().await;
}

/// Chinese prose has no space between a label and its number (F-12).
#[tokio::test]
async fn r1_a_21_numbers_next_to_chinese_text_are_checked() {
    let forged = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益99，证据 res:test-evidence-1。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rf\",\"usage\":{}}}\n\n";
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(forged),
        ScriptedResponse::sse(forged),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let err = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("numeric contradiction"), "{err}");
    ep.shutdown().await;

    let honest = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"截至2026年1月31日（2026-01-31），总损益68，证据res:test-evidence-1。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rh\",\"usage\":{}}}\n\n";
    let ep2 = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(honest),
    ])
    .await;
    let rt2 = runtime_for(&ep2.url, &library);
    let session2 = rt2.store().create_session(&library.id).unwrap();
    rt2.run_turn(run_request(
        &library,
        &session2,
        responses_connection(&ep2.url),
    ))
    .await
    .unwrap();
    ep2.shutdown().await;

    let forged_ref = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益68，证据res:forged-7。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"rr\",\"usage\":{}}}\n\n";
    let ep3 = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(forged_ref),
        ScriptedResponse::sse(forged_ref),
    ])
    .await;
    let rt3 = runtime_for(&ep3.url, &library);
    let session3 = rt3.store().create_session(&library.id).unwrap();
    let err = rt3
        .run_turn(run_request(
            &library,
            &session3,
            responses_connection(&ep3.url),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("res:forged-7"), "{err}");
    ep3.shutdown().await;
}

fn seed_history(rt: &AgentRuntime<DeltaModelClient, Library>, session: &str, tag: &str) {
    use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
    let seed = rt
        .store()
        .create_run(&NewRun {
            session_id: session.into(),
            generation: rt.store().latest_generation(session).unwrap() + 1,
            scope_snapshot: frozen_scope("scope-1"),
            tool_schema_hash: "seed".into(),
            model_ref: "seed".into(),
            budget: budget_small(),
        })
        .unwrap();
    rt.store()
        .update_run_state(&seed.id, RunState::Failed)
        .unwrap();
    for i in 0..3 {
        for (kind, text) in [(MK::User, "历史问题"), (MK::Assistant, "历史回答")] {
            rt.store()
                .append_message(&NewMessage {
                    session_id: session.into(),
                    run_id: seed.id.clone(),
                    kind,
                    payload: json!({ "text": format!("{tag} {text} {i}：{}", "请解释持仓与费用。".repeat(30)) }),
                    status: MS::Complete,
                    connection_ref: None,
                })
                .unwrap();
        }
    }
}

/// A second compaction summarizes the first summary too; otherwise the
/// earlier history silently leaves the model context (context-state §4).
#[tokio::test]
async fn r1_a_18_second_compaction_carries_the_first_summary_forward() {
    let summary = |text: &str| {
        format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"rs\",\"usage\":{{}}}}}}\n\n")
    };
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(&summary("FIRST-SUMMARY-7")),
        ScriptedResponse::sse(PLAIN_DONE),
        ScriptedResponse::sse(&summary("SECOND-SUMMARY-8")),
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "early");
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    rt.run_turn(req).await.unwrap();
    seed_history(&rt, &session, "late");
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    rt.run_turn(req).await.unwrap();
    let bodies = ep.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4, "two compactions and two answers");
    assert!(
        bodies[2].contains("FIRST-SUMMARY-7"),
        "second summary input lost the first summary: {}",
        bodies[2]
    );
    assert!(bodies[3].contains("SECOND-SUMMARY-8"), "{}", bodies[3]);
    ep.shutdown().await;
}

/// A failed checkpoint write leaves the previous checkpoint active and the
/// transcript intact (R1-A-18).
#[tokio::test]
async fn r1_a_18_checkpoint_write_failure_keeps_the_previous_entry() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(&text_sse("FIRST-CHECKPOINT")),
        ScriptedResponse::sse(PLAIN_DONE),
        ScriptedResponse::sse(&text_sse("SHOULD-NOT-SWITCH")),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "keep");
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    rt.run_turn(req).await.unwrap();
    let first = rt
        .store()
        .active_checkpoint(&session)
        .unwrap()
        .expect("first compaction wrote a checkpoint");
    seed_history(&rt, &session, "more");
    rt.store()
        .fail_next_checkpoint
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    let err = rt.run_turn(req).await.unwrap_err();
    assert!(
        err.to_string().contains("checkpoint"),
        "write failure must surface: {err}"
    );
    let active = rt.store().active_checkpoint(&session).unwrap().unwrap();
    assert_eq!(
        active.id, first.id,
        "a failed write switched the checkpoint"
    );
    let kept = rt.store().messages(&session, None).unwrap();
    for tag in ["keep", "more"] {
        assert!(kept.iter().any(|m| m.payload.to_string().contains(tag)));
    }
    ep.shutdown().await;
}

/// Switching to a smaller-window model compacts again and keeps the old transcript.
#[tokio::test]
async fn r1_a_18_smaller_window_model_compacts_without_dropping_history() {
    let summary = |text: &str| {
        format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"rs\",\"usage\":{{}}}}}}\n\n")
    };
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(&summary("WIDE-SUMMARY")),
        ScriptedResponse::sse(PLAIN_DONE),
        ScriptedResponse::sse(&summary("NARROW-SUMMARY")),
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "wide-history");
    let mut wide = responses_connection(&ep.url);
    wide.model_id = "wide-model".into();
    let mut req = run_request(&library, &session, wide);
    req.budget.context_window_tokens = 800;
    rt.run_turn(req).await.unwrap();
    let before = rt.store().messages(&session, None).unwrap().len();
    seed_history(&rt, &session, "extra-history");
    let mut narrow = responses_connection(&ep.url);
    narrow.model_id = "narrow-model".into();
    let mut req = run_request(&library, &session, narrow.clone());
    req.budget.context_window_tokens = 250;
    req.connection = narrow;
    rt.run_turn(req).await.unwrap();
    let cp = rt
        .store()
        .active_checkpoint(&session)
        .unwrap()
        .expect("narrow model compacted");
    assert!(
        cp.model_ref.contains("narrow-model"),
        "checkpoint records the model that summarized: {}",
        cp.model_ref
    );
    assert!(cp.context_version.contains("ctx-v1:"));
    assert!(rt.store().messages(&session, None).unwrap().len() > before);
    ep.shutdown().await;
}

/// Scope change opens a new session. The old transcript stays stored and is
/// not sent, even when it would not fit the next window.
#[tokio::test]
async fn r1_a_17_scope_switch_opens_a_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![ScriptedResponse::sse(PLAIN_DONE)]).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "OLD-SCOPE-MARKER");
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.scope_snapshot = other_scope();
    req.budget.context_window_tokens = 500;
    let outcome = rt.run_turn(req).await.unwrap();
    assert_ne!(outcome.session_id, session);
    let body = ep.bodies.lock().unwrap()[0].clone();
    assert!(
        !body.contains("OLD-SCOPE-MARKER"),
        "old scope was sent: {body}"
    );
    assert!(rt
        .store()
        .messages(&session, None)
        .unwrap()
        .iter()
        .any(|m| m.payload.to_string().contains("OLD-SCOPE-MARKER")));
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_21_chat_numbers_are_not_receipt_claims() {
    let chat = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"随便聊聊，有 3 个想法。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"c\",\"usage\":{}}}\n\n";
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![ScriptedResponse::sse(chat)]).await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    rt.run_turn(run_request(
        &library,
        &session,
        responses_connection(&ep.url),
    ))
    .await
    .unwrap();
    ep.shutdown().await;
}

#[tokio::test]
async fn r1_a_21_report_without_a_receipt_is_rejected() {
    let report = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"总损益 68。\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{}}}\n\n";
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(report),
        ScriptedResponse::sse(report),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let err = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no receipt"),
        "a report number needs a receipt: {err}"
    );
    ep.shutdown().await;
}

/// Synthetic AC-02 library, real runtime and ProductionHost. The report's 68
/// and its parts come from the stored tool receipt, which can be opened.
#[tokio::test]
async fn r1_a_21_production_host_report_opens_the_golden_68() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.sqlite");
    let library = Arc::new(Library::create(&path, "USD").unwrap());
    library.ensure_account("acc-us", "US", "broker").unwrap();
    library.ensure_asset("USD", "fiat").unwrap();
    library.ensure_asset("AAPL", "equity").unwrap();
    library
        .ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")
        .unwrap();
    let csv = "\
occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote
2026-01-05T14:00:00Z,deposit,USD,2000,,,src-open,,
2026-01-06T14:00:00Z,buy,AAPL,10,100,1,src-buy,NASDAQ:AAPL,USD
2026-01-08T14:00:00Z,sell,AAPL,4,110,1,src-sell,NASDAQ:AAPL,USD
";
    let preview = library.preview_csv("acc-us", csv, "map-1").unwrap();
    let valid: Vec<i64> = preview
        .rows
        .iter()
        .filter(|r| r.state == "valid")
        .map(|r| r.row_no)
        .collect();
    library
        .commit_preview(&preview.batch_id, "map-1", &preview.file_hash, &valid)
        .unwrap();
    library
        .upsert_bar(
            "NASDAQ:AAPL",
            "2026-01-09",
            "2026-01-09T14:30:00Z",
            "2026-01-09T21:00:00Z",
            "105",
            "105",
            "105",
            "105",
            "1",
            "file",
            "raw",
            "v1",
        )
        .unwrap();
    let start = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let end = chrono::Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
    let frozen = ScopeSnapshot::freeze(
        "scope-e2e",
        vec![AccountId::new("acc-us")],
        start,
        end,
        CurrencyCode::usd(),
        ScopeView::Portfolio,
        library.ledger_revision().unwrap(),
    );
    let tool_args = serde_json::json!({
        "scope": {
            "account_ids": ["acc-us"],
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-01-10T00:00:00Z",
            "reporting_currency": "USD"
        },
        "period_start": "2026-01-01T00:00:00Z",
        "period_end": "2026-01-10T00:00:00Z"
    });
    // Evidence ids are content-addressed: one direct host call gives the id
    // the runtime's call will produce for the same scope and ledger.
    let probe = library
        .create_run(&NewRun {
            session_id: library.create_session(&library.id).unwrap(),
            generation: 1,
            scope_snapshot: frozen.clone(),
            tool_schema_hash: "probe".into(),
            model_ref: "probe".into(),
            budget: budget_small(),
        })
        .unwrap();
    let probe_ctx = CallContext {
        request_id: "probe".into(),
        library_id: library.id.clone(),
        actor_kind: ActorKind::Ai,
        run_id: Some(probe.id),
        generation: Some(1),
        scope_ref: frozen.scope_ref.clone(),
        deadline: None,
    };
    let evidence = ProductionHost::new(library.clone())
        .execute("explain_pnl", &probe_ctx, &tool_args)
        .unwrap()
        .evidence_refs[0]
        .clone();
    let arguments = tool_args.to_string();
    let item = serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": "call_pnl",
            "name": "explain_pnl",
            "arguments": arguments
        }
    });
    let done = serde_json::json!({
        "type": "response.completed",
        "response": {"id": "r1", "usage": {}}
    });
    let tool = format!(
        "event: response.output_item.done\ndata: {item}\n\nevent: response.completed\ndata: {done}\n\n"
    );
    let report = format!(
        "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"总损益 68，已实现 38.6，未实现 29.4，证据 {evidence}。\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r2\",\"usage\":{{}}}}}}\n\n"
    );
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(&tool),
        ScriptedResponse::sse(&report),
    ])
    .await;
    let client = DeltaModelClient::new(
        responses_connection(&ep.url),
        Arc::new(StaticCredentials { key: "k".into() }),
    )
    .unwrap();
    let store = Library::open(&path).unwrap();
    let host = Arc::new(ProductionHost::new(library.clone()));
    let rt = AgentRuntime::new(client, store, host);
    let session = rt.store().create_session(&library.id).unwrap();
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.scope_snapshot = frozen;
    let outcome = rt.run_turn(req).await.expect("golden report");
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(outcome.evidence_refs.iter().any(|r| r == &evidence));
    assert!(outcome.report_text.contains("68"));
    let opened = library.open_evidence(&evidence).unwrap();
    assert_eq!(opened.target_type, "result");
    for part in ["68", "38.6", "29.4"] {
        assert!(
            opened.body.contains(part),
            "opened receipt missing {part}: {}",
            opened.body
        );
    }
    let event_ref = outcome
        .evidence_refs
        .iter()
        .find(|r| r.starts_with("event:"))
        .expect("event evidence");
    let event = library.open_evidence(event_ref).unwrap();
    assert_eq!(event.target_type, "event");
    assert!(!event.body.is_empty());
    let note = library
        .save_journal("复盘", "黄金样本", &[], None, Some("acc-us"))
        .unwrap();
    let note_ref = library.journal_evidence_id(&note).unwrap();
    let note_body = library.open_evidence(&note_ref).unwrap();
    assert_eq!(note_body.revision.as_deref(), Some("1"));
    assert_eq!(note_body.body, "黄金样本");
    library
        .save_journal("复盘", "修订后", &[], Some(&note), Some("acc-us"))
        .unwrap();
    assert_eq!(library.open_evidence(&note_ref).unwrap().body, "黄金样本");
    ep.shutdown().await;
}

fn text_sse(text: &str) -> String {
    format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"t\",\"usage\":{{}}}}}}\n\n")
}

/// With tool receipts every number in the answer is checked, not only text
/// that names a PnL figure. Without receipts, a market value is still a
/// report number (F-12 / A-21).
#[tokio::test]
async fn r1_a_21_numbers_are_checked_without_pnl_keywords() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let forged = text_sse("组合市值 99999。");
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1),
        ScriptedResponse::sse(&forged),
        ScriptedResponse::sse(&forged),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let err = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await
        .expect_err("a number outside the receipts was accepted");
    assert!(err.to_string().contains("99999"), "{err}");
    ep.shutdown().await;

    let bare = text_sse("你的组合市值是 50000。");
    let ep2 = fake::spawn_endpoint(vec![
        ScriptedResponse::sse(&bare),
        ScriptedResponse::sse(&bare),
    ])
    .await;
    let rt2 = runtime_for(&ep2.url, &library);
    let session2 = rt2.store().create_session(&library.id).unwrap();
    let err = rt2
        .run_turn(run_request(
            &library,
            &session2,
            responses_connection(&ep2.url),
        ))
        .await
        .expect_err("a market value without a receipt was accepted");
    assert!(err.to_string().contains("no receipt"), "{err}");
    ep2.shutdown().await;
}

// ---- F-09 / R1-A-18: revocation while a run is compacting -----------------------
//
// R1 revocation is defined in `delta_app::ai::grants`: revoking a model
// connection, or narrowing the accounts the model may read (notes reach the
// model through their account). A revoked run stops its summary, writes no
// checkpoint, ends in the terminal `revoked` state, and sends nothing further.

use delta_app::ai::grants::Grants;
use delta_app::ai::runtime::{ModelClient, ModelError, ModelEvent, ModelRequest, StopReason};
use futures::stream::BoxStream;
use futures::StreamExt;

/// Wait until the endpoint has received `n` requests, so a revocation is
/// issued while the summary request is in flight, not before it is sent.
async fn wait_for_requests(requests: &Arc<std::sync::atomic::AtomicUsize>, n: usize) {
    for _ in 0..200 {
        if requests.load(std::sync::atomic::Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the endpoint never received request {n}");
}

fn revoked_runs(rt: &AgentRuntime<DeltaModelClient, Library>, session: &str) -> usize {
    rt.store()
        .runs(session)
        .unwrap()
        .iter()
        .filter(|run| run.state == RunState::Revoked)
        .count()
}

/// The summary request is slow; the connection is revoked while it is in flight.
#[tokio::test]
async fn r1_a_18_revoking_the_connection_during_compaction_stops_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse {
            delay_ms: 3000,
            ..ScriptedResponse::sse(&text_sse("LATE-SUMMARY"))
        },
        ScriptedResponse::sse(&text_sse("SUMMARY-AFTER-RESTORE")),
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let grants = rt.grants().clone();
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "REVOKE-OLD-SCOPE");
    let before = rt.store().messages(&session, None).unwrap().len();
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;

    let requests = ep.requests.clone();
    let revoker = tokio::spawn({
        let grants = grants.clone();
        async move {
            wait_for_requests(&requests, 1).await;
            grants.revoke_connection("conn-test")
        }
    });
    let started = std::time::Instant::now();
    let result = rt.run_turn(req).await;
    assert_eq!(revoker.await.unwrap(), 1, "exactly one running run stopped");
    match result {
        Err(AppError::Revoked(why)) => assert!(why.contains("conn-test"), "{why}"),
        other => panic!("expected a revoked run, got {other:?}"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2500),
        "revocation must interrupt the summary request, took {:?}",
        started.elapsed()
    );
    // The run is in a terminal state and says it was revoked.
    assert_eq!(revoked_runs(&rt, &session), 1);
    assert!(rt
        .store()
        .runs(&session)
        .unwrap()
        .iter()
        .all(|r| r.state.is_terminal()));
    // No checkpoint; the transcript is intact; the lease is released.
    assert!(rt.store().active_checkpoint(&session).unwrap().is_none());
    assert_eq!(
        rt.store().messages(&session, None).unwrap().len(),
        before + 1,
        "history intact; only the prompt of the revoked run was added"
    );
    assert_eq!(grants.active_runs(), 0);
    // Only the summary request ever left the process.
    assert_eq!(ep.requests.load(std::sync::atomic::Ordering::SeqCst), 1);

    // While revoked, a new run is refused at admission and sends nothing.
    let refused = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    assert!(matches!(refused, Err(AppError::Revoked(_))), "{refused:?}");
    assert_eq!(ep.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        rt.store().runs(&session).unwrap().len(),
        2,
        "no run row for a refused run"
    );

    // Restoring the connection lets the same session carry on and compact.
    grants.restore_connection("conn-test");
    let mut again = run_request(&library, &session, responses_connection(&ep.url));
    again.budget.context_window_tokens = 600;
    let outcome = rt.run_turn(again).await.expect("run after restore");
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(rt.store().active_checkpoint(&session).unwrap().is_some());
    ep.shutdown().await;
}

/// Narrowing the authorized accounts stops a run that reaches a removed
/// account; a narrowing that still covers the run leaves it alone.
#[tokio::test]
async fn r1_a_18_narrowing_accounts_during_compaction_stops_only_the_affected_run() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![
        ScriptedResponse {
            delay_ms: 3000,
            ..ScriptedResponse::sse(&text_sse("LATE-SUMMARY"))
        },
        ScriptedResponse {
            delay_ms: 700,
            ..ScriptedResponse::sse(&text_sse("KEPT-SUMMARY"))
        },
        ScriptedResponse::sse(PLAIN_DONE),
    ])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let grants = rt.grants().clone();
    let session = rt.store().create_session(&library.id).unwrap();
    seed_history(&rt, &session, "NARROW-OLD-SCOPE");
    let mut req = run_request(&library, &session, responses_connection(&ep.url));
    req.budget.context_window_tokens = 600;
    let requests = ep.requests.clone();
    let narrower = tokio::spawn({
        let grants = grants.clone();
        async move {
            wait_for_requests(&requests, 1).await;
            // acc-us (the scope of the run) is no longer authorized.
            grants.restrict_accounts(["acc-crypto"])
        }
    });
    let started = std::time::Instant::now();
    let result = rt.run_turn(req).await;
    assert_eq!(narrower.await.unwrap(), 1);
    match result {
        Err(AppError::Revoked(why)) => assert!(why.contains("acc-us"), "{why}"),
        other => panic!("expected a revoked run, got {other:?}"),
    }
    // Narrowing interrupts the in-flight summary; it does not merely refuse
    // the checkpoint once the slow summary has finished.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2500),
        "narrowing must interrupt the summary request, took {:?}",
        started.elapsed()
    );
    assert_eq!(revoked_runs(&rt, &session), 1);
    assert!(rt.store().active_checkpoint(&session).unwrap().is_none());
    assert_eq!(ep.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    // The old scope is refused at admission while it stays unauthorized.
    let refused = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    assert!(matches!(refused, Err(AppError::Revoked(_))), "{refused:?}");
    assert_eq!(ep.requests.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Authorizing the account again, then narrowing to a set that still
    // contains it, does not touch a running run.
    grants.restrict_accounts(["acc-us", "acc-crypto"]);
    let mut kept = run_request(&library, &session, responses_connection(&ep.url));
    kept.budget.context_window_tokens = 600;
    let requests = ep.requests.clone();
    let widen = tokio::spawn({
        let grants = grants.clone();
        async move {
            wait_for_requests(&requests, 2).await;
            grants.restrict_accounts(["acc-us"])
        }
    });
    let outcome = rt
        .run_turn(kept)
        .await
        .expect("a covered run is not stopped");
    assert_eq!(widen.await.unwrap(), 0, "nothing was stopped");
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(rt.store().active_checkpoint(&session).unwrap().is_some());
    ep.shutdown().await;
}

/// A model client that finishes the summary and, as the stream is dropped,
/// revokes the connection: the revocation lands after the last cancel check
/// of the summary and before the checkpoint write.
struct RevokeAfterSummary {
    connection: ModelConnectionConfig,
    grants: Grants,
    requests: std::sync::atomic::AtomicUsize,
}

struct RevokeOnDrop(Option<Box<dyn FnOnce() + Send>>);

impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        if let Some(revoke) = self.0.take() {
            revoke();
        }
    }
}

impl ModelClient for RevokeAfterSummary {
    fn connection(&self) -> &ModelConnectionConfig {
        &self.connection
    }

    fn stream(
        &self,
        _req: ModelRequest,
        _cancel: CancelToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let grants = self.grants.clone();
        let guard = RevokeOnDrop(Some(Box::new(move || {
            grants.revoke_connection("conn-test");
        })));
        let events = vec![
            ModelEvent::TextDelta {
                delta: "SUMMARY-THAT-MUST-NOT-LAND".into(),
            },
            ModelEvent::Completed {
                stop_reason: StopReason::End,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events).map(move |event| {
            let _keep_alive = &guard;
            event
        })))
    }
}

#[tokio::test]
async fn r1_a_18_a_revocation_after_the_summary_still_writes_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let grants = Grants::new();
    let client = RevokeAfterSummary {
        connection: responses_connection("http://127.0.0.1:9"),
        grants: grants.clone(),
        requests: std::sync::atomic::AtomicUsize::new(0),
    };
    let store = Library::open(&library.path).unwrap();
    let rt = AgentRuntime::new(client, store, Arc::new(TestHost::ok())).with_grants(grants.clone());
    let session = rt.store().create_session(&library.id).unwrap();
    // seed_history is typed for the HTTP client; seed the same shape here.
    {
        use delta_app::ai::session::{MessageKind as MK, MessageStatus as MS, NewMessage, NewRun};
        let seed = rt
            .store()
            .create_run(&NewRun {
                session_id: session.clone(),
                generation: 1,
                scope_snapshot: frozen_scope("scope-1"),
                tool_schema_hash: "seed".into(),
                model_ref: "seed".into(),
                budget: budget_small(),
            })
            .unwrap();
        rt.store()
            .update_run_state(&seed.id, RunState::Failed)
            .unwrap();
        for i in 0..3 {
            for (kind, text) in [(MK::User, "历史问题"), (MK::Assistant, "历史回答")] {
                rt.store()
                    .append_message(&NewMessage {
                        session_id: session.clone(),
                        run_id: seed.id.clone(),
                        kind,
                        payload: json!({ "text": format!("{text} {i}：{}", "请解释持仓与费用。".repeat(30)) }),
                        status: MS::Complete,
                        connection_ref: None,
                    })
                    .unwrap();
            }
        }
    }
    let mut req = run_request(
        &library,
        &session,
        responses_connection("http://127.0.0.1:9"),
    );
    req.budget.context_window_tokens = 600;
    let result = rt.run_turn(req).await;
    match result {
        Err(AppError::Revoked(why)) => assert!(why.contains("conn-test"), "{why}"),
        other => panic!("expected a revoked run, got {other:?}"),
    }
    assert!(
        rt.store().active_checkpoint(&session).unwrap().is_none(),
        "the summary finished, but the revoked run must not write it"
    );
    let runs = rt.store().runs(&session).unwrap();
    assert!(runs.iter().any(|r| r.state == RunState::Revoked));
    assert!(runs.iter().all(|r| r.state.is_terminal()));
    // Only the summary request was made: nothing went out after the revocation.
    assert!(
        rt.store()
            .messages(&session, None)
            .unwrap()
            .iter()
            .all(|m| !m.payload.to_string().contains("SUMMARY-THAT-MUST-NOT-LAND")),
        "the revoked summary must not reach the transcript"
    );
}

/// Revoking while the model is answering (no compaction) also ends the run in
/// the terminal revoked state, without executing a tool call it had asked for.
#[tokio::test]
async fn r1_a_18_revoking_during_a_model_turn_executes_no_tool_and_ends_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let ep = fake::spawn_endpoint(vec![ScriptedResponse {
        delay_ms: 3000,
        ..ScriptedResponse::sse(RESP_TOOL_THEN_DONE[0].1)
    }])
    .await;
    let rt = runtime_for(&ep.url, &library);
    let session = rt.store().create_session(&library.id).unwrap();
    let grants = rt.grants().clone();
    let requests = ep.requests.clone();
    let revoker = tokio::spawn(async move {
        wait_for_requests(&requests, 1).await;
        grants.revoke_connection("conn-test")
    });
    let result = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    assert_eq!(revoker.await.unwrap(), 1);
    assert!(matches!(result, Err(AppError::Revoked(_))), "{result:?}");
    let runs = rt.store().runs(&session).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, RunState::Revoked);
    assert!(
        rt.store().tool_calls(&runs[0].id).unwrap().is_empty(),
        "a revoked run executes no tool"
    );
    // A restart leaves the terminal state alone.
    assert_eq!(rt.store().mark_interrupted_runs().unwrap(), 0);
    assert_eq!(
        rt.store().runs(&session).unwrap()[0].state,
        RunState::Revoked
    );
    ep.shutdown().await;
}

/// Host whose first tool call revokes the connection, as a user would while a
/// tool is running. The second call the model asked for in the same turn must
/// not run.
struct RevokeOnFirstCall {
    grants: Grants,
    calls: std::sync::atomic::AtomicUsize,
}

impl AnalysisHost for RevokeOnFirstCall {
    fn execute(
        &self,
        _tool: &str,
        ctx: &CallContext,
        _args: &Value,
    ) -> Result<ResultEnvelope<Value>, AppError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.grants.revoke_connection("conn-test");
        }
        let mut envelope = ResultEnvelope::new(
            &ctx.request_id,
            json!({ "metric": "total_pnl", "value": "68" }),
        );
        envelope.evidence_refs = vec!["res:test-evidence-1".into()];
        Ok(envelope)
    }
}

fn summary_call_event(call_id: &str, as_of: &str) -> String {
    let arguments = json!({
        "scope": {
            "account_ids": ["acc-us"],
            "start_at": "2026-01-01T00:00:00Z",
            "end_at": "2026-01-31T00:00:00Z",
            "reporting_currency": "USD"
        },
        "as_of": as_of
    })
    .to_string();
    let item = json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": "get_portfolio_summary",
            "arguments": arguments
        }
    });
    format!("event: response.output_item.done\ndata: {item}\n\n")
}

/// The model asks for two tools in one turn. The user revokes while the first
/// runs: the second is never executed, is recorded as interrupted, and the run
/// ends revoked with no tool output sent back.
#[tokio::test]
async fn r1_a_18_revocation_between_two_tool_calls_skips_the_second() {
    let dir = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap());
    let body = format!(
        "{}{}event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r1\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}\n\n",
        summary_call_event("call_1", "2026-01-31T00:00:00Z"),
        summary_call_event("call_2", "2026-01-30T00:00:00Z"),
    );
    let ep = fake::spawn_endpoint(vec![ScriptedResponse::sse(&body)]).await;
    let grants = Grants::new();
    let host = Arc::new(RevokeOnFirstCall {
        grants: grants.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let client = DeltaModelClient::new(
        responses_connection(&ep.url),
        Arc::new(StaticCredentials { key: "k".into() }),
    )
    .unwrap();
    let rt = AgentRuntime::new(client, Library::open(&library.path).unwrap(), host.clone())
        .with_grants(grants);
    let session = rt.store().create_session(&library.id).unwrap();
    let result = rt
        .run_turn(run_request(
            &library,
            &session,
            responses_connection(&ep.url),
        ))
        .await;
    assert!(matches!(result, Err(AppError::Revoked(_))), "{result:?}");
    assert_eq!(
        host.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second tool must not run after the revocation"
    );
    let runs = rt.store().runs(&session).unwrap();
    assert_eq!(runs[0].state, RunState::Revoked);
    let calls = rt.store().tool_calls(&runs[0].id).unwrap();
    let status_of = |id: &str| {
        calls
            .iter()
            .find(|c| c.call_id == id)
            .map(|c| c.status.clone())
    };
    assert_eq!(status_of("call_1").as_deref(), Some("ok"));
    assert_eq!(status_of("call_2").as_deref(), Some("interrupted"));
    assert_eq!(
        ep.requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "nothing more is sent to the model after the revocation"
    );
    ep.shutdown().await;
}
