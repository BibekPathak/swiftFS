# Reproducible build environment for SwiftFS.
# Uses the same Rust toolchain channel as rust-toolchain.toml.
FROM rust:1.93-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --workspace

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends fuse3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/swiftfs /usr/local/bin/swiftfs
COPY --from=builder /src/target/release/swiftfs-bench /usr/local/bin/swiftfs-bench
ENTRYPOINT ["/usr/local/bin/swiftfs"]
CMD ["--help"]
