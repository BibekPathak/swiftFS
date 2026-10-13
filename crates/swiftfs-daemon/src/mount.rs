//! Mount orchestration: load an image, build the index and store, and run a
//! read-only FUSE session on a dedicated Tokio runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuser::{Config, MountOption, Session, SessionACL};
use swiftfs_cache::{ChunkFetcher, FetcherConfig, RamCache};
use swiftfs_format::load_manifest;
use swiftfs_storage::{ChunkStore, LocalStore};
use tracing::{info, warn};

use crate::fs::Swiftfs;
use crate::index::ManifestIndex;

/// Configuration for a mount.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Path to the SwiftFS image directory.
    pub image: PathBuf,
    /// Mountpoint.
    pub target: PathBuf,
    /// Allow other users to access the mount.
    pub allow_other: bool,
    /// Enable `auto_unmount` so the mount is torn down when the process exits.
    pub auto_unmount: bool,
    /// Number of `fuser` session worker threads (Linux only).
    pub n_threads: usize,
    /// Max time to spend loading the manifest and mounting before giving up.
    pub mount_timeout: Duration,
}

impl MountConfig {
    /// Defaults for a read-only local mount.
    pub fn new(image: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            target: target.into(),
            allow_other: false,
            auto_unmount: false,
            n_threads: 4,
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

fn build_chunk_fetcher(
    store: Arc<dyn ChunkStore>,
    _disk_cache_config: Option<(PathBuf, u64)>,
) -> Arc<ChunkFetcher> {
    let ram = Arc::new(RamCache::new(256 * 1024 * 1024)); // 256 MiB RAM cache;

    let fetcher_config = FetcherConfig::default();

    Arc::new(ChunkFetcher::new(
        store,
        ram,
        None,
        fetcher_config,
    ))
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

    let store: Arc<dyn ChunkStore> = Arc::new(LocalStore::for_image(&config.image));

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

    let fetcher = build_chunk_fetcher(store, None);

    let mut fuser_config = Config::default();
    fuser_config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("swiftfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if config.allow_other {
        fuser_config.mount_options.push(MountOption::AutoUnmount);
        fuser_config.acl = SessionACL::All;
    } else {
        fuser_config.acl = SessionACL::Owner;
    }
    #[cfg(target_os = "linux")]
    {
        fuser_config.n_threads = Some(config.n_threads.max(1));
    }

    let fs = Swiftfs::new(index, fetcher, handle, uid, gid);

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
}

impl MountedHandle {
    /// Unmount and join the session, then drop the runtime.
    pub fn unmount(self) -> Result<(), MountError> {
        let MountedHandle { session, runtime } = self;
        let res = session.umount_and_join();
        drop(runtime);
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
    let store: Arc<dyn ChunkStore> = Arc::new(LocalStore::for_image(&config.image));

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

    let fetcher = build_chunk_fetcher(store, None);

    let mut fuser_config = Config::default();
    fuser_config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("swiftfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if config.allow_other {
        fuser_config.mount_options.push(MountOption::AutoUnmount);
        fuser_config.acl = SessionACL::All;
    } else {
        fuser_config.acl = SessionACL::Owner;
    }
    #[cfg(target_os = "linux")]
    {
        fuser_config.n_threads = Some(config.n_threads.max(1));
    }

    let fs = Swiftfs::new(index, fetcher, handle, uid, gid);

    let session = Session::new(fs, &config.target, &fuser_config)?.spawn()?;

    Ok(MountedHandle { session, runtime })
}

#[allow(dead_code)]
fn build_fuser_config(config: &MountConfig) -> Config {
    let mut options = vec![
        MountOption::FSName("swiftfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    let acl = if config.allow_other {
        options.push(MountOption::AutoUnmount);
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    let mut fuser_config = Config::default();
    fuser_config.mount_options = options;
    fuser_config.acl = acl;
    fuser_config.n_threads = Some(config.n_threads.max(1));
    fuser_config
}