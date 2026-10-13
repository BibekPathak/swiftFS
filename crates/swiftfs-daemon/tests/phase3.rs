//! Phase 3 tests: manifest index correctness, byte-range reads, EOF,
//! error handling, and (FUSE-gated) real mount integration.
//!
//! Index/read tests run everywhere. Mount integration tests need `/dev/fuse`
//! and are marked `#[ignore]`; run them with `cargo test -- --ignored` on a
//! machine where FUSE mounts are permitted.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use swiftfs_daemon::index::ManifestIndex;
use swiftfs_format::{build_image, load_manifest, BuildOptions};
use swiftfs_storage::{ChunkLocation, ChunkStore, LocalStore};
use tempfile::TempDir;

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn build_sample() -> (TempDir, TempDir, std::path::PathBuf) {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");

    write(&src.path().join("a.txt"), b"hello world");
    write(&src.path().join("empty"), b"");
    let big: Vec<u8> = (0..2_500_000u32).map(|i| (i % 251) as u8).collect();
    write(&src.path().join("big.bin"), &big);
    write(&src.path().join("sub/x"), b"dup");
    write(&src.path().join("sub/y"), b"dup");
    fs::create_dir_all(src.path().join("emptydir")).unwrap();

    build_image(
        src.path(),
        &image,
        &BuildOptions {
            chunk_size: 1 << 20,
        },
    )
    .unwrap();
    (src, out, image)
}

fn index_of(image: &Path) -> Arc<ManifestIndex> {
    Arc::new(ManifestIndex::new(load_manifest(image).unwrap()).unwrap())
}

#[test]
fn index_resolves_root_and_children() {
    let (_src, _out, image) = build_sample();
    let idx = index_of(&image);

    assert_eq!(idx.root(), 1);
    let root = idx.node(1).unwrap();
    assert_eq!(root.path, "/");

    // Root children include a.txt, big.bin, empty, sub, emptydir.
    let names: Vec<&str> = idx
        .children(1)
        .iter()
        .map(|&ino| idx.node(ino).unwrap().path.as_str())
        .collect();
    for want in ["/a.txt", "/big.bin", "/empty", "/sub", "/emptydir"] {
        assert!(names.contains(&want), "missing {want} in {names:?}");
    }

    // lookup by name within root.
    let a = idx.lookup(1, "a.txt").unwrap();
    assert_eq!(idx.node(a).unwrap().path, "/a.txt");
    let sub = idx.lookup(1, "sub").unwrap();
    let x = idx.lookup(sub, "x").unwrap();
    assert_eq!(idx.node(x).unwrap().path, "/sub/x");

    // Unknown names yield None.
    assert!(idx.lookup(1, "nope").is_none());
}

#[test]
fn chunks_for_range_maps_boundaries() {
    let (_src, _out, image) = build_sample();
    let idx = index_of(&image);
    let big = idx.lookup(1, "big.bin").unwrap();
    let node = idx.node(big).unwrap();

    // 2.5 MiB file, 1 MiB chunks -> 3 chunks.
    assert_eq!(node.chunks.len(), 3);

    // Range wholly inside chunk 0.
    let plan = idx.chunks_for_range(node, 10, 100);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].in_chunk_offset, 10);
    assert_eq!(plan[0].len, 100);

    // Range spanning chunk 0 and 1.
    let plan = idx.chunks_for_range(node, 1_000_000, 500_000);
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].len, 48_576); // from 1_000_000 to 1_048_576
    assert_eq!(plan[1].in_chunk_offset, 0);
    assert_eq!(plan[1].len, 451_424);

    // Range past EOF is clamped (returns fewer bytes).
    let plan = idx.chunks_for_range(node, node.size - 10, 1000);
    assert_eq!(plan.iter().map(|p| p.len).sum::<u32>(), 10);

    // Range entirely past EOF yields no chunks.
    assert!(idx.chunks_for_range(node, node.size, 100).is_empty());
}

#[tokio::test]
async fn local_store_reads_exact_object_bytes_and_reports_errors() {
    let (_src, _out, image) = build_sample();
    let idx = index_of(&image);
    let a = idx.lookup(1, "a.txt").unwrap();
    let node = idx.node(a).unwrap();
    let chunk = &node.chunks[0];

    let store = LocalStore::for_image(&image);
    let data = store
        .fetch(&ChunkLocation {
            object: chunk.object.clone(),
            offset: chunk.object_offset,
            length: chunk.length,
        })
        .await
        .unwrap();
    assert_eq!(data, b"hello world");

    // Missing object -> ChunkNotFound.
    let err = store
        .fetch(&ChunkLocation {
            object: "does-not-exist".to_string(),
            offset: 0,
            length: 4,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, swiftfs_core::SwiftfsError::ChunkNotFound(_)));

    // Range beyond object length -> IntegrityFailure.
    let err = store
        .fetch(&ChunkLocation {
            object: chunk.object.clone(),
            offset: 0,
            length: 10_000,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        swiftfs_core::SwiftfsError::IntegrityFailure(_)
    ));
}

#[test]
fn reconstruct_file_matches_source_byte_for_byte() {
    // The index + local store together must reproduce the source file exactly,
    // which is what the FUSE `read` path does.
    let (_src, _out, image) = build_sample();
    let idx = index_of(&image);
    let big = idx.lookup(1, "big.bin").unwrap();
    let node = idx.node(big).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let store = LocalStore::for_image(&image);

    let mut assembled = Vec::new();
    for chunk in &node.chunks {
        let data = rt
            .block_on(store.fetch(&ChunkLocation {
                object: chunk.object.clone(),
                offset: chunk.object_offset,
                length: chunk.length,
            }))
            .unwrap();
        // Verify hash like the FUSE layer does.
        assert_eq!(swiftfs_format::hash_bytes(&data), chunk.hash);
        assembled.extend_from_slice(&data);
    }
    assert_eq!(assembled.len() as u64, node.size);

    let source = fs::read(_src.path().join("big.bin")).unwrap();
    assert_eq!(assembled, source);
}

// ---------------------------------------------------------------------------
// FUSE integration tests (require /dev/fuse; run with --ignored)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod fuse {
    use super::*;
    use swiftfs_daemon::MountConfig;

    fn mount_or_skip(image: &Path, target: &Path) -> Option<swiftfs_daemon::mount::MountedHandle> {
        let mut config = MountConfig::new(image, target);
        config.n_threads = 2;
        match swiftfs_daemon::mount::mount_image_background(&config) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("SKIP: cannot mount FUSE here: {e}");
                None
            }
        }
    }

    #[test]
    #[ignore = "requires /dev/fuse and mount permission"]
    fn mount_list_and_read_roundtrip() {
        let (src, _out, image) = build_sample();
        let mnt = TempDir::new().unwrap();

        let Some(handle) = mount_or_skip(&image, mnt.path()) else {
            return;
        };

        // Listing does not require fetching contents.
        let names: Vec<String> = fs::read_dir(mnt.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"big.bin".to_string()));
        assert!(names.contains(&"sub".to_string()));

        // Read small file.
        let a = fs::read(mnt.path().join("a.txt")).unwrap();
        assert_eq!(a, b"hello world");

        // Empty file.
        let e = fs::read(mnt.path().join("empty")).unwrap();
        assert!(e.is_empty());

        // Large multi-chunk file matches source exactly.
        let big = fs::read(mnt.path().join("big.bin")).unwrap();
        let src_big = fs::read(src.path().join("big.bin")).unwrap();
        assert_eq!(big, src_big);

        // Nested read.
        let x = fs::read(mnt.path().join("sub/x")).unwrap();
        assert_eq!(x, b"dup");

        // Metadata sizes are stable.
        let meta = fs::metadata(mnt.path().join("big.bin")).unwrap();
        assert_eq!(meta.len() as usize, src_big.len());

        handle.unmount().unwrap();
    }

    #[test]
    #[ignore = "requires /dev/fuse and mount permission"]
    fn mount_disallows_writes() {
        let (_src, _out, image) = build_sample();
        let mnt = TempDir::new().unwrap();
        let Some(handle) = mount_or_skip(&image, mnt.path()) else {
            return;
        };

        let err = fs::write(mnt.path().join("a.txt"), b"nope").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ReadOnlyFilesystem);

        let create = fs::File::create(mnt.path().join("new.txt")).unwrap_err();
        assert_eq!(create.kind(), std::io::ErrorKind::ReadOnlyFilesystem);

        handle.unmount().unwrap();
    }

    #[test]
    #[ignore = "requires /dev/fuse and mount permission"]
    fn read_at_chunk_boundaries_and_past_eof() {
        use std::io::{Read, Seek, SeekFrom};

        let (src, _out, image) = build_sample();
        let mnt = TempDir::new().unwrap();
        let Some(handle) = mount_or_skip(&image, mnt.path()) else {
            return;
        };

        let big_src = fs::read(src.path().join("big.bin")).unwrap();
        let mut f = fs::File::open(mnt.path().join("big.bin")).unwrap();

        // Read a window straddling the 1 MiB chunk boundary.
        let mut buf = vec![0u8; 4096];
        f.seek(SeekFrom::Start(1_048_576 - 2048)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, big_src[1_048_576 - 2048..1_048_576 - 2048 + 4096]);

        // Read exactly at EOF yields zero bytes.
        f.seek(SeekFrom::Start(big_src.len() as u64)).unwrap();
        let n = f.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // Read beyond EOF yields zero bytes.
        f.seek(SeekFrom::Start(big_src.len() as u64 + 10_000))
            .unwrap();
        let n = f.read(&mut buf).unwrap();
        assert_eq!(n, 0);

        // A read that starts before EOF but extends past it returns the tail.
        f.seek(SeekFrom::Start(big_src.len() as u64 - 100)).unwrap();
        let n = f.read(&mut buf).unwrap();
        assert_eq!(n, 100);
        assert_eq!(&buf[..100], &big_src[big_src.len() - 100..]);

        // Close the file handle before unmounting; an open fd can block umount.
        drop(f);

        handle.unmount().unwrap();
    }

    #[test]
    #[ignore = "requires /dev/fuse and mount permission"]
    fn repeated_mount_unmount_is_clean() {
        let (_src, _out, image) = build_sample();
        for _ in 0..5 {
            let mnt = TempDir::new().unwrap();
            let Some(handle) = mount_or_skip(&image, mnt.path()) else {
                return;
            };
            assert!(mnt.path().join("a.txt").exists());
            handle.unmount().unwrap();
            // After unmount the directory is empty again.
            assert!(fs::read_dir(mnt.path()).unwrap().next().is_none());
        }
    }

    fn mount_with_read_ahead(
        image: &Path,
        target: &Path,
        read_ahead: bool,
    ) -> Option<swiftfs_daemon::mount::MountedHandle> {
        let mut config = MountConfig::new(image, target);
        config.n_threads = 2;
        config.read_ahead = read_ahead;
        match swiftfs_daemon::mount::mount_image_background(&config) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("SKIP: cannot mount FUSE here: {e}");
                None
            }
        }
    }

    #[test]
    #[ignore = "requires /dev/fuse and mount permission"]
    fn read_ahead_returns_correct_bytes_on_and_off() {
        // Read-ahead must never change the bytes a sequential read returns.
        let (src, _out, image) = build_sample();
        let src_big = fs::read(src.path().join("big.bin")).unwrap();

        for enabled in [true, false] {
            let mnt = TempDir::new().unwrap();
            let Some(handle) = mount_with_read_ahead(&image, mnt.path(), enabled) else {
                return;
            };

            // Read sequentially in 256 KiB windows to exercise the heuristic.
            use std::io::{Read, Seek, SeekFrom};
            let mut f = fs::File::open(mnt.path().join("big.bin")).unwrap();
            let mut assembled = Vec::new();
            let mut buf = vec![0u8; 256 * 1024];
            f.seek(SeekFrom::Start(0)).unwrap();
            loop {
                let n = f.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                assembled.extend_from_slice(&buf[..n]);
            }
            drop(f);
            assert_eq!(
                assembled, src_big,
                "read-ahead={enabled} changed file contents"
            );
            handle.unmount().unwrap();
        }
    }
}
