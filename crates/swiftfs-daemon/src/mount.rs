//! Mount orchestration: load an image, build the index and store, and run a
//! read-only FUSE session on a dedicated Tokio runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuser::{Config, MountOption, Session, SessionACL};
use swiftfs_cache::{ChunkFetcher, DiskCache, FetcherConfig, RamCache};
use swiftfs_format::load_manifest;
use swiftfs_storage::{ChunkStore, HttpStore, HttpStoreConfig, LocalStore};
use tracing::{info, warn};

use crate::fs::Swiftfs;
use crate::index::ManifestIndex;
use crate::metrics_server::MetricsServer;

/// Which storage backend serves chunk bytes.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Content-addressed chunk objects in the image's `chunks/` directory.
    Local,
    /// HTTP byte-range requests against a remote object server. The object id
    /// is appended to `base_url`.
    Http { base_url: String },
}

/// Optional persistent disk cache configuration.
#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    /// Directory that holds content-addressed chunk files.
    pub dir: PathBuf,
    /// Approximate capacity in bytes.
    pub capacity_bytes: u64,
}

/// Configuration for a mount.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Path to the SwiftFS image directory.
    pub image: PathBuf,
    /// Mountpoint.
    pub target: PathBuf,
    /// Storage backend that serves chunk bytes.
    pub backend: Backend,
    /// Whether to expose the mount to other users (`allow_other`).
    pub allow_other: bool,
    /// Enable `auto_unmount` so the mount is torn down when the process exits.
    pub auto_unmount: bool,
    /// Number of `fuser` session worker threads (Linux only).
    pub n_threads: usize,
    /// Enable sequential read-ahead prefetching.
    pub read_ahead: bool,
    /// Bounded RAM cache capacity in bytes (0 disables the RAM cache tier is
    /// not supported; a value of 0 is treated as the default).
    pub ram_cache_bytes: u64,
    /// Optional persistent disk cache.
    pub disk_cache: Option<DiskCacheConfig>,
    /// Maximum concurrent backend fetches.
    pub max_concurrent_fetches: usize,
    /// Per-request storage timeout.
    pub storage_timeout: Duration,
    /// Optional `host:port` to serve Prometheus metrics on.
    pub metrics_listen: Option<String>,
    /// Max time to spend loading the manifest and mounting before giving up.
    pub mount_timeout: Duration,
}

/// Default RAM cache capacity (256 MiB).
pub const DEFAULT_RAM_CACHE_BYTES: u64 = 256 * 1024 * 1024;

impl MountConfig {
    /// Defaults for a read-only local mount.
    pub fn new(image: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            target: target.into(),
            backend: Backend::Local,
            allow_other: false,
            auto_unmount: false,
            n_threads: 4,
            read_ahead: true,
            ram_cache_bytes: DEFAULT_RAM_CACHE_BYTES,
            disk_cache: None,
            max_concurrent_fetches: 32,
            storage_timeout: Duration::from_secs(15),
            metrics_listen: None,
            mount_timeout: Duration::from_secs(10),
        }
    }
}

/// Errors from mount orchestration.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("image error: {0}")]
    Image(#[from] swiftfs_core::SwiftfsError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tokio runtime error: {0}")]
    Runtime(String),
}

/// Build the storage backend selected by `config`.
fn build_backend(config: &MountConfig) -> Result<Arc<dyn ChunkStore>, MountError> {
    match &config.backend {
        Backend::Local => Ok(Arc::new(LocalStore::for_image(&config.image))),
        Backend::Http { base_url } => {
            let mut http_config = HttpStoreConfig::new(base_url.clone());
            http_config.timeout = config.storage_timeout;
            let store = HttpStore::new(http_config)?;
            Ok(Arc::new(store))
        }
    }
}

/// Build a `ChunkFetcher` with the configured RAM cache, optional disk cache,
/// concurrency bound, and timeout.
fn build_chunk_fetcher(
    backend: Arc<dyn ChunkStore>,
    config: &MountConfig,
) -> Result<Arc<ChunkFetcher>, MountError> {
    let ram_bytes = if config.ram_cache_bytes == 0 {
        DEFAULT_RAM_CACHE_BYTES
    } else {
        config.ram_cache_bytes
    };
    let ram = Arc::new(RamCache::new(ram_bytes));

    let disk = match &config.disk_cache {
        Some(dc) => Some(Arc::new(DiskCache::new(&dc.dir, dc.capacity_bytes)?)),
        None => None,
    };

    let fetcher_config = FetcherConfig {
        max_concurrent_fetches: config.max_concurrent_fetches.max(1),
        fetch_timeout: config.storage_timeout,
    };

    Ok(Arc::new(ChunkFetcher::new(
        backend,
        ram,
        disk,
        fetcher_config,
    )))
}

/// Build the `fuser` `Config` from a `MountConfig`.
fn build_fuser_config(config: &MountConfig) -> Config {
    let mut options = vec![
        MountOption::RO,
        MountOption::FSName("swiftfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if config.allow_other {
        options.push(MountOption::CUSTOM("allow_other".to_string()));
    }
    if config.auto_unmount {
        options.push(MountOption::AutoUnmount);
    }
    let acl = if config.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };

    let mut fuser_config = Config::default();
    fuser_config.mount_options = options;
    fuser_config.acl = acl;
    #[cfg(target_os = "linux")]
    {
        fuser_config.n_threads = Some(config.n_threads.max(1));
    }
    fuser_config
}
/// Mount `config.image` at `config.target`, blocking until the filesystem is
/// unmounted.
///
/// A dedicated Tokio multi-threaded runtime is created here. The FUSE session
/// runs on its own threads; `read` callbacks bridge to this runtime via a
/// oneshot channel (see `fs.rs`). The runtime is dropped after the session
/// ends, cancelling any in-flight storage tasks.
#[allow(unsafe_code)] // narrowly scoped uid/gid FFI; see the SAFETY note below
pub fn mount_image(config: &MountConfig) -> Result<(), MountError> {
    let t0 = std::time::Instant::now();
    let manifest = load_manifest(&config.image)?;
    let index = Arc::new(ManifestIndex::new(manifest)?);
    info!(
        nodes = index.manifest().nodes.len(),
        load_ms = t0.elapsed().as_millis(),
        "manifest loaded"
    );

    let store = build_backend(config)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("swiftfs-io")
        .build()
        .map_err(|e| MountError::Runtime(e.to_string()))?;
    let handle = runtime.handle().clone();

    // Resolve the mounting user for reported file ownership.
    //
    // SAFETY: `getuid`/`getgid` are infallible FFI calls with no preconditions
    // and no memory-safety implications. They cannot fail on Linux.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

    let fetcher = build_chunk_fetcher(store, config)?;

    // Start the optional metrics endpoint; it lives as long as this function.
    let _metrics = start_metrics(config, &fetcher)?;

    let fuser_config = build_fuser_config(config);

    let fs = Swiftfs::with_read_ahead(index, fetcher, handle, uid, gid, config.read_ahead);

    let session = Session::new(fs, &config.target, &fuser_config)?;
    info!(target = %config.target.display(), "filesystem mounted");

    let result = session.run();
    // Dropping the runtime cancels in-flight tasks after the session loop ends.
    drop(runtime);

    match result {
        Ok(()) => {
            info!("filesystem unmounted cleanly");
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, "session ended with error");
            Err(MountError::Io(e))
        }
    }
}

/// Convenience wrapper used by the CLI and tests.
pub fn mount_path(image: &Path, target: &Path) -> Result<(), MountError> {
    mount_image(&MountConfig::new(image, target))
}

/// A mounted filesystem running in a background `fuser` thread, used by tests
/// and any embedder that wants control over the session lifetime.
pub struct MountedHandle {
    session: fuser::BackgroundSession,
    runtime: tokio::runtime::Runtime,
    metrics: Option<MetricsServer>,
}

impl MountedHandle {
    /// Unmount and join the session, then drop the runtime and metrics server.
    pub fn unmount(self) -> Result<(), MountError> {
        let MountedHandle {
            session,
            runtime,
            metrics,
        } = self;
        let res = session.umount_and_join();
        drop(runtime);
        drop(metrics);
        res.map_err(MountError::Io)
    }
}

/// Mount `config.image` at `config.target` and return a handle without blocking.
///
/// The returned handle owns the Tokio runtime backing storage I/O and the
/// background `fuser` session. Call [`MountedHandle::unmount`] to tear down.
#[allow(unsafe_code)] // narrowly scoped uid/gid FFI
pub fn mount_image_background(config: &MountConfig) -> Result<MountedHandle, MountError> {
    let manifest = load_manifest(&config.image)?;
    let index = Arc::new(ManifestIndex::new(manifest)?);
    let store = build_backend(config)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("swiftfs-io")
        .build()
        .map_err(|e| MountError::Runtime(e.to_string()))?;
    let handle = runtime.handle().clone();

    // SAFETY: `getuid`/`getgid` are infallible FFI calls with no preconditions
    // and no memory-safety implications.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

    let fetcher = build_chunk_fetcher(store, config)?;

    // Keep the metrics endpoint alive for the lifetime of the mount handle.
    let metrics = start_metrics(config, &fetcher)?;

    let fuser_config = build_fuser_config(config);

    let fs = Swiftfs::with_read_ahead(index, fetcher, handle, uid, gid, config.read_ahead);

    let session = Session::new(fs, &config.target, &fuser_config)?.spawn()?;

    Ok(MountedHandle {
        session,
        runtime,
        metrics,
    })
}

/// Start the metrics endpoint if configured.
fn start_metrics(
    config: &MountConfig,
    fetcher: &Arc<ChunkFetcher>,
) -> Result<Option<MetricsServer>, MountError> {
    match &config.metrics_listen {
        Some(addr) => {
            let server = MetricsServer::start(addr, Arc::clone(fetcher))?;
            info!(addr = %server.addr(), "metrics endpoint listening");
            Ok(Some(server))
        }
        None => Ok(None),
    }
}
