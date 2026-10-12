# Troubleshooting

## `fusermount3: command not found` / permission denied on mount

SwiftFS uses FUSE. Ensure:

- `/dev/fuse` exists and is accessible (this environment has `crw-rw-rw-`).
- The `fuse3` userspace tools are installed (`fusermount3`).
- Your user can mount FUSE filesystems (typically via the `fuse` group).

Tests that require an actual mount are marked separately from unprivileged unit
tests; CI that cannot mount will skip them explicitly rather than silently pass.

## `pkg-config` cannot find fuse3

`fuser` links against system `libfuse3`. Install the development package
(e.g. `fuse3-devel` / `libfuse3-dev`) so `pkg-config --modversion fuse3`
succeeds.

## HTTP range requests are not supported by the server

The HTTP backend requires `Range` support (`206 Partial Content`). If a server
ignores `Range` and returns `200` with the full body, the backend validates the
response and reports a clear error rather than silently serving wrong bytes.

## Mount hangs or unmount leaves a stale mount

Use `swiftfs unmount <target>` (Phase 3). As a fallback, `fusermount3 -u <target>`
or `umount <target>`. Check `dmesg` for FUSE-related kernel messages.
