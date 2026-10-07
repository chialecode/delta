//! Narrow Rust model client (D-08 / rust-model-client.md).
//!
//! Layering: transport (HTTP/TLS via reqwest, proxy policy, timeouts, bounded
//! responses, limited retry) → SSE decode (eventsource-stream) → protocol
//! adapters (Responses / Chat Completions) → typed ModelEvents. The client
//! never touches business data, sessions or tools; it does not log request
//! bodies or credentials; it never retries once events are visible.

mod adapters;

use adapters::{encode_request, AdapterState};
use delta_app::ai::runtime::{
    ModelClient, ModelConnectionConfig, ModelError, ModelEvent, ModelRequest, ProxyPolicy,
};
use delta_app::contracts::CancelToken;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Resolves the API key for a credential reference (keyring in production,
/// static in tests). The key never gets logged or serialized.
pub trait CredentialSource: Send + Sync {
    fn resolve(&self, credential_ref: &str) -> Result<String, ModelError>;
}

pub struct OsCredentials;
impl CredentialSource for OsCredentials {
    fn resolve(&self, reference: &str) -> Result<String, ModelError> {
        crate::sqlite::app::read_os_credential(reference).map_err(|_| ModelError {
            code: "CREDENTIAL_UNAVAILABLE".into(),
            retryable: false,
            safe_message: "Credential unavailable in the OS credential store".into(),
            provider_request_id: None,
            retry_after_secs: None,
        })
    }
}

/// Persistent user configuration is stricter than the transport's test routing
/// seam: no URL credentials, query/fragment secrets, or remote plaintext.
pub fn validate_saved_connection(
    c: &ModelConnectionConfig,
) -> Result<(), crate::error::InfraError> {
    let fail = || {
        crate::error::InfraError::Rejected("invalid model settings: HTTPS (or loopback HTTP), no URL credentials/query/fragment, and explicit model/key reference/timeouts required".into())
    };
    let url = reqwest::Url::parse(&c.base_url).map_err(|_| fail())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && is_loopback_host(&url)))
        || c.id.trim().is_empty()
        || c.model_id.trim().is_empty()
        || c.credential_ref.trim().is_empty()
        || c.context_window_tokens < 256
        || c.connect_timeout_secs == 0
        || c.idle_timeout_secs == 0
        || c.request_timeout_secs == 0
    {
        return Err(fail());
    }
    if let ProxyPolicy::Custom(raw) = &c.proxy {
        let proxy = reqwest::Url::parse(raw).map_err(|_| fail())?;
        if !matches!(proxy.scheme(), "http" | "https")
            || !proxy.username().is_empty()
            || proxy.password().is_some()
            || proxy.query().is_some()
            || proxy.fragment().is_some()
        {
            return Err(fail());
        }
    }
    Ok(())
}

/// Static source for controlled tests (loopback endpoints only).
pub struct StaticCredentials {
    pub key: String,
}

impl CredentialSource for StaticCredentials {
    fn resolve(&self, _credential_ref: &str) -> Result<String, ModelError> {
        Ok(self.key.clone())
    }
}

/// Cumulative/record bounds (rust-model-client.md §5; B-locked values).
pub const MAX_EVENT_BYTES: usize = 1024 * 1024; // 1 MiB per SSE record
pub const MAX_TOTAL_STREAM_BYTES: usize = 32 * 1024 * 1024; // per request
pub const MAX_EVENTS_PER_REQUEST: usize = 50_000;
/// Bounded event queue between the socket task and the caller. `send().await`
/// applies backpressure instead of buffering the stream without limit.
pub const EVENT_QUEUE_CAPACITY: usize = 64;
const MAX_RETRIES: usize = 2;

#[derive(Clone)]
pub struct DeltaModelClient {
    connection: ModelConnectionConfig,
    credentials: Arc<dyn CredentialSource>,
    http: reqwest::Client,
}

/// Route a request takes to the endpoint, decided from the proxy policy and
/// the base URL before any connection is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyRoute {
    /// No proxy.
    Direct,
    /// Whatever the system / environment configures.
    System,
    /// The proxy named in the connection.
    Custom,
}

fn invalid_argument(message: &str) -> ModelError {
    ModelError {
        code: "INVALID_ARGUMENT".into(),
        retryable: false,
        safe_message: message.into(),
        provider_request_id: None,
        retry_after_secs: None,
    }
}

/// True for `localhost` and loopback addresses, the only plaintext targets a
/// credential may be sent to without TLS.
fn is_loopback_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Decide the proxy route (F-23). Rules:
/// - loopback endpoints are always reached directly, under every policy: a
///   proxy's own loopback is not the user's local service, and a plaintext
///   request through it would hand the bearer token to the proxy;
/// - a plaintext `http` endpoint that is not loopback is never proxied, since
///   the bearer token would cross the proxy in the clear: the system policy
///   connects directly and an explicit proxy is rejected;
/// - otherwise the policy applies as configured.
pub fn proxy_route(policy: &ProxyPolicy, base_url: &str) -> Result<ProxyRoute, ModelError> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| invalid_argument("base URL is not a valid absolute URL"))?;
    let plaintext = match url.scheme() {
        "https" => false,
        "http" => true,
        _ => return Err(invalid_argument("base URL must use http or https")),
    };
    if is_loopback_host(&url) {
        return Ok(ProxyRoute::Direct);
    }
    Ok(match policy {
        ProxyPolicy::Direct => ProxyRoute::Direct,
        ProxyPolicy::System if plaintext => ProxyRoute::Direct,
        ProxyPolicy::System => ProxyRoute::System,
        ProxyPolicy::Custom(_) if plaintext => {
            return Err(invalid_argument(
                "a plaintext http endpoint cannot be reached through a proxy; use https or connect directly",
            ))
        }
        ProxyPolicy::Custom(_) => ProxyRoute::Custom,
    })
}

impl DeltaModelClient {
    pub fn new(
        connection: ModelConnectionConfig,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ModelError> {
        Self::build(connection, credentials, None)
    }

    /// Like [`Self::new`] with a custom DNS resolver (pinned addresses, DoH,
    /// or a deliberately hanging one in tests of the connect budget).
    pub fn with_dns_resolver(
        connection: ModelConnectionConfig,
        credentials: Arc<dyn CredentialSource>,
        resolver: Arc<dyn reqwest::dns::Resolve>,
    ) -> Result<Self, ModelError> {
        Self::build(connection, credentials, Some(resolver))
    }

    /// The route requests take to the endpoint.
    pub fn route(&self) -> ProxyRoute {
        // Validated in `build`, so this cannot fail for a constructed client.
        proxy_route(&self.connection.proxy, &self.connection.base_url).unwrap_or(ProxyRoute::Direct)
    }

    fn build(
        connection: ModelConnectionConfig,
        credentials: Arc<dyn CredentialSource>,
        resolver: Option<Arc<dyn reqwest::dns::Resolve>>,
    ) -> Result<Self, ModelError> {
        if connection.base_url.contains("key=") || connection.base_url.contains("api_key=") {
            return Err(invalid_argument(
                "credentials must not be embedded in the URL",
            ));
        }
        let route = proxy_route(&connection.proxy, &connection.base_url)?;
        let connect = connection.connect_timeout_secs.max(1);
        let idle = connection.idle_timeout_secs.max(1);
        let total = connection.request_timeout_secs.max(1);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(connect))
            .read_timeout(std::time::Duration::from_secs(idle))
            .timeout(std::time::Duration::from_secs(total))
            // Redirects are not followed: a 3xx must not move the bearer
            // token to another host (R1-A-16).
            .redirect(reqwest::redirect::Policy::none());
        builder = match (route, &connection.proxy) {
            (ProxyRoute::Direct, _) => builder.no_proxy(),
            (ProxyRoute::System, _) => builder,
            (ProxyRoute::Custom, ProxyPolicy::Custom(proxy_url)) => {
                // Only http(s) proxies are built in; the URL is not echoed.
                let proxy = reqwest::Proxy::all(proxy_url.as_str())
                    .map_err(|_| invalid_argument("proxy URL is not a valid http(s) proxy"))?;
                builder.proxy(proxy)
            }
            (ProxyRoute::Custom, _) => unreachable!("a custom route comes from a custom policy"),
        };
        if let Some(resolver) = resolver {
            builder = builder.dns_resolver(resolver);
        }
        let http = builder.build().map_err(|e| ModelError {
            code: "PROTOCOL_ERROR".into(),
            retryable: false,
            safe_message: format!("http client init failed: {}", e.without_url()),
            provider_request_id: None,
            retry_after_secs: None,
        })?;
        Ok(Self {
            connection,
            credentials,
            http,
        })
    }

    fn endpoint(&self) -> String {
        let base = self.connection.base_url.trim_end_matches('/');
        match self.connection.protocol {
            delta_app::ai::runtime::Protocol::OpenaiResponses => {
                format!("{base}/responses")
            }
            delta_app::ai::runtime::Protocol::OpenaiChatCompletions => {
                format!("{base}/chat/completions")
            }
        }
    }

    async fn send_once(&self, body: &serde_json::Value) -> Result<reqwest::Response, ModelError> {
        let key = self.credentials.resolve(&self.connection.credential_ref)?;
        let url = self.endpoint();
        self.http
            .post(&url)
            .bearer_auth(key)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| self.transport_error(e))
    }

    /// Classify a transport failure by phase. Only a failed connect (DNS, TCP,
    /// TLS) is known not to have reached the provider, so only that is safe to
    /// retry; a timeout after the connection was made is not.
    fn transport_error(&self, e: reqwest::Error) -> ModelError {
        let connect = e.is_connect();
        let timeout = e.is_timeout();
        // reqwest errors carry URLs; strip them from the message.
        let e = e.without_url();
        let safe_message = if connect && timeout {
            format!(
                "connect timed out after {}s",
                self.connection.connect_timeout_secs.max(1)
            )
        } else if connect {
            format!("connection failed: {e}")
        } else if timeout {
            "request timed out".to_string()
        } else {
            format!("request failed: {e}")
        };
        ModelError {
            code: "PROVIDER_UNAVAILABLE".into(),
            retryable: connect,
            safe_message,
            provider_request_id: None,
            retry_after_secs: None,
        }
    }

    /// Connect phase with bounded retries. 401/403 never retry; 429/5xx and
    /// transport errors retry at most MAX_RETRIES times, honoring Retry-After.
    async fn connect(&self, body: &serde_json::Value) -> Result<reqwest::Response, ModelError> {
        let mut attempt = 0usize;
        loop {
            let response = match self.send_once(body).await {
                Ok(r) => r,
                Err(e) if e.retryable && attempt < MAX_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(200 * (1 << attempt)))
                        .await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let safe = match status.as_u16() {
                401 | 403 => "authentication failed (credentials rejected)".to_string(),
                429 => "rate limited by provider".to_string(),
                s if s >= 500 => format!("provider server error ({s})"),
                s => format!("provider rejected the request ({s})"),
            };
            let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
            if !retryable || attempt >= MAX_RETRIES {
                return Err(ModelError {
                    code: if status.as_u16() == 429 {
                        "RATE_LIMITED".into()
                    } else {
                        "PROVIDER_UNAVAILABLE".into()
                    },
                    retryable,
                    safe_message: safe,
                    provider_request_id: None,
                    retry_after_secs: retry_after,
                });
            }
            attempt += 1;
            let delay = std::time::Duration::from_millis(
                retry_after
                    .map(|s| s * 1000)
                    .unwrap_or(200 * (1 << attempt)),
            );
            tokio::time::sleep(delay.min(std::time::Duration::from_secs(5))).await;
        }
    }
}

impl ModelClient for DeltaModelClient {
    fn connection(&self) -> &ModelConnectionConfig {
        &self.connection
    }

    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancelToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        let mut state =
            AdapterState::for_protocol(self.connection.protocol).map_err(|e| ModelError {
                code: "UNSUPPORTED_CAPABILITY".into(),
                retryable: false,
                safe_message: e.safe_message,
                provider_request_id: None,
                retry_after_secs: None,
            })?;
        let body = encode_request(&self.connection, &req)?;
        let this = self.clone();

        let (tx, rx) = mpsc::channel::<ModelEvent>(EVENT_QUEUE_CAPACITY);
        tokio::spawn(async move {
            // Cancellation covers the connect phase too (rust-model-client §5).
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                r = this.connect(&body) => match r {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(ModelEvent::Failed { error: e }).await;
                        return;
                    }
                }
            };
            // Byte-level bounds: per-record and cumulative caps.
            let byte_stream = response.bytes_stream();
            let bounded = bounded_bytes(byte_stream);
            let events = bounded.eventsource().map(|res| match res {
                Ok(ev) => (ev.event, ev.data, false),
                Err(e) => (
                    "__error__".to_string(),
                    format!("stream decode failed: {e}"),
                    true,
                ),
            });
            tokio::pin!(events);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        // Cancellation drops the connection; no further events.
                        return;
                    }
                    ev = events.next() => {
                        match ev {
                            Some((kind, data, is_error)) => {
                                let decoded = if is_error {
                                    vec![ModelEvent::Failed {
                                        error: ModelError {
                                            code: "PROTOCOL_ERROR".into(),
                                            retryable: false,
                                            safe_message: data,
                                            provider_request_id: None,
                                            retry_after_secs: None,
                                        },
                                    }]
                                } else {
                                    adapters::handle_event(&mut state, &kind, &data)
                                };
                                for ev in decoded {
                                    if tx.send(ev).await.is_err() {
                                        return; // receiver dropped
                                    }
                                }
                            }
                            None => {
                                // EOF: adapter decides whether a terminal was seen.
                                if let Some(ev) = adapters::on_eof(&mut state) {
                                    let _ = tx.send(ev).await;
                                }
                                return;
                            }
                        }
                    }
                }
            }
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
            as BoxStream<'static, ModelEvent>)
    }
}

/// Stream error for byte-level bound enforcement.
#[derive(Debug)]
pub enum StreamError {
    /// Transport error with the URL already stripped.
    Reqwest(reqwest::Error),
    RecordTooLarge,
    LimitExceeded,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Reqwest(e) => write!(f, "{e}"),
            StreamError::RecordTooLarge => write!(f, "SSE record exceeds size limit"),
            StreamError::LimitExceeded => write!(f, "stream size limit exceeded"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Bytes of the SSE record currently being received. An SSE record ends at
/// a blank line (`\n\n`, `\r\n\r\n` or `\r\r`), which may span chunks.
#[derive(Default)]
struct RecordMeter {
    since_boundary: usize,
    prev: u8,
    line_has_content: bool,
}

impl RecordMeter {
    /// Feed one chunk; `false` once any single record exceeds the cap.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        for &b in chunk {
            match b {
                b'\n' if self.prev == b'\r' => {}
                b'\n' | b'\r' => {
                    if !self.line_has_content {
                        self.since_boundary = 0;
                    }
                    self.line_has_content = false;
                }
                _ => {
                    self.line_has_content = true;
                    self.since_boundary += 1;
                    if self.since_boundary > MAX_EVENT_BYTES {
                        return false;
                    }
                }
            }
            self.prev = b;
        }
        true
    }
}

/// Enforce per-record and total byte caps over the raw stream. The first
/// violation or transport error is yielded once and ends the stream.
fn bounded_bytes(
    s: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures::Stream<Item = Result<bytes::Bytes, StreamError>> + Send + 'static {
    use futures::stream;
    let s = Box::pin(s);
    stream::unfold(
        (s, RecordMeter::default(), 0usize, false),
        |(mut s, mut meter, total, done)| async move {
            if done {
                return None;
            }
            match s.next().await {
                Some(Ok(chunk)) => {
                    let total = total + chunk.len();
                    if total > MAX_TOTAL_STREAM_BYTES {
                        Some((Err(StreamError::LimitExceeded), (s, meter, total, true)))
                    } else if !meter.feed(&chunk) {
                        Some((Err(StreamError::RecordTooLarge), (s, meter, total, true)))
                    } else {
                        Some((Ok(chunk), (s, meter, total, false)))
                    }
                }
                Some(Err(e)) => Some((
                    Err(StreamError::Reqwest(e.without_url())),
                    (s, meter, total, true),
                )),
                None => None,
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_meter_resets_at_blank_lines_across_chunks() {
        let mut m = RecordMeter::default();
        let record = format!("data: {}\n", "x".repeat(1000));
        for _ in 0..2000 {
            // ~2 MB in total, each record far below the cap; the blank-line
            // boundary arrives in its own chunk.
            assert!(m.feed(record.as_bytes()));
            assert!(m.feed(b"\r\n"));
        }
        assert_eq!(m.since_boundary, 0);
    }

    #[test]
    fn record_meter_rejects_one_oversized_record() {
        let mut m = RecordMeter::default();
        let line = format!("data: {}\n", "y".repeat(64 * 1024));
        let mut ok = true;
        for _ in 0..20 {
            ok &= m.feed(line.as_bytes());
        }
        assert!(!ok, "a 1.25 MiB multi-line record must be rejected");
    }
}
