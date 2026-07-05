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
cargo test              # all tests (188 unit + 4 e2e)
cargo test --lib        # unit tests only
cargo test --test e2e   # e2e integration tests only
```

Tests use `tempfile::tempdir()` for isolated SQLite databases. No shared state.

## Feature flags

By default both `comments` and `webmentions` features are enabled. To run tests for only one feature:

```sh
cargo test --no-default-features --features comments   # 143 unit + 3 e2e
cargo test --no-default-features --features webmentions # 188 unit + 4 e2e
```

The `webmentions` feature adds tests for SSRF protection, microformats parsing, the webmention worker, and the webmention ingress endpoint.

## Linting

```sh
cargo clippy -- -D warnings
cargo fmt --check
```

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

SQLite is blocking I/O. Running it on the tokio runtime would block the event loop. Every `CommentsRepo` method wraps queries in `spawn_blocking`, moving work to a dedicated thread pool.

### Separate RepoError

The DB layer should not know about HTTP status codes. `RepoError` has `Internal` and `NotFound` variants. A `From<RepoError> for AppError` impl maps them to 500/404. Keeps the DB layer testable without axum in scope.

### Constant-time admin token

Naive `==` short-circuits on the first differing byte, leaking the token prefix through timing. `subtle::ConstantTimeEq` compares all bytes regardless of mismatch position. Both values zero-padded to the same length.

### Custom SSRF redirect policy

An attacker could send a webmention with a `source` URL that redirects to `http://169.254.169.254/` (AWS metadata). The custom policy re-checks each redirect target against the IP blocklist before following.

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
4. Add to the table in `README.md` and `docs/deployment.md`.
5. Test in `config::tests`.
