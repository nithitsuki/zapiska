# Development

## Requirements

- Rust 1.85 or later.
- Cargo.
- A Unix shell for the local service commands.

`rusqlite` uses its `bundled` feature. A system SQLite development package is
not required for the application build.

## Setup

```sh
git clone https://github.com/nithitsuki/zapiska.git
cd zapiska
cargo build
```

## Run locally

```sh
export ADMIN_TOKEN=test-token
cargo run
```

Check the server in another shell:

```sh
curl http://127.0.0.1:3000/healthz
curl http://127.0.0.1:3000/swagger-ui/
```

The server loads `.env` from the working directory when the file exists.

## Feature builds

The default build enables `comments` and `webmentions`.

The `comments` feature is empty at present. It remains in the feature list for
the comments-only build command.

Build the comments-only variant with:

```sh
cargo build --release --no-default-features --features comments
```

The `webmentions` feature adds the worker, microformats parser, SSRF module,
webmention ingress, and webmention-specific avatar fetches.

## Tests

Run the default suite:

```sh
RUSTFLAGS="-D warnings" cargo test
```

Run the comments-only suite:

```sh
RUSTFLAGS="-D warnings" cargo test --no-default-features --features comments
```

The current suite has 342 default-feature tests and 281 comments-only tests.
The count changes when tests change.

The integration targets are:

```sh
cargo test --test e2e
cargo test --test pentest
cargo test --test worker_notify
```

`worker_notify` is compiled only when `webmentions` is enabled.

The test suite uses a temporary directory for each SQLite database. Tests do
not share application data.

## Lint and format

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Run both commands before you stage a change.

## CI

`.github/workflows/ci.yml` runs on pushes to `main`, version tags, and pull
requests to `main`.

The normal jobs are:

- Format and clippy check.
- Default build and test.
- Comments-only build and test.

Version tags also build these targets:

- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`
- `aarch64-apple-darwin`

The release job attaches binary archives to the GitHub release. The Docker job
builds `linux/amd64` and `linux/arm64` images and pushes version, major and
minor, and `latest` tags to GHCR.

Cargo jobs use `Swatinem/rust-cache`. Docker builds use BuildKit cache mounts
and a GitHub Actions cache.

## Design rules

### SQLite work

SQLite work is blocking. Repository methods run it inside `spawn_blocking` so
the Tokio runtime can keep handling network work.

### Repository errors

The repository does not return HTTP status codes. `RepoError` maps to
`AppError` in the HTTP layer.

### Admin token comparison

The admin token uses `subtle::ConstantTimeEq`. The comparison pads both values
to the same length before it checks the bytes.

### Webmention fetches

The webmention client checks the source host and resolved addresses before a
fetch. The redirect policy checks redirect hosts and literal IP addresses.
The client permits HTTP and HTTPS.

### Notifications

`src/notify/` contains Telegram, Slack, Discord, and batcher modules. The
batcher stores open windows in memory. Delivery runs in spawned tasks and does
not change the comment response.

## Test layout

- Inline unit tests live beside the module that they test.
- HTTP integration tests live in `tests/e2e.rs`.
- Adversarial input tests live in `tests/pentest.rs`.
- Webmention notification tests live in `tests/worker_notify.rs`.
- Shared HTTP helpers live in `src/http/test_support.rs`.

The pentest suite checks HTML and XML injection, unsafe URL schemes, SQL
payloads, path traversal, CRLF and NUL input, and reaction payloads.

## Add an endpoint

1. Add the handler in `src/http/`.
2. Add an OpenAPI path with `utoipa`.
3. Add the route in `build_app`.
4. Add schemas to `src/openapi.rs` when needed.
5. Add unit or integration tests.
6. Run format, clippy, and the affected test suites.
7. Update `docs/api.md`, `SPEC.md`, and the relevant user guide.

## Add a configuration value

1. Add the field to `Config` in `src/config.rs`.
2. Parse and validate it in `Config::from_env`.
3. Add a `ConfigError` variant when invalid input can fail startup.
4. Add the value to `.env.example` and `SPEC.md`.
5. Add default, override, validation, and redaction tests.
6. Update `docs/deployment.md` and any feature guide.

## Documentation rules

Project documentation follows ASD-STE100 Issue 9 as far as the technical
content allows.

- Use approved words or project technical terms.
- Use a word with one meaning in one consistent context.
- Use American English spelling.
- Keep a multi-word noun to three words when the technical term allows it.
- Use active voice.
- Use the imperative form for procedures.
- Keep procedure sentences to 20 words or fewer.
- Keep descriptive sentences to 25 words or fewer.
- Use a vertical list for complex text.
- Do not use semicolons.
- Do not use contractions.
- Use `WARNING` for injury or death risk.
- Use `CAUTION` for equipment or data damage risk.

Code, environment variable names, endpoint paths, JSON, and quoted product
terms can use their required technical form. Do not alter a command to satisfy
a prose rule.
