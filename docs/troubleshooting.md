# Troubleshooting

## `fusermount3: command not found` / permission denied on mount

SwiftFS uses FUSE. Ensure:

- `/dev/fuse` exists and is accessible (this environment has `crw-rw-rw-`).
- The `fuse3` userspace tools are installed (`fusermount3`).
- Your user can mount FUSE filesystems (typically via the `fuse` group).

Tests that require an actual mount are marked `#[ignore]` and run explicitly
with `cargo test -- --ignored`; a CI job installs `fuse3`, asserts `/dev/fuse`
exists, and treats a failed mount test as a failure rather than skipping it.

## `pkg-config` cannot find fuse3

`fuser` links against system `libfuse3`. Install the development package
(e.g. `fuse3-devel` / `libfuse3-dev`) so `pkg-config --modversion fuse3`
succeeds.

## HTTP range requests are not supported by the server

The HTTP backend requires `Range` support (`206 Partial Content`). If a server
ignores `Range` and returns `200` with the full body, the backend validates the
response and reports a clear error rather than silently serving wrong bytes.

## Mount hangs or unmount leaves a stale mount

Use `swiftfs unmount <target>`. As a fallback, `fusermount3 -u <target>`
or `umount <target>`. Check `dmesg` for FUSE-related kernel messages.

## `--disk-cache-bytes` has no effect

`--disk-cache-bytes` only applies when a disk cache directory is given:
pass `--disk-cache-dir DIR` (the byte limit defaults to 1 GiB). Supplying
`--disk-cache-bytes` without `--disk-cache-dir` is a configuration error.
