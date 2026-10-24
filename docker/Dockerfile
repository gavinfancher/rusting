# syntax=docker/dockerfile:1.7
#
# Multi-stage build for the two Hearts binaries (coordinator, worker).
#
#   docker buildx build --platform linux/amd64 -f docker/Dockerfile --target worker      -t hearts-worker .
#   docker buildx build --platform linux/amd64 -f docker/Dockerfile --target coordinator -t hearts-coordinator .
#
# Stages:
#   chef        base toolchain + protoc + cargo-chef
#   planner     computes recipe.json (a "dependency-only" description of the workspace)
#   builder     cooks (compiles) dependencies from the recipe, THEN compiles our code.
#               Docker caches the cook layer, so editing source does not recompile deps.
#   coordinator / worker   tiny distroless runtime images, one binary each.

FROM rust:1.99-bookworm AS chef
# protoc is needed by crates/hearts-proto/build.rs (tonic-prost-build).
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# hearts-proto's build.rs reads ../../proto at build time, so it must exist
# before dependencies are cooked.
COPY proto ./proto
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json -p coordinator -p worker
# Now the real sources (also embeds migrations/ and web/ into the coordinator).
COPY . .
# The target dir is part of the image layer (no cache mount) so later stages can COPY from it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release -p coordinator -p worker

# distroless/cc = glibc + libgcc + CA certs, no shell, no package manager.
# ":nonroot" runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot AS coordinator
COPY --from=builder /app/target/release/coordinator /usr/local/bin/coordinator
EXPOSE 8080 50051
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/coordinator"]

FROM gcr.io/distroless/cc-debian12:nonroot AS worker
COPY --from=builder /app/target/release/worker /usr/local/bin/worker
EXPOSE 50052 9090
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/worker"]
