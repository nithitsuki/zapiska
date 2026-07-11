# 1. Recipe stage to prepare dependency cooking
FROM rust:1.86.0-alpine AS planner
RUN apk add --no-cache build-base musl-dev pkgconfig
WORKDIR /app
RUN cargo install cargo-chef
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# 2. Cacher stage to build and cache dependencies
FROM rust:1.86.0-alpine AS cacher
RUN apk add --no-cache build-base musl-dev pkgconfig
WORKDIR /app
RUN cargo install cargo-chef
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# 3. Builder stage to build the actual application
FROM rust:1.86.0-alpine AS builder
RUN apk add --no-cache build-base musl-dev pkgconfig
WORKDIR /app
COPY . .
# Copy pre-compiled dependencies from the cacher stage
COPY --from=cacher /app/target /app/target
RUN cargo build --release

# 4. Final minimal runtime stage
FROM alpine:3.21.3

RUN apk add --no-cache ca-certificates sqlite-libs && \
    addgroup -S appgroup && adduser -S appuser -G appgroup

WORKDIR /app
COPY --from=builder /app/target/release/zapiska /app/zapiska

# Set up secure persistent storage volume
RUN mkdir /data && chown appuser:appgroup /data
VOLUME /data

USER appuser

EXPOSE 3000

ENV BIND_ADDR=0.0.0.0:3000
ENV DATABASE_PATH=/data/comments.db
ENV RUST_LOG=info

CMD ["/app/zapiska"]
