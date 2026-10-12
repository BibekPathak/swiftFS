//! Integration tests for image building, validation, inspection, and verify.

use std::fs;
use std::path::Path;

use swiftfs_core::SwiftfsError;
use swiftfs_format::{
    build_image, inspect_image, load_manifest, read_node_range, verify_image, BuildOptions,
    NodeKind,
};
use tempfile::TempDir;

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn opts(chunk_size: u32) -> BuildOptions {
    BuildOptions { chunk_size }
}

#[test]
fn builds_nested_tree_and_preserves_structure() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");

    write(&src.path().join("a.txt"), b"hello");
    write(&src.path().join("sub/b.txt"), b"world");
    write(&src.path().join("sub/deep/c.bin"), &[0u8; 10]);
    fs::create_dir_all(src.path().join("empty_dir")).unwrap();

    let summary = build_image(src.path(), &image, &opts(4)).unwrap();
    assert_eq!(summary.file_count, 3);

    let manifest = load_manifest(&image).unwrap();
    assert!(manifest.node_by_path("/a.txt").is_some());
    assert!(manifest.node_by_path("/sub").is_some());
    assert!(manifest.node_by_path("/sub/deep/c.bin").is_some());
    let empty = manifest.node_by_path("/empty_dir").unwrap();
    assert_eq!(empty.kind, NodeKind::Dir);
    assert_eq!(empty.size, 0);
    assert!(empty.chunks.is_empty());
}

#[test]
fn empty_file_produces_no_chunks() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("empty"), b"");

    build_image(src.path(), &image, &opts(1024)).unwrap();
    let manifest = load_manifest(&image).unwrap();
    let node = manifest.node_by_path("/empty").unwrap();
    assert_eq!(node.size, 0);
    assert!(node.chunks.is_empty());
}

#[test]
fn partial_final_chunk_is_preserved() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    // 10 bytes with chunk size 4 -> chunks of 4, 4, 2.
    write(&src.path().join("f"), b"0123456789");

    build_image(src.path(), &image, &opts(4)).unwrap();
    let manifest = load_manifest(&image).unwrap();
    let node = manifest.node_by_path("/f").unwrap();
    let lengths: Vec<u32> = node.chunks.iter().map(|c| c.length).collect();
    assert_eq!(lengths, vec![4, 4, 2]);
    assert_eq!(node.chunks[2].offset, 8);
}

#[test]
fn identical_chunks_are_deduplicated() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    let block = vec![7u8; 4096];
    write(&src.path().join("one"), &block);
    write(&src.path().join("two"), &block);

    let summary = build_image(src.path(), &image, &opts(4096)).unwrap();
    assert_eq!(summary.total_chunks, 2);
    assert_eq!(summary.unique_chunks, 1);

    // Only one object file should exist.
    let objects = fs::read_dir(image.join("chunks")).unwrap().count();
    assert_eq!(objects, 1);
}

#[test]
fn byte_ranges_match_source_exactly() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");

    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    write(&src.path().join("blob.bin"), &data);

    build_image(src.path(), &image, &opts(1024)).unwrap();
    let manifest = load_manifest(&image).unwrap();
    let node = manifest.node_by_path("/blob.bin").unwrap();

    // Full file.
    let full = read_node_range(&image, node, 0, data.len() as u64).unwrap();
    assert_eq!(full, data);

    // Several ranges, including across chunk boundaries.
    for (off, len) in [
        (0u64, 500u64),
        (1000, 100),
        (1023, 3),
        (2048, 4096),
        (9999, 1),
    ] {
        let got = read_node_range(&image, node, off, len).unwrap();
        assert_eq!(
            got,
            &data[off as usize..(off + len) as usize],
            "range {off}..{}",
            off + len
        );
    }
}

#[test]
fn verify_succeeds_on_valid_image() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("x"), b"some content here");
    build_image(src.path(), &image, &opts(8)).unwrap();

    let summary = verify_image(&image).unwrap();
    assert!(summary.scanned_objects >= 1);
}

#[test]
fn verify_detects_corrupted_object() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("x"), b"some content here");
    build_image(src.path(), &image, &opts(8)).unwrap();

    // Corrupt the first object.
    let mut objects: Vec<_> = fs::read_dir(image.join("chunks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    objects.sort();
    let victim = &objects[0];
    let mut bytes = fs::read(victim).unwrap();
    bytes[0] ^= 0xff;
    fs::write(victim, bytes).unwrap();

    let err = verify_image(&image).unwrap_err();
    assert!(
        matches!(err, SwiftfsError::IntegrityFailure(_)),
        "got {err:?}"
    );
}

#[test]
fn verify_detects_truncated_object() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("x"), b"content-longer-than-one-chunk");
    build_image(src.path(), &image, &opts(4)).unwrap();

    let objects: Vec<_> = fs::read_dir(image.join("chunks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let victim = &objects[0];
    fs::write(victim, b"").unwrap(); // truncate to empty

    let err = verify_image(&image).unwrap_err();
    assert!(
        matches!(err, SwiftfsError::IntegrityFailure(_)),
        "got {err:?}"
    );
}

#[test]
fn rejects_symlinks_explicitly() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("real"), b"data");

    #[cfg(unix)]
    std::os::unix::fs::symlink(src.path().join("real"), src.path().join("link")).unwrap();

    #[cfg(unix)]
    {
        let err = build_image(src.path(), &image, &opts(1024)).unwrap_err();
        assert!(
            matches!(err, SwiftfsError::UnsupportedFileType(_)),
            "got {err:?}"
        );
    }
}

#[test]
fn rejects_non_empty_existing_output() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("a"), b"a");
    fs::create_dir_all(&image).unwrap();
    write(&image.join("pre-existing"), b"do not clobber");

    let err = build_image(src.path(), &image, &opts(1024)).unwrap_err();
    assert!(matches!(err, SwiftfsError::InvalidImage(_)), "got {err:?}");
}

#[test]
fn rejects_malformed_manifest_json() {
    let err = swiftfs_format::Manifest::from_json(b"not json").unwrap_err();
    assert!(matches!(err, SwiftfsError::InvalidImage(_)));
}

#[test]
fn rejects_unknown_format_version() {
    let json = br#"{
        "format_version": 999,
        "chunk_size": 1048576,
        "hash_algorithm": "blake3",
        "source": {"total_bytes":0,"file_count":0,"dir_count":1},
        "nodes": [{"inode":1,"path":"/","kind":"dir","mode":493,"mtime":0,"size":0,"chunks":[]}]
    }"#;
    let err = swiftfs_format::Manifest::from_json(json).unwrap_err();
    assert!(matches!(err, SwiftfsError::InvalidImage(_)), "got {err:?}");
}

#[test]
fn rejects_path_traversal_in_manifest() {
    let json = br#"{
        "format_version": 1,
        "chunk_size": 1048576,
        "hash_algorithm": "blake3",
        "source": {"total_bytes":0,"file_count":0,"dir_count":1},
        "nodes": [
            {"inode":1,"path":"/","kind":"dir","mode":493,"mtime":0,"size":0,"chunks":[]},
            {"inode":2,"path":"/../etc/passwd","kind":"file","mode":420,"mtime":0,"size":0,"chunks":[]}
        ]
    }"#;
    let err = swiftfs_format::Manifest::from_json(json).unwrap_err();
    assert!(matches!(err, SwiftfsError::PathRejected(_)), "got {err:?}");
}

#[test]
fn rejects_duplicate_inode() {
    let json = br#"{
        "format_version": 1,
        "chunk_size": 1048576,
        "hash_algorithm": "blake3",
        "source": {"total_bytes":0,"file_count":0,"dir_count":1},
        "nodes": [
            {"inode":1,"path":"/","kind":"dir","mode":493,"mtime":0,"size":0,"chunks":[]},
            {"inode":1,"path":"/x","kind":"file","mode":420,"mtime":0,"size":0,"chunks":[]}
        ]
    }"#;
    let err = swiftfs_format::Manifest::from_json(json).unwrap_err();
    assert!(matches!(err, SwiftfsError::InvalidImage(_)), "got {err:?}");
}

#[test]
fn rejects_inconsistent_chunk_tiling() {
    // size says 10 but chunks cover only 4.
    let json = br#"{
        "format_version": 1,
        "chunk_size": 1048576,
        "hash_algorithm": "blake3",
        "source": {"total_bytes":0,"file_count":1,"dir_count":1},
        "nodes": [
            {"inode":1,"path":"/","kind":"dir","mode":493,"mtime":0,"size":0,"chunks":[]},
            {"inode":2,"path":"/x","kind":"file","mode":420,"mtime":0,"size":10,
             "chunks":[{"offset":0,"length":4,"hash":"0000000000000000000000000000000000000000000000000000000000000000","object":"o","object_offset":0}]}
        ]
    }"#;
    let err = swiftfs_format::Manifest::from_json(json).unwrap_err();
    assert!(matches!(err, SwiftfsError::InvalidImage(_)), "got {err:?}");
}

#[test]
fn inspect_reports_consistent_counts() {
    let src = TempDir::new().unwrap();
    let out = TempDir::new().unwrap();
    let image = out.path().join("img");
    write(&src.path().join("a"), &[1u8; 100]);
    write(&src.path().join("b"), &[2u8; 100]);
    build_image(src.path(), &image, &opts(64)).unwrap();

    let s = inspect_image(&image).unwrap();
    assert_eq!(s.file_count, 2);
    assert_eq!(s.total_bytes, 200);
    assert_eq!(s.chunk_count, 4); // 2 files x ceil(100/64)=2
    assert!(s.unique_chunk_count <= s.chunk_count);
}

mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(60))]

        // Reading any valid range returns the source bytes exactly.
        #[test]
        fn arbitrary_ranges_match_source(
            data in proptest::collection::vec(any::<u8>(), 0..5000),
            chunk_size in 1u32..512,
            offset in 0u64..5000,
        ) {
            let src = TempDir::new().unwrap();
            let out = TempDir::new().unwrap();
            let image = out.path().join("img");
            write(&src.path().join("f"), &data);
            build_image(src.path(), &image, &opts(chunk_size)).unwrap();

            let manifest = load_manifest(&image).unwrap();
            let node = manifest.node_by_path("/f").unwrap();

            let offset = offset.min(data.len() as u64);
            let max_len = data.len() as u64 - offset;
            let len = if max_len == 0 { 0 } else { offset % (max_len + 1) };
            let len = len.min(max_len);

            let got = read_node_range(&image, node, offset, len).unwrap();
            prop_assert_eq!(got, &data[offset as usize..(offset + len) as usize]);
        }

        // Every byte of the file is reachable through single-byte reads.
        #[test]
        fn single_byte_reads_roundtrip(
            data in proptest::collection::vec(any::<u8>(), 0..800),
            chunk_size in 1u32..64,
        ) {
            let src = TempDir::new().unwrap();
            let out = TempDir::new().unwrap();
            let image = out.path().join("img");
            write(&src.path().join("f"), &data);
            build_image(src.path(), &image, &opts(chunk_size)).unwrap();

            let manifest = load_manifest(&image).unwrap();
            let node = manifest.node_by_path("/f").unwrap();

            for (i, expected) in data.iter().enumerate() {
                let got = read_node_range(&image, node, i as u64, 1).unwrap();
                prop_assert_eq!(got[0], *expected);
            }
        }
    }
}
