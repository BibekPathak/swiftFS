//! Image builder: traverse a directory, chunk regular files, hash and
//! deduplicate chunks, and write a manifest plus chunk objects atomically.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use swiftfs_core::{Result, SwiftfsError, ROOT_INODE};
use tracing::{info, warn};

use crate::manifest::{
    hash_bytes, Chunk, Manifest, Node, NodeKind, SourceStats, FORMAT_VERSION, HASH_ALGORITHM,
};

/// Name of the manifest file within an image directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Subdirectory within an image directory holding chunk objects.
pub const CHUNKS_DIR: &str = "chunks";

/// On-disk layout of an image.
///
/// Chunk objects are stored one per unique content blob, named `<inode>` with
/// the object id `chunks/<inode>`. Each object contains the chunk bytes at
/// `object_offset == 0`, so chunk objects and chunks are 1:1 in this simple
/// layout. (Deduplication reuses the same object for identical content.)
#[derive(Debug)]
pub struct ImageLayout {
    /// Image root directory.
    pub root: PathBuf,
}

impl ImageLayout {
    /// Create a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Path to `manifest.json`.
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    /// Path to the chunks directory.
    pub fn chunks_dir(&self) -> PathBuf {
        self.root.join(CHUNKS_DIR)
    }

    /// Path to a chunk object by object id.
    pub fn object_path(&self, object: &str) -> PathBuf {
        self.chunks_dir().join(object)
    }
}

/// Options controlling image construction.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Chunk size in bytes. Must be in `1..=MAX_CHUNK_SIZE`.
    pub chunk_size: u32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            chunk_size: crate::manifest::DEFAULT_CHUNK,
        }
    }
}

/// Summary returned by a successful build.
#[derive(Debug, Clone)]
pub struct BuildSummary {
    /// Number of file nodes.
    pub file_count: u64,
    /// Number of directory nodes.
    pub dir_count: u64,
    /// Total logical bytes.
    pub total_bytes: u64,
    /// Number of unique chunk objects written.
    pub unique_chunks: u64,
    /// Total chunks referenced (including duplicates).
    pub total_chunks: u64,
}

/// Build an image from `source` into `output`.
///
/// The source is traversed recursively. Regular files are split into chunks,
/// which are hashed and deduplicated. The image is written to a temporary
/// directory and atomically renamed to `output` on success. If `output` already
/// exists and is non-empty, the build fails rather than clobbering it.
pub fn build_image(source: &Path, output: &Path, options: &BuildOptions) -> Result<BuildSummary> {
    let meta = fs::metadata(source)?;
    if !meta.is_dir() {
        return Err(SwiftfsError::InvalidImage(format!(
            "source {:?} is not a directory",
            source
        )));
    }
    if options.chunk_size == 0 || options.chunk_size > crate::manifest::MAX_CHUNK_SIZE {
        return Err(SwiftfsError::InvalidImage(format!(
            "chunk_size {} out of range",
            options.chunk_size
        )));
    }

    let staging = staging_path_for(output);
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(staging.join(CHUNKS_DIR))?;

    let build_result = build_into(source, &staging, options);
    match build_result {
        Ok(summary) => {
            publish_staged(output, &staging)?;
            Ok(summary)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

fn build_into(source: &Path, staging: &Path, options: &BuildOptions) -> Result<BuildSummary> {
    let layout = ImageLayout::new(staging);

    let mut nodes: Vec<Node> = Vec::new();
    nodes.push(Node {
        inode: ROOT_INODE,
        path: "/".to_string(),
        kind: NodeKind::Dir,
        mode: mode_of(&fs::metadata(source)?),
        mtime: mtime_of(&fs::metadata(source)?),
        size: 0,
        chunks: Vec::new(),
    });

    let mut file_count = 0u64;
    let mut dir_count = 1u64;
    let mut total_bytes = 0u64;
    let mut total_chunks = 0u64;

    // inode -> object id, so identical directory subtrees are still unique but
    // chunk content is deduplicated below via `dedup`.
    let mut next_inode: u64 = ROOT_INODE + 1;
    // content hash -> object id
    let mut dedup: HashMap<String, String> = HashMap::new();
    // Dedup of identical files by (size, content-hash list) is handled naturally
    // because identical chunks map to the same object.
    let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();

    collect_entries(source, &mut entries)?;

    for (rel, abs) in entries {
        let logical_path = format!("/{rel}");
        let meta = fs::symlink_metadata(&abs)?;
        let ft = meta.file_type();

        if ft.is_dir() {
            let inode = next_inode;
            next_inode += 1;
            dir_count += 1;
            nodes.push(Node {
                inode,
                path: logical_path,
                kind: NodeKind::Dir,
                mode: mode_of(&meta),
                mtime: mtime_of(&meta),
                size: 0,
                chunks: Vec::new(),
            });
        } else if ft.is_file() {
            let inode = next_inode;
            next_inode += 1;
            file_count += 1;
            let size = meta.len();
            total_bytes += size;
            let chunks = chunk_file(&abs, size, options.chunk_size, &layout, &mut dedup)?;
            total_chunks += chunks.len() as u64;
            nodes.push(Node {
                inode,
                path: logical_path,
                kind: NodeKind::File,
                mode: mode_of(&meta),
                mtime: mtime_of(&meta),
                size,
                chunks,
            });
        } else if ft.is_symlink() {
            return Err(SwiftfsError::UnsupportedFileType(format!(
                "symlink {:?} (symlinks are not supported)",
                abs
            )));
        } else {
            return Err(SwiftfsError::UnsupportedFileType(format!(
                "special file {:?}",
                abs
            )));
        }
    }

    let unique_chunks = dedup.len() as u64;
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        chunk_size: options.chunk_size,
        hash_algorithm: HASH_ALGORITHM.to_string(),
        source: SourceStats {
            total_bytes,
            file_count,
            dir_count,
        },
        nodes,
    };
    manifest.validate()?;

    let json = manifest.to_json()?;
    let manifest_path = layout.manifest_path();
    let tmp_manifest = manifest_path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp_manifest)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_manifest, &manifest_path)?;

    info!(
        files = file_count,
        dirs = dir_count,
        bytes = total_bytes,
        unique_chunks,
        total_chunks,
        "image built"
    );

    Ok(BuildSummary {
        file_count,
        dir_count,
        total_bytes,
        unique_chunks,
        total_chunks,
    })
}

/// Recursively collect relative-path -> absolute-path entries for regular files
/// and directories. Symlinks are not followed during traversal.
fn collect_entries(dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    collect_entries_inner(dir, Path::new(""), out)
}

fn collect_entries_inner(
    dir: &Path,
    prefix: &Path,
    out: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == MANIFEST_FILE || name == CHUNKS_DIR {
            continue;
        }
        let abs = entry.path();
        let rel = prefix.join(&file_name);
        let meta = fs::symlink_metadata(&abs)?;
        let ft = meta.file_type();

        if ft.is_dir() {
            let rel_str = rel_to_string(&rel);
            out.insert(rel_str, abs.clone());
            collect_entries_inner(&abs, &rel, out)?;
        } else {
            // Files (and symlinks/specials, which are rejected in build_into).
            out.insert(rel_to_string(&rel), abs);
        }
    }
    Ok(())
}

fn rel_to_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Split a file into chunks, writing deduplicated chunk objects and returning
/// the ordered chunk list.
fn chunk_file(
    path: &Path,
    size: u64,
    chunk_size: u32,
    layout: &ImageLayout,
    dedup: &mut HashMap<String, String>,
) -> Result<Vec<Chunk>> {
    let mut file = fs::File::open(path)?;
    let mut chunks = Vec::new();
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; chunk_size as usize];

    while offset < size {
        let remaining = size - offset;
        let want = remaining.min(u64::from(chunk_size)) as usize;
        read_exact_at_most(&mut file, &mut buf[..want])?;
        let data = &buf[..want];
        let hash = hash_bytes(data);

        let object = if let Some(existing) = dedup.get(&hash) {
            existing.clone()
        } else {
            let object_id = hash.clone();
            let object_path = layout.object_path(&object_id);
            write_atomic(&object_path, data)?;
            dedup.insert(hash.clone(), object_id.clone());
            object_id
        };

        chunks.push(Chunk {
            offset,
            length: want as u32,
            hash,
            object,
            object_offset: 0,
        });
        offset += want as u64;
    }
    Ok(chunks)
}

fn read_exact_at_most(file: &mut fs::File, buf: &mut [u8]) -> Result<()> {
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => {
                return Err(SwiftfsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "file shorter than expected while chunking",
                )))
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Write `data` to `path` atomically via a temp file and rename.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn staging_path_for(output: &Path) -> PathBuf {
    let file_name = output
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{file_name}.staging.{}", std::process::id()))
}

/// Publish a fully built staging directory at `output`, refusing to clobber a
/// non-empty destination.
fn publish_staged(output: &Path, staging: &Path) -> Result<()> {
    if output.exists() {
        let mut entries = fs::read_dir(output)?;
        if entries.next().is_some() {
            let _ = fs::remove_dir_all(staging);
            return Err(SwiftfsError::InvalidImage(format!(
                "output {:?} already exists and is not empty",
                output
            )));
        }
        fs::remove_dir(output)?;
    }
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    match fs::rename(staging, output) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Cross-device rename fallback: copy then remove.
            warn!("rename failed ({e}), copying staged image into place");
            copy_dir(staging, output)?;
            fs::remove_dir_all(staging)?;
            Ok(())
        }
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn mtime_of(meta: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
}

#[cfg(not(unix))]
fn mtime_of(_meta: &fs::Metadata) -> i64 {
    0
}
