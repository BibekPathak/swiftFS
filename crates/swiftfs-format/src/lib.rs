//! SwiftFS image format: manifest definition, builder, validation, and verify.
//!
//! The image is a directory containing `manifest.json` and a `chunks/`
//! directory of content-addressed chunk objects. See `docs/image-format.md`.

#![forbid(unsafe_code)]

pub mod builder;
pub mod manifest;
pub mod verify;

pub use builder::{
    build_image, BuildOptions, BuildSummary, ImageLayout, CHUNKS_DIR, MANIFEST_FILE,
};
pub use manifest::{
    hash_bytes, Chunk, Manifest, Node, NodeKind, SourceStats, FORMAT_VERSION, HASH_ALGORITHM,
    MAX_CHUNK_SIZE,
};
pub use verify::{
    inspect_image, load_manifest, read_node_range, verify_image, InspectSummary, VerifySummary,
};
