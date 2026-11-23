use std::io::Read;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::Timeframe;
use reqwest::Client;

use crate::providers::{ProviderError, DEFAULT_USER_AGENT};

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETRY_AFTER_SECS: u64 = 5;

pub fn validate_provider_base_url(base: &str) -> Result<(), ProviderError> {
    let url = reqwest::Url::parse(base).map_err(|_| ProviderError::Parse {
        provider: "configuration".into(),
        detail: "invalid provider base URL".into(),
    })?;
    let loopback = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(ProviderError::Parse {
            provider: "configuration".into(),
            detail: "base URL must use HTTPS (HTTP only for loopback), without credentials, query or fragment".into(),
        });
    }
    Ok(())
}

pub(crate) fn sanitize_untrusted_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in raw.chars() {
        if count >= max_chars {
            out.push_str("...");
            break;
        }
        if ch.is_control() {
            if matches!(ch, '\t' | '\n' | '\r') {
                out.push(' ');
                count += 1;
            }
        } else {
            out.push(ch);
            count += 1;
        }
    }
    out
}

pub(crate) fn build_provider_url(
    provider: &str,
    base_url: &str,
    segments: &[&str],
    query: &[(&str, &str)],
    trailing_slash: bool,
) -> Result<String, ProviderError> {
    let mut url =
        reqwest::Url::parse(base_url.trim_end_matches('/')).map_err(|_| ProviderError::Parse {
            provider: provider.into(),
            detail: "invalid provider base URL".into(),
        })?;
    {
        let mut segs = url.path_segments_mut().map_err(|_| ProviderError::Parse {
            provider: provider.into(),
            detail: "provider base URL cannot be a base".into(),
        })?;
        segs.pop_if_empty();
        for s in segments {
            segs.push(s);
        }
        if trailing_slash {
            segs.push("");
        }
    }
    if !query.is_empty() {
        let mut qp = url.query_pairs_mut();
        for (k, v) in query {
            qp.append_pair(k, v);
        }
    }
    Ok(url.to_string())
}

pub fn build_http_client(timeout_ms: u64) -> Result<Client, ProviderError> {
    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        if let Some(prev) = attempt.previous().last() {
            if prev.host_str() != attempt.url().host_str()
                || prev.port_or_known_default() != attempt.url().port_or_known_default()
            {
                return attempt.error("cross-host redirect blocked");
            }
        }
        attempt.follow()
    });
    Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .redirect(redirect_policy)
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(timeout_ms.min(5000)))
        .build()
        .map_err(|e| ProviderError::Http {
            provider: "reqwest".into(),
            status: None,
            message: e.to_string(),
            retryable: false,
        })
}

/// Execute an HTTP GET with bounded retries and exponential backoff on HTTP 429 / 5xx / transient I/O.
pub async fn get_text_with_retry(
    client: &Client,
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    max_retries: usize,
) -> Result<String, ProviderError> {
    let body = get_bytes_with_retry(client, provider, url, headers, max_retries).await?;
    String::from_utf8(body).map_err(|_| ProviderError::Parse {
        provider: provider.into(),
        detail: "invalid UTF-8 response".into(),
    })
}

pub async fn get_bytes_with_retry(
    client: &Client,
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    max_retries: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut attempt = 0usize;
    loop {
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(mut resp) => {
                let status = resp.status();
                let code = status.as_u16();
                if status.is_success() {
                    let mut body = Vec::new();
                    let mut chunk_err = None;
                    loop {
                        match resp.chunk().await {
                            Ok(Some(chunk)) => {
                                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                                    return Err(ProviderError::Parse {
                                        provider: provider.into(),
                                        detail: "response exceeds 16 MiB".into(),
                                    });
                                }
                                body.extend_from_slice(&chunk);
                            }
                            Ok(None) => break,
                            Err(e) => {
                                chunk_err = Some(e);
                                break;
                            }
                        }
                    }
                    if let Some(err) = chunk_err {
                        if attempt < max_retries {
                            attempt += 1;
                            tokio::time::sleep(Duration::from_millis(50 * (1 << attempt.min(5))))
                                .await;
                            continue;
                        }
                        return Err(ProviderError::Http {
                            provider: provider.into(),
                            status: Some(code),
                            message: err.without_url().to_string(),
                            retryable: true,
                        });
                    }
                    return Ok(body);
                }
                if code == 429 || (500..600).contains(&code) {
                    if attempt < max_retries {
                        let retry_delay = if code == 429 {
                            resp.headers()
                                .get(reqwest::header::RETRY_AFTER)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .map(|secs| Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)))
                        } else {
                            None
                        };
                        attempt += 1;
                        let delay = retry_delay
                            .unwrap_or_else(|| Duration::from_millis(50 * (1 << attempt.min(5))));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    if code == 429 {
                        return Err(ProviderError::RateLimited {
                            provider: provider.to_string(),
                        });
                    }
                }
                return Err(ProviderError::Http {
                    provider: provider.to_string(),
                    status: Some(code),
                    message: format!("HTTP {code}"),
                    retryable: code == 429 || (500..600).contains(&code),
                });
            }
            Err(err) => {
                if attempt < max_retries {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(50 * (1 << attempt.min(5)))).await;
                    continue;
                }
                return Err(ProviderError::Http {
                    provider: provider.to_string(),
                    status: err.status().map(|s| s.as_u16()),
                    message: err.without_url().to_string(),
                    retryable: true,
                });
            }
        }
    }
}

/// Stable bar row id: daily bars keep the historical `session_date` key, intraday bars key on
/// the UTC open instant so the 24 hourly bars of one date never collide.
pub fn provider_bar_id(
    instrument_id: &str,
    timeframe: Timeframe,
    source: &str,
    session_date: NaiveDate,
    open_at: DateTime<Utc>,
    dataset_version: i64,
) -> String {
    if timeframe.is_intraday() {
        format!(
            "BAR:{instrument_id}:{timeframe}:raw:{source}:{}:{dataset_version}",
            open_at.format("%Y-%m-%dT%H:%MZ")
        )
    } else {
        format!("BAR:{instrument_id}:1d:raw:{source}:{session_date}:{dataset_version}")
    }
}

pub(crate) fn provider_timestamp(seconds: i64) -> Result<DateTime<Utc>, ProviderError> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .ok_or_else(|| ProviderError::Parse {
            provider: "market-data".into(),
            detail: "timestamp out of range".into(),
        })
}

/// Reads the single CSV member of a Binance archive entirely in memory with a hard
/// decompressed byte cap (`MAX_RESPONSE_BYTES` = 16 MiB) enforced via `Read::take`
/// so forged ZIP headers / zip bombs cannot trigger unbounded memory growth.
pub(crate) fn read_single_csv_member(archive_bytes: &[u8]) -> Result<String, ProviderError> {
    let parse = |detail: String| ProviderError::Parse {
        provider: "binance-archive".into(),
        detail,
    };
    let cursor = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| parse(e.to_string()))?;
    if archive.len() != 1 {
        return Err(parse("expected one CSV member".into()));
    }
    let file = archive.by_index(0).map_err(|e| parse(e.to_string()))?;
    let safe_name = file
        .enclosed_name()
        .filter(|p| p.components().count() == 1)
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
        });
    if file.size() > MAX_RESPONSE_BYTES as u64
        || safe_name.is_none()
        || file.is_symlink()
        || file.is_dir()
        || !file.is_file()
    {
        return Err(parse("unsafe archive member".into()));
    }
    let mut buf = Vec::new();
    file.take((MAX_RESPONSE_BYTES as u64) + 1)
        .read_to_end(&mut buf)
        .map_err(|e| parse(e.to_string()))?;
    if buf.len() > MAX_RESPONSE_BYTES {
        return Err(parse("decompressed archive member exceeds 16 MiB".into()));
    }
    String::from_utf8(buf).map_err(|e| parse(e.to_string()))
}
