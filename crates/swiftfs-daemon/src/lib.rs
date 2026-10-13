//! SwiftFS daemon library: manifest indexing, the read-only FUSE filesystem,
//! and mount orchestration.

// This crate contains the one narrow piece of `unsafe` in SwiftFS: reading the
// mounting user's uid/gid via `libc::getuid`/`getgid`, which are trivial,
// infallible FFI calls with no memory safety implications. Every other crate
// uses `#![forbid(unsafe_code)]`. See docs/design-decisions.md (D7).
#![deny(unsafe_code)]

pub mod benchmark;
pub mod fs;
pub mod index;
pub mod mount;

pub use benchmark::{run_benchmark, run_full_suite, BenchConfig, BenchmarkResult};
pub use fs::Swiftfs;
pub use index::{ChunkRead, ManifestIndex};
pub use mount::{mount_image, MountConfig, MountError};
