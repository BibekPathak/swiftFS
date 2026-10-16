//! Optional Prometheus-text metrics endpoint.
//!
//! When enabled, a small thread serves `GET /metrics` over a plain
//! `TcpListener` (no async runtime, no new dependencies). The body is rendered
//! from the live [`ChunkFetcher`](swiftfs_cache::ChunkFetcher) metrics, which
//! are the same counters the read path updates.
//!
//! The endpoint is intentionally minimal: one line per metric in Prometheus
//! text exposition format, plus the chunk-latency histogram via
//! [`swiftfs_core::metrics::write_histogram`]. It is read-only and never
//! mutates state.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use swiftfs_cache::ChunkFetcher;

/// Render the fetcher's metrics as Prometheus text.
pub fn render(fetcher: &ChunkFetcher) -> String {
    let m = fetcher.metrics();
    let mut out = String::new();

    let mut counter = |name: &str, help: &str, v: u64| {
        out.push_str(&format!("# HELP {name} {help}\n"));
        out.push_str(&format!("# TYPE {name} counter\n"));
        out.push_str(&format!("{name} {v}\n"));
    };
    counter(
        "swiftfs_chunk_requests_total",
        "Chunk lookups (RAM/disk/backend).",
        m.requests.get(),
    );
    counter(
        "swiftfs_ram_cache_hits_total",
        "Chunk lookups served from the RAM cache.",
        m.ram_hits.get(),
    );
    counter(
        "swiftfs_disk_cache_hits_total",
        "Chunk lookups served from the disk cache.",
        m.disk_hits.get(),
    );
    counter(
        "swiftfs_backend_fetches_total",
        "Backend fetches actually performed after coalescing.",
        m.backend_fetches.get(),
    );
    counter(
        "swiftfs_backend_bytes_total",
        "Actual bytes returned by the backend (sum of verified chunk lengths).",
        m.backend_bytes.get(),
    );
    counter(
        "swiftfs_coalesced_requests_total",
        "Requests that joined an in-flight fetch.",
        m.coalesced.get(),
    );
    counter(
        "swiftfs_integrity_failures_total",
        "Chunk integrity verification failures.",
        m.integrity_failures.get(),
    );
    counter(
        "swiftfs_fetch_failures_total",
        "Backend/timeout failures.",
        m.fetch_failures.get(),
    );

    out.push_str("# HELP swiftfs_chunk_latency_seconds End-to-end chunk retrieval latency.\n");
    swiftfs_core::metrics::write_histogram(
        &mut out,
        "swiftfs_chunk_latency_seconds",
        "End-to-end chunk retrieval latency.",
        &m.latency,
    );

    out
}

/// A running metrics endpoint. Dropping it signals the serving thread to stop.
pub struct MetricsServer {
    running: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl MetricsServer {
    /// Start serving metrics on `addr`. The listener binds immediately, so an
    /// invalid address or a port in use is reported as an error.
    pub fn start(addr: &str, fetcher: Arc<ChunkFetcher>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);

        std::thread::Builder::new()
            .name("swiftfs-metrics".into())
            .spawn(move || {
                while flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let body = render(&fetcher);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let mut buf = [0u8; 512];
                            let _ = stream.read(&mut buf); // consume request head
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            running,
            addr: local,
        })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_core_metrics() {
        let store = std::sync::Arc::new(swiftfs_storage::LocalStore::new("/nonexistent"));
        let ram = std::sync::Arc::new(swiftfs_cache::RamCache::new(1024));
        let fetcher = ChunkFetcher::new(store, ram, None, swiftfs_cache::FetcherConfig::default());
        let text = render(&fetcher);
        assert!(text.contains("swiftfs_chunk_requests_total"));
        assert!(text.contains("swiftfs_backend_fetches_total"));
        assert!(text.contains("swiftfs_backend_bytes_total"));
        assert!(text.contains("swiftfs_chunk_latency_seconds_count"));
    }
}
