# zapiska multi-stage build with aggressive buildkit caching.
#
# Cache strategy:
# - Buildkit cache mounts (--mount=type=cache) persist the cargo registry
#   and the full /app/target across rebuilds, so unchanged dependencies are
#   never recompiled. The same mount is shared between the cacher and
#   builder stages within a build.
# - CI persists these mounts across runs with the GHA cache backend
#   (cache-from/cache-to type=gha in the workflow), so tag-to-tag builds
#   reuse dependency layers instead of recompiling ~400 crates.

# 1. Recipe stage — snapshot dependency metadata for cargo-chef
FROM rust:1.97.0-alpine AS planner
RUN apk add --no-cache build-base musl-dev curl
RUN --mount=type=cache,target=/usr/local/cargo/registry cargo install cargo-chef
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# 2. Cacher stage — compile all dependencies once; results land in the
#    shared /app/target cache mount
FROM rust:1.97.0-alpine AS cacher
RUN apk add --no-cache build-base musl-dev curl
RUN --mount=type=cache,target=/usr/local/cargo/registry cargo install cargo-chef
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo chef cook --release --recipe-path recipe.json

# 3. Builder stage — reuses the cached dependency artifacts; only the app
#    crate itself recompiles on source changes. The binary is copied out of
#    the cache mount (mount contents are not part of the layer snapshot).
FROM rust:1.97.0-alpine AS builder
RUN apk add --no-cache build-base musl-dev curl
WORKDIR /app
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && \
    mkdir -p /app/bin && cp /app/target/release/zapiska /app/bin/zapiska

# 4. Final minimal runtime stage
FROM alpine:3.21.3

RUN apk add --no-cache ca-certificates && \
    addgroup -S appgroup && adduser -S appuser -G appgroup

WORKDIR /app
COPY --from=builder /app/bin/zapiska /app/zapiska

# Set up secure persistent storage volume
RUN mkdir /data && chown appuser:appgroup /data
VOLUME /data

USER appuser

EXPOSE 3000

ENV BIND_ADDR=0.0.0.0:3000
ENV DATABASE_PATH=/data/comments.db
ENV RUST_LOG=info

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:3000/healthz >/dev/null 2>&1 || exit 1

CMD ["/app/zapiska"]
