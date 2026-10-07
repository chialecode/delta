//! F-23 / R1-A-16: the connect budget and the proxy policy of the model client.
//!
//! Every test here is deterministic and offline. The connect phase is stalled
//! by a hanging resolver or by a local socket that accepts and never answers;
//! `ProxyPolicy::Direct` keeps any machine-wide proxy out of the result; and
//! the proxy that is exercised is a local fake. No test contacts an external
//! address.

mod fake;
use fake::*;

use delta_app::ai::runtime::{
    ModelConnectionConfig, ModelError, ModelEvent, Protocol, ProxyPolicy,
};
use delta_infra::model::{proxy_route, DeltaModelClient, ProxyRoute, StaticCredentials};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn failed_error_of(events: &[ModelEvent]) -> Option<ModelError> {
    events.iter().find_map(|e| match e {
        ModelEvent::Failed { error } => Some(error.clone()),
        _ => None,
    })
}

/// A 1s connect budget against a 60s total budget, never through a proxy.
fn connect_budget(base_url: &str) -> ModelConnectionConfig {
    let mut conn = responses_connection(base_url);
    conn.proxy = ProxyPolicy::Direct;
    conn.connect_timeout_secs = 1;
    conn.idle_timeout_secs = 30;
    conn.request_timeout_secs = 60;
    conn
}

/// The failure belongs to the connect phase and was retried as one.
fn assert_connect_timeout(error: &ModelError, elapsed: std::time::Duration) {
    assert!(
        error.safe_message.starts_with("connect timed out"),
        "not a connect-phase timeout: {}",
        error.safe_message
    );
    assert!(error.retryable, "a connect failure is retryable");
    assert_eq!(error.code, "PROVIDER_UNAVAILABLE");
    // Three attempts of 1s plus the 0.4s and 0.8s backoffs. The lower bound
    // proves the retries happened; the upper bound is far below the 60s total.
    assert!(
        elapsed >= std::time::Duration::from_millis(3000),
        "expected three connect attempts, elapsed {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(12),
        "the connect budget must not wait out the total budget, elapsed {elapsed:?}"
    );
}

/// A resolver whose lookups never finish: the name server is a black hole.
struct HangingResolver {
    lookups: Arc<AtomicUsize>,
}

impl reqwest::dns::Resolve for HangingResolver {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

/// Resolves every name to a closed loopback port, so a direct connection is
/// refused at once and nothing leaves the machine.
struct LoopbackResolver;

impl reqwest::dns::Resolve for LoopbackResolver {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async {
            let addrs: Vec<std::net::SocketAddr> = vec!["127.0.0.1:9".parse().unwrap()];
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn creds() -> Arc<StaticCredentials> {
    Arc::new(StaticCredentials {
        key: "test-secret-key".into(),
    })
}

#[tokio::test]
async fn r1_a_16_connect_timeout_hung_resolver_is_a_retried_connect_failure() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let client = DeltaModelClient::with_dns_resolver(
        connect_budget("https://model.blackhole.test"),
        creds(),
        Arc::new(HangingResolver {
            lookups: lookups.clone(),
        }),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let elapsed = started.elapsed();
    let error = failed_error_of(&events).expect("a hung resolver must fail");
    assert_connect_timeout(&error, elapsed);
    assert!(
        !error.safe_message.contains("blackhole"),
        "the host must not leak into the message: {}",
        error.safe_message
    );
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        3,
        "one attempt plus two connect retries"
    );
}

/// A server that completes the TCP handshake and then says nothing: the TLS
/// handshake never finishes, so the connect budget has to fire.
#[tokio::test]
async fn r1_a_16_connect_timeout_stalled_tls_handshake_is_a_retried_connect_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let seen_bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (accepted_in, seen_in) = (accepted.clone(), seen_bytes.clone());
    let holder = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            accepted_in.fetch_add(1, Ordering::SeqCst);
            // Read the ClientHello so it is on record, then stay silent.
            let mut buf = [0u8; 4096];
            if let Ok(n) = socket.read(&mut buf).await {
                seen_in.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            held.push(socket);
        }
    });
    let client = client_for(connect_budget(&format!("https://127.0.0.1:{port}")));
    let started = std::time::Instant::now();
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let elapsed = started.elapsed();
    let error = failed_error_of(&events).expect("a stalled handshake must fail");
    assert_connect_timeout(&error, elapsed);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        3,
        "one attempt plus two connect retries"
    );
    // The handshake never completed, so no HTTP request, and no bearer token,
    // reached the server.
    let wire = seen_bytes.lock().unwrap().clone();
    assert!(!wire.is_empty(), "the client did start a TLS handshake");
    assert!(
        !String::from_utf8_lossy(&wire).contains("test-secret-key"),
        "the key must not appear before the TLS handshake finishes"
    );
    holder.abort();
}

#[test]
fn r1_a_16_proxy_route_follows_the_policy_and_protects_plaintext_credentials() {
    let custom = ProxyPolicy::Custom("http://127.0.0.1:3128".into());
    let cases: Vec<(&ProxyPolicy, &str, Result<ProxyRoute, ()>)> = vec![
        (
            &ProxyPolicy::System,
            "https://api.example.com/v1",
            Ok(ProxyRoute::System),
        ),
        (
            &ProxyPolicy::Direct,
            "https://api.example.com/v1",
            Ok(ProxyRoute::Direct),
        ),
        (
            &custom,
            "https://api.example.com/v1",
            Ok(ProxyRoute::Custom),
        ),
        // Plaintext remote: never proxied, and an explicit proxy is refused.
        (
            &ProxyPolicy::System,
            "http://llm.lan:8000/v1",
            Ok(ProxyRoute::Direct),
        ),
        (
            &ProxyPolicy::Direct,
            "http://llm.lan:8000/v1",
            Ok(ProxyRoute::Direct),
        ),
        (&custom, "http://llm.lan:8000/v1", Err(())),
        (&custom, "http://203.0.113.9/v1", Err(())),
        // Loopback stays local under the system policy.
        (
            &ProxyPolicy::System,
            "http://127.0.0.1:9000",
            Ok(ProxyRoute::Direct),
        ),
        (
            &ProxyPolicy::System,
            "http://localhost:9000",
            Ok(ProxyRoute::Direct),
        ),
        (
            &ProxyPolicy::System,
            "http://[::1]:9000",
            Ok(ProxyRoute::Direct),
        ),
        // Loopback is direct even under a custom proxy: the proxy's loopback
        // is not the local service, and plaintext would expose the token.
        (&custom, "http://127.0.0.1:9000", Ok(ProxyRoute::Direct)),
        (&custom, "https://localhost:8443", Ok(ProxyRoute::Direct)),
        // Not an http(s) endpoint at all.
        (&ProxyPolicy::System, "ftp://example.com", Err(())),
    ];
    for (policy, url, expected) in cases {
        let got = proxy_route(policy, url).map_err(|_| ());
        assert_eq!(got, expected, "{policy:?} for {url}");
    }
    assert!(proxy_route(&ProxyPolicy::System, "not a url").is_err());
    // A connection saved before the policy existed follows the system.
    assert_eq!(ProxyPolicy::default(), ProxyPolicy::System);
    let legacy: ModelConnectionConfig = serde_json::from_value(serde_json::json!({
        "id": "c", "protocol": "openai-responses", "base_url": "https://x.test",
        "model_id": "m", "credential_ref": "r", "context_window_tokens": 1,
        "connect_timeout_secs": 1, "idle_timeout_secs": 1, "request_timeout_secs": 1
    }))
    .unwrap();
    assert_eq!(legacy.proxy, ProxyPolicy::System);
    // The policy round-trips through the stored JSON shape.
    for policy in [ProxyPolicy::System, ProxyPolicy::Direct, custom] {
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(serde_json::from_str::<ProxyPolicy>(&json).unwrap(), policy);
    }
}

/// A local proxy that records each request head and refuses the tunnel.
struct FakeProxy {
    url: String,
    heads: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeProxy {
    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

async fn spawn_fake_proxy() -> FakeProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let sink = heads.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());
            let _ = socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                )
                .await;
        }
    });
    FakeProxy {
        url: format!("http://127.0.0.1:{port}"),
        heads,
        task,
    }
}

#[tokio::test]
async fn r1_a_16_custom_proxy_is_used_for_an_https_endpoint() {
    let proxy = spawn_fake_proxy().await;
    let mut conn = responses_connection("https://model.example.test/v1");
    conn.proxy = ProxyPolicy::Custom(proxy.url.clone());
    conn.connect_timeout_secs = 2;
    let client = client_for(conn);
    assert_eq!(client.route(), ProxyRoute::Custom);
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    let error = failed_error_of(&events).expect("the fake proxy refuses the tunnel");
    assert!(!error.safe_message.contains("example.test"), "{error:?}");
    assert!(
        !error.safe_message.contains(&proxy.url),
        "the proxy URL must not leak: {error:?}"
    );
    let heads = proxy.heads();
    assert!(!heads.is_empty(), "the request must have used the proxy");
    assert!(
        heads
            .iter()
            .all(|h| h.starts_with("CONNECT model.example.test:443")),
        "the proxy only ever sees an opaque tunnel request: {heads:?}"
    );
    assert!(
        heads
            .iter()
            .all(|h| !h.to_ascii_lowercase().contains("authorization")
                && !h.contains("test-secret-key")),
        "the key never reaches the proxy in the clear: {heads:?}"
    );
    proxy.task.abort();
}

#[tokio::test]
async fn r1_a_16_plaintext_remote_endpoint_refuses_a_proxy_and_ignores_the_system_one() {
    let proxy = spawn_fake_proxy().await;
    let mut conn = responses_connection("http://203.0.113.9:8000/v1");
    conn.proxy = ProxyPolicy::Custom(proxy.url.clone());
    let refused = DeltaModelClient::new(conn.clone(), creds())
        .err()
        .expect("a plaintext remote endpoint must not be built with a proxy");
    assert_eq!(refused.code, "INVALID_ARGUMENT");
    assert!(refused.safe_message.contains("plaintext"), "{refused:?}");
    assert!(
        !refused.safe_message.contains(&proxy.url),
        "the proxy URL must not be echoed"
    );
    // The system policy builds, but its route is direct.
    conn.proxy = ProxyPolicy::System;
    let client = client_for(conn);
    assert_eq!(client.route(), ProxyRoute::Direct);
    assert!(proxy.heads().is_empty(), "nothing was sent to the proxy");
    proxy.task.abort();
}

/// Environment proxies are process-global, so the System policy is checked in
/// child processes whose environment this test controls. The children run the
/// ignored probe below against the fake proxy; no other proxy is involved.
#[tokio::test]
async fn r1_a_16_system_policy_follows_the_environment_proxy_and_direct_ignores_it() {
    let exe = std::env::current_exe().unwrap();
    let run_child = |mode: &'static str, proxy_url: String| {
        let exe = exe.clone();
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(exe);
            cmd.args([
                "--exact",
                "r1_a_16_proxy_probe_child",
                "--ignored",
                "--nocapture",
            ]);
            for var in [
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "no_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "NO_PROXY",
                "REQUEST_METHOD",
            ] {
                cmd.env_remove(var);
            }
            cmd.env("HTTP_PROXY", &proxy_url)
                .env("HTTPS_PROXY", &proxy_url)
                .env("DELTA_PROXY_PROBE_MODE", mode);
            let out = cmd.output().expect("run the probe child");
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            // A filter that matched nothing would also exit 0; the probe
            // must have actually run.
            assert!(
                stdout.contains(" 1 passed;"),
                "the probe child did not run: {stdout}"
            );
            (out.status.success(), stdout)
        })
    };

    // System policy + https remote: the environment proxy carries the request.
    let proxy = spawn_fake_proxy().await;
    let (ok, out) = run_child("system-https", proxy.url.clone()).await.unwrap();
    assert!(ok, "system-https probe failed: {out}");
    assert!(
        proxy
            .heads()
            .iter()
            .any(|h| h.starts_with("CONNECT model.example.test:443")),
        "System must follow the environment proxy: {:?} / {out}",
        proxy.heads()
    );
    proxy.task.abort();

    // Direct ignores the same environment.
    let proxy = spawn_fake_proxy().await;
    let (ok, out) = run_child("direct-https", proxy.url.clone()).await.unwrap();
    assert!(ok, "direct-https probe failed: {out}");
    assert!(
        proxy.heads().is_empty(),
        "Direct must not use the environment proxy: {:?}",
        proxy.heads()
    );
    proxy.task.abort();

    // System policy + plaintext remote: the request does not go to the proxy.
    let proxy = spawn_fake_proxy().await;
    let (ok, out) = run_child("system-plaintext-remote", proxy.url.clone())
        .await
        .unwrap();
    assert!(ok, "system-plaintext-remote probe failed: {out}");
    assert!(
        proxy.heads().is_empty(),
        "plaintext credentials must not cross the proxy: {:?}",
        proxy.heads()
    );
    proxy.task.abort();
}

/// Child of the test above. Ignored so a normal run skips it; the parent runs
/// it with `--ignored` and a controlled environment.
#[tokio::test]
#[ignore = "run by the environment-proxy test, which controls the environment"]
async fn r1_a_16_proxy_probe_child() {
    let Ok(mode) = std::env::var("DELTA_PROXY_PROBE_MODE") else {
        return;
    };
    let (url, policy, resolver): (&str, ProxyPolicy, bool) = match mode.as_str() {
        "system-https" => ("https://model.example.test/v1", ProxyPolicy::System, false),
        // The name resolves to a closed loopback port, so a direct attempt is
        // refused at once instead of reaching for the network.
        "direct-https" => ("https://model.example.test/v1", ProxyPolicy::Direct, true),
        "system-plaintext-remote" => ("http://model.plain.test:9/v1", ProxyPolicy::System, true),
        other => panic!("unknown probe mode {other}"),
    };
    let mut conn = responses_connection(url);
    conn.proxy = policy;
    conn.connect_timeout_secs = 2;
    let client = if resolver {
        DeltaModelClient::with_dns_resolver(conn, creds(), Arc::new(LoopbackResolver)).unwrap()
    } else {
        DeltaModelClient::new(conn, creds()).unwrap()
    };
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(
        failed_error_of(&events).is_some(),
        "no endpoint exists, so the request must fail"
    );
}

/// A loopback endpoint is reached directly under every policy: a custom proxy
/// would carry a plaintext request, bearer token included, to the proxy, and
/// the proxy's own loopback is not the user's local model.
#[tokio::test]
async fn r1_a_16_loopback_endpoint_never_goes_through_a_custom_proxy() {
    let proxy = spawn_fake_proxy().await;
    let ep = spawn_endpoint(vec![ScriptedResponse::sse(RESP_TEXT_STREAM)]).await;
    let mut conn = responses_connection(&ep.url);
    conn.proxy = ProxyPolicy::Custom(proxy.url.clone());
    let client = client_for(conn);
    assert_eq!(client.route(), ProxyRoute::Direct);
    let events = collect(&client, sample_request(Protocol::OpenaiResponses)).await;
    assert!(
        proxy.heads().is_empty(),
        "a plaintext loopback request crossed the proxy: {:?}",
        proxy.heads()
    );
    assert_eq!(ep.requests.load(Ordering::SeqCst), 1);
    assert!(failed_error_of(&events).is_none(), "{events:?}");
    proxy.task.abort();
    ep.shutdown().await;
}
