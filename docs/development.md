# Development

## Prerequisites

- Rust 1.85+ (edition 2024)
- libsqlite3-dev and pkg-config

```sh
sudo apt-get install libsqlite3-dev pkg-config
```

## Setup

```sh
git clone <repo-url>
cd zapiska
cargo build
```

## Running tests

```sh
cargo test              # all tests (249 unit + 5 integration)
cargo test --lib        # unit tests only
cargo test --test e2e   # e2e integration tests only
```

Tests use `tempfile::tempdir()` for isolated SQLite databases. No shared state.

## Feature flags

By default both `comments` and `webmentions` features are enabled. To run tests for only one feature:

```sh
cargo test --no-default-features --features comments     # 193 tests
cargo test --no-default-features --features webmentions  # 254 tests
```

The `webmentions` feature adds tests for SSRF protection, microformats parsing, the webmention worker, and the webmention ingress endpoint.

## Linting

```sh
cargo clippy -- -D warnings
cargo fmt --check
```

## CI/CD (GitHub Actions)

`.github/workflows/ci.yml` runs on push to `main` and on PRs:

- **lint** — fmt + clippy (`-D warnings`).
- **test** — default features, full suite.
- **test-comments-only** — `--no-default-features --features comments`; catches feature-gate regressions (the `webmentions` feature gates whole modules).
- On `v*` tags only: **release** builds six cross-platform binaries (linux gnu/musl/arm64, windows msvc, macos arm64/x86_64) and attaches tarballs to the GitHub release; **docker-publish** builds a multi-arch (amd64/arm64) image and pushes `ghcr.io/<repo>:{version}`, `{major}.{minor}`, and `latest`.

`rusqlite` is compiled with the `bundled` feature, so no system SQLite is needed on any runner (or in the Docker build).

## Running locally

```sh
export ADMIN_TOKEN=test
cargo run
# then:
curl http://127.0.0.1:3000/healthz
curl http://127.0.0.1:3000/swagger-ui/
```

## Design notes

### spawn_blocking for SQL

SQLite is blocking I/O. Running it on the tokio runtime would block the event loop. Every `Repo` method wraps queries in `spawn_blocking`, moving work to a dedicated thread pool.

### Separate RepoError

The DB layer should not know about HTTP status codes. `RepoError` has `Internal` and `NotFound` variants. A `From<RepoError> for AppError` impl maps them to 500/404. Keeps the DB layer testable without axum in scope.

### Constant-time admin token

Naive `==` short-circuits on the first differing byte, leaking the token prefix through timing. `subtle::ConstantTimeEq` compares all bytes regardless of mismatch position. Both values zero-padded to the same length.

### Custom SSRF redirect policy

An attacker could send a webmention with a `source` URL that redirects to `http://169.254.169.254/` (AWS metadata). The custom policy re-checks each redirect target against the IP blocklist before following.

### Notification channels & batching

`src/notify/` is one module per channel (`telegram.rs`, `slack.rs`, `discord.rs`), each owning its wire format and escaping, plus a shared `batcher.rs` that collects comments into per-page (or global) windows and flushes one digest per window (`NOTIFY_BATCH_SECS`, `NOTIFY_BATCH_THRESHOLD`, `NOTIFY_BATCH_GRANULARITY`). `0` window = immediate delivery. All sends are fire-and-forget tasks with a 10s timeout — a failing channel never affects the request. Adding a channel = new file in `notify/`, a config field, and a branch in `deliver_new_comment` / `deliver_digest_to_channels`.

## Test structure

- **Unit tests**: inline (`#[cfg(test)] mod tests`) in each module.
- **Integration tests**: in `tests/e2e.rs`. Real server on a random port, full lifecycle via `reqwest`.
- **Test helpers** in `src/http/test_support.rs`: `test_state()`, `request()`, `form_request()`.

## Adding an endpoint

1. Handler function in `src/http/`.
2. `#[utoipa::path(...)]` for OpenAPI.
3. Route to `build_app()` in `src/http/mod.rs`.
4. Schemas to `src/openapi.rs`.
5. Tests (inline or in `tests/e2e.rs`).
6. `cargo clippy -- -D warnings && cargo test`.

## Adding a config variable

1. Field on `Config` in `src/config.rs`.
2. Parse in `Config::from_env()` with default + validation.
3. `ConfigError` variant if validation can fail.
4. Add to the table in `SPEC.md` (configuration section) and `.env.example`.
5. Test in `config::tests` (defaults, overrides, validation failures, redaction).
