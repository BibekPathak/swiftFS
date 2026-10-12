# Image format

The SwiftFS image is a directory containing a JSON manifest and a set of chunk
objects. The format is **versioned and immutable**: an image is built once and
read many times.

```
image/
├── manifest.json          # versioned manifest (see below)
└── chunks/                # content-addressed chunk objects
    ├── <hash>             # optionally grouped, one file per unique chunk
    └── ...
```

> This document defines the Phase 1 schema. The concrete serde structs and
> builder land in Phase 2; the field set below is the contract they implement.

## Manifest

```jsonc
{
  "format_version": 1,
  "chunk_size": 1048576,
  "hash_algorithm": "blake3",
  "source": { "total_bytes": 123456789, "file_count": 42 },
  "nodes": [
    {
      "inode": 1,
      "path": "/",                 // absolute, normalized, must not escape root
      "kind": "dir",
      "mode": 493,                 // POSIX mode bits
      "mtime": 1700000000,         // seconds since epoch
      "size": 0
    },
    {
      "inode": 7,
      "path": "/model/weights.bin",
      "kind": "file",
      "mode": 420,
      "mtime": 1700000000,
      "size": 2097153,             // ends in a partial chunk
      "chunks": [
        { "offset": 0,       "length": 1048576, "hash": "…", "object": "…", "object_offset": 0 },
        { "offset": 1048576, "length": 1048576, "hash": "…", "object": "…", "object_offset": 0 },
        { "offset": 2097152, "length": 1,       "hash": "…", "object": "…", "object_offset": 0 }
      ]
    }
  ]
}
```

### Field notes

- `format_version` — rejects unknown versions rather than guessing.
- `chunk_size` — default 1 MiB (`swiftfs_core::DEFAULT_CHUNK_SIZE`), configurable.
- `hash_algorithm` — `blake3` by default.
- `nodes[].inode` — stable, unique, non-zero. Root is `1` (`ROOT_INODE`).
- `nodes[].path` — normalized absolute path. Validated against traversal,
  collisions, and duplicates.
- `kind` — `dir` or `file`. Anything else is rejected explicitly.
- `chunks[]` — ordered by file offset; `offset + length` must not exceed
  `size`, and the concatenation of chunk lengths must equal `size`.
- `object` + `object_offset` — where the chunk lives within a chunk object,
  enabling deduplication by content hash.

## Chunking rules

- Regular files are split into `chunk_size` pieces; the final piece may be
  shorter (partial chunk).
- Empty files produce zero chunks and `size == 0`.
- Empty directories are represented with `size == 0`.
- Identical chunks are deduplicated by content hash: multiple files or offsets
  may reference the same `object`.

## Validation requirements

A manifest is untrusted input and is validated before use:

- Reject unknown `format_version`.
- Enforce absolute, normalized, non-escaping paths; reject `..`, absolute
  prefixes inside entries, and path collisions.
- Enforce unique, non-zero inode numbers and a valid root.
- Check all integer conversions and offset/length arithmetic for overflow.
- Check that each chunk's `offset + length` stays within the file `size` and
  that chunks tile the file exactly.
- Reject unsupported file kinds.
- Avoid allocating buffers sized directly from unvalidated chunk lengths.

## Atomic write

The builder writes chunk objects and the manifest to a temporary location and
renames into place, so a crash cannot leave a partially written image that
appears valid. See [`design-decisions.md`](design-decisions.md).
