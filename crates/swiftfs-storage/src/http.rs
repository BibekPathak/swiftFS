//! HTTP byte-range storage backend.
//!
//! Performs `GET` requests with a `Range` header to fetch a chunk from an
//! object server. It validates the response status and length, applies a
//! per-request timeout, and retries transient failures with exponential
//! backoff. Permanent failures (e.g. 404, 416) are not retried.
//!
//! Server requirements: the object server must support HTTP range requests,
//! answering `206 Partial Content` with a correct `Content-Length`. A `200 OK`
//! response to a ranged request indicates the server ignored the range; because
//! that would require downloading the whole object (and could be huge), it is
//! treated as an error rather than silently accepted, unless the requested range
//! covers the whole object.
//!
//! Cancellation: dropping the future returned by [`HttpStore::fetch`] cancels
//! the in-flight request (reqwest propagates the drop to hyper).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use swiftfs_core::metrics::{Counter, Histogram};
use swiftfs_core::{Result, SwiftfsError};
use tracing::{debug, warn};

use crate::{ChunkLocation, ChunkStore};

/// HTTP backend configuration.
#[derive(Debug, Clone)]
pub struct HttpStoreConfig {
    /// Base URL; the object id is appended as a path segment.
    pub base_url: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Maximum number of attempts (1 = no retries).
    pub max_attempts: u32,
    /// Initial backoff before the first retry.
    pub initial_backoff: Duration,
    /// Upper bound on a single request's body size, to avoid unbounded
    /// allocation from a hostile server.
    pub max_bytes: u32,
}

impl HttpStoreConfig {
    /// Build a config for `base_url` with sensible defaults.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: Duration::from_secs(10),
            max_attempts: 4,
            initial_backoff: Duration::from_millis(50),
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Metrics for the HTTP backend.
#[derive(Debug, Default)]
pub struct HttpStoreMetrics {
    /// Requests issued (including retries).
    pub requests: Counter,
    /// Successful chunk fetches.
    pub successes: Counter,
    /// Retried attempts.
    pub retries: Counter,
    /// Permanent failures.
    pub failures: Counter,
    /// Bytes successfully fetched.
    pub bytes: Counter,
    /// Fetch latency histogram.
    pub latency: Histogram,
}

/// HTTP byte-range backend.
pub struct HttpStore {
    client: Client,
    config: HttpStoreConfig,
    metrics: Arc<HttpStoreMetrics>,
}

impl HttpStore {
    /// Construct a backend. Fails only if the HTTP client cannot be built.
    pub fn new(config: HttpStoreConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| SwiftfsError::Storage(format!("http client build failed: {e}")))?;
        Ok(Self {
            client,
            config,
            metrics: Arc::new(HttpStoreMetrics::default()),
        })
    }

    /// Access metrics for reporting.
    pub fn metrics(&self) -> Arc<HttpStoreMetrics> {
        Arc::clone(&self.metrics)
    }

    fn object_url(&self, object: &str) -> String {
        format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            object
        )
    }

    /// Decide whether an error is worth retrying.
    fn is_retryable(err: &SwiftfsError) -> bool {
        match err {
            SwiftfsError::Storage(_) => true,
            _ => false,
        }
    }
}

#[async_trait]
impl ChunkStore for HttpStore {
    async fn fetch(&self, location: &ChunkLocation) -> Result<Vec<u8>> {
        if location.length == 0 {
            return Ok(Vec::new());
        }
        if location.length > self.config.max_bytes {
            return Err(SwiftfsError::Storage(format!(
                "requested chunk length {} exceeds max_bytes {}",
                location.length, self.config.max_bytes
            )));
        }

        let url = self.object_url(&location.object);
        let end = location
            .offset
            .checked_add(u64::from(location.length) - 1)
            .ok_or_else(|| SwiftfsError::Storage("range overflow".into()))?;
        let range = format!("bytes={}-{}", location.offset, end);

        let started = Instant::now();
        let mut backoff = self.config.initial_backoff;
        let mut last_err: Option<SwiftfsError> = None;

        for attempt in 1..=self.config.max_attempts {
            self.metrics.requests.inc();
            match self.try_fetch(&url, &range, location.length).await {
                Ok(data) => {
                    self.metrics.successes.inc();
                    self.metrics.bytes.add(data.len() as u64);
                    self.metrics.latency.observe_duration(started.elapsed());
                    debug!(object = %location.object, len = data.len(), attempt, "http fetch");
                    return Ok(data);
                }
                Err(e) => {
                    if attempt < self.config.max_attempts && Self::is_retryable(&e) {
                        self.metrics.retries.inc();
                        debug!(object = %location.object, attempt, error = %e, "retrying");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(2));
                        last_err = Some(e);
                        continue;
                    }
                    self.metrics.failures.inc();
                    self.metrics.latency.observe_duration(started.elapsed());
                    warn!(object = %location.object, attempt, error = %e, "http fetch failed");
                    return Err(e);
                }
            }
        }

        self.metrics.failures.inc();
        Err(last_err.unwrap_or_else(|| SwiftfsError::Storage("http fetch exhausted".into())))
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

impl HttpStore {
    async fn try_fetch(&self, url: &str, range: &str, expect_len: u32) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(url)
            .header(RANGE, range)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    SwiftfsError::Storage(format!("timeout fetching {url}: {e}"))
                } else {
                    SwiftfsError::Storage(format!("request to {url} failed: {e}"))
                }
            })?;

        let status = resp.status();

        // Permanent errors: no retry.
        if status == StatusCode::NOT_FOUND {
            return Err(SwiftfsError::ChunkNotFound(url.to_string()));
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(SwiftfsError::IntegrityFailure(format!(
                "server reports range not satisfiable for {url} ({range})"
            )));
        }

        // Transient server errors: retry.
        if status.is_server_error() {
            return Err(SwiftfsError::Storage(format!(
                "server error {status} for {url}"
            )));
        }

        if status == StatusCode::OK {
            // Server ignored the Range header. Only acceptable if we asked for
            // the whole object; otherwise refuse rather than buffer it all.
            let body = resp
                .bytes()
                .await
                .map_err(|e| SwiftfsError::Storage(format!("reading {url}: {e}")))?;
            if body.len() == expect_len as usize {
                return Ok(body.to_vec());
            }
            return Err(SwiftfsError::Storage(format!(
                "server ignored Range for {url}: got {} bytes, expected exactly {}",
                body.len(),
                expect_len
            )));
        }

        if status != StatusCode::PARTIAL_CONTENT {
            return Err(SwiftfsError::Storage(format!(
                "unexpected status {status} for {url}"
            )));
        }

        // Validate the Content-Range length if present.
        if let Some(cr) = resp.headers().get(CONTENT_RANGE) {
            if let Ok(s) = cr.to_str() {
                if let Some(total_len) = parse_content_range_len(s) {
                    if total_len != u64::from(expect_len) {
                        return Err(SwiftfsError::IntegrityFailure(format!(
                            "Content-Range length {total_len} != expected {expect_len} for {url}"
                        )));
                    }
                }
            }
        }

        let body = resp
            .bytes()
            .await
            .map_err(|e| SwiftfsError::Storage(format!("reading {url}: {e}")))?;

        if body.len() != expect_len as usize {
            return Err(SwiftfsError::IntegrityFailure(format!(
                "short read from {url}: got {} bytes, expected {}",
                body.len(),
                expect_len
            )));
        }

        Ok(body.to_vec())
    }
}

/// Parse the length from a `Content-Range: bytes START-END/LEN` header.
fn parse_content_range_len(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?;
    total.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range_length() {
        assert_eq!(parse_content_range_len("bytes 0-9/100"), Some(100));
        assert_eq!(parse_content_range_len("bytes 5-14/*"), None);
        assert_eq!(parse_content_range_len("garbage"), None);
    }

    #[test]
    fn object_url_joins_cleanly() {
        let store = HttpStore::new(HttpStoreConfig::new("http://127.0.0.1:9/base/")).unwrap();
        assert_eq!(store.object_url("abc"), "http://127.0.0.1:9/base/abc");
    }
}
